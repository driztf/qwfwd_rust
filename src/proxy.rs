//! The proxy: wires the subsystems together, runs the event loop and
//! dispatches console commands to whichever subsystem owns them.

mod svc;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::ban::{self, Bans};
use crate::cmd::{Args, Command, Shell};
use crate::cvar;
use crate::msg::MSG_BUF_SIZE;
use crate::pacer::Smoothing;
use crate::peer::{self, PeerPacket, Peers};
use crate::protocol::{QWFWD_DEFAULT_PORT, QWFWD_URL, QWFWD_VERSION, QWFWD_VERSION_SHORT};
use crate::query::{self, Query, Resolution};
use crate::whitelist::{self, Whitelist};
use crate::{console, cprint, dprint, net};

use svc::{Challenges, LookupSlot, PendingConnect};

const TICK_INTERVAL: Duration = Duration::from_millis(100);
const CONFIG_NAME: &str = "qwfwd.cfg";

/// Command line settings; they take priority over the config file.
pub struct Params {
    pub port: Option<u16>,
    pub ip: String,
    pub argv: Vec<String>,
}

/// Work completed off the main loop, delivered back to it.
pub enum Event {
    /// A datagram from a remote server on a peer's socket.
    PeerPacket(PeerPacket),
    /// A client's connect request whose remote host has been looked up.
    ConnectResolved(PendingConnect, Option<SocketAddrV4>),
    /// Master servers or server filters have been looked up.
    Resolved(Resolution),
    /// A peer's socket can no longer be read, so the peer is useless.
    PeerLost { userid: i32, error: String },
}

/// A console command the shell hands to the proxy, tagged with the
/// subsystem it operates on.
#[derive(Clone, Copy)]
pub enum Handler {
    Bans(ban::Cmd),
    Whitelist(whitelist::Cmd),
    Query(query::Cmd),
    Peers(peer::Cmd),
}

pub struct Proxy {
    shell: Shell<Handler>,
    bans: Bans,
    whitelist: Whitelist,
    query: Query,
    challenges: Challenges,
    /// Connect requests whose host is being looked up, by client address.
    lookups: HashMap<SocketAddrV4, LookupSlot>,
    /// The smoothing cvars as last read; refreshed when one of them changes.
    smoothing: Smoothing,
    peers: Peers,
    events: mpsc::Sender<Event>,
    reload_requested: bool,
}

impl Proxy {
    fn new(events: mpsc::Sender<Event>) -> Self {
        let mut shell = Shell::new();
        let cvars = &mut shell.cvars;
        cvars.get("developer", "0", 0);
        cvars.get("*version", QWFWD_VERSION, cvar::READONLY | cvar::SERVERINFO);
        cvars.get("hostname", "unnamed qwfwd", cvar::SERVERINFO);
        cvars.get("maxclients", "128", cvar::SERVERINFO);
        cvars.get("hostport", "", cvar::SERVERINFO);
        cvars.get("countrycode", "", cvar::SERVERINFO);
        cvars.get("city", "", cvar::SERVERINFO);
        cvars.get("coords", "", cvar::SERVERINFO);
        Smoothing::register_cvars(cvars);

        for (name, cmd) in ban::COMMANDS {
            shell.register(name, Command::External(Handler::Bans(*cmd)));
        }
        for (name, cmd) in whitelist::COMMANDS {
            shell.register(name, Command::External(Handler::Whitelist(*cmd)));
        }
        for (name, cmd) in query::COMMANDS {
            shell.register(name, Command::External(Handler::Query(*cmd)));
        }
        for (name, cmd) in peer::COMMANDS {
            shell.register(name, Command::External(Handler::Peers(*cmd)));
        }

        let smoothing = Smoothing::from_cvars(&shell.cvars);
        Proxy {
            shell,
            smoothing,
            bans: Bans::default(),
            whitelist: Whitelist::default(),
            query: Query::new(),
            challenges: Challenges::default(),
            lookups: HashMap::new(),
            peers: Peers::default(),
            events,
            reload_requested: false,
        }
    }

    fn max_clients(&self) -> usize {
        usize::try_from(self.shell.cvars.int("maxclients")).unwrap_or(0)
    }

    /// Runs buffered console commands until the buffer is empty or a `wait` is hit.
    fn execute_buffer(&mut self) {
        while let Some(line) = self.shell.cbuf.next_line() {
            if let Some((handler, args)) = self.shell.execute_line(&line) {
                self.dispatch(handler, &args);
            }
            if self.shell.cbuf.take_wait() {
                break;
            }
        }
        // Console commands are the only way cvars change, so this is the
        // one place the cached settings can go stale.
        if Smoothing::cvars_modified(&mut self.shell.cvars) {
            self.smoothing = Smoothing::from_cvars(&self.shell.cvars);
        }
    }

    fn dispatch(&mut self, handler: Handler, args: &Args) {
        match handler {
            Handler::Bans(cmd) => cmd(&mut self.bans, &mut self.shell.cbuf, args),
            Handler::Whitelist(cmd) => cmd(&mut self.whitelist, args),
            Handler::Query(cmd) => cmd(&mut self.query, args),
            Handler::Peers(cmd) => cmd(&self.peers, &self.smoothing, args),
        }
    }

    /// Periodic housekeeping, run on the tick rather than per packet.
    fn tick(&mut self, socket: &UdpSocket) {
        if std::mem::take(&mut self.reload_requested) {
            self.whitelist.purge();
            self.shell
                .cbuf
                .insert_text(format!("exec {CONFIG_NAME}\n").as_bytes());
        }
        self.execute_buffer();
        self.peers.flush(&self.smoothing);
        self.peers.maintenance();
        self.peers.drop_dead();
        self.query.frame(
            &mut self.shell.cvars,
            socket,
            self.peers.len(),
            &self.events,
        );
        self.bans.clean_expired();
    }

    fn handle_event(&mut self, socket: &UdpSocket, event: Event) {
        match event {
            Event::PeerPacket(packet) => {
                if !self.bans.is_banned(packet.from) {
                    self.peers.handle_server_packet(socket, packet);
                }
            }
            Event::ConnectResolved(pending, to) => self.lookup_finished(socket, pending, to),
            Event::Resolved(resolution) => self.query.apply_resolution(resolution),
            Event::PeerLost { userid, error } => self.peers.lose(userid, &error),
        }
    }
}

pub async fn run(params: Params) -> Result<(), String> {
    cprint!("\nqwfwd v{QWFWD_VERSION_SHORT} by Ivan 'qqshka' Bolsunov.\n");
    cprint!("For non-commercial use only. No warranty. Use at your own risk.\n");
    cprint!("{QWFWD_URL}\n\n");

    let (events, mut event_rx) = mpsc::channel(256);
    let mut proxy = Proxy::new(events);

    proxy
        .shell
        .cbuf
        .insert_text(format!("exec {CONFIG_NAME}\n").as_bytes());
    proxy.execute_buffer();
    proxy
        .shell
        .cbuf
        .insert_text(format!("exec {}\n", ban::LISTIP_NAME).as_bytes());
    proxy.execute_buffer();

    let socket = init_network(&mut proxy, &params).await?;
    Query::register_cvars(&mut proxy.shell.cvars);
    proxy.shell.cvars.locked = true;

    proxy.shell.stuff_cmds(&params.argv);
    proxy.execute_buffer();

    cprint!(
        "qwfwd: ready to rock at {}:{}\n",
        proxy.shell.cvars.string("net_ip"),
        proxy.shell.cvars.int("net_port")
    );

    let (mut console_rx, console_ack) = spawn_console();
    let mut hangup = hangup_signal()?;
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    let mut msg = Vec::with_capacity(MSG_BUF_SIZE);

    while !proxy.shell.exit_requested() {
        msg.resize(MSG_BUF_SIZE, 0);
        // Wake exactly when the next smoothed packet is due, not on the tick.
        let pacer_deadline = proxy.peers.next_deadline().map(tokio::time::Instant::from);
        tokio::select! {
            received = socket.recv_from(&mut msg) => match received {
                Ok((len, from)) => {
                    if len >= MSG_BUF_SIZE {
                        dprint!("oversize packet from {} dropped\n", from.ip());
                    } else if let Some(from) = net::v4(from) {
                        msg.truncate(len);
                        proxy.handle_client_packet(&socket, from, &mut msg);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                    dprint!("connection reset on the proxy socket\n");
                }
                Err(err) if net::is_oversize(&err) => {
                    dprint!("oversize packet on the proxy socket dropped\n");
                }
                Err(err) => return Err(format!("recvfrom on the proxy socket: {err}")),
            },
            Some(event) = event_rx.recv() => proxy.handle_event(&socket, event),
            Some(line) = console_rx.recv() => {
                proxy.shell.cbuf.insert_text(line.as_bytes());
                proxy.execute_buffer();
                if !proxy.shell.exit_requested() {
                    let _ = console_ack.try_send(());
                }
            }
            _ = hangup.recv() => proxy.reload_requested = true,
            _ = ticker.tick() => proxy.tick(&socket),
            _ = tokio::time::sleep_until(pacer_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if pacer_deadline.is_some() => proxy.peers.flush(&proxy.smoothing),
        }
    }

    Ok(())
}

/// Registers the `net_ip`/`net_port` cvars (command line beats config) and
/// binds the proxy socket.
async fn init_network(proxy: &mut Proxy, params: &Params) -> Result<UdpSocket, String> {
    let cvars = &mut proxy.shell.cvars;
    let ip = if params.ip.is_empty() {
        "0.0.0.0"
    } else {
        &params.ip
    };
    let port = params.port.unwrap_or(QWFWD_DEFAULT_PORT).to_string();

    if params.ip.is_empty() {
        cvars.get("net_ip", ip, cvar::NOSET);
    } else {
        cvars.full_set("net_ip", ip, cvar::NOSET);
    }
    if params.port.is_none() {
        cvars.get("net_port", &port, cvar::NOSET);
    } else {
        cvars.full_set("net_port", &port, cvar::NOSET);
    }

    let ip: Ipv4Addr = cvars
        .string("net_ip")
        .parse()
        .map_err(|_| format!("invalid net_ip {}", cvars.string("net_ip")))?;
    let port = u16::try_from(cvars.int("net_port"))
        .map_err(|_| format!("invalid net_port {}", cvars.int("net_port")))?;

    let socket = UdpSocket::bind(SocketAddrV4::new(ip, port))
        .await
        .map_err(|err| format!("cannot bind {ip}:{port}: {err}"))?;
    dprint!("listening on {ip}:{port}\n");
    Ok(socket)
}

/// Runs an interactive line editor (history, cursor movement) on the terminal
/// and feeds each entered line to the returned receiver; the channel stays
/// silent when stdin is not a terminal.
///
/// The editor puts the terminal into raw mode while a line is being edited,
/// so after handing over a line it waits for an acknowledgement that the
/// command ran before prompting again. When the proxy is shutting down the
/// acknowledgement never comes and the terminal is left in its normal mode.
fn spawn_console() -> (mpsc::Receiver<String>, mpsc::Sender<()>) {
    let (line_tx, line_rx) = mpsc::channel(1);
    let (ack_tx, mut ack_rx) = mpsc::channel(1);
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return (line_rx, ack_tx);
    }

    std::thread::spawn(move || {
        let Ok(mut editor) = DefaultEditor::new() else {
            return;
        };
        if let Ok(printer) = editor.create_external_printer() {
            console::set_printer(Box::new(printer));
        }
        loop {
            let line = match editor.readline("] ") {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => "quit".to_owned(),
                Err(_) => return,
            };
            if !line.trim().is_empty() {
                let _ = editor.add_history_entry(&line);
            }
            if line_tx.blocking_send(line).is_err() || ack_rx.blocking_recv().is_none() {
                return;
            }
        }
    });
    (line_rx, ack_tx)
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
