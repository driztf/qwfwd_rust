//! Wire-level constants shared by the QuakeWorld and Quake III protocols.

pub const QWFWD_VERSION: &str = "qwfwd 1.40-dev";
pub const QWFWD_VERSION_SHORT: &str = "1.40-dev";
pub const QWFWD_URL: &str = "https://github.com/QW-Group/qwfwd/";
pub const QWFWD_DEFAULT_PORT: u16 = 30000;
pub const QWFWD_PRX_KEY: &[u8] = b"prx";

pub const QW_VERSION: &str = "2.40";
pub const QW_PROTOCOL_VERSION: i32 = 28;

pub const QW_DEFAULT_SERVER_PORT: i32 = 27500;
pub const Q3_DEFAULT_SERVER_PORT: i32 = 27960;

// Out of band message ids: M = master, S = server, C = client, A = any.
pub const S2C_CHALLENGE: u8 = b'c';
pub const S2C_CONNECTION: u8 = b'j';
pub const A2A_PING: u8 = b'k';
pub const A2A_ACK: u8 = b'l';
pub const A2C_PRINT: u8 = b'n';
pub const S2M_HEARTBEAT: u8 = b'a';
pub const A2C_CLIENT_COMMAND: u8 = b'B';

/// Server to client: the server dropped us.
pub const SVC_DISCONNECT: u8 = 2;
/// Client to server: a string command follows.
pub const CLC_STRINGCMD: u8 = 4;
