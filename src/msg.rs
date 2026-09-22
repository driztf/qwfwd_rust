//! Little-endian message reading and bounded message writing.

use crate::cprint;

pub const MSG_BUF_SIZE: usize = 8192;
pub const MAX_MSGLEN: usize = 1450;
pub const PACKET_HEADER: usize = 8;
const MAX_STRING: usize = 2048;

/// Cursor over a received datagram; every read returns `None` past the end.
pub struct MsgReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> MsgReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        MsgReader { data, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(bytes)
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub fn read_long(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a NUL terminated string, dropping 0xff bytes like the original
    /// clients and servers do.
    pub fn read_string(&mut self) -> Vec<u8> {
        self.read_until(|b| b == 0)
    }

    /// Like [`read_string`](Self::read_string) but also stops at a newline.
    pub fn read_string_line(&mut self) -> Vec<u8> {
        self.read_until(|b| b == 0 || b == b'\n')
    }

    fn read_until(&mut self, stop: impl Fn(u8) -> bool) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < MAX_STRING - 1 {
            let Some(b) = self.read_byte() else { break };
            if b == 255 {
                continue;
            }
            if stop(b) {
                break;
            }
            out.push(b);
        }
        out
    }
}

/// Growable message with a hard size limit; writes past the limit mark the
/// message as overflowed so callers can refuse to send it.
pub struct MsgWriter {
    data: Vec<u8>,
    max: usize,
    overflowed: bool,
}

impl MsgWriter {
    pub fn new(max: usize) -> Self {
        MsgWriter {
            data: Vec::new(),
            max,
            overflowed: false,
        }
    }

    /// Starts an out-of-band message (a `-1` sequence number).
    pub fn out_of_band(max: usize) -> Self {
        let mut msg = MsgWriter::new(max);
        msg.write_long(-1);
        msg
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.data.len() + bytes.len() > self.max {
            cprint!(
                "MsgWriter: overflow: cur = {}, len = {}, max = {}\n",
                self.data.len(),
                bytes.len(),
                self.max
            );
            self.data.clear();
            self.overflowed = true;
        }
        self.data.extend_from_slice(bytes);
    }

    pub fn write_byte(&mut self, value: u8) {
        self.write(&[value]);
    }

    pub fn write_short(&mut self, value: i16) {
        self.write(&value.to_le_bytes());
    }

    pub fn write_long(&mut self, value: i32) {
        self.write(&value.to_le_bytes());
    }

    /// Appends text as a NUL terminated string, extending a previous string
    /// instead of leaving a stray terminator in the middle.
    pub fn print(&mut self, text: &[u8]) {
        if self.data.last() == Some(&0) {
            self.data.pop();
        }
        self.write(text);
        self.write(&[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_tracks_short_reads() {
        let mut r = MsgReader::new(&[0xff, 0xff, 0xff, 0xff, b'h', 0xff, b'i', 0, b'x']);
        assert_eq!(r.read_long(), Some(-1));
        assert_eq!(r.read_string(), b"hi");
        assert_eq!(r.read_byte(), Some(b'x'));
        assert_eq!(r.read_long(), None);
        assert_eq!(r.read_byte(), None);
    }

    #[test]
    fn string_line_stops_at_newline() {
        let mut r = MsgReader::new(b"getchallenge\nrest");
        assert_eq!(r.read_string_line(), b"getchallenge");
        assert_eq!(r.read_string(), b"rest");
    }

    #[test]
    fn writer_print_merges_terminators() {
        let mut w = MsgWriter::out_of_band(64);
        w.write_byte(b'n');
        w.print(b"a\n");
        w.print(b"b\n");
        assert_eq!(w.as_bytes(), b"\xff\xff\xff\xffna\nb\n\0");
        assert!(!w.overflowed());
    }

    #[test]
    fn writer_flags_overflow() {
        let mut w = MsgWriter::new(4);
        w.write_long(1);
        w.write_byte(2);
        assert!(w.overflowed());
    }
}
