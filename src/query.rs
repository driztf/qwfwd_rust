//! Master server registration and the server list clients can query.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use crate::cmd::{self, Args};
use crate::console::developer;
use crate::cvar::Cvars;
use crate::msg::{MSG_BUF_SIZE, MsgWriter};
use crate::protocol::{A2C_PRINT, S2M_HEARTBEAT};
use crate::proxy::Proxy;
use crate::{cprint, dprint, net};

/// How often at most one server gets pinged.
const SERVER_RATE: Duration = Duration::from_millis(100);
const SERVER_PING_QUERY: &[u8] = b"\xff\xff\xff\xffk\n";
/// Minimum interval between pings of the same server.
const SERVER_MIN_PING_INTERVAL: Duration = Duration::from_secs(60);
/// A server that stays silent this long is forgotten.
const SERVER_DEAD_TIME: Duration = Duration::from_secs(60 * 60);

/// Sent with its NUL terminator, as the original did.
const MASTER_QUERY: &[u8] = b"c\n\0";
const MASTER_QUERY_INTERVAL: Duration = Duration::from_secs(60 * 30);
/// Retry interval while a master has not answered yet.
const MASTER_QUERY_RETRY: Duration = Duration::from_secs(60);
/// Masters are re-resolved this often so DNS changes get picked up.
const MASTERS_REINIT_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);
const MASTER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60 * 5);

const DEFAULT_MASTER_SERVERS: &str =
    "master.quakeworld.nu qwmaster.fodquake.net master.quakeservers.net";
const DEFAULT_MASTER_PORT: u16 = 27000;
/// Some masters list unusable servers; filter them out by default.
const DEFAULT_SERVER_FILTER: &str = "127.0.0.1";

const MAX_MASTERS: usize = 8;
const MAX_SERVERS: usize = 512;
const MAX_FILTERS: usize = 16;

const UNREACHABLE_PING: i32 = 0xFFFF;

struct Master {
    addr: SocketAddrV4,
    next_query: Instant,
}

struct Server {
    addr: SocketAddrV4,
    /// Whether the last ping was answered.
    reply: bool,
    ping_sent_at: Option<Instant>,
    ping_reply_at: Option<Instant>,
    ping: i32,
}

pub struct Query {
    /// Stands in for "never" when computing server liveness.
    epoch: Instant,
    masters: Vec<Master>,
    masters_init_at: Instant,
    next_heartbeat: Instant,
    heartbeat_sequence: i32,
    servers: Vec<Server>,
    filters: Vec<Ipv4Addr>,
    ping_index: usize,
    last_ping_at: Option<Instant>,
}

impl Query {
    pub fn new() -> Self {
        let now = Instant::now();
        Query {
            epoch: now,
            masters: Vec::new(),
            masters_init_at: now,
            next_heartbeat: now,
            heartbeat_sequence: 0,
            servers: Vec::new(),
            filters: Vec::new(),
            ping_index: 0,
            last_ping_at: None,
        }
    }

    pub fn register_cvars(cvars: &mut Cvars) {
        cvars.get("masters_query", "1", 0);
        cvars.get("masters_heartbeat", "1", 0);
        cvars.get("masters", DEFAULT_MASTER_SERVERS, 0);
        cvars.get("masters_filter_servers", DEFAULT_SERVER_FILTER, 0);
    }

    pub async fn frame(&mut self, cvars: &mut Cvars, socket: &UdpSocket, peer_count: usize) {
        self.check_filters_modified(cvars).await;
        self.check_masters_modified(cvars).await;
        self.query_masters(cvars, socket);
        self.heartbeat_masters(cvars, socket, peer_count);
        self.ping_servers(cvars, socket);
    }

    /// Whether a packet from the proxy socket is a master server list reply.
    pub fn is_master_reply(data: &[u8]) -> bool {
        data.starts_with(b"\xff\xff\xff\xffd\n")
    }

    fn reset_masters(&mut self) {
        self.masters.clear();
        self.masters_init_at = Instant::now();
        self.heartbeat_sequence = 0;
        self.trigger_heartbeat();
    }

    fn trigger_heartbeat(&mut self) {
        self.next_heartbeat = Instant::now();
    }

    fn master_by_addr(&mut self, addr: SocketAddrV4) -> Option<&mut Master> {
        self.masters.iter_mut().find(|m| m.addr == addr)
    }

    async fn add_master(&mut self, spec: &str) -> bool {
        let (host, port) = match spec.split_once(':') {
            Some((host, port)) => (host, crate::parse::atoi(port.as_bytes())),
            None => (spec, 0),
        };
        let port = u16::try_from(port)
            .ok()
            .filter(|&p| p > 0 && p < 65535)
            .unwrap_or(DEFAULT_MASTER_PORT);

        if host.is_empty() {
            cprint!("failed to add master server: {spec}\n");
            return false;
        }
        let Some(addr) = net::resolve(host, port).await else {
            cprint!("failed to add master server: {spec}\n");
            return false;
        };
        if self.master_by_addr(addr).is_some() {
            cprint!("failed to add master server: {spec} - already added!\n");
            return false;
        }
        if self.masters.len() >= MAX_MASTERS {
            cprint!("failed to add master server: {spec}\n");
            return false;
        }

        self.masters.push(Master {
            addr,
            next_query: Instant::now(),
        });
        cprint!("master server added: {spec}\n");
        true
    }

    async fn check_masters_modified(&mut self, cvars: &mut Cvars) {
        if self.masters_init_at.elapsed() > MASTERS_REINIT_INTERVAL {
            dprint!("forcing masters re-init\n");
            cvars.mark_modified("masters");
        }
        let masters_changed = cvars.take_modified("masters");
        let query_changed = cvars.take_modified("masters_query");
        if !masters_changed && !query_changed {
            return;
        }

        self.reset_masters();
        for spec in cmd::tokens(cvars.string("masters")) {
            self.add_master(&spec).await;
        }
    }

    fn query_masters(&mut self, cvars: &Cvars, socket: &UdpSocket) {
        if cvars.int("masters_query") == 0 {
            return;
        }
        let now = Instant::now();
        for master in &mut self.masters {
            if now < master.next_query {
                continue;
            }
            dprint!("query master: {}\n", master.addr);
            net::send(socket, MASTER_QUERY, master.addr);
            master.next_query = now + MASTER_QUERY_RETRY;
        }
    }

    fn heartbeat_masters(&mut self, cvars: &Cvars, socket: &UdpSocket, peer_count: usize) {
        if cvars.int("masters_heartbeat") == 0 {
            return;
        }
        let now = Instant::now();
        if now < self.next_heartbeat {
            return;
        }
        self.next_heartbeat = now + MASTER_HEARTBEAT_INTERVAL;
        self.heartbeat_sequence += 1;

        let heartbeat = format!(
            "{}\n{}\n{}\n",
            S2M_HEARTBEAT as char, self.heartbeat_sequence, peer_count
        );
        if developer() > 1 {
            dprint!("heartbeat:\n{heartbeat}\n");
        }
        for master in &self.masters {
            dprint!("heartbeat master: {}\n", master.addr);
            net::send(socket, heartbeat.as_bytes(), master.addr);
        }
    }

    pub fn parse_master_reply(&mut self, cvars: &Cvars, from: SocketAddrV4, data: &[u8]) {
        if cvars.int("masters_query") == 0 {
            dprint!("master server reply ignored\n");
            return;
        }
        dprint!("master server reply from {from}\n");

        let Some(master) = self.master_by_addr(from) else {
            cprint!("Reply from not registered master server\n");
            return;
        };
        master.next_query = Instant::now() + MASTER_QUERY_INTERVAL;
        dprint!("master server returned {} bytes\n", data.len());

        for (i, entry) in data[6..].as_chunks::<6>().0.iter().enumerate() {
            let addr = SocketAddrV4::new(
                Ipv4Addr::new(entry[0], entry[1], entry[2], entry[3]),
                u16::from_be_bytes([entry[4], entry[5]]),
            );
            if developer() > 1 {
                dprint!("SERVER: {i:4} {addr}\n");
            }
            self.add_server(addr);
        }
    }

    fn add_server(&mut self, addr: SocketAddrV4) {
        if self.servers.len() >= MAX_SERVERS || self.servers.iter().any(|s| s.addr == addr) {
            return;
        }
        if self.filters.contains(addr.ip()) {
            dprint!("filtered: {addr}\n");
            return;
        }
        self.servers.push(Server {
            addr,
            reply: false,
            ping_sent_at: None,
            ping_reply_at: None,
            ping: UNREACHABLE_PING,
        });
    }

    /// Pings one server per call, cycling through the list at a gentle rate.
    fn ping_servers(&mut self, cvars: &Cvars, socket: &UdpSocket) {
        if cvars.int("masters_query") == 0 || self.servers.is_empty() {
            return;
        }
        let now = Instant::now();
        if self
            .last_ping_at
            .is_some_and(|last| now.duration_since(last) < SERVER_RATE)
        {
            return;
        }
        self.last_ping_at = Some(now);

        let index = if self.ping_index < self.servers.len() {
            self.ping_index
        } else {
            0
        };
        self.ping_index = index + 1;
        let server = &mut self.servers[index];

        let sent = server.ping_sent_at.unwrap_or(self.epoch);
        let replied = server.ping_reply_at.unwrap_or(self.epoch);
        if !server.reply && sent.saturating_duration_since(replied) > SERVER_DEAD_TIME {
            dprint!("dead -> {}\n", server.addr);
            self.servers.remove(index);
            self.ping_index = index;
            return;
        }

        if server
            .ping_sent_at
            .is_some_and(|sent| now.duration_since(sent) < SERVER_MIN_PING_INTERVAL)
        {
            return;
        }
        server.ping_sent_at = Some(now);
        server.reply = false;
        net::send(socket, SERVER_PING_QUERY, server.addr);
    }

    pub fn ping_reply(&mut self, cvars: &Cvars, from: SocketAddrV4) {
        if cvars.int("masters_query") == 0 {
            dprint!("server reply ignored\n");
            return;
        }
        let epoch = self.epoch;
        if let Some(server) = self.servers.iter_mut().find(|s| s.addr == from) {
            let now = Instant::now();
            let sent = server.ping_sent_at.unwrap_or(epoch);
            server.ping = now.saturating_duration_since(sent).as_millis() as i32;
            server.ping_reply_at = Some(now);
            server.reply = true;
        }
    }

    /// Answers a `pingstatus` query with every known server and its ping.
    pub fn ping_status(&self, cvars: &Cvars, socket: &UdpSocket, from: SocketAddrV4) {
        let mut msg = MsgWriter::out_of_band(MSG_BUF_SIZE);
        msg.write_byte(A2C_PRINT);

        // Without master queries the list would be stale, so send none.
        if cvars.int("masters_query") != 0 {
            for server in &self.servers {
                msg.write(&server.addr.ip().octets());
                msg.write_short(server.addr.port() as i16);
                msg.write_short(server.ping as i16);
            }
        }

        if msg.overflowed() {
            cprint!("SVC_QRY_PingStatus: overflow\n");
            return;
        }
        net::send(socket, msg.as_bytes(), from);
    }

    async fn add_filter(&mut self, spec: &str) -> bool {
        if self.filters.len() >= MAX_FILTERS {
            cprint!("failed to add server filter: {spec} - filter list are full!\n");
            return false;
        }
        let host = spec.split_once(':').map_or(spec, |(host, _)| host);
        if host.is_empty() {
            cprint!("failed to add server filter: {spec}\n");
            return false;
        }
        let Some(addr) = net::resolve(host, 0).await else {
            cprint!("failed to add server filter: {spec}\n");
            return false;
        };
        if self.filters.contains(addr.ip()) {
            cprint!("failed to add server filter: {spec} - already added!\n");
            return false;
        }
        self.filters.push(*addr.ip());
        cprint!("server filter added: {spec}\n");
        true
    }

    async fn check_filters_modified(&mut self, cvars: &mut Cvars) {
        if !cvars.take_modified("masters_filter_servers") {
            return;
        }
        self.filters.clear();
        for spec in cmd::tokens(cvars.string("masters_filter_servers")) {
            self.add_filter(&spec).await;
        }
        let filters = &self.filters;
        self.servers.retain(|s| {
            let filtered = filters.contains(s.addr.ip());
            if filtered {
                dprint!("filtered: {}\n", s.addr);
            }
            !filtered
        });
    }
}

impl Proxy {
    pub fn register_query_commands(&mut self) {
        self.cmds.register("svlist", cmd_svlist);
        self.cmds.register("heartbeat", cmd_heartbeat);
    }
}

fn cmd_svlist(proxy: &mut Proxy, _args: &Args) {
    cprint!("=== server list ===\n");
    cprint!("### {:<21} ping\n", "address");
    cprint!("--------------------------------------\n");
    for (i, server) in proxy.query.servers.iter().enumerate() {
        cprint!("{:3} {:<21} {}\n", i + 1, server.addr, server.ping);
    }
    cprint!("--------------------------------------\n");
    cprint!("{} servers\n", proxy.query.servers.len());
}

fn cmd_heartbeat(proxy: &mut Proxy, _args: &Args) {
    proxy.query.trigger_heartbeat();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_master_replies() {
        assert!(Query::is_master_reply(
            b"\xff\xff\xff\xffd\n\x01\x02\x03\x04\x6b\x6c"
        ));
        assert!(!Query::is_master_reply(b"\xff\xff\xff\xffn"));
        assert!(!Query::is_master_reply(b"\xff\xff\xff\xffd"));
    }

    #[test]
    fn master_reply_adds_servers_from_registered_masters_only() {
        let mut cvars = Cvars::default();
        Query::register_cvars(&mut cvars);
        let mut query = Query::new();
        let master = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 27000);
        query.filters.push(Ipv4Addr::new(127, 0, 0, 1));

        let reply = b"\xff\xff\xff\xffd\n\x0a\x00\x00\x02\x6b\x6c\x7f\x00\x00\x01\x6b\x6c\x0a\x00\x00\x02\x6b\x6c\xff";
        query.parse_master_reply(&cvars, master, reply);
        assert!(query.servers.is_empty());

        query.masters.push(Master {
            addr: master,
            next_query: Instant::now(),
        });
        query.parse_master_reply(&cvars, master, reply);
        assert_eq!(query.servers.len(), 1);
        assert_eq!(
            query.servers[0].addr,
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 27500)
        );
        assert!(query.masters[0].next_query > Instant::now() + Duration::from_secs(60));
    }
}
