//! UDP helpers: IPv4 resolution and out-of-band datagrams.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use tokio::net::UdpSocket;

use crate::dprint;
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
        dprint!("resolve: wrong host: {host}\n");
    }
    found
}

/// Whether a receive failed because the datagram was larger than the
/// buffer. Windows reports that as an error (WSAEMSGSIZE) where other
/// systems truncate the datagram and return it.
pub fn is_oversize(err: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        err.raw_os_error() == Some(10040)
    }
    #[cfg(not(windows))]
    {
        let _ = err;
        false
    }
}

/// Opens an unbound IPv4 socket for talking to one remote server.
pub fn open_ephemeral_socket() -> std::io::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

pub fn send(socket: &UdpSocket, data: &[u8], to: SocketAddrV4) {
    if let Err(err) = socket.try_send_to(data, SocketAddr::V4(to))
        && !matches!(
            err.kind(),
            ErrorKind::WouldBlock | ErrorKind::ConnectionRefused
        )
    {
        dprint!("sendto {to}: {err}\n");
    }
}

/// Sends a datagram with the `-1` out-of-band header.
pub fn send_oob(socket: &UdpSocket, to: SocketAddrV4, data: &[u8]) {
    let mut msg = MsgWriter::out_of_band(MAX_MSGLEN + PACKET_HEADER);
    msg.write(data);
    if msg.overflowed() {
        dprint!("out-of-band message to {to} too long, dropped\n");
        return;
    }
    send(socket, msg.as_bytes(), to);
}

pub fn send_oob_print(socket: &UdpSocket, to: SocketAddrV4, text: &str) {
    send_oob(socket, to, text.as_bytes());
}
