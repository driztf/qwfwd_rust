//! Server-side handling of connectionless packets from clients: challenges,
//! connection requests, status queries.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Instant;

use tokio::net::UdpSocket;

use super::{Event, Proxy};
use crate::cmd::Args;
use crate::msg::{MSG_BUF_SIZE, MsgReader, MsgWriter};
use crate::peer::{PeerState, Protocol, Registration};
use crate::protocol::{
    A2A_ACK, A2A_PING, A2C_PRINT, Q3_CONNECT_PAYLOAD, Q3_DEFAULT_SERVER_PORT,
    QW_DEFAULT_SERVER_PORT, QW_PROTOCOL_VERSION, QW_VERSION, QWFWD_PRX_KEY, QWFWD_VERSION_SHORT,
    S2C_CHALLENGE, S2C_CONNECTION,
};
use crate::query::Query;
use crate::{dprint, huff, info, net, parse};

/// Large enough that an attacker cannot cycle out legitimate challenges.
const MAX_CHALLENGES: usize = 1024;
/// Host lookups in flight for connect requests, over all clients; beyond
/// this a connect naming a host is refused until one finishes.
const MAX_LOOKUPS: usize = 64;

/// A validated connect request waiting for its remote host to be looked up.
pub struct PendingConnect {
    pub from: SocketAddrV4,
    pub host: String,
    pub port: u16,
    pub userinfo: Vec<u8>,
    pub qport: i32,
    pub proto: Protocol,
    /// The challenge the client connected with, for the Q3 reconnect dance.
    pub challenge: i32,
    /// Which of the client's requests this is; an older one's result is ignored.
    pub generation: u64,
}

/// A client's host lookup in flight, and the request that superseded it, if any.
pub struct LookupSlot {
    generation: u64,
    queued: Option<PendingConnect>,
}

pub struct Challenge {
    pub addr: SocketAddrV4,
    pub challenge: i32,
    pub proto: Protocol,
    issued_at: Instant,
}

#[derive(Default)]
pub struct Challenges {
    list: Vec<Challenge>,
}

impl Challenges {
    pub fn find(&self, addr: SocketAddrV4) -> Option<&Challenge> {
        self.list.iter().find(|c| c.addr == addr)
    }

    /// Returns the challenge for `addr`, issuing a fresh one when there is
    /// none (recycling the oldest slot when full). The protocol is always
    /// updated to the one just requested.
    fn get_or_issue(&mut self, addr: SocketAddrV4, proto: Protocol) -> &mut Challenge {
        let index = match self.list.iter().position(|c| c.addr == addr) {
            Some(i) => i,
            None => {
                let fresh = Challenge {
                    addr,
                    challenge: rand::random(),
                    proto,
                    issued_at: Instant::now(),
                };
                if self.list.len() < MAX_CHALLENGES {
                    self.list.push(fresh);
                    self.list.len() - 1
                } else {
                    let oldest = (0..self.list.len())
                        .min_by_key(|&i| self.list[i].issued_at)
                        .expect("challenge list is full, so not empty");
                    self.list[oldest] = fresh;
                    oldest
                }
            }
        };
        let entry = &mut self.list[index];
        entry.proto = proto;
        entry
    }
}

impl Proxy {
    /// Handles a datagram from a client on the proxy socket.
    pub fn handle_client_packet(
        &mut self,
        socket: &UdpSocket,
        from: SocketAddrV4,
        msg: &mut Vec<u8>,
    ) {
        if self.bans.is_banned(from) {
            return;
        }
        if msg.as_slice() == [A2A_ACK] {
            self.query.ping_reply(&self.shell.cvars, from);
            return;
        }

        let connectionless = match MsgReader::new(msg).read_long() {
            None => return,
            Some(-1) => true,
            Some(_) => false,
        };
        if connectionless && !self.sv_connectionless(socket, from, msg) {
            return;
        }

        let smoothing = self.smoothing();
        self.peers
            .forward_from_client(from, msg, connectionless, &smoothing);
    }

    /// Handles an out-of-band packet from a client. Returns whether the packet
    /// should also be forwarded to the client's remote server.
    fn sv_connectionless(
        &mut self,
        socket: &UdpSocket,
        from: SocketAddrV4,
        msg: &mut Vec<u8>,
    ) -> bool {
        if Query::is_master_reply(msg) {
            self.query.parse_master_reply(&self.shell.cvars, from, msg);
            return false;
        }

        let mut text = {
            let mut reader = MsgReader::new(msg);
            reader.read_long();
            reader.read_string()
        };

        // Q3 clients Huffman-compress everything after "connect ".
        if text.starts_with(b"connect ")
            && self
                .challenges
                .find(from)
                .is_some_and(|c| c.proto == Protocol::Q3)
        {
            huff::decompress(msg, Q3_CONNECT_PAYLOAD, MSG_BUF_SIZE);
            let mut reader = MsgReader::new(msg);
            reader.read_long();
            text = reader.read_string();
        }

        let args = Args::tokenize(&text);
        match args.arg(0) {
            b"ping" | [A2A_PING] => self.svc_ping(socket, from),
            b"pingstatus" => self.query.ping_status(&self.shell.cvars, socket, from),
            b"connect" => self.svc_direct_connect(socket, from, &args),
            b"getchallenge" => {
                let proto = if text == b"getchallenge\n" {
                    Protocol::Qw
                } else {
                    Protocol::Q3
                };
                self.svc_get_challenge(socket, from, proto);
            }
            b"status" => self.svc_status(socket, from, &args),
            // There is no proxy rcon; the remote server gets to decide.
            b"rcon" => return true,
            _ => {}
        }
        false
    }

    fn svc_ping(&self, socket: &UdpSocket, from: SocketAddrV4) {
        net::send(socket, &[A2A_ACK], from);
    }

    fn svc_get_challenge(&mut self, socket: &UdpSocket, from: SocketAddrV4, proto: Protocol) {
        // Q3 game packets are scrambled with the challenge, so a reconnecting
        // client must be handed the challenge the remote server issued.
        let server_challenge = match proto {
            Protocol::Q3 => self
                .peers
                .get(from)
                .filter(|p| p.state() == PeerState::Connected)
                .map(|p| p.challenge()),
            Protocol::Qw => None,
        };

        let entry = self.challenges.get_or_issue(from, proto);
        if let Some(challenge) = server_challenge {
            dprint!("handing {from} the server's challenge for its q3 reconnect\n");
            entry.challenge = challenge;
        }
        let challenge = entry.challenge;
        dprint!(
            "challenge {}: {from} {challenge}\n",
            match proto {
                Protocol::Qw => "qw",
                Protocol::Q3 => "q3",
            }
        );

        match proto {
            Protocol::Qw => {
                let mut reply = format!("{}{challenge}", S2C_CHALLENGE as char).into_bytes();
                reply.push(0);
                net::send_oob(socket, from, &reply);
            }
            Protocol::Q3 => {
                net::send_oob_print(socket, from, &format!("challengeResponse {challenge}"));
            }
        }
    }

    fn check_protocol(
        &self,
        socket: &UdpSocket,
        from: SocketAddrV4,
        version: i32,
        proto: Protocol,
    ) -> bool {
        if proto == Protocol::Qw && version != QW_PROTOCOL_VERSION {
            print_to(
                socket,
                from,
                &format!("\nServer is version {QW_VERSION}.\n"),
            );
            dprint!("* rejected connect from version {version}\n");
            return false;
        }
        true
    }

    fn check_challenge(&self, socket: &UdpSocket, from: SocketAddrV4, challenge: i32) -> bool {
        match self.challenges.find(from) {
            None => {
                print_to(socket, from, "\nNo challenge for address.\n");
                false
            }
            Some(c) if c.challenge != challenge => {
                print_to(socket, from, "\nBad challenge.\n");
                false
            }
            Some(_) => true,
        }
    }

    fn check_userinfo(
        &self,
        socket: &UdpSocket,
        from: SocketAddrV4,
        userinfo: &[u8],
    ) -> Option<Vec<u8>> {
        if !info::validate(userinfo) {
            print_to(socket, from, "\nInvalid userinfo. Restart your qwcl\n");
            return None;
        }
        Some(userinfo.to_vec())
    }

    /// Validates a connect request and starts looking up its remote host;
    /// [`finish_connect`](Self::finish_connect) completes it.
    fn svc_direct_connect(&mut self, socket: &UdpSocket, from: SocketAddrV4, args: &Args) {
        let Some(entry) = self.challenges.find(from) else {
            print_to(socket, from, "\nNo challenge for address.\n");
            return;
        };
        let (proto, challenge) = (entry.proto, entry.challenge);

        let (mut userinfo, qport) = match proto {
            Protocol::Qw => {
                if !self.check_protocol(socket, from, parse::atoi(args.arg(1)), proto) {
                    return;
                }
                let qport = parse::atoi(args.arg(2));
                if !self.check_challenge(socket, from, parse::atoi(args.arg(3))) {
                    return;
                }
                let Some(userinfo) = self.check_userinfo(socket, from, args.arg(4)) else {
                    return;
                };
                (userinfo, qport)
            }
            Protocol::Q3 => {
                let Some(userinfo) = self.check_userinfo(socket, from, args.arg(1)) else {
                    return;
                };
                let value = |key: &[u8]| parse::atoi(info::value_for_key(&userinfo, key));
                if !self.check_protocol(socket, from, value(b"protocol"), proto) {
                    return;
                }
                let qport = value(b"qport");
                if !self.check_challenge(socket, from, value(b"challenge")) {
                    return;
                }
                (userinfo, qport)
            }
        };

        if self.peers.len() >= self.max_clients() {
            print_to(
                socket,
                from,
                &format!(
                    "\nproxy@{} is full\n\n",
                    self.shell.cvars.string("hostname")
                ),
            );
            return;
        }

        let prx = info::value_for_key(&userinfo, QWFWD_PRX_KEY).to_vec();
        if prx.is_empty() {
            match proto {
                Protocol::Qw => print_to(socket, from, "\nprx userinfo key is not set\n"),
                Protocol::Q3 => {
                    net::send_oob_print(socket, from, "print\nprx userinfo key is not set\n")
                }
            }
            return;
        }

        // "a@b@c" chains proxies: connect to a and hand the rest on as the new prx key.
        let target: &[u8] = match prx.iter().position(|&b| b == b'@') {
            Some(at) if at + 1 < prx.len() => {
                info::set_value_for_key(
                    &mut userinfo,
                    QWFWD_PRX_KEY,
                    &prx[at + 1..],
                    info::MAX_INFO_STRING,
                    false,
                );
                &prx[..at]
            }
            _ => {
                info::remove_key(&mut userinfo, QWFWD_PRX_KEY);
                &prx
            }
        };

        let (host, port) = match target.iter().position(|&b| b == b':') {
            Some(colon) => (&target[..colon], parse::atoi(&target[colon + 1..])),
            None => (
                target,
                match proto {
                    Protocol::Qw => QW_DEFAULT_SERVER_PORT,
                    Protocol::Q3 => Q3_DEFAULT_SERVER_PORT,
                },
            ),
        };
        let Ok(port) = u16::try_from(port).ok().filter(|&p| p > 0).ok_or(()) else {
            print_to(
                socket,
                from,
                "\nport number in prx userinfo key is invalid\n",
            );
            return;
        };

        // Let the remote server see that this client arrives through qwfwd.
        info::set_value_for_star_key(
            &mut userinfo,
            b"*qwfwd",
            QWFWD_VERSION_SHORT.as_bytes(),
            info::MAX_INFO_STRING,
            true,
        );

        let pending = PendingConnect {
            from,
            host: String::from_utf8_lossy(host).into_owned(),
            port,
            userinfo,
            qport,
            proto,
            challenge,
            generation: 0,
        };
        // Most prx keys name an address, which needs no lookup.
        if let Ok(ip) = pending.host.parse::<Ipv4Addr>() {
            self.finish_connect(socket, pending, Some(SocketAddrV4::new(ip, port)));
            return;
        }
        self.queue_lookup(pending);
    }

    /// Starts the host lookup for a connect request. One lookup runs per
    /// client at a time: a request arriving while one is in flight waits for
    /// it and supersedes it, so the newest request is the one applied and a
    /// client cannot pile up lookups.
    fn queue_lookup(&mut self, mut pending: PendingConnect) {
        let from = pending.from;
        if let Some(slot) = self.lookups.get_mut(&from) {
            slot.generation += 1;
            slot.queued = Some(pending);
            return;
        }
        if self.lookups.len() >= MAX_LOOKUPS {
            dprint!("lookup for {from} refused: {MAX_LOOKUPS} already in flight\n");
            return;
        }
        pending.generation = 1;
        let slot = LookupSlot {
            generation: 1,
            queued: None,
        };
        self.lookups.insert(from, slot);
        self.spawn_lookup(pending);
    }

    fn spawn_lookup(&self, pending: PendingConnect) {
        let events = self.events.clone();
        tokio::spawn(async move {
            let to = net::resolve(&pending.host, pending.port).await;
            let _ = events.send(Event::ConnectResolved(pending, to)).await;
        });
    }

    /// A host lookup finished: completes the connect when it is still the
    /// client's newest request, otherwise starts the one that superseded it.
    pub(super) fn lookup_finished(
        &mut self,
        socket: &UdpSocket,
        pending: PendingConnect,
        to: Option<SocketAddrV4>,
    ) {
        let from = pending.from;
        let superseded_by = match self.lookups.get_mut(&from) {
            None => return,
            Some(slot) if pending.generation == slot.generation => None,
            Some(slot) => {
                let next = slot.queued.take().map(|mut next| {
                    next.generation = slot.generation;
                    next
                });
                Some(next)
            }
        };
        match superseded_by {
            Some(Some(next)) => self.spawn_lookup(next),
            Some(None) => {}
            None => {
                self.lookups.remove(&from);
                self.finish_connect(socket, pending, to);
            }
        }
    }

    /// Completes a connect request once its remote host is known.
    pub(super) fn finish_connect(
        &mut self,
        socket: &UdpSocket,
        pending: PendingConnect,
        to: Option<SocketAddrV4>,
    ) {
        let PendingConnect {
            from,
            userinfo,
            qport,
            proto,
            challenge,
            ..
        } = pending;
        let max_clients = self.max_clients();
        let index = match to {
            Some(to) if self.whitelist.allows(*to.ip()) && !self.bans.is_banned(to) => {
                let registration = Registration {
                    to,
                    from,
                    userinfo: &userinfo,
                    qport,
                    proto,
                };
                self.peers.register(registration, max_clients, &self.events)
            }
            _ => None,
        };
        let Some(index) = index else {
            dprint!("peer {from} was not added\n");
            return;
        };
        dprint!("peer {from} added or reused\n");

        match proto {
            Protocol::Qw => {
                net::send_oob_print(socket, from, &(S2C_CONNECTION as char).to_string())
            }
            Protocol::Q3 => {
                let Some(peer) = self.peers.get_index(index) else {
                    return;
                };
                if peer.state() == PeerState::Connected {
                    if peer.challenge() == challenge {
                        net::send_oob_print(socket, from, "connectResponse");
                    } else {
                        // The client must come back with the server's challenge.
                        net::send_oob_print(socket, from, "print\n/reconnect ASAP!\n");
                    }
                }
            }
        }
    }

    /// Answers a `status` query the way a QuakeWorld server would, so server
    /// browsers can list the proxy and its clients.
    fn svc_status(&self, socket: &UdpSocket, from: SocketAddrV4, args: &Args) {
        const OLDSTYLE: i32 = 0;
        const SERVERINFO: i32 = 1;
        const PLAYERS: i32 = 2;
        const SPECTATORS: i32 = 4;

        let mut msg = MsgWriter::out_of_band(MSG_BUF_SIZE);
        msg.write_byte(A2C_PRINT);

        let opt = if args.argc() > 1 {
            parse::atoi(args.arg(1))
        } else {
            OLDSTYLE
        };

        if opt == OLDSTYLE || opt & SERVERINFO != 0 {
            let mut line = self.shell.serverinfo.render(&self.shell.cvars);
            line.push(b'\n');
            msg.print(&line);
        }

        if opt == OLDSTYLE || opt & (PLAYERS | SPECTATORS) != 0 {
            for peer in self.peers.iter() {
                let (frags, ping, skin) = (0, 666, "");
                let (top, bottom) = peer.colors();
                let mut line = format!(
                    "{} {frags} {} {ping} \"",
                    peer.userid(),
                    peer.minutes_connected()
                )
                .into_bytes();
                line.extend_from_slice(peer.name());
                line.extend_from_slice(format!("\" \"{skin}\" {top} {bottom}\n").as_bytes());
                msg.print(&line);
            }
        }

        if !msg.overflowed() {
            net::send(socket, msg.as_bytes(), from);
        }
    }
}

/// Sends an `A2C_PRINT` message to a client.
fn print_to(socket: &UdpSocket, to: SocketAddrV4, text: &str) {
    let mut data = vec![A2C_PRINT];
    data.extend_from_slice(text.as_bytes());
    net::send_oob(socket, to, &data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr(last: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, last), 27001)
    }

    #[test]
    fn challenges_are_stable_per_address_and_recycle_oldest() {
        let mut challenges = Challenges::default();
        let first = challenges.get_or_issue(addr(1), Protocol::Qw).challenge;
        assert_eq!(
            challenges.get_or_issue(addr(1), Protocol::Q3).challenge,
            first
        );
        assert_eq!(challenges.find(addr(1)).unwrap().proto, Protocol::Q3);

        for i in 2..=255 {
            challenges.get_or_issue(addr(i), Protocol::Qw);
        }
        for port in 1..=(MAX_CHALLENGES as u16) {
            challenges.get_or_issue(
                SocketAddrV4::new(Ipv4Addr::new(10, 1, 0, 1), port),
                Protocol::Qw,
            );
        }
        assert_eq!(challenges.list.len(), MAX_CHALLENGES);
        assert!(challenges.find(addr(1)).is_none());
    }
}
