//! IP filters: banned networks and "safe" networks that can never be banned.
//!
//! Filters are dotted quads where a zero octet matches anything, so
//! `addip 192.246.40` covers a whole class C.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::Args;
use crate::console::{developer, qstr};
use crate::proxy::Proxy;
use crate::{cprint, dprint, parse};

const LISTIP_NAME: &str = "qwfwd_listip.cfg";
const MAX_IPFILTERS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterKind {
    Ban,
    Safe,
}

impl FilterKind {
    fn label(self) -> &'static str {
        match self {
            FilterKind::Ban => " ban",
            FilterKind::Safe => "safe",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct IpFilter {
    compare: [u8; 4],
    mask: [u8; 4],
    /// Unix time of expiry; zero means permanent.
    expires: f64,
    kind: FilterKind,
}

impl IpFilter {
    fn matches(&self, ip: Ipv4Addr) -> bool {
        ip.octets()
            .iter()
            .zip(self.mask)
            .zip(self.compare)
            .all(|((&octet, mask), compare)| octet & mask == compare)
    }

    fn same_rule(&self, other: &IpFilter) -> bool {
        self.mask == other.mask && self.compare == other.compare
    }

    fn ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.compare)
    }
}

fn parse_filter(s: &[u8]) -> Option<([u8; 4], [u8; 4])> {
    let mut compare = [0u8; 4];
    let mut mask = [0u8; 4];
    let mut i = 0;
    for octet in 0..4 {
        if !s.get(i).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        compare[octet] = parse::atoi(&s[start..i]) as u8;
        if compare[octet] != 0 {
            mask[octet] = 255;
        }
        if i >= s.len() {
            break;
        }
        i += 1;
    }
    Some((compare, mask))
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn padded_ip(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("{:3}.{:3}.{:3}.{:3}", o[0], o[1], o[2], o[3])
}

#[derive(Default)]
pub struct Bans {
    filters: Vec<IpFilter>,
}

impl Bans {
    pub fn is_banned(&self, addr: SocketAddrV4) -> bool {
        let banned = self
            .filters
            .iter()
            .any(|f| f.kind == FilterKind::Ban && f.matches(*addr.ip()));
        if banned && developer() > 1 {
            dprint!("banned {addr}\n");
        }
        banned
    }

    pub fn clean_expired(&mut self) {
        let now = unix_now();
        self.filters.retain(|f| f.expires == 0.0 || f.expires > now);
    }

    fn can_add_ban(&self, filter: &IpFilter) -> bool {
        filter.compare != [0; 4]
            && !self
                .filters
                .iter()
                .any(|f| f.same_rule(filter) && f.kind == FilterKind::Safe)
    }

    fn list(&self, kind: FilterKind) {
        let now = unix_now();
        for (i, f) in self
            .filters
            .iter()
            .enumerate()
            .filter(|(_, f)| f.kind == kind)
        {
            let expiry = if f.expires != 0.0 {
                let mut left = (f.expires - now) as i64;
                let days = left / 86_400;
                left -= days * 86_400;
                let hours = left / 3_600;
                left -= hours * 3_600;
                let minutes = left / 60;
                let seconds = left - minutes * 60;
                if days != 0 {
                    format!("|{days:4}d:{hours:2}h")
                } else if hours != 0 {
                    format!("|{hours:4}h:{minutes:2}m")
                } else {
                    format!("|{minutes:4}m:{seconds:2}s")
                }
            } else {
                "|permanent".to_owned()
            };
            cprint!("{i:3}|{}|{}{expiry}\n", padded_ip(f.ip()), f.kind.label());
        }
    }
}

impl Proxy {
    pub fn register_ban_commands(&mut self) {
        self.cmds.register("addip", cmd_addip);
        self.cmds.register("removeip", cmd_removeip);
        self.cmds.register("listip", cmd_listip);
        self.cmds.register("writeip", cmd_writeip);
        self.cmds.register("banip", cmd_banip);
        self.cmds.register("banremove", cmd_banremove);
        self.cmds.register("banlist", cmd_banlist);
    }

    /// Loads the persisted ban list.
    pub fn ban_init(&mut self) {
        self.cmds
            .cbuf
            .insert_text(format!("exec {LISTIP_NAME}\n").as_bytes());
        self.execute_buffer();
    }
}

fn cmd_addip(proxy: &mut Proxy, args: &Args) {
    let Some((compare, mask)) = parse_filter(args.arg(1)).filter(|(c, _)| *c != [0; 4]) else {
        cprint!("Bad filter address: {}\n", qstr(args.arg(1)));
        return;
    };

    let kind = match args.arg(2) {
        b"" | b"ban" => FilterKind::Ban,
        b"safe" => FilterKind::Safe,
        other => {
            cprint!("Wrong filter type {}, use ban or safe\n", qstr(other));
            return;
        }
    };

    // "+10" bans for ten seconds from now; a bare number is an absolute unix time.
    let when = args.arg(3);
    let (base, when) = match when.strip_prefix(b"+") {
        Some(relative) => (unix_now(), relative),
        None => (0.0, when),
    };
    let expires = parse::float_prefix(when).map_or(0.0, |t| t + base);

    let filter = IpFilter {
        compare,
        mask,
        expires,
        kind,
    };
    let filters = &mut proxy.bans.filters;
    match filters.iter().position(|f| f.same_rule(&filter)) {
        Some(i) => filters[i] = filter,
        None if filters.len() >= MAX_IPFILTERS => cprint!("IP filter list is full\n"),
        None => filters.push(filter),
    }
}

fn cmd_removeip(proxy: &mut Proxy, args: &Args) {
    let Some((compare, mask)) = parse_filter(args.arg(1)) else {
        cprint!("Bad filter address: {}\n", qstr(args.arg(1)));
        return;
    };
    match proxy
        .bans
        .filters
        .iter()
        .position(|f| f.mask == mask && f.compare == compare)
    {
        Some(i) => {
            proxy.bans.filters.remove(i);
            cprint!("Removed.\n");
        }
        None => cprint!("Didn't find {}.\n", qstr(args.arg(1))),
    }
}

fn cmd_listip(proxy: &mut Proxy, _args: &Args) {
    let now = unix_now();
    cprint!("Filter list:\n");
    for f in &proxy.bans.filters {
        let expiry = if f.expires != 0.0 {
            format!(" | {} s", (f.expires - now) as i64)
        } else {
            String::new()
        };
        cprint!("{} | {}{expiry}\n", padded_ip(f.ip()), f.kind.label());
    }
}

fn cmd_writeip(proxy: &mut Proxy, _args: &Args) {
    cprint!("Writing {LISTIP_NAME}.\n");
    let safe_first = |f: &&IpFilter| f.kind == FilterKind::Safe;
    let contents: String = proxy
        .bans
        .filters
        .iter()
        .filter(safe_first)
        .chain(proxy.bans.filters.iter().filter(|f| !safe_first(f)))
        .map(|f| {
            format!(
                "addip {} {} {:.0}\n",
                f.ip(),
                f.kind.label().trim_start(),
                f.expires
            )
        })
        .collect();
    if std::fs::write(LISTIP_NAME, contents).is_err() {
        cprint!("Couldn't open {LISTIP_NAME}\n");
    }
}

fn cmd_banip(proxy: &mut Proxy, args: &Args) {
    if args.argc() < 3 {
        cprint!("usage: {} <ip> <time<s m h d>>\n", args.arg_str(0));
        return;
    }
    let Some((compare, mask)) = parse_filter(args.arg(1)) else {
        cprint!("ban: bad ip address: {}\n", qstr(args.arg(1)));
        return;
    };
    let filter = IpFilter {
        compare,
        mask,
        expires: 0.0,
        kind: FilterKind::Ban,
    };
    if !proxy.bans.can_add_ban(&filter) {
        cprint!("ban: can't ban such ip: {}\n", qstr(args.arg(1)));
        return;
    }

    let spec = args.arg(2);
    let digits = spec
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(spec.len());
    let (amount, unit) = spec.split_at(digits);
    if amount.is_empty() || unit.len() != 1 {
        cprint!("ban: wrong time arg\n");
        return;
    }
    let amount = parse::atoi(amount).clamp(0, 999);
    let multiplier = match unit[0] {
        b's' => 1,
        b'm' => 60,
        b'h' => 60 * 60,
        b'd' => 60 * 60 * 24,
        _ => {
            cprint!("ban: wrong time arg\n");
            return;
        }
    };
    let seconds = amount * multiplier;

    cprint!(
        "{} was banned for {amount}{}\n",
        padded_ip(filter.ip()),
        unit[0] as char
    );
    let plus = if seconds != 0 { "+" } else { "" };
    proxy
        .cmds
        .cbuf
        .add_text(format!("addip {} ban {plus}{seconds}\n", filter.ip()).as_bytes());
    proxy.cmds.cbuf.add_text(b"writeip\n");
}

fn cmd_banremove(proxy: &mut Proxy, args: &Args) {
    if args.argc() < 2 {
        cprint!("usage: {} [banid]\n", args.arg_str(0));
        cmd_banlist(proxy, args);
        return;
    }
    let id = parse::atoi(args.arg(1));
    let Some(filter) = usize::try_from(id)
        .ok()
        .and_then(|i| proxy.bans.filters.get(i))
    else {
        cprint!("Wrong ban id: {id}\n");
        return;
    };
    if filter.kind == FilterKind::Safe {
        cprint!("Can't remove such ban with id: {id}\n");
        return;
    }
    cprint!("{} was unbanned\n", padded_ip(filter.ip()));
    proxy.bans.filters.remove(id as usize);
    proxy.cmds.cbuf.add_text(b"writeip\n");
}

fn cmd_banlist(proxy: &mut Proxy, _args: &Args) {
    if proxy.bans.filters.is_empty() {
        cprint!("Ban list: empty\n");
        return;
    }
    cprint!(
        "Ban list:\n{}\n{:>3}|{:>15}|{:>4}|{:>9}\n",
        "-".repeat(35),
        "id",
        "ip mask",
        "type",
        "expire"
    );
    proxy.bans.list(FilterKind::Safe);
    proxy.bans.list(FilterKind::Ban);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(ip: [u8; 4]) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::from(ip), 27500)
    }

    #[test]
    fn parses_partial_filters() {
        assert_eq!(
            parse_filter(b"192.246.40"),
            Some(([192, 246, 40, 0], [255, 255, 255, 0]))
        );
        assert_eq!(
            parse_filter(b"10.0.0.1"),
            Some(([10, 0, 0, 1], [255, 0, 0, 255]))
        );
        assert_eq!(parse_filter(b"x.1.1.1"), None);
        assert_eq!(parse_filter(b""), None);
    }

    #[test]
    fn bans_match_networks_and_expire() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"addip 192.246.40");
        proxy.execute_line(b"addip 10.1.1.1 safe");
        proxy.execute_line(b"addip 10.2.2.2 ban +0.5");
        assert!(proxy.bans.is_banned(addr([192, 246, 40, 7])));
        assert!(!proxy.bans.is_banned(addr([192, 246, 41, 7])));
        assert!(!proxy.bans.is_banned(addr([10, 1, 1, 1])));
        assert!(proxy.bans.is_banned(addr([10, 2, 2, 2])));
        assert_eq!(proxy.bans.filters.len(), 3);

        std::thread::sleep(std::time::Duration::from_millis(600));
        proxy.bans.clean_expired();
        assert!(!proxy.bans.is_banned(addr([10, 2, 2, 2])));
        assert_eq!(proxy.bans.filters.len(), 2);

        proxy.execute_line(b"removeip 192.246.40");
        assert!(!proxy.bans.is_banned(addr([192, 246, 40, 7])));
    }

    #[test]
    fn banip_respects_safe_list_and_queues_addip() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"addip 10.1.1.1 safe");
        proxy.execute_line(b"banip 10.1.1.1 10m");
        assert!(proxy.cmds.cbuf.next_line().is_none());

        proxy.execute_line(b"banip 10.3.3.3 2h");
        assert_eq!(
            proxy.cmds.cbuf.next_line().unwrap(),
            b"addip 10.3.3.3 ban +7200"
        );
        assert_eq!(proxy.cmds.cbuf.next_line().unwrap(), b"writeip");

        proxy.execute_line(b"banip 10.3.3.3 2x");
        assert!(proxy.cmds.cbuf.next_line().is_none());
    }
}
