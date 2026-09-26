//! The interactive console, driven through a pseudo-terminal: line editing,
//! history, pasted input and a clean exit that leaves the terminal usable.
#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::termios::{LocalFlags, tcgetattr};

const TIMEOUT: Duration = Duration::from_secs(3);

const UP: &[u8] = b"\x1b[A";
const LEFT: &[u8] = b"\x1b[D";
const CTRL_A: &[u8] = b"\x01";
const CTRL_C: &[u8] = b"\x03";
const CTRL_E: &[u8] = b"\x05";

/// The proxy running with its console on a pseudo-terminal.
struct Console {
    child: Child,
    master: File,
    /// Kept open so the terminal's mode can be inspected after the proxy exits.
    slave: OwnedFd,
    output: Arc<Mutex<Vec<u8>>>,
    dir: PathBuf,
}

impl Console {
    fn start() -> Self {
        let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join(format!("qwfwd-console-{}-{port}", std::process::id()));
        std::fs::create_dir_all(dir.join("qwfwd")).unwrap();
        std::fs::write(
            dir.join("qwfwd/qwfwd.cfg"),
            "set masters \"\"\nset masters_query 0\nset masters_heartbeat 0\n",
        )
        .unwrap();

        let pty = openpty(None, None).unwrap();
        let stdio = || Stdio::from(File::from(pty.slave.try_clone().unwrap()));
        let child = Command::new(env!("CARGO_BIN_EXE_qwfwd"))
            .arg(port.to_string())
            .arg("127.0.0.1")
            .current_dir(&dir)
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio())
            .spawn()
            .unwrap();

        let master = File::from(pty.master);
        let output = Arc::new(Mutex::new(Vec::new()));
        thread::spawn({
            let mut reader = master.try_clone().unwrap();
            let output = Arc::clone(&output);
            move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    output.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            }
        });

        let console = Console {
            child,
            master,
            slave: pty.slave,
            output,
            dir,
        };
        console.wait_for(b"ready to rock", 1);
        console
    }

    fn send(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
        self.master.flush().unwrap();
    }

    fn occurrences(&self, needle: &[u8]) -> usize {
        let output = self.output.lock().unwrap();
        output
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    /// Waits until `needle` has appeared `count` times in the terminal output.
    fn wait_for(&self, needle: &[u8], count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.occurrences(needle) < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {:?} x{count}; output so far:\n{}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&self.output.lock().unwrap())
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "proxy did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether the terminal is back in canonical, echoing mode.
    fn terminal_is_cooked(&self) -> bool {
        let flags = tcgetattr(&self.slave).unwrap().local_flags;
        flags.contains(LocalFlags::ICANON) && flags.contains(LocalFlags::ECHO)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn history_recall_and_cursor_editing() {
    let mut console = Console::start();

    console.send(b"echo first\r");
    console.wait_for(b"first ", 1);
    console.send(b"echo second\r");
    console.wait_for(b"second ", 1);

    // Up twice recalls "echo first"; Enter runs it again.
    console.send(&[UP, UP, b"\r"].concat());
    console.wait_for(b"first ", 2);

    // Move the cursor back three places and insert a character.
    console.send(&[b"echo abc", LEFT, LEFT, LEFT, b"X\r"].concat());
    console.wait_for(b"Xabc ", 1);

    // Ctrl-A to the start, insert the command name, Ctrl-E to the end.
    console.send(&[b"UNSEEN", CTRL_A, b"echo ", CTRL_E, b" tail\r"].concat());
    console.wait_for(b"UNSEEN tail ", 1);
}

#[test]
fn pasted_input_is_processed_without_waiting_for_more_keys() {
    let mut console = Console::start();
    // Everything in one write, as a paste or a script would deliver it.
    console.send(b"cvarlist\r");
    let started = Instant::now();
    console.wait_for(b"variables", 1);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "output only arrived after {:?}",
        started.elapsed()
    );
}

#[test]
fn ctrl_c_quits_and_restores_the_terminal() {
    let mut console = Console::start();
    console.send(b"echo alive\r");
    console.wait_for(b"alive ", 1);
    assert!(!console.terminal_is_cooked(), "editing should use raw mode");

    console.send(CTRL_C);
    let status = console.wait_for_exit();
    assert!(status.success(), "exit status {status}");
    assert!(console.terminal_is_cooked(), "terminal left in raw mode");
}
