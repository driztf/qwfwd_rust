//! Client-side handling of connectionless packets from remote servers: the
//! proxy plays the client while completing the handshake on a peer's behalf.

use super::{Peer, PeerState, Protocol};
use crate::cmd::Args;
use crate::console::qstr;
use crate::msg::{MSG_BUF_SIZE, MsgReader, MsgWriter};
use crate::protocol::{
    A2C_CLIENT_COMMAND, A2C_PRINT, Q3_CONNECT_PAYLOAD, QW_PROTOCOL_VERSION, S2C_CHALLENGE,
    S2C_CONNECTION, SVC_DISCONNECT,
};
use crate::{dprint, huff, info, net, parse};

impl Peer {
    /// Handles an out-of-band packet from the remote server. Returns whether
    /// the packet should also be passed on to the client.
    pub fn cl_connectionless(&mut self, data: &[u8]) -> bool {
        match self.proto {
            Protocol::Qw => self.cl_connectionless_qw(data),
            Protocol::Q3 => self.cl_connectionless_q3(data),
        }
    }

    fn cl_connectionless_qw(&mut self, data: &[u8]) -> bool {
        let mut reader = MsgReader::new(data);
        reader.read_long();
        let Some(command) = reader.read_byte() else {
            return false;
        };

        match command {
            S2C_CHALLENGE => {
                dprint!("{}: challenge\n", self.to.ip());
                self.challenge = parse::atoi(&reader.read_string());
                self.send_connect_qw();
                false
            }
            S2C_CONNECTION => {
                dprint!("{}: connection\n", self.to.ip());
                if self.state == PeerState::Connected {
                    dprint!("Dup connect received. Ignored.\n");
                } else {
                    self.state = PeerState::Connected;
                }
                false
            }
            A2C_CLIENT_COMMAND => {
                dprint!("{}: client command\n", self.to.ip());
                false
            }
            // Let the client see whatever the server is trying to tell it.
            A2C_PRINT => true,
            SVC_DISCONNECT => {
                dprint!("{}: svc_disconnect\n", self.to.ip());
                false
            }
            other => {
                dprint!(
                    "unhandled out-of-band message from {}: {}{}\n",
                    self.to.ip(),
                    other as char,
                    qstr(&reader.read_string())
                );
                false
            }
        }
    }

    fn send_connect_qw(&self) {
        if self.state != PeerState::Challenge {
            return;
        }
        let mut packet = MsgWriter::out_of_band(MSG_BUF_SIZE);
        packet.write(
            format!(
                "connect {QW_PROTOCOL_VERSION} {} {} \"",
                self.qport, self.challenge
            )
            .as_bytes(),
        );
        packet.write(&self.userinfo);
        packet.write(b"\"\n");
        net::send(&self.socket, packet.as_bytes(), self.to);
    }

    fn cl_connectionless_q3(&mut self, data: &[u8]) -> bool {
        let mut reader = MsgReader::new(data);
        reader.read_long();
        let line = reader.read_string_line();
        let args = Args::tokenize(&line);
        let command = args.arg_str(0);
        dprint!("CL packet {}: {}\n", self.to, qstr(&line));

        if command.eq_ignore_ascii_case("challengeResponse") {
            if self.state != PeerState::Challenge {
                dprint!("Unwanted challenge response received.  Ignored.\n");
            } else {
                self.challenge = parse::atoi(args.arg(1));
                dprint!("challengeResponse: {}\n", self.challenge);
                self.send_connect_q3();
            }
            return false;
        }

        if command.eq_ignore_ascii_case("connectResponse") {
            match self.state {
                PeerState::Connected => dprint!("Dup connect received.  Ignored.\n"),
                PeerState::Drop => {
                    dprint!("connectResponse packet while not connecting.  Ignored.\n")
                }
                PeerState::Challenge => {
                    dprint!("connectResponse\n");
                    self.state = PeerState::Connected;
                }
            }
            return false;
        }

        // The server dropped the client but still receives our packets.
        if command.eq_ignore_ascii_case("disconnect") {
            self.state = PeerState::Drop;
            return true;
        }

        if command.eq_ignore_ascii_case("print") {
            dprint!("{}", qstr(&reader.read_string()));
            return true;
        }

        false
    }

    fn send_connect_q3(&self) {
        if self.state != PeerState::Challenge {
            return;
        }
        let mut userinfo = self.userinfo.clone();
        info::set_value_for_key(
            &mut userinfo,
            b"challenge",
            self.challenge.to_string().as_bytes(),
            info::MAX_INFO_STRING + 100,
            true,
        );

        let mut msg = MsgWriter::out_of_band(MSG_BUF_SIZE);
        let mut text = b"connect \"".to_vec();
        text.extend_from_slice(&userinfo);
        text.push(b'"');
        msg.print(&text);

        let mut packet = msg.into_bytes();
        huff::compress(&mut packet, Q3_CONNECT_PAYLOAD);
        net::send(&self.socket, &packet, self.to);
    }
}
