//! Console command buffer, tokenizer, aliases and the built-in script commands.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::console::qstr;
use crate::proxy::Proxy;
use crate::{cprint, fs, parse};

pub const MAX_ARGS: usize = 80;
pub const MAX_TOKEN: usize = 1024;
const MAX_ALIAS_NAME: usize = 32;
const MAX_CMD_BUF: usize = 1 << 20;
const MAX_LINE: usize = 1024;

pub type CmdFn = fn(&mut Proxy, &Args);

/// A tokenized command line. Tokens are raw bytes because connection packets
/// carry Quake-encoded player names.
#[derive(Debug, Default)]
pub struct Args {
    argv: Vec<Vec<u8>>,
}

impl Args {
    /// Splits a line into tokens; a newline ends the command.
    pub fn tokenize(text: &[u8]) -> Self {
        let mut argv = Vec::new();
        let mut rest = text;
        loop {
            let skip = rest
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t' | b'\r'))
                .unwrap_or(rest.len());
            rest = &rest[skip..];
            if rest.is_empty() || rest[0] == b'\n' {
                break;
            }
            let Some((token, remaining)) = com_parse(rest) else {
                break;
            };
            rest = remaining;
            if argv.len() >= MAX_ARGS {
                break;
            }
            argv.push(token);
        }
        Args { argv }
    }

    pub fn argc(&self) -> usize {
        self.argv.len()
    }

    /// Argument `i`, or empty when out of range.
    pub fn arg(&self, i: usize) -> &[u8] {
        self.argv.get(i).map_or(&[], Vec::as_slice)
    }

    pub fn arg_str(&self, i: usize) -> Cow<'_, str> {
        String::from_utf8_lossy(self.arg(i))
    }

    /// Arguments `from..=to` joined by single spaces.
    pub fn join(&self, from: usize, to: usize) -> String {
        let Some(last) = self.argc().checked_sub(1) else {
            return String::new();
        };
        let to = to.min(last);
        if from > to {
            return String::new();
        }
        self.argv[from..=to]
            .iter()
            .map(|a| String::from_utf8_lossy(a))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Parses one token: skips whitespace and `//` comments, handles quoted
/// strings without escapes. Returns the token and the remaining input.
pub fn com_parse(data: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut i = 0;
    loop {
        while i < data.len() && matches!(data[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }
        if data[i] == b'/' && data.get(i + 1) == Some(&b'/') {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
        } else {
            break;
        }
    }

    let mut token = Vec::new();
    if data[i] == b'"' {
        i += 1;
        while i < data.len() && data[i] != b'"' {
            if token.len() < MAX_TOKEN - 1 {
                token.push(data[i]);
            }
            i += 1;
        }
        if i < data.len() {
            i += 1;
        }
        return Some((token, &data[i..]));
    }

    while i < data.len() && !matches!(data[i], b' ' | b'\t' | b'\r' | b'\n') {
        if token.len() < MAX_TOKEN - 1 {
            token.push(data[i]);
        }
        i += 1;
    }
    Some((token, &data[i..]))
}

/// All tokens of a string, for space separated cvar lists.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.as_bytes();
    while let Some((token, remaining)) = com_parse(rest) {
        out.push(String::from_utf8_lossy(&token).into_owned());
        rest = remaining;
    }
    out
}

/// Pending console text, executed one `\n` or `;` separated command at a time.
#[derive(Default)]
pub struct Cbuf {
    text: Vec<u8>,
    /// Set by the `wait` command to defer the rest of the buffer to the next frame.
    pub wait: bool,
}

impl Cbuf {
    pub fn add_text(&mut self, text: &[u8]) {
        if self.text.len() + text.len() > MAX_CMD_BUF {
            cprint!("Cbuf_AddText: overflow\n");
            return;
        }
        self.text.extend_from_slice(text);
    }

    /// Queues text to run before anything already buffered.
    pub fn insert_text(&mut self, text: &[u8]) {
        if self.text.len() + text.len() + 1 > MAX_CMD_BUF {
            cprint!("Cbuf_InsertText: overflow\n");
            return;
        }
        let mut merged = Vec::with_capacity(self.text.len() + text.len() + 1);
        merged.extend_from_slice(text);
        merged.push(b'\n');
        merged.append(&mut self.text);
        self.text = merged;
    }

    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        if self.text.is_empty() {
            return None;
        }

        let mut quotes = 0;
        let mut end = 0;
        while end < self.text.len() {
            match self.text[end] {
                b'\n' => break,
                b';' if quotes % 2 == 0 => break,
                b'"' => quotes += 1,
                _ => {}
            }
            end += 1;
        }

        let mut line = if end < MAX_LINE {
            self.text[..end].to_vec()
        } else {
            cprint!("Cbuf_ExecuteEx: too long\n");
            Vec::new()
        };
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        self.text.drain(..(end + 1).min(self.text.len()));
        Some(line)
    }
}

struct Alias {
    name: String,
    value: String,
}

#[derive(Default)]
pub struct Commands {
    commands: BTreeMap<String, (&'static str, CmdFn)>,
    aliases: BTreeMap<String, Alias>,
    pub cbuf: Cbuf,
}

impl Commands {
    pub fn register(&mut self, name: &'static str, func: CmdFn) {
        let key = name.to_ascii_lowercase();
        if self.commands.contains_key(&key) {
            cprint!("Cmd_AddCommand: {name} already defined\n");
            return;
        }
        self.commands.insert(key, (name, func));
    }

    fn find(&self, name: &str) -> Option<CmdFn> {
        self.commands
            .get(&name.to_ascii_lowercase())
            .map(|(_, func)| *func)
    }

    fn alias_value(&self, name: &str) -> Option<&str> {
        self.aliases
            .get(&name.to_ascii_lowercase())
            .map(|a| a.value.as_str())
    }
}

impl Proxy {
    pub fn register_script_commands(&mut self) {
        self.cmds.register("exec", cmd_exec);
        self.cmds.register("echo", cmd_echo);
        self.cmds.register("alias", cmd_alias);
        self.cmds.register("wait", cmd_wait);
        self.cmds.register("cmdlist", cmd_cmdlist);
        self.cmds.register("help", cmd_help);
        self.cmds.register("unaliasall", cmd_unaliasall);
        self.cmds.register("unalias", cmd_unalias);
        self.cmds.register("if", cmd_if);
    }

    /// Runs buffered console commands until the buffer is empty or a `wait` is hit.
    pub fn execute_buffer(&mut self) {
        while let Some(line) = self.cmds.cbuf.next_line() {
            self.execute_line(&line);
            if self.cmds.cbuf.wait {
                self.cmds.cbuf.wait = false;
                break;
            }
        }
    }

    pub fn execute_line(&mut self, text: &[u8]) {
        let expanded = self.expand_cvars(text);
        let args = Args::tokenize(&expanded);
        if args.argc() == 0 {
            return;
        }

        let name = args.arg_str(0).into_owned();
        if let Some(func) = self.cmds.find(&name) {
            func(self, &args);
            return;
        }
        if self.cvar_command(&args) {
            return;
        }
        if let Some(value) = self.cmds.alias_value(&name) {
            let value = value.to_owned();
            self.cmds.cbuf.insert_text(value.as_bytes());
            return;
        }
        cprint!("Unknown command \"{name}\"\n");
    }

    /// Replaces `$cvar` references outside of quotes with the cvar's value.
    /// The longest name prefix that names a cvar wins, so `$hostname_x`
    /// expands `hostname` and keeps `_x`.
    fn expand_cvars(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut quotes = 0;
        let mut i = 0;
        while i < data.len() && out.len() < MAX_LINE - 1 {
            let c = data[i];
            if c == b'"' {
                quotes += 1;
            }
            if c != b'$' || quotes % 2 == 1 {
                out.push(c);
                i += 1;
                continue;
            }

            i += 1;
            let start = i;
            let mut best = None;
            while i < data.len() && data[i] > 32 && data[i] != b'$' && i - start < 254 {
                i += 1;
                if let Some(var) = self.cvars.find(&String::from_utf8_lossy(&data[start..i])) {
                    best = Some(var);
                }
            }
            let name = &data[start..i];
            match best {
                Some(var) => {
                    out.extend_from_slice(var.string.as_bytes());
                    out.extend_from_slice(&name[var.name.len()..]);
                }
                None => {
                    out.push(b'$');
                    out.extend_from_slice(name);
                }
            }
        }
        out.truncate(MAX_LINE - 1);
        out
    }

    /// Queues `+command ...` groups from the command line, each running until
    /// the next `+` or `-` argument.
    pub fn stuff_cmds(&mut self, argv: &[String]) {
        let Some(rest) = argv.get(1..) else { return };
        let text = rest.join(" ");
        let bytes = text.as_bytes();
        let mut build = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'+' {
                i += 1;
                continue;
            }
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'+' && bytes[i] != b'-' {
                i += 1;
            }
            build.extend_from_slice(&bytes[start..i]);
            build.push(b'\n');
        }
        if !build.is_empty() {
            self.cmds.cbuf.insert_text(&build);
        }
    }
}

fn cmd_exec(proxy: &mut Proxy, args: &Args) {
    if args.argc() != 2 {
        cprint!("exec <filename> : execute a script file\n");
        return;
    }
    let name = args.arg_str(1);
    if !fs::safe_path(&name) {
        cprint!("exec: absolute paths are prohibited\n");
        return;
    }
    if !fs::file_extension(&name).eq_ignore_ascii_case(".cfg") {
        cprint!("exec: cfg extension required\n");
        return;
    }
    match fs::read_config(&name) {
        Some(text) => {
            cprint!("execing {name}\n");
            proxy.cmds.cbuf.insert_text(&text);
        }
        None => cprint!("exec: couldn't exec {name}\n"),
    }
}

fn cmd_echo(_proxy: &mut Proxy, args: &Args) {
    let line: String = (1..args.argc())
        .map(|i| format!("{} ", qstr(args.arg(i))))
        .collect();
    cprint!("{line}\n");
}

fn cmd_alias(proxy: &mut Proxy, args: &Args) {
    if args.argc() == 1 {
        cprint!("Current alias commands:\n");
        for alias in proxy.cmds.aliases.values() {
            cprint!("{} : {}\n\n", alias.name, alias.value);
        }
        return;
    }

    let name = args.arg_str(1).into_owned();
    if name.len() >= MAX_ALIAS_NAME {
        cprint!("Alias name is too long\n");
        return;
    }
    let value = args.join(2, args.argc().saturating_sub(1));
    proxy
        .cmds
        .aliases
        .insert(name.to_ascii_lowercase(), Alias { name, value });
}

fn cmd_unalias(proxy: &mut Proxy, args: &Args) {
    if args.argc() != 2 {
        cprint!("unalias <alias>: erase an existing alias\n");
        return;
    }
    let name = args.arg_str(1);
    if name.len() >= MAX_ALIAS_NAME {
        cprint!("Alias name is too long\n");
        return;
    }
    if proxy
        .cmds
        .aliases
        .remove(&name.to_ascii_lowercase())
        .is_none()
    {
        cprint!("Unknown alias \"{name}\"\n");
    }
}

fn cmd_unaliasall(proxy: &mut Proxy, _args: &Args) {
    proxy.cmds.aliases.clear();
}

fn cmd_wait(proxy: &mut Proxy, _args: &Args) {
    proxy.cmds.cbuf.wait = true;
}

fn cmd_cmdlist(proxy: &mut Proxy, _args: &Args) {
    for (name, _) in proxy.cmds.commands.values() {
        cprint!("{name}\n");
    }
    cprint!("------------\n{} commands\n", proxy.cmds.commands.len());
}

fn cmd_help(_proxy: &mut Proxy, _args: &Args) {
    cprint!("Use cmdlist to get a list of commands or cvarlist to get a list of variables.\n");
}

fn is_numeric(s: &[u8]) -> bool {
    match s {
        [d, ..] if d.is_ascii_digit() => true,
        [b'-' | b'+', b'.', ..] => true,
        [b'-' | b'+', d, ..] if d.is_ascii_digit() => true,
        [b'.', d, ..] if d.is_ascii_digit() => true,
        _ => false,
    }
}

fn cmd_if(proxy: &mut Proxy, args: &Args) {
    let argc = args.argc();
    if argc < 5 {
        cprint!("usage: if <expr1> <op> <expr2> <command> [else <command>]\n");
        return;
    }

    let (lhs, op, rhs) = (args.arg(1), args.arg(2), args.arg(3));
    let (lhs_f, rhs_f) = (parse::atof(lhs), parse::atof(rhs));
    let result = match op {
        b"==" | b"=" | b"!=" | b"<>" => {
            let equal = if is_numeric(lhs) && is_numeric(rhs) {
                lhs_f == rhs_f
            } else {
                lhs == rhs
            };
            if op[0] == b'=' { equal } else { !equal }
        }
        b">" => lhs_f > rhs_f,
        b"<" => lhs_f < rhs_f,
        b">=" => lhs_f >= rhs_f,
        b"<=" => lhs_f <= rhs_f,
        b"isin" => args.arg_str(3).contains(&*args.arg_str(1)),
        b"!isin" => !args.arg_str(3).contains(&*args.arg_str(1)),
        _ => {
            cprint!("unknown operator: {}\n", qstr(op));
            cprint!("valid operators are ==, =, !=, <>, >, <, >=, <=, isin, !isin\n");
            return;
        }
    };

    let is_else = |i: usize| args.arg(i).eq_ignore_ascii_case(b"else");
    let branch: Vec<Cow<str>> = if result {
        (4..argc)
            .skip_while(|&i| i == 4 && args.arg(i).eq_ignore_ascii_case(b"then"))
            .take_while(|&i| !is_else(i))
            .map(|i| args.arg_str(i))
            .collect()
    } else {
        let Some(else_at) = (4..argc).find(|&i| is_else(i)) else {
            return;
        };
        (else_at + 1..argc).map(|i| args.arg_str(i)).collect()
    };
    proxy.cmds.cbuf.insert_text(branch.join(" ").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_quotes_comments_and_newlines() {
        let args = Args::tokenize(b"connect 28 1234 -5 \"\\name\\a b\" // comment\nnext");
        // A comment swallows its newline, so "next" still belongs to this command.
        assert_eq!(args.argc(), 6);
        assert_eq!(args.arg(0), b"connect");
        assert_eq!(args.arg(3), b"-5");
        assert_eq!(args.arg(4), b"\\name\\a b");
        assert_eq!(args.arg(5), b"next");
        assert_eq!(args.arg(9), b"");
        assert_eq!(args.join(1, 3), "28 1234 -5");
        assert_eq!(args.join(1, 99), "28 1234 -5 \\name\\a b next");
        assert_eq!(args.join(4, 1), "");
        assert_eq!(Args::tokenize(b"  \n").argc(), 0);
        assert_eq!(Args::tokenize(b"a\nb").argc(), 1);
        assert_eq!(Args::tokenize(b"\"unterminated").arg(0), b"unterminated");
    }

    #[test]
    fn tokens_splits_cvar_lists() {
        assert_eq!(
            tokens("a.example:27000 b.example \"c d\""),
            ["a.example:27000", "b.example", "c d"]
        );
    }

    #[test]
    fn cbuf_splits_on_newline_and_semicolon() {
        let mut cbuf = Cbuf::default();
        cbuf.add_text(b"echo a; echo \"b;c\"\r\nset x 1");
        cbuf.insert_text(b"first");
        assert_eq!(cbuf.next_line().unwrap(), b"first");
        assert_eq!(cbuf.next_line().unwrap(), b"echo a");
        assert_eq!(cbuf.next_line().unwrap(), b" echo \"b;c\"");
        assert_eq!(cbuf.next_line().unwrap(), b"set x 1");
        assert!(cbuf.next_line().is_none());
    }

    #[test]
    fn stuff_cmds_extracts_plus_groups() {
        let mut proxy = Proxy::new_for_tests();
        let argv: Vec<String> = [
            "qwfwd", "30000", "+set", "hostname", "x", "-foo", "+echo", "hi",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        proxy.stuff_cmds(&argv);
        assert_eq!(proxy.cmds.cbuf.next_line().unwrap(), b"set hostname x ");
        assert_eq!(proxy.cmds.cbuf.next_line().unwrap(), b"echo hi");
    }

    #[test]
    fn executes_commands_cvars_and_aliases() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"set foo 12");
        assert_eq!(proxy.cvars.int("foo"), 12);
        proxy.execute_line(b"foo 13");
        assert_eq!(proxy.cvars.int("foo"), 13);
        proxy.execute_line(b"alias bump inc foo 2");
        proxy.execute_line(b"bump");
        proxy.execute_buffer();
        assert_eq!(proxy.cvars.int("foo"), 15);
        proxy.execute_line(b"set bar $foo_suffix");
        assert_eq!(proxy.cvars.string("bar"), "15_suffix");
        proxy.execute_line(b"set baz \"$foo\"");
        assert_eq!(proxy.cvars.string("baz"), "$foo");
    }

    #[test]
    fn if_command_branches() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"if 2 > 1 then set a yes else set a no");
        proxy.execute_buffer();
        assert_eq!(proxy.cvars.string("a"), "yes");
        proxy.execute_line(b"if x isin xyz set b no else set b yes");
        proxy.execute_buffer();
        assert_eq!(proxy.cvars.string("b"), "no");
        proxy.execute_line(b"if 1 == 2 set c never");
        proxy.execute_buffer();
        assert_eq!(proxy.cvars.string("c"), "");
    }
}
