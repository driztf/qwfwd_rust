//! End-to-end tests: a fake QuakeWorld server and client on either side of the
//! real `qwfwd` binary, talking over loopback UDP.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The proxy's own Huffman codec, compiled into the test to build the
/// compressed `connect` a Q3 client sends.
#[path = "../src/huff.rs"]
#[allow(dead_code)]
mod huff;

const OOB: &[u8] = b"\xff\xff\xff\xff";
const TIMEOUT: Duration = Duration::from_secs(3);

/// Finds a free UDP port on loopback.
fn free_port() -> u16 {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Every datagram the fake server received, with its arrival time.
type Received = Arc<Mutex<Vec<(Instant, Vec<u8>)>>>;

/// Minimal QW server: answers the handshake, echoes game packets, records everything.
struct FakeServer {
    port: u16,
    received: Received,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// A QuakeWorld server.
    fn start() -> Self {
        Self::start_with(false)
    }

    /// A Quake III server: text challenge, `connectResponse` on connect.
    fn start_q3() -> Self {
        Self::start_with(true)
    }

    fn start_with(q3: bool) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = thread::spawn({
            let (received, stop) = (Arc::clone(&received), Arc::clone(&stop));
            move || {
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((len, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let data = &buf[..len];
                    received
                        .lock()
                        .unwrap()
                        .push((Instant::now(), data.to_vec()));

                    let reply: Vec<u8> = if data == [OOB, b"getchallenge\n"].concat() {
                        [OOB, b"c777\0"].concat()
                    } else if data == [OOB, b"getchallenge"].concat() {
                        [OOB, b"challengeResponse 777"].concat()
                    } else if data.starts_with(&[OOB, b"connect "].concat()) {
                        if q3 {
                            [OOB, b"connectResponse"].concat()
                        } else {
                            [OOB, b"j"].concat()
                        }
                    } else if data.starts_with(&[OOB, b"rcon"].concat()) {
                        [OOB, b"nrcon ok\n"].concat()
                    } else if !data.starts_with(OOB) && data.len() > 10 {
                        // Game traffic is echoed back; a bare keepalive (netchan
                        // header only) gets no reply, as from a real server.
                        [&[1, 0, 0, 0, 1, 0, 0, 0][..], b"echo:", &data[10..]].concat()
                    } else {
                        continue;
                    };
                    socket.send_to(&reply, from).unwrap();
                }
            }
        });

        FakeServer {
            port,
            received,
            stop,
            thread: Some(thread),
        }
    }

    fn received(&self, matches: impl Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, d)| matches(d))
            .map(|(_, d)| d.clone())
            .collect()
    }

    fn wait_for(&self, what: &str, matches: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        wait_until(what, || !self.received(&matches).is_empty());
        self.received(&matches).remove(0)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The real proxy binary running in a scratch directory with its own config.
struct Proxy {
    child: Child,
    addr: SocketAddr,
    dir: PathBuf,
}

impl Proxy {
    fn start(config: &str, extra_args: &[&str]) -> Self {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("qwfwd-e2e-{}-{port}", std::process::id()));
        std::fs::create_dir_all(dir.join("qwfwd")).unwrap();
        std::fs::write(dir.join("qwfwd/qwfwd.cfg"), config).unwrap();
        let log = std::fs::File::create(dir.join("proxy.log")).unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_qwfwd"))
            .arg(port.to_string())
            .arg("127.0.0.1")
            .args(extra_args)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let proxy = Proxy {
            child,
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            dir,
        };
        wait_until("proxy to start listening", || {
            proxy.log().contains("ready to rock")
        });
        proxy
    }

    fn log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(self.dir.join("proxy.log")).unwrap()).into_owned()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    socket: UdpSocket,
    proxy: SocketAddr,
}

impl Client {
    fn connect(proxy: SocketAddr) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        Client { socket, proxy }
    }

    fn send(&self, payload: &[u8]) {
        self.socket.send_to(payload, self.proxy).unwrap();
    }

    fn try_recv(&self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 8192];
        self.socket
            .recv_from(&mut buf)
            .ok()
            .map(|(len, _)| buf[..len].to_vec())
    }

    fn recv(&self, what: &str) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(reply) = self.try_recv() {
                return reply;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
        }
    }

    fn ask(&self, payload: &[u8]) -> Vec<u8> {
        self.send(payload);
        self.recv(&format!("reply to {}", String::from_utf8_lossy(payload)))
    }

    fn oob(&self, text: &[u8]) -> Vec<u8> {
        self.ask(&[OOB, text].concat())
    }

    /// Returns the challenge number handed out for this client.
    fn get_challenge(&self) -> Vec<u8> {
        let reply = self.oob(b"getchallenge\n");
        let body = reply
            .strip_prefix(OOB)
            .and_then(|r| r.strip_prefix(b"c"))
            .and_then(|r| r.strip_suffix(b"\0"))
            .unwrap_or_else(|| panic!("unexpected challenge reply {reply:?}"));
        assert!(
            !body.is_empty() && body.iter().all(|b| b.is_ascii_digit() || *b == b'-'),
            "challenge is not a number: {reply:?}"
        );
        body.to_vec()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Game packet: 10 bytes of netchan header (a fresh sequence number, the
/// acknowledged sequence and the qport) followed by the payload.
fn game_packet(payload: &[u8]) -> Vec<u8> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQUENCE: AtomicU32 = AtomicU32::new(5);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    [
        &sequence.to_le_bytes()[..],
        &[6, 0, 0, 0, 5, 0][..],
        payload,
    ]
    .concat()
}

/// Completes the QW handshake so game packets flow through to `server`.
/// `userinfo_extra` is appended to the userinfo, e.g. `\spectator\1`.
fn connect_client(proxy: &Proxy, server: &FakeServer, userinfo_extra: &[u8]) -> Client {
    let client = Client::connect(proxy.addr);
    let challenge = client.get_challenge();
    let connect = [
        OOB,
        b"connect 28 5 ",
        &challenge[..],
        b" \"\\name\\p\\prx\\127.0.0.1:",
        server.port.to_string().as_bytes(),
        userinfo_extra,
        b"\"\n",
    ]
    .concat();
    assert_eq!(client.ask(&connect), [OOB, b"j"].concat());
    wait_until("proxy to log the connection", || {
        proxy.log().contains(": connection")
    });
    client
}

const BASE_CONFIG: &str = "\
set hostname \"smoke proxy\"
set masters \"\"
set masters_query 0
set masters_heartbeat 0
set developer 2
alias hello echo hi $hostname
hello
";

#[test]
fn qw_session_is_proxied_end_to_end() {
    let server = FakeServer::start();
    let proxy = Proxy::start(BASE_CONFIG, &["+set", "countrycode", "se"]);
    let client = Client::connect(proxy.addr);

    assert_eq!(client.oob(b"k\n"), b"l");

    let status = client.oob(b"status");
    assert!(contains(&status, b"\\hostname\\smoke proxy"), "{status:?}");
    assert!(contains(&status, b"\\countrycode\\se"), "{status:?}");
    assert!(
        contains(&status, b"\\*version\\qwfwd 1.40-dev"),
        "{status:?}"
    );
    assert!(
        !contains(&status, b"666"),
        "no players expected: {status:?}"
    );

    let challenge = client.get_challenge();
    // Quake names carry high-bit bytes, so the userinfo is built as raw bytes.
    let userinfo = [
        &b"\\name\\tes\xf4er\\topcolor\\4\\bottomcolor\\13\\prx\\127.0.0.1:"[..],
        server.port.to_string().as_bytes(),
    ]
    .concat();
    let connect = [
        OOB,
        b"connect 28 5 ",
        &challenge,
        b" \"",
        &userinfo,
        b"\"\n",
    ]
    .concat();
    assert_eq!(client.ask(&connect), [OOB, b"j"].concat());

    let forwarded = server.wait_for("proxy connect", |d| {
        d.starts_with(&[OOB, b"connect "].concat())
    });
    assert!(
        forwarded.starts_with(&[OOB, b"connect 28 5 777 \""].concat()),
        "server challenge not used: {forwarded:?}"
    );
    assert!(
        !contains(&forwarded, b"\\prx\\"),
        "prx key leaked: {forwarded:?}"
    );
    assert!(contains(&forwarded, b"\\*qwfwd\\1.40-dev"), "{forwarded:?}");
    assert!(
        contains(&forwarded, b"\\name\\tes\xf4er"),
        "name bytes mangled: {forwarded:?}"
    );

    // Give the proxy a moment to receive the server's connection ack.
    wait_until("proxy to log the connection", || {
        proxy.log().contains(": connection")
    });

    let reply = client.ask(&game_packet(b"hello"));
    assert!(
        reply.ends_with(b"echo:hello"),
        "game traffic not relayed: {reply:?}"
    );

    assert_eq!(client.oob(b"rcon pw status"), [OOB, b"nrcon ok\n"].concat());

    let status = client.oob(b"status");
    assert!(
        contains(&status, b"0 0 666 \"tes\xf4er\" \"\" 4 13\n"),
        "{status:?}"
    );

    let drop = game_packet(b"\x04drop\0");
    client.send(&drop);
    wait_until("drop to reach the server three times", || {
        server.received(|d| d == drop).len() == 3
    });
    wait_until("peer to be dropped", || {
        !contains(&client.oob(b"status"), b"666")
    });

    let log = proxy.log();
    assert!(
        log.contains("hi smoke proxy"),
        "alias/$cvar expansion missing:\n{log}"
    );
    assert!(log.contains("ready to rock at 127.0.0.1:"), "{log}");
    assert!(log.contains("added or reused"), "{log}");
    assert!(log.contains("dropped"), "{log}");
}

#[test]
fn connect_requests_are_validated() {
    let _server = FakeServer::start();
    let proxy = Proxy::start(BASE_CONFIG, &[]);
    let client = Client::connect(proxy.addr);

    let reply = client.oob(b"connect 28 5 1 \"\\name\\x\"");
    assert!(contains(&reply, b"No challenge"), "{reply:?}");

    let challenge = client.get_challenge();
    assert_eq!(
        client.get_challenge(),
        challenge,
        "challenge must be stable per address"
    );

    let reply = client.oob(b"connect 28 5 123 \"\\name\\x\\prx\\127.0.0.1\"");
    assert!(contains(&reply, b"Bad challenge"), "{reply:?}");

    let reply = client.oob(&[b"connect 27 5 ", &challenge[..], b" \"\\name\\x\""].concat());
    assert!(contains(&reply, b"Server is version 2.40"), "{reply:?}");

    let reply = client.oob(&[b"connect 28 5 ", &challenge[..], b" \"\\name\\x\""].concat());
    assert!(
        contains(&reply, b"prx userinfo key is not set"),
        "{reply:?}"
    );

    let reply = client.oob(&[b"connect 28 5 ", &challenge[..], b" \"\\name\\\\x\""].concat());
    assert!(contains(&reply, b"Invalid userinfo"), "{reply:?}");

    // A bare "getchallenge" (no newline) is how Q3 clients ask.
    let reply = client.oob(b"getchallenge");
    assert!(
        reply.starts_with(&[OOB, b"challengeResponse "].concat()),
        "{reply:?}"
    );
}

#[test]
fn tracking_spectator_drop_is_detected() {
    let server = FakeServer::start();
    let proxy = Proxy::start(BASE_CONFIG, &[]);
    let client = connect_client(&proxy, &server, b"\\spectator\\1");

    // ezQuake's autocam puts a clc_tmove ahead of the reliable drop.
    let tmove = [5u8, 0x10, 0x27, 0xf0, 0xd8, 0x04, 0x73];
    let drop = game_packet(&[&tmove[..], b"\x04drop\0"].concat());
    client.send(&drop);
    wait_until("drop to reach the server three times", || {
        server.received(|d| d == drop).len() == 3
    });
    wait_until("peer to be dropped", || {
        !contains(&client.oob(b"status"), b"666")
    });
}

#[test]
fn prx_host_names_are_looked_up_off_the_loop() {
    let server = FakeServer::start();
    let proxy = Proxy::start(BASE_CONFIG, &[]);
    let client = Client::connect(proxy.addr);
    let challenge = client.get_challenge();
    // A name rather than an address goes through the resolver task.
    let connect = [
        OOB,
        b"connect 28 5 ",
        &challenge[..],
        b" \"\\name\\p\\prx\\localhost:",
        server.port.to_string().as_bytes(),
        b"\"\n",
    ]
    .concat();
    assert_eq!(client.ask(&connect), [OOB, b"j"].concat());
    wait_until("proxy to log the connection", || {
        proxy.log().contains(": connection")
    });
    let reply = client.ask(&game_packet(b"hello"));
    assert!(reply.ends_with(b"echo:hello"), "{reply:?}");
}

#[test]
fn banned_clients_are_ignored() {
    let proxy = Proxy::start(&format!("{BASE_CONFIG}addip 127.0.0.1\n"), &[]);
    let client = Client::connect(proxy.addr);

    client.send(&[OOB, b"ping"].concat());
    assert!(client.try_recv().is_none(), "banned client got a reply");
}

/// A Q3 client's `connect`: the userinfo carries the challenge, and
/// everything after `connect ` is Huffman-compressed.
fn q3_connect_packet(challenge: &[u8], server_port: u16) -> Vec<u8> {
    let mut packet = [
        OOB,
        b"connect \"\\name\\q3p\\protocol\\68\\qport\\5\\challenge\\",
        challenge,
        b"\\prx\\127.0.0.1:",
        server_port.to_string().as_bytes(),
        b"\"",
    ]
    .concat();
    huff::compress(&mut packet, 12);
    packet
}

#[test]
fn q3_client_is_proxied_end_to_end() {
    let server = FakeServer::start_q3();
    let proxy = Proxy::start(BASE_CONFIG, &[]);
    let client = Client::connect(proxy.addr);

    // A bare "getchallenge" marks the client as Q3.
    let reply = client.oob(b"getchallenge");
    let prefix = [OOB, b"challengeResponse "].concat();
    let challenge = reply
        .strip_prefix(prefix.as_slice())
        .unwrap_or_else(|| panic!("unexpected challenge reply {reply:?}"))
        .to_vec();

    // The first connect starts the proxy's own handshake with the server; the
    // client hears nothing back until that completes.
    client.send(&q3_connect_packet(&challenge, server.port));
    assert!(client.try_recv().is_none(), "no reply expected yet");

    let request = server.wait_for("q3 getchallenge", |d| d == [OOB, b"getchallenge"].concat());
    assert_eq!(request, [OOB, b"getchallenge"].concat());
    let mut forwarded = server.wait_for("proxy connect", |d| {
        d.starts_with(&[OOB, b"connect "].concat())
    });
    huff::decompress(&mut forwarded, 12, 8192);
    assert!(
        contains(&forwarded, b"\\challenge\\777"),
        "server challenge not used: {forwarded:?}"
    );
    assert!(contains(&forwarded, b"\\*qwfwd\\1.40-dev"), "{forwarded:?}");
    assert!(
        !contains(&forwarded, b"\\prx\\"),
        "prx key leaked: {forwarded:?}"
    );
    wait_until("proxy to log connectResponse", || {
        proxy.log().contains("connectResponse")
    });

    // Q3 scrambles game packets with the challenge, so a reconnecting client
    // must be handed the server's challenge, and then gets its connectResponse.
    assert_eq!(
        client.oob(b"getchallenge"),
        [OOB, b"challengeResponse 777"].concat()
    );
    let reply = client.ask(&q3_connect_packet(b"777", server.port));
    assert_eq!(reply, [OOB, b"connectResponse"].concat());

    let reply = client.ask(&game_packet(b"q3hello"));
    assert!(
        reply.ends_with(b"echo:q3hello"),
        "game traffic not relayed: {reply:?}"
    );
}

#[cfg(unix)]
#[test]
fn sighup_reloads_the_config_and_purges_the_whitelist() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let server = FakeServer::start();
    let blocking = format!("{BASE_CONFIG}set hostname before\nwhitelistadd 10.9.9.9\n");
    let proxy = Proxy::start(&blocking, &[]);
    let client = Client::connect(proxy.addr);
    assert!(
        contains(&client.oob(b"status"), b"\\hostname\\before"),
        "initial hostname"
    );

    // The whitelist does not include the fake server, so connecting goes nowhere.
    let challenge = client.get_challenge();
    let connect = [
        OOB,
        b"connect 28 5 ",
        &challenge[..],
        b" \"\\name\\p\\prx\\127.0.0.1:",
        server.port.to_string().as_bytes(),
        b"\"\n",
    ]
    .concat();
    client.send(&connect);
    assert!(
        client.try_recv().is_none(),
        "connect should be refused silently"
    );
    assert!(
        server.received(|d| d.starts_with(OOB)).is_empty(),
        "nothing should reach the server"
    );

    std::fs::write(
        proxy.dir.join("qwfwd/qwfwd.cfg"),
        format!("{BASE_CONFIG}set hostname after\n"),
    )
    .unwrap();
    kill(Pid::from_raw(proxy.child.id() as i32), Signal::SIGHUP).unwrap();
    wait_until("config to be re-executed", || {
        contains(&client.oob(b"status"), b"\\hostname\\after")
    });
    assert_eq!(proxy.log().matches("execing qwfwd.cfg").count(), 2);

    // With the whitelist purged the same connect now goes through.
    let connected = connect_client(&proxy, &server, b"");
    let reply = connected.ask(&game_packet(b"hello"));
    assert!(reply.ends_with(b"echo:hello"), "{reply:?}");
}
