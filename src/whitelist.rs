//! Optional allow-list of remote servers clients may be forwarded to.

use std::net::Ipv4Addr;

use crate::cmd::Args;
use crate::{cprint, dprint};

const MAX_ADDRS: usize = 4096;

#[derive(Default)]
pub struct Whitelist {
    addrs: Vec<Ipv4Addr>,
}

impl Whitelist {
    /// An empty whitelist allows everything.
    pub fn allows(&self, ip: Ipv4Addr) -> bool {
        if self.addrs.is_empty() {
            return true;
        }
        if self.addrs.contains(&ip) {
            dprint!("connection from {ip} allowed: address found in whitelist\n");
            true
        } else {
            dprint!("connection from {ip} dropped: address NOT in whitelist\n");
            false
        }
    }

    pub fn purge(&mut self) {
        self.addrs.clear();
    }
}

pub type Cmd = fn(&mut Whitelist, &Args);

pub const COMMANDS: &[(&str, Cmd)] = &[
    ("whitelist", cmd_whitelist),
    ("whitelistadd", cmd_whitelistadd),
    ("whitelistremove", cmd_whitelistremove),
    ("whitelistpurge", cmd_whitelistpurge),
];

fn parse_ip(args: &Args) -> Option<Ipv4Addr> {
    let text = args.arg_str(1);
    let ip = text.parse().ok();
    if ip.is_none() {
        cprint!("error: invalid IP address {text}\n");
    }
    ip
}

fn cmd_whitelist(whitelist: &mut Whitelist, _args: &Args) {
    cprint!("whitelist: {} addresses\n", whitelist.addrs.len());
    for ip in &whitelist.addrs {
        cprint!("{ip}\n");
    }
}

fn cmd_whitelistadd(whitelist: &mut Whitelist, args: &Args) {
    if args.argc() != 2 {
        cprint!("usage: whitelistadd <ip>\n");
        return;
    }
    if whitelist.addrs.len() >= MAX_ADDRS {
        cprint!("error: whitelist is full\n");
        return;
    }
    let Some(ip) = parse_ip(args) else { return };
    if whitelist.addrs.contains(&ip) {
        cprint!("error: {ip} has already been added to the whitelist\n");
        return;
    }
    whitelist.addrs.push(ip);
}

fn cmd_whitelistremove(whitelist: &mut Whitelist, args: &Args) {
    if args.argc() != 2 {
        cprint!("usage: whitelistremove <ip>\n");
        return;
    }
    let Some(ip) = parse_ip(args) else { return };
    match whitelist.addrs.iter().position(|&a| a == ip) {
        Some(i) => {
            whitelist.addrs.remove(i);
            cprint!("{ip} removed from whitelist\n");
        }
        None => cprint!("error: {ip} not found in whitelist\n"),
    }
}

fn cmd_whitelistpurge(whitelist: &mut Whitelist, _args: &Args) {
    whitelist.purge();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(whitelist: &mut Whitelist, line: &[u8]) {
        let args = Args::tokenize(line);
        let name = args.arg_str(0).into_owned();
        let (_, cmd) = COMMANDS
            .iter()
            .find(|(n, _)| *n == name)
            .expect("known whitelist command");
        cmd(whitelist, &args);
    }

    #[test]
    fn empty_whitelist_allows_all() {
        assert!(Whitelist::default().allows(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn whitelist_commands_manage_entries() {
        let mut whitelist = Whitelist::default();
        run(&mut whitelist, b"whitelistadd 10.0.0.1");
        run(&mut whitelist, b"whitelistadd not-an-ip");
        assert!(whitelist.allows(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!whitelist.allows(Ipv4Addr::new(10, 0, 0, 2)));
        run(&mut whitelist, b"whitelistremove 10.0.0.1");
        assert!(whitelist.allows(Ipv4Addr::new(10, 0, 0, 2)));
        run(&mut whitelist, b"whitelistadd 10.0.0.3");
        run(&mut whitelist, b"whitelistpurge");
        assert!(whitelist.addrs.is_empty());
    }
}
