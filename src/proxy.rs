//! Proxy state, startup sequence and the main event loop.

use std::io::IsTerminal;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::ban::Bans;
use crate::cmd::{Args, Commands};
use crate::cvar::{self, Cvars};
use crate::info::MAX_INFO_STRING;
use crate::msg::MSG_BUF_SIZE;
use crate::peer::{Peer, PeerPacket};
use crate::protocol::{QWFWD_DEFAULT_PORT, QWFWD_URL, QWFWD_VERSION, QWFWD_VERSION_SHORT};
use crate::query::Query;
use crate::svc::Challenges;
use crate::whitelist::Whitelist;
use crate::{cprint, dprint, info, net};

const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const CONFIG_NAME: &str = "qwfwd.cfg";

/// Command line settings; they take priority over the config file.
pub struct Params {
    pub port: i32,
    pub ip: String,
    pub argv: Vec<String>,
}

pub struct Proxy {
    pub cvars: Cvars,
    pub cmds: Commands,
    pub bans: Bans,
    pub whitelist: Whitelist,
    pub query: Query,
    pub challenges: Challenges,
    pub peers: Vec<Peer>,
    pub next_userid: i32,
    pub peer_tx: mpsc::Sender<PeerPacket>,
    want_exit: bool,
    reload_requested: bool,
}

impl Proxy {
    pub fn new(peer_tx: mpsc::Sender<PeerPacket>) -> Self {
        let mut proxy = Proxy {
            cvars: Cvars::default(),
            cmds: Commands::default(),
            bans: Bans::default(),
            whitelist: Whitelist::default(),
            query: Query::new(),
            challenges: Challenges::default(),
            peers: Vec::new(),
            next_userid: 0,
            peer_tx,
            want_exit: false,
            reload_requested: false,
        };

        proxy.register_script_commands();
        proxy.register_cvar_commands();

        proxy.cvars.get("developer", "0", 0);
        proxy
            .cvars
            .get("*version", QWFWD_VERSION, cvar::READONLY | cvar::SERVERINFO);
        proxy
            .cvars
            .get("hostname", "unnamed qwfwd", cvar::SERVERINFO);
        proxy.cvars.get("maxclients", "128", cvar::SERVERINFO);
        proxy.cvars.get("hostport", "", cvar::SERVERINFO);
        proxy.cvars.get("countrycode", "", cvar::SERVERINFO);
        proxy.cvars.get("city", "", cvar::SERVERINFO);
        proxy.cvars.get("coords", "", cvar::SERVERINFO);

        proxy.cmds.register("quit", cmd_quit);
        proxy.cmds.register("serverinfo", cmd_serverinfo);
        proxy.register_whitelist_commands();
        proxy.register_ban_commands();
        proxy.register_peer_commands();
        proxy.register_query_commands();

        proxy
    }

    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        Proxy::new(mpsc::channel(1).0)
    }

    /// Per-frame housekeeping; runs after every event.
    async fn frame(&mut self, socket: &UdpSocket) {
        if std::mem::take(&mut self.reload_requested) {
            self.whitelist.purge();
            self.cmds
                .cbuf
                .insert_text(format!("exec {CONFIG_NAME}\n").as_bytes());
        }
        self.execute_buffer();
        self.peer_maintenance();
        self.drop_dead_peers();
        self.query
            .frame(&mut self.cvars, socket, self.peers.len())
            .await;
        self.bans.clean_expired();
    }
}

pub async fn run(params: Params) -> Result<(), String> {
    cprint!("\nqwfwd v{QWFWD_VERSION_SHORT} by Ivan 'qqshka' Bolsunov.\n");
    cprint!("For non-commercial use only. No warranty. Use at your own risk.\n");
    cprint!("{QWFWD_URL}\n\n");

    let (peer_tx, mut peer_rx) = mpsc::channel(256);
    let mut proxy = Proxy::new(peer_tx);

    proxy
        .cmds
        .cbuf
        .insert_text(format!("exec {CONFIG_NAME}\n").as_bytes());
    proxy.execute_buffer();
    proxy.ban_init();

    let socket = init_network(&mut proxy, &params).await?;
    Query::register_cvars(&mut proxy.cvars);
    proxy.cvars.locked = true;

    proxy.stuff_cmds(&params.argv);
    proxy.execute_buffer();

    cprint!(
        "qwfwd: ready to rock at {}:{}\n",
        proxy.cvars.string("net_ip"),
        proxy.cvars.int("net_port")
    );

    let mut stdin_rx = spawn_stdin_reader();
    let mut hangup = hangup_signal()?;
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    let mut msg = Vec::with_capacity(MSG_BUF_SIZE);

    while !proxy.want_exit {
        msg.resize(MSG_BUF_SIZE, 0);
        tokio::select! {
            received = socket.recv_from(&mut msg) => match received {
                Ok((len, from)) => {
                    if len >= MSG_BUF_SIZE {
                        cprint!("NET_GetPacket: Oversize packet from {}\n", from.ip());
                    } else if let Some(from) = net::v4(from) {
                        msg.truncate(len);
                        proxy.handle_client_packet(&socket, from, &mut msg).await;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                    dprint!("NET_GetPacket: Connection was forcibly closed\n");
                }
                Err(err) => return Err(format!("NET_GetPacket: recvfrom: {err}")),
            },
            Some(packet) = peer_rx.recv() => proxy.handle_server_packet(&socket, packet),
            Some(line) = stdin_rx.recv() => proxy.cmds.cbuf.insert_text(line.as_bytes()),
            _ = hangup.recv() => proxy.reload_requested = true,
            _ = ticker.tick() => {}
        }
        proxy.frame(&socket).await;
    }

    Ok(())
}

/// Registers the `net_ip`/`net_port` cvars (command line beats config) and
/// binds the proxy socket.
async fn init_network(proxy: &mut Proxy, params: &Params) -> Result<UdpSocket, String> {
    let ip = if params.ip.is_empty() {
        "0.0.0.0"
    } else {
        &params.ip
    };
    let port = if params.port != 0 {
        params.port
    } else {
        i32::from(QWFWD_DEFAULT_PORT)
    }
    .to_string();

    if params.ip.is_empty() {
        proxy.cvars.get("net_ip", ip, cvar::NOSET);
    } else {
        proxy.cvars.full_set("net_ip", ip, cvar::NOSET);
    }
    if params.port == 0 {
        proxy.cvars.get("net_port", &port, cvar::NOSET);
    } else {
        proxy.cvars.full_set("net_port", &port, cvar::NOSET);
    }

    let ip: Ipv4Addr = proxy
        .cvars
        .string("net_ip")
        .parse()
        .map_err(|_| format!("NET_Init: invalid net_ip {}", proxy.cvars.string("net_ip")))?;
    let port = u16::try_from(proxy.cvars.int("net_port"))
        .map_err(|_| format!("NET_Init: invalid net_port {}", proxy.cvars.int("net_port")))?;

    let socket = UdpSocket::bind(SocketAddrV4::new(ip, port))
        .await
        .map_err(|err| format!("NET_UDP_OpenSocket: bind: {err}"))?;
    dprint!("UDP Initialized\n");
    Ok(socket)
}

/// Feeds console lines from an interactive terminal; the channel stays silent otherwise.
fn spawn_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(16);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

#[cfg(unix)]
type HangupSignal = tokio::signal::unix::Signal;

#[cfg(unix)]
fn hangup_signal() -> Result<HangupSignal, String> {
    use tokio::signal::unix::{SignalKind, signal};
    signal(SignalKind::hangup()).map_err(|err| format!("failed to install SIGHUP handler: {err}"))
}

#[cfg(not(unix))]
struct HangupSignal;

#[cfg(not(unix))]
impl HangupSignal {
    async fn recv(&mut self) -> Option<()> {
        std::future::pending().await
    }
}

#[cfg(not(unix))]
fn hangup_signal() -> Result<HangupSignal, String> {
    Ok(HangupSignal)
}

fn cmd_quit(proxy: &mut Proxy, args: &Args) {
    if args.argc() > 1 {
        std::process::exit(0);
    }
    proxy.want_exit = true;
}

/// Examine or change the serverinfo string.
fn cmd_serverinfo(proxy: &mut Proxy, args: &Args) {
    match args.argc() {
        1 => {
            cprint!("Server info settings:\n");
            info::print(&proxy.cvars.serverinfo);
            cprint!("[{}/{}]\n", proxy.cvars.serverinfo.len(), MAX_INFO_STRING);
        }
        2 => {
            let key = args.arg(1);
            let value = info::value_for_key(&proxy.cvars.serverinfo, key);
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
            let key_str = args.arg_str(1).into_owned();
            let value = args.arg_str(2).into_owned();
            match proxy.cvars.find(&key_str) {
                Some(var) if var.flags & cvar::SERVERINFO != 0 => {
                    let name = var.name.clone();
                    proxy.cvars.set(&name, &value);
                }
                _ => info::set_value_for_key(
                    &mut proxy.cvars.serverinfo,
                    key,
                    value.as_bytes(),
                    MAX_INFO_STRING,
                    true,
                ),
            }
        }
        _ => cprint!("Usage: serverinfo [ <key> [ <value> ] ]\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serverinfo_command_edits_info_and_cvars() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"serverinfo hostname proxied");
        assert_eq!(proxy.cvars.string("hostname"), "proxied");
        assert_eq!(
            info::value_for_key(&proxy.cvars.serverinfo, b"hostname"),
            b"proxied"
        );
        proxy.execute_line(b"serverinfo custom yes");
        assert_eq!(
            info::value_for_key(&proxy.cvars.serverinfo, b"custom"),
            b"yes"
        );
        proxy.execute_line(b"serverinfo *version nope");
        assert_eq!(
            info::value_for_key(&proxy.cvars.serverinfo, b"*version"),
            QWFWD_VERSION.as_bytes()
        );
    }

    #[test]
    fn quit_requests_exit() {
        let mut proxy = Proxy::new_for_tests();
        proxy.execute_line(b"quit");
        assert!(proxy.want_exit);
    }
}
