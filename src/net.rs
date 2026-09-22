//! UDP helpers: IPv4 resolution and out-of-band datagrams.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use tokio::net::UdpSocket;

use crate::cprint;
use crate::msg::{MAX_MSGLEN, MsgWriter, PACKET_HEADER};

pub fn v4(addr: SocketAddr) -> Option<SocketAddrV4> {
    match addr {
        SocketAddr::V4(addr) => Some(addr),
        SocketAddr::V6(_) => None,
    }
}

/// Resolves a host name or dotted quad to an IPv4 address.
pub async fn resolve(host: &str, port: u16) -> Option<SocketAddrV4> {
    let found = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.filter_map(v4).next(),
        Err(_) => None,
    };
    if found.is_none() {
        cprint!("resolve: wrong host: {host}\n");
    }
    found
}

/// Opens an unbound IPv4 socket for talking to one remote server.
pub async fn open_ephemeral_socket() -> std::io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await
}

pub fn send(socket: &UdpSocket, data: &[u8], to: SocketAddrV4) {
    if let Err(err) = socket.try_send_to(data, SocketAddr::V4(to))
        && !matches!(
            err.kind(),
            ErrorKind::WouldBlock | ErrorKind::ConnectionRefused
        )
    {
        cprint!("NET_SendPacket: sendto: {err}\n");
    }
}

/// Sends a datagram with the `-1` out-of-band header.
pub fn send_oob(socket: &UdpSocket, to: SocketAddrV4, data: &[u8]) {
    let mut msg = MsgWriter::out_of_band(MAX_MSGLEN + PACKET_HEADER);
    msg.write(data);
    if msg.overflowed() {
        cprint!("Netchan_OutOfBand: overflowed\n");
        return;
    }
    send(socket, msg.as_bytes(), to);
}

pub fn send_oob_print(socket: &UdpSocket, to: SocketAddrV4, text: &str) {
    send_oob(socket, to, text.as_bytes());
}
