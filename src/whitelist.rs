//! Optional allow-list of remote servers clients may be forwarded to.

use std::net::Ipv4Addr;

use crate::cmd::Args;
use crate::proxy::Proxy;
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

impl Proxy {
    pub fn register_whitelist_commands(&mut self) {
        self.cmds.register("whitelist", cmd_whitelist);
        self.cmds.register("whitelistadd", cmd_whitelistadd);
        self.cmds.register("whitelistremove", cmd_whitelistremove);
        self.cmds.register("whitelistpurge", cmd_whitelistpurge);
    }
}

fn parse_ip(args: &Args) -> Option<Ipv4Addr> {
    let text = args.arg_str(1);
    let ip = text.parse().ok();
    if ip.is_none() {
        cprint!("error: invalid IP address {text}\n");
    }
    ip
}

fn cmd_whitelist(proxy: &mut Proxy, _args: &Args) {
    cprint!("whitelist: {} addresses\n", proxy.whitelist.addrs.len());
    for ip in &proxy.whitelist.addrs {
        cprint!("{ip}\n");
    }
}

fn cmd_whitelistadd(proxy: &mut Proxy, args: &Args) {
    if args.argc() != 2 {
        cprint!("usage: whitelistadd <ip>\n");
        return;
    }
    if proxy.whitelist.addrs.len() >= MAX_ADDRS {
        cprint!("error: whitelist is full\n");
        return;
    }
    let Some(ip) = parse_ip(args) else { return };
    if proxy.whitelist.addrs.contains(&ip) {
        cprint!("error: {ip} has already been added to the whitelist\n");
        return;
    }
    proxy.whitelist.addrs.push(ip);
}

fn cmd_whitelistremove(proxy: &mut Proxy, args: &Args) {
    if args.argc() != 2 {
        cprint!("usage: whitelistremove <ip>\n");
        return;
    }
    let Some(ip) = parse_ip(args) else { return };
    match proxy.whitelist.addrs.iter().position(|&a| a == ip) {
        Some(i) => {
            proxy.whitelist.addrs.remove(i);
            cprint!("{ip} removed from whitelist\n");
        }
        None => cprint!("error: {ip} not found in whitelist\n"),
    }
}

fn cmd_whitelistpurge(proxy: &mut Proxy, _args: &Args) {
    proxy.whitelist.purge();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_whitelist_allows_all() {
        let proxy = Proxy::new_for_tests();
        assert!(proxy.whitelist.allows(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn whitelist_commands_manage_entries() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"whitelistadd 10.0.0.1");
        proxy.execute_line(b"whitelistadd not-an-ip");
        assert!(proxy.whitelist.allows(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!proxy.whitelist.allows(Ipv4Addr::new(10, 0, 0, 2)));
        proxy.execute_line(b"whitelistremove 10.0.0.1");
        assert!(proxy.whitelist.allows(Ipv4Addr::new(10, 0, 0, 2)));
        proxy.execute_line(b"whitelistadd 10.0.0.3");
        proxy.execute_line(b"whitelistpurge");
        assert!(proxy.whitelist.addrs.is_empty());
    }
}
