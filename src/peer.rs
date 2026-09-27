//! Forwarded clients ("peers"): one remote-facing socket per connected client.

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cmd::Args;
use crate::console::qstr;
use crate::msg::{MSG_BUF_SIZE, MsgReader, MsgWriter};
use crate::protocol::{A2A_ACK, CLC_STRINGCMD};
use crate::proxy::Proxy;
use crate::{cprint, dprint, info, net, parse};

/// Clients silent this long are dropped.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);
const CHALLENGE_RESEND: Duration = Duration::from_secs(2);
/// Q3 idle probe: after this much silence, poke the server so it tells us if it dropped the client.
const Q3_IDLE: Duration = Duration::from_secs(1);
const Q3_PROBE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Qw,
    Q3,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerState {
    /// Scheduled for removal.
    Drop,
    /// Waiting on a challenge from the remote server.
    Challenge,
    Connected,
}

/// A datagram received on a peer's remote-facing socket.
pub struct PeerPacket {
    pub userid: i32,
    pub from: SocketAddrV4,
    pub data: Vec<u8>,
}

pub struct Peer {
    pub userid: i32,
    /// The client.
    pub from: SocketAddrV4,
    /// The remote server.
    pub to: SocketAddrV4,
    pub socket: Arc<UdpSocket>,
    reader: JoinHandle<()>,
    pub state: PeerState,
    pub proto: Protocol,
    pub challenge: i32,
    pub userinfo: Vec<u8>,
    pub name: Vec<u8>,
    pub top_color: i32,
    pub bottom_color: i32,
    pub qport: i32,
    pub last_seen: Instant,
    pub connected_at: Instant,
    last_challenge_at: Option<Instant>,
    last_q3_probe_at: Option<Instant>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Peer {
    fn apply_userinfo(&mut self, userinfo: &[u8]) {
        self.userinfo = userinfo.to_vec();
        self.name = info::value_for_key(userinfo, b"name").to_vec();
        self.top_color = parse_color(userinfo, b"topcolor");
        self.bottom_color = parse_color(userinfo, b"bottomcolor");
    }

    pub fn minutes_connected(&self) -> u64 {
        self.connected_at.elapsed().as_secs() / 60
    }
}

fn parse_color(userinfo: &[u8], key: &[u8]) -> i32 {
    parse::atoi(info::value_for_key(userinfo, key)).clamp(0, 16)
}

/// What a peer's reader task reports to the main loop.
pub enum PeerEvent {
    Packet(PeerPacket),
    /// The peer's socket can no longer be read, so the peer is useless.
    Lost {
        userid: i32,
        error: String,
    },
}

/// Pumps datagrams from a peer's socket into the main loop.
async fn peer_reader(userid: i32, socket: Arc<UdpSocket>, tx: mpsc::Sender<PeerEvent>) {
    let mut buf = vec![0u8; MSG_BUF_SIZE];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, from)) => {
                if len >= MSG_BUF_SIZE {
                    cprint!("NET_GetPacket: Oversize packet from {}\n", from.ip());
                    continue;
                }
                let Some(from) = net::v4(from) else { continue };
                let packet = PeerPacket {
                    userid,
                    from,
                    data: buf[..len].to_vec(),
                };
                if tx.send(PeerEvent::Packet(packet)).await.is_err() {
                    return;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                dprint!("NET_GetPacket: Connection was forcibly closed\n");
            }
            Err(err) if net::is_oversize(&err) => {
                cprint!("NET_GetPacket: Oversize packet\n");
            }
            Err(err) => {
                // Nobody would read this socket again; have the peer dropped
                // rather than leave the client with a one-way connection.
                let lost = PeerEvent::Lost {
                    userid,
                    error: err.to_string(),
                };
                let _ = tx.send(lost).await;
                return;
            }
        }
    }
}

impl Proxy {
    pub fn register_peer_commands(&mut self) {
        self.cmds.register("cllist", cmd_cllist);
    }

    pub fn peer_by_addr(&self, from: SocketAddrV4) -> Option<&Peer> {
        self.peers.iter().find(|p| p.from == from)
    }

    fn peer_by_addr_mut(&mut self, from: SocketAddrV4) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.from == from)
    }

    /// Registers a client for forwarding to `host:port`, reusing an existing
    /// peer for the same client address. Returns the peer's index.
    pub async fn peer_new(
        &mut self,
        host: &str,
        port: u16,
        from: SocketAddrV4,
        userinfo: &[u8],
        qport: i32,
        proto: Protocol,
    ) -> Option<usize> {
        let to = net::resolve(host, port).await?;
        if !self.whitelist.allows(*to.ip()) || self.bans.is_banned(to) {
            return None;
        }

        if let Some(index) = self.peers.iter().position(|p| p.from == from) {
            let peer = &mut self.peers[index];
            peer.to = to;
            // A reconnecting Q3 client keeps its state; the server side is unaware of the reconnect.
            if proto != Protocol::Q3 {
                peer.state = PeerState::Challenge;
            }
            peer.qport = qport;
            peer.proto = proto;
            peer.apply_userinfo(userinfo);
            peer.last_seen = Instant::now();
            return Some(index);
        }

        if self.peers.len() >= self.max_clients() {
            return None;
        }
        let socket = Arc::new(net::open_ephemeral_socket().await.ok()?);
        self.next_userid += 1;
        let userid = self.next_userid;
        let reader = tokio::spawn(peer_reader(
            userid,
            Arc::clone(&socket),
            self.peer_tx.clone(),
        ));

        let now = Instant::now();
        let mut peer = Peer {
            userid,
            from,
            to,
            socket,
            reader,
            state: PeerState::Challenge,
            proto,
            challenge: 0,
            userinfo: Vec::new(),
            name: Vec::new(),
            top_color: 0,
            bottom_color: 0,
            qport,
            last_seen: now,
            connected_at: now,
            last_challenge_at: None,
            last_q3_probe_at: None,
        };
        peer.apply_userinfo(userinfo);
        self.peers.push(peer);
        Some(self.peers.len() - 1)
    }

    pub fn max_clients(&self) -> usize {
        usize::try_from(self.cvars.int("maxclients")).unwrap_or(0)
    }

    /// Handles a datagram from a client on the proxy socket.
    pub async fn handle_client_packet(
        &mut self,
        socket: &UdpSocket,
        from: SocketAddrV4,
        msg: &mut Vec<u8>,
    ) {
        if self.bans.is_banned(from) {
            return;
        }
        if msg.as_slice() == [A2A_ACK] {
            self.query.ping_reply(&self.cvars, from);
            return;
        }

        let connectionless = match MsgReader::new(msg).read_long() {
            None => return,
            Some(-1) => true,
            Some(_) => false,
        };
        if connectionless && !self.sv_connectionless(socket, from, msg).await {
            return;
        }

        let Some(peer) = self.peer_by_addr_mut(from) else {
            return;
        };
        if peer.state == PeerState::Connected {
            let mut copies = 1;
            if peer.proto == Protocol::Qw && !connectionless && is_drop_command(msg) {
                peer.state = PeerState::Drop;
                // The client is leaving; repeat so the server hears it despite packet loss.
                copies = 3;
            }
            for _ in 0..copies {
                net::send(&peer.socket, msg, peer.to);
            }
        }
        peer.last_seen = Instant::now();
    }

    /// Handles a datagram from a remote server on a peer's socket.
    /// Handles what a peer's reader task reported.
    pub fn handle_peer_event(&mut self, socket: &UdpSocket, event: PeerEvent) {
        match event {
            PeerEvent::Packet(packet) => self.handle_server_packet(socket, packet),
            PeerEvent::Lost { userid, error } => {
                if let Some(peer) = self.peers.iter_mut().find(|p| p.userid == userid) {
                    cprint!("NET_GetPacket: recvfrom: {error}, dropping peer {userid}\n");
                    peer.state = PeerState::Drop;
                }
            }
        }
    }

    pub fn handle_server_packet(&mut self, socket: &UdpSocket, packet: PeerPacket) {
        if self.bans.is_banned(packet.from) {
            return;
        }
        let Some(peer) = self.peers.iter_mut().find(|p| p.userid == packet.userid) else {
            return;
        };
        if peer.to != packet.from {
            return;
        }

        match MsgReader::new(&packet.data).read_long() {
            None => {}
            Some(-1) => {
                if peer.cl_connectionless(&packet.data) {
                    net::send(socket, &packet.data, peer.from);
                }
            }
            Some(_) if peer.state == PeerState::Connected => {
                net::send(socket, &packet.data, peer.from);
            }
            Some(_) => {}
        }
    }

    /// Times out silent peers, re-sends pending challenges and probes idle Q3 servers.
    pub fn peer_maintenance(&mut self) {
        let now = Instant::now();
        for peer in &mut self.peers {
            if peer.proto == Protocol::Q3
                && peer.state == PeerState::Connected
                && now.duration_since(peer.last_seen) > Q3_IDLE
                && peer
                    .last_q3_probe_at
                    .is_none_or(|t| now.duration_since(t) > Q3_PROBE_INTERVAL)
            {
                peer.last_q3_probe_at = Some(now);
                let mut probe = MsgWriter::new(6);
                probe.write_long(0);
                probe.write_short(peer.qport as i16);
                net::send(&peer.socket, probe.as_bytes(), peer.to);
            }

            if peer.state == PeerState::Challenge
                && peer
                    .last_challenge_at
                    .is_none_or(|t| now.duration_since(t) > CHALLENGE_RESEND)
            {
                peer.last_challenge_at = Some(now);
                let request = match peer.proto {
                    Protocol::Qw => "getchallenge\n",
                    Protocol::Q3 => "getchallenge",
                };
                net::send_oob_print(&peer.socket, peer.to, request);
            }

            if now.duration_since(peer.last_seen) >= PEER_TIMEOUT {
                dprint!("peer {} timed out\n", peer.from);
                peer.state = PeerState::Drop;
            }
        }
    }

    pub fn drop_dead_peers(&mut self) {
        self.peers.retain(|peer| {
            if peer.state == PeerState::Drop {
                dprint!("peer {} dropped\n", peer.from);
            }
            peer.state != PeerState::Drop
        });
    }
}

/// A QuakeWorld game packet whose first command is the client's `drop`.
/// The netchan header occupies the first 10 bytes.
fn is_drop_command(msg: &[u8]) -> bool {
    if msg.len() <= 10 || msg[10] != CLC_STRINGCMD {
        return false;
    }
    let text = &msg[11..];
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    &text[..end] == b"drop"
}

fn cmd_cllist(proxy: &mut Proxy, _args: &Args) {
    cprint!("=== client list ===\n");
    cprint!(
        "##id## {:<21} {:<21} time name\n",
        "address from",
        "address to"
    );
    cprint!("-----------------------------------------------------------------------\n");
    for peer in &proxy.peers {
        cprint!(
            "{:6} {:<21} {:<21} {:4} {}\n",
            peer.userid,
            peer.from,
            peer.to,
            peer.minutes_connected(),
            qstr(&peer.name)
        );
    }
    cprint!("-----------------------------------------------------------------------\n");
    cprint!("{} clients\n", proxy.peers.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_drop_command() {
        let mut msg = vec![0u8; 10];
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"drop\0");
        assert!(is_drop_command(&msg));
        msg.truncate(15);
        assert!(is_drop_command(&msg));
        msg.extend_from_slice(b"ped");
        assert!(!is_drop_command(&msg));
        assert!(!is_drop_command(&[0u8; 11]));
        assert!(!is_drop_command(b"short"));
    }

    #[tokio::test]
    async fn lost_peer_reader_drops_the_peer() {
        let mut proxy = Proxy::new_for_tests();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let from = "127.0.0.1:27001".parse().unwrap();
        let index = proxy
            .peer_new("127.0.0.1", 27500, from, b"\\name\\x", 5, Protocol::Qw)
            .await
            .unwrap();
        let userid = proxy.peers[index].userid;

        let lost = PeerEvent::Lost {
            userid,
            error: "socket gone".to_string(),
        };
        proxy.handle_peer_event(&socket, lost);
        assert!(proxy.peers[index].state == PeerState::Drop);
        proxy.drop_dead_peers();
        assert!(proxy.peers.is_empty());
    }

    #[test]
    fn colors_are_clamped() {
        assert_eq!(parse_color(b"\\topcolor\\4", b"topcolor"), 4);
        assert_eq!(parse_color(b"\\topcolor\\99", b"topcolor"), 16);
        assert_eq!(parse_color(b"\\topcolor\\-3", b"topcolor"), 0);
        assert_eq!(parse_color(b"\\name\\x", b"topcolor"), 0);
    }
}
