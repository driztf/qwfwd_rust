//! Forwarded clients ("peers"): one remote-facing socket per connected client.

mod clc;

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cmd::Args;
use crate::console::qstr;
use crate::msg::{MSG_BUF_SIZE, MsgReader, MsgWriter};
use crate::protocol::{CLC_STRINGCMD, NETCHAN_HEADER};
use crate::proxy::Event;
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
    userid: i32,
    /// The client.
    from: SocketAddrV4,
    /// The remote server.
    to: SocketAddrV4,
    socket: Arc<UdpSocket>,
    reader: JoinHandle<()>,
    state: PeerState,
    proto: Protocol,
    challenge: i32,
    userinfo: Vec<u8>,
    name: Vec<u8>,
    top_color: i32,
    bottom_color: i32,
    qport: i32,
    last_seen: Instant,
    connected_at: Instant,
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
        self.refresh_from_userinfo();
    }

    fn refresh_from_userinfo(&mut self) {
        self.name = info::value_for_key(&self.userinfo, b"name").to_vec();
        self.top_color = parse_color(&self.userinfo, b"topcolor");
        self.bottom_color = parse_color(&self.userinfo, b"bottomcolor");
    }

    /// Mirrors a mid-game `setinfo "key" "value"` command into the userinfo,
    /// so the proxy's view of the client (name, colours) stays current.
    fn apply_stringcmd(&mut self, command: &[u8]) {
        let args = Args::tokenize(command);
        if args.argc() != 3 || !args.arg(0).eq_ignore_ascii_case(b"setinfo") {
            return;
        }
        dprint!(
            "{}: setinfo {} = {}\n",
            self.from,
            qstr(args.arg(1)),
            qstr(args.arg(2))
        );
        info::set_value_for_key(
            &mut self.userinfo,
            args.arg(1),
            args.arg(2),
            info::MAX_INFO_STRING,
            true,
        );
        self.refresh_from_userinfo();
    }

    pub fn userid(&self) -> i32 {
        self.userid
    }

    pub fn state(&self) -> PeerState {
        self.state
    }

    /// The challenge the remote server issued.
    pub fn challenge(&self) -> i32 {
        self.challenge
    }

    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Top and bottom colours from the userinfo.
    pub fn colors(&self) -> (i32, i32) {
        (self.top_color, self.bottom_color)
    }

    pub fn minutes_connected(&self) -> u64 {
        self.connected_at.elapsed().as_secs() / 60
    }
}

fn parse_color(userinfo: &[u8], key: &[u8]) -> i32 {
    parse::atoi(info::value_for_key(userinfo, key)).clamp(0, 16)
}

/// Pumps datagrams from a peer's socket into the main loop.
async fn peer_reader(userid: i32, socket: Arc<UdpSocket>, events: mpsc::Sender<Event>) {
    let mut buf = vec![0u8; MSG_BUF_SIZE];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, from)) => {
                if len >= MSG_BUF_SIZE {
                    dprint!("oversize packet from {} dropped\n", from.ip());
                    continue;
                }
                let Some(from) = net::v4(from) else { continue };
                let packet = PeerPacket {
                    userid,
                    from,
                    data: buf[..len].to_vec(),
                };
                if events.send(Event::PeerPacket(packet)).await.is_err() {
                    return;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                dprint!("connection reset on peer {userid}'s socket\n");
            }
            Err(err) if net::is_oversize(&err) => {
                cprint!("NET_GetPacket: Oversize packet\n");
            }
            Err(err) => {
                // Nobody would read this socket again; have the peer dropped
                // rather than leave the client with a one-way connection.
                let lost = Event::PeerLost {
                    userid,
                    error: err.to_string(),
                };
                let _ = events.send(lost).await;
                return;
            }
        }
    }
}

/// A client to start forwarding, once its connect request has been validated.
pub struct Registration<'a> {
    /// The remote server.
    pub to: SocketAddrV4,
    /// The client.
    pub from: SocketAddrV4,
    pub userinfo: &'a [u8],
    pub qport: i32,
    pub proto: Protocol,
}

/// The table of forwarded clients.
#[derive(Default)]
pub struct Peers {
    list: Vec<Peer>,
    next_userid: i32,
}

impl Peers {
    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Peer> {
        self.list.iter()
    }

    pub fn get(&self, from: SocketAddrV4) -> Option<&Peer> {
        self.list.iter().find(|p| p.from == from)
    }

    pub fn get_index(&self, index: usize) -> Option<&Peer> {
        self.list.get(index)
    }

    /// Registers a client for forwarding, reusing an existing peer for the
    /// same client address. Returns the peer's index, or `None` when the
    /// table is full or no socket could be opened.
    pub fn register(
        &mut self,
        registration: Registration,
        max_clients: usize,
        events: &mpsc::Sender<Event>,
    ) -> Option<usize> {
        let Registration {
            to,
            from,
            userinfo,
            qport,
            proto,
        } = registration;
        if let Some(index) = self.list.iter().position(|p| p.from == from) {
            if self.list[index].state == PeerState::Drop {
                // Its reader is gone: a fresh peer replaces it rather than
                // reviving a socket nobody reads.
                self.list.remove(index);
            } else {
                let peer = &mut self.list[index];
                let now = Instant::now();
                peer.to = to;
                // A reconnecting Q3 client keeps its state; the server side is unaware of the reconnect.
                if proto != Protocol::Q3 {
                    peer.state = PeerState::Challenge;
                    peer.connected_at = now;
                }
                peer.qport = qport;
                peer.proto = proto;
                peer.apply_userinfo(userinfo);
                peer.last_seen = now;
                return Some(index);
            }
        }

        if self.list.len() >= max_clients {
            return None;
        }
        let socket = Arc::new(net::open_ephemeral_socket().ok()?);
        self.next_userid += 1;
        let userid = self.next_userid;
        let reader = tokio::spawn(peer_reader(userid, Arc::clone(&socket), events.clone()));

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
        self.list.push(peer);
        Some(self.list.len() - 1)
    }

    /// Forwards a client's packet to its server, honouring the `setinfo` and
    /// `drop` commands it may carry. Connectionless packets are the ones the
    /// caller decided should reach the server (e.g. `rcon`).
    pub fn forward_from_client(&mut self, from: SocketAddrV4, msg: &[u8], connectionless: bool) {
        let Some(index) = self.list.iter().position(|p| p.from == from) else {
            return;
        };
        let peer = &mut self.list[index];
        let now = Instant::now();
        let mut dropping = false;
        if peer.state == PeerState::Connected {
            if peer.proto == Protocol::Qw && !connectionless {
                for command in stringcmds(msg) {
                    if command == b"drop" {
                        dropping = true;
                    } else {
                        peer.apply_stringcmd(command);
                    }
                }
            }
            // A leaving client's drop is repeated so the server hears it despite packet loss.
            let copies = if dropping { 3 } else { 1 };
            for _ in 0..copies {
                net::send(&peer.socket, msg, peer.to);
            }
        }
        peer.last_seen = now;
        if dropping {
            dprint!("peer {from} dropped\n");
            self.list.remove(index);
        }
    }

    /// Handles a datagram from a remote server on a peer's socket, passing
    /// game traffic and selected out-of-band messages on to the client.
    pub fn handle_server_packet(&mut self, socket: &UdpSocket, packet: PeerPacket) {
        let Some(peer) = self.list.iter_mut().find(|p| p.userid == packet.userid) else {
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
    pub fn maintenance(&mut self) {
        let now = Instant::now();
        for peer in &mut self.list {
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

    /// A peer's socket can no longer be read (its reader task reported
    /// `error` and ended): the peer is useless and is marked for dropping.
    pub fn lose(&mut self, userid: i32, error: &str) {
        if let Some(peer) = self.list.iter_mut().find(|p| p.userid == userid) {
            cprint!("peer {userid}: recvfrom failed: {error}; dropping\n");
            peer.state = PeerState::Drop;
        }
    }

    pub fn drop_dead(&mut self) {
        self.list.retain(|peer| {
            if peer.state == PeerState::Drop {
                dprint!("peer {} dropped\n", peer.from);
            }
            peer.state != PeerState::Drop
        });
    }
}

/// The string commands the proxy acts on (`setinfo`, `drop`) in a QuakeWorld
/// game packet.
///
/// They travel in the reliable stream at the front of the packet, but so do
/// binary commands such as a tracking spectator's `clc_tmove`, whose size
/// depends on protocol extensions the proxy does not see negotiated. Rather
/// than parse the stream, look for the `clc_stringcmd` byte immediately
/// followed by one of the command names; those markers do not occur by
/// accident in move data.
fn stringcmds(msg: &[u8]) -> impl Iterator<Item = &[u8]> {
    const NAMES: [&[u8]; 2] = [b"setinfo", b"drop"];
    let mut rest = msg.get(NETCHAN_HEADER..).unwrap_or(&[]);
    std::iter::from_fn(move || {
        let start = (0..rest.len()).find(|&i| {
            rest[i] == CLC_STRINGCMD && NAMES.iter().any(|name| rest[i + 1..].starts_with(name))
        })?;
        let text = &rest[start + 1..];
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        rest = text.get(end + 1..).unwrap_or(&[]);
        Some(&text[..end])
    })
}

/// A peer command.
pub type Cmd = fn(&Peers, &Args);

pub const COMMANDS: &[(&str, Cmd)] = &[("cllist", cmd_cllist)];

fn cmd_cllist(peers: &Peers, _args: &Args) {
    cprint!("=== client list ===\n");
    cprint!(
        "##id## {:<21} {:<21} time name\n",
        "address from",
        "address to"
    );
    cprint!("-----------------------------------------------------------------------\n");
    for peer in peers.iter() {
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
    cprint!("{} clients\n", peers.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_stringcmds_anywhere_in_the_reliable_stream() {
        let cmds = |msg: &[u8]| stringcmds(msg).map(<[u8]>::to_vec).collect::<Vec<_>>();
        const TMOVE: u8 = 5;

        // A player leaving: drop is the only content, with or without its terminator.
        let mut msg = vec![0u8; 10];
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"drop\0");
        assert_eq!(cmds(&msg), [b"drop".to_vec()]);
        msg.truncate(15);
        assert_eq!(cmds(&msg), [b"drop".to_vec()]);
        msg.extend_from_slice(b"ped");
        assert_eq!(cmds(&msg), [b"dropped".to_vec()]);

        // A player: setinfo right after the header, then unreliable move data.
        let mut msg = vec![0u8; 10];
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"setinfo \"smooth\" \"1\"\n\0");
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"say hi\0");
        msg.extend_from_slice(&[3, 1, 2, 3]);
        assert_eq!(cmds(&msg), [b"setinfo \"smooth\" \"1\"\n".to_vec()]);

        // A tracking spectator: a binary tmove precedes the commands.
        let mut msg = vec![0u8; 10];
        msg.push(TMOVE);
        msg.extend_from_slice(&[0x10, 0x27, 0xf0, 0xd8, 0x04, 0x73]);
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"setinfo \"smooth\" \"0\"\n\0");
        msg.push(CLC_STRINGCMD);
        msg.extend_from_slice(b"drop\0");
        msg.extend_from_slice(&[3, 1, 2, 3]);
        assert_eq!(
            cmds(&msg),
            [b"setinfo \"smooth\" \"0\"\n".to_vec(), b"drop".to_vec()]
        );

        // The marker is only honoured after the header, and needs the stringcmd byte.
        assert!(cmds(b"\x04setinfo x y\0").is_empty());
        assert!(cmds(&[&[0u8; 10][..], b"setinfo x y\0"].concat()).is_empty());
        assert!(cmds(&[&[0u8; 10][..], b"\x04say drop\0"].concat()).is_empty());
        assert!(cmds(&[0u8; 11]).is_empty());
        assert!(cmds(b"short").is_empty());
    }

    #[tokio::test]
    async fn lost_reader_marks_the_peer_for_dropping() {
        let (events, _rx) = mpsc::channel(1);
        let mut peers = Peers::default();
        let registration = Registration {
            to: "127.0.0.1:27500".parse().unwrap(),
            from: "127.0.0.1:27001".parse().unwrap(),
            userinfo: b"\\name\\x",
            qport: 5,
            proto: Protocol::Qw,
        };
        let index = peers.register(registration, 8, &events).unwrap();
        let userid = peers.get_index(index).unwrap().userid();

        peers.lose(userid, "socket gone");
        assert!(peers.get_index(index).unwrap().state() == PeerState::Drop);

        // A reconnect before the tick removes it gets a fresh peer, not the dead one.
        let registration = Registration {
            to: "127.0.0.1:27500".parse().unwrap(),
            from: "127.0.0.1:27001".parse().unwrap(),
            userinfo: b"\\name\\x",
            qport: 5,
            proto: Protocol::Qw,
        };
        let index = peers.register(registration, 8, &events).unwrap();
        assert_eq!(peers.len(), 1);
        let peer = peers.get_index(index).unwrap();
        assert!(peer.userid() != userid);
        assert!(peer.state() == PeerState::Challenge);
        peers.drop_dead();
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn colors_are_clamped() {
        assert_eq!(parse_color(b"\\topcolor\\4", b"topcolor"), 4);
        assert_eq!(parse_color(b"\\topcolor\\99", b"topcolor"), 16);
        assert_eq!(parse_color(b"\\topcolor\\-3", b"topcolor"), 0);
        assert_eq!(parse_color(b"\\name\\x", b"topcolor"), 0);
    }
}
