//! Console scripting: tokenizer, command buffer, aliases, the command
//! registry and the built-in script commands.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::console::qstr;
use crate::cvar::Cvars;
use crate::info::{self, MAX_INFO_STRING, ServerInfo};
use crate::{console, cprint, cvar, fs, parse};

/// Caps the command buffer so a self-referencing alias cannot eat all memory.
const MAX_CMD_BUF: usize = 1 << 20;

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

    if data[i] == b'"' {
        let start = i + 1;
        let end = data[start..]
            .iter()
            .position(|&b| b == b'"')
            .map_or(data.len(), |n| start + n);
        let rest = data.get(end + 1..).unwrap_or(&[]);
        return Some((data[start..end].to_vec(), rest));
    }

    let end = data[i..]
        .iter()
        .position(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(data.len(), |n| i + n);
    Some((data[i..end].to_vec(), &data[end..]))
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
    wait: bool,
}

impl Cbuf {
    pub fn add_text(&mut self, text: &[u8]) {
        if self.text.len() + text.len() > MAX_CMD_BUF {
            cprint!("command buffer full, text dropped\n");
            return;
        }
        self.text.extend_from_slice(text);
    }

    /// Queues text to run before anything already buffered.
    pub fn insert_text(&mut self, text: &[u8]) {
        if self.text.len() + text.len() + 1 > MAX_CMD_BUF {
            cprint!("command buffer full, text dropped\n");
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

        let mut line = self.text[..end].to_vec();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        self.text.drain(..(end + 1).min(self.text.len()));
        Some(line)
    }

    /// Whether a `wait` was requested; clears the request.
    pub fn take_wait(&mut self) -> bool {
        std::mem::take(&mut self.wait)
    }
}

/// A script command: runs on the shell itself.
pub type ShellCmd<X> = fn(&mut Shell<X>, &Args);

/// How a registered command runs: by the shell itself, on the cvars, or by
/// whoever owns the shell, which `X` identifies.
#[derive(Clone, Copy)]
pub enum Command<X> {
    Shell(ShellCmd<X>),
    Cvars(cvar::Cmd),
    External(X),
}

struct Alias {
    name: String,
    value: String,
}

/// The console: variables, the command buffer, aliases and the command
/// registry. Commands it cannot run itself are handed back to the owner.
pub struct Shell<X> {
    pub cvars: Cvars,
    pub serverinfo: ServerInfo,
    pub cbuf: Cbuf,
    aliases: BTreeMap<String, Alias>,
    commands: BTreeMap<String, (&'static str, Command<X>)>,
    exit_requested: bool,
}

impl<X: Copy> Default for Shell<X> {
    fn default() -> Self {
        Self::new()
    }
}

impl<X: Copy> Shell<X> {
    pub fn new() -> Self {
        let mut shell = Shell {
            cvars: Cvars::default(),
            serverinfo: ServerInfo::default(),
            cbuf: Cbuf::default(),
            aliases: BTreeMap::new(),
            commands: BTreeMap::new(),
            exit_requested: false,
        };
        let script: [(&'static str, ShellCmd<X>); 11] = [
            ("exec", cmd_exec),
            ("echo", cmd_echo),
            ("alias", cmd_alias),
            ("wait", cmd_wait),
            ("cmdlist", cmd_cmdlist),
            ("help", cmd_help),
            ("unaliasall", cmd_unaliasall),
            ("unalias", cmd_unalias),
            ("if", cmd_if),
            ("quit", cmd_quit),
            ("serverinfo", cmd_serverinfo),
        ];
        for (name, func) in script {
            shell.register(name, Command::Shell(func));
        }
        for (name, func) in cvar::COMMANDS {
            shell.register(name, Command::Cvars(*func));
        }
        shell
    }

    pub fn register(&mut self, name: &'static str, command: Command<X>) {
        let key = name.to_ascii_lowercase();
        if self.commands.contains_key(&key) {
            cprint!("command {name} is already defined\n");
            return;
        }
        self.commands.insert(key, (name, command));
    }

    /// Whether `quit` has been run.
    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    /// Runs one line. A command owned by someone else is returned, along
    /// with its arguments, for the owner to run.
    pub fn execute_line(&mut self, text: &[u8]) -> Option<(X, Args)> {
        let expanded = self.expand_cvars(text);
        let args = Args::tokenize(&expanded);
        if args.argc() == 0 {
            return None;
        }

        let name = args.arg_str(0).into_owned();
        let key = name.to_ascii_lowercase();
        let external = match self.commands.get(&key).map(|(_, command)| *command) {
            Some(Command::Shell(func)) => {
                func(self, &args);
                None
            }
            Some(Command::Cvars(func)) => {
                func(&mut self.cvars, &args);
                None
            }
            Some(Command::External(target)) => Some((target, args)),
            None => {
                if !self.cvars.console_command(&args) {
                    match self.aliases.get(&key) {
                        Some(alias) => {
                            let value = alias.value.clone();
                            self.cbuf.insert_text(value.as_bytes());
                        }
                        None => cprint!("Unknown command \"{name}\"\n"),
                    }
                }
                None
            }
        };
        // Console commands are the only way cvars change at runtime.
        // Only when it changed: the level is process-wide, and a shell that
        // has no say in it (a test's, say) must not reset it.
        if self.cvars.take_modified("developer") {
            console::set_developer(self.cvars.int("developer"));
        }
        external
    }

    /// Replaces `$cvar` references outside of quotes with the cvar's value.
    /// The longest name prefix that names a cvar wins, so `$hostname_x`
    /// expands `hostname` and keeps `_x`.
    fn expand_cvars(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut quotes = 0;
        let mut i = 0;
        while i < data.len() {
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
            while i < data.len() && data[i] > 32 && data[i] != b'$' {
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
            self.cbuf.insert_text(&build);
        }
    }
}

fn cmd_exec<X: Copy>(shell: &mut Shell<X>, args: &Args) {
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
            shell.cbuf.insert_text(&text);
        }
        None => cprint!("exec: couldn't exec {name}\n"),
    }
}

fn cmd_echo<X: Copy>(_shell: &mut Shell<X>, args: &Args) {
    let line: String = (1..args.argc())
        .map(|i| format!("{} ", qstr(args.arg(i))))
        .collect();
    cprint!("{line}\n");
}

fn cmd_alias<X: Copy>(shell: &mut Shell<X>, args: &Args) {
    if args.argc() == 1 {
        cprint!("Current alias commands:\n");
        for alias in shell.aliases.values() {
            cprint!("{} : {}\n\n", alias.name, alias.value);
        }
        return;
    }

    let name = args.arg_str(1).into_owned();
    let value = args.join(2, args.argc().saturating_sub(1));
    shell
        .aliases
        .insert(name.to_ascii_lowercase(), Alias { name, value });
}

fn cmd_unalias<X: Copy>(shell: &mut Shell<X>, args: &Args) {
    if args.argc() != 2 {
        cprint!("unalias <alias>: erase an existing alias\n");
        return;
    }
    let name = args.arg_str(1);
    if shell.aliases.remove(&name.to_ascii_lowercase()).is_none() {
        cprint!("Unknown alias \"{name}\"\n");
    }
}

fn cmd_unaliasall<X: Copy>(shell: &mut Shell<X>, _args: &Args) {
    shell.aliases.clear();
}

fn cmd_wait<X: Copy>(shell: &mut Shell<X>, _args: &Args) {
    shell.cbuf.wait = true;
}

fn cmd_cmdlist<X: Copy>(shell: &mut Shell<X>, _args: &Args) {
    for (name, _) in shell.commands.values() {
        cprint!("{name}\n");
    }
    cprint!("------------\n{} commands\n", shell.commands.len());
}

fn cmd_help<X: Copy>(_shell: &mut Shell<X>, _args: &Args) {
    cprint!("Use cmdlist to get a list of commands or cvarlist to get a list of variables.\n");
}

/// `quit` shuts down cleanly once the current work is done; with any
/// argument it exits at once.
fn cmd_quit<X: Copy>(shell: &mut Shell<X>, args: &Args) {
    if args.argc() > 1 {
        std::process::exit(0);
    }
    shell.exit_requested = true;
}

/// Examine or change the serverinfo string. Keys backed by a cvar change
/// the cvar; anything else is stored alongside.
fn cmd_serverinfo<X: Copy>(shell: &mut Shell<X>, args: &Args) {
    match args.argc() {
        1 => {
            let rendered = shell.serverinfo.render(&shell.cvars);
            cprint!("Server info settings:\n");
            info::print(&rendered);
            cprint!("[{}/{}]\n", rendered.len(), MAX_INFO_STRING);
        }
        2 => {
            let rendered = shell.serverinfo.render(&shell.cvars);
            let value = info::value_for_key(&rendered, args.arg(1));
            if value.is_empty() {
                cprint!("No such key {}\n", args.arg_str(1));
            } else {
                cprint!(
                    "Serverinfo {}: \"{}\"\n",
                    args.arg_str(1),
                    String::from_utf8_lossy(value)
                );
            }
        }
        3 => {
            let key = args.arg(1);
            if key.first() == Some(&b'*') {
                cprint!("Star variables cannot be changed.\n");
                return;
            }
            let value = args.arg_str(2).into_owned();
            match shell.cvars.find(&args.arg_str(1)) {
                Some(var) if var.flags & cvar::SERVERINFO != 0 => {
                    let name = var.name.clone();
                    shell.cvars.set(&name, &value);
                }
                _ => shell.serverinfo.set(key, value.as_bytes()),
            }
        }
        _ => cprint!("Usage: serverinfo [ <key> [ <value> ] ]\n"),
    }
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

fn cmd_if<X: Copy>(shell: &mut Shell<X>, args: &Args) {
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
    shell.cbuf.insert_text(branch.join(" ").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell whose only external command is `ext`, which reports its arguments.
    fn shell() -> Shell<&'static str> {
        let mut shell = Shell::new();
        shell.register("ext", Command::External("ext"));
        shell
    }

    /// Runs every line in the buffer the way the proxy does.
    fn run_buffer(shell: &mut Shell<&'static str>) -> Vec<String> {
        let mut external = Vec::new();
        while let Some(line) = shell.cbuf.next_line() {
            if let Some((target, args)) = shell.execute_line(&line) {
                external.push(format!("{target} {}", args.join(1, usize::MAX)));
            }
            if shell.cbuf.take_wait() {
                break;
            }
        }
        external
    }

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
        let mut shell = shell();
        let argv: Vec<String> = [
            "qwfwd", "30000", "+set", "hostname", "x", "-foo", "+echo", "hi",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        shell.stuff_cmds(&argv);
        assert_eq!(shell.cbuf.next_line().unwrap(), b"set hostname x ");
        assert_eq!(shell.cbuf.next_line().unwrap(), b"echo hi");
    }

    #[test]
    fn executes_commands_cvars_and_aliases() {
        let mut shell = shell();
        shell.execute_line(b"set foo 12");
        assert_eq!(shell.cvars.int("foo"), 12);
        shell.execute_line(b"foo 13");
        assert_eq!(shell.cvars.int("foo"), 13);
        shell.execute_line(b"alias bump inc foo 2");
        shell.execute_line(b"bump");
        run_buffer(&mut shell);
        assert_eq!(shell.cvars.int("foo"), 15);
        shell.execute_line(b"set bar $foo_suffix");
        assert_eq!(shell.cvars.string("bar"), "15_suffix");
        shell.execute_line(b"set baz \"$foo\"");
        assert_eq!(shell.cvars.string("baz"), "$foo");
    }

    #[test]
    fn external_commands_are_handed_back() {
        let mut shell = shell();
        assert!(shell.execute_line(b"echo internal").is_none());
        let (target, args) = shell.execute_line(b"EXT one two").unwrap();
        assert_eq!(target, "ext");
        assert_eq!(args.join(1, 2), "one two");

        shell.cbuf.add_text(b"ext a; wait; ext b");
        assert_eq!(run_buffer(&mut shell), ["ext a"]);
        assert_eq!(run_buffer(&mut shell), ["ext b"]);
    }

    #[test]
    fn if_command_branches() {
        let mut shell = shell();
        shell.execute_line(b"if 2 > 1 then set a yes else set a no");
        run_buffer(&mut shell);
        assert_eq!(shell.cvars.string("a"), "yes");
        shell.execute_line(b"if x isin xyz set b no else set b yes");
        run_buffer(&mut shell);
        assert_eq!(shell.cvars.string("b"), "no");
        shell.execute_line(b"if 1 == 2 set c never");
        run_buffer(&mut shell);
        assert_eq!(shell.cvars.string("c"), "");
    }

    #[test]
    fn serverinfo_command_edits_cvars_and_extras() {
        let mut shell = shell();
        shell.cvars.get("hostname", "unnamed", cvar::SERVERINFO);
        shell
            .cvars
            .get("*version", "v1", cvar::READONLY | cvar::SERVERINFO);
        shell.execute_line(b"serverinfo hostname proxied");
        assert_eq!(shell.cvars.string("hostname"), "proxied");
        shell.execute_line(b"serverinfo custom yes");
        shell.execute_line(b"serverinfo *version nope");
        let rendered = shell.serverinfo.render(&shell.cvars);
        assert_eq!(info::value_for_key(&rendered, b"hostname"), b"proxied");
        assert_eq!(info::value_for_key(&rendered, b"custom"), b"yes");
        assert_eq!(info::value_for_key(&rendered, b"*version"), b"v1");
    }

    #[test]
    fn developer_cvar_drives_console_verbosity() {
        let mut shell = shell();
        shell.cvars.get("developer", "0", 0);
        shell.execute_line(b"set developer 2");
        assert_eq!(console::developer(), 2);
        shell.execute_line(b"developer 0");
        assert_eq!(console::developer(), 0);
    }

    #[test]
    fn quit_requests_exit() {
        let mut shell = shell();
        assert!(!shell.exit_requested());
        shell.execute_line(b"quit");
        assert!(shell.exit_requested());
    }
}
