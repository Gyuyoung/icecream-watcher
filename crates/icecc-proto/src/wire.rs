//! Wire primitives for the Icecream scheduler protocol.
//!
//! Framing, verified against a live `icecc-scheduler` 1.4 (protocol 43):
//!
//! ```text
//! connect TCP
//!   ->  4 bytes: our version, 0, 0, 0     (raw little-endian, NOT htonl)
//!   <-  4 bytes: peer version             (little-endian)
//!   ->  4 bytes: min(ours, peer)          (little-endian)
//!   <-  4 bytes: confirmation == agreed
//! then, repeating:
//!       u32 big-endian  length   (covers type + payload)
//!       u32 big-endian  type
//!       payload
//! ```
//!
//! Note the asymmetry: the version handshake is little-endian raw bytes, while
//! every scalar inside a message payload is big-endian (`htonl`/`ntohl`).
//! See `icecream/services/comm.cpp` (`MsgChannel::update_state`, `operator>>`).

use std::fmt;

/// Protocol version we advertise. The peer answers with its own and both sides
/// settle on `min(ours, theirs)`, so this is an upper bound, not a requirement.
/// `PROTOCOL_VERSION` in `icecream/services/comm.h`.
pub const PROTOCOL_VERSION: u32 = 44;

/// Oldest peer we will talk to (`MIN_PROTOCOL_VERSION` upstream).
pub const MIN_PROTOCOL_VERSION: u32 = 21;

/// Below this, `MON_GET_CS` carries the full `GetCSMsg` body (environment lists
/// and per-version feature fields) instead of the short monitor form. We decline
/// to decode that rather than carry the legacy path; see ARCHITECTURE.md §2.6.
pub const MIN_MON_GET_CS_VERSION: u32 = 29;

/// `MAX_MSG_SIZE` in `icecream/services/comm.cpp`.
pub const MAX_MSG_SIZE: u32 = 1024 * 1024;

/// Default scheduler TCP port, and the UDP port used for discovery.
pub const DEFAULT_SCHEDULER_PORT: u16 = 8765;

/// Default Icecream network name (`DiscoverSched`, `scheduler.cpp`).
pub const DEFAULT_NETNAME: &str = "ICECREAM";

/// Message type tags. The upstream enum is ASCII-ordered from `UNKNOWN = 'A'`,
/// so these are stable numeric values, not arbitrary indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MsgType {
    Unknown = 65,
    Ping = 66,
    End = 67,
    JobLocalDone = 79,
    MonLogin = 82,
    MonGetCs = 83,
    MonJobBegin = 84,
    MonJobDone = 85,
    MonLocalJobBegin = 86,
    MonStats = 87,
}

impl MsgType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            65 => Self::Unknown,
            66 => Self::Ping,
            67 => Self::End,
            79 => Self::JobLocalDone,
            82 => Self::MonLogin,
            83 => Self::MonGetCs,
            84 => Self::MonJobBegin,
            85 => Self::MonJobDone,
            86 => Self::MonLocalJobBegin,
            87 => Self::MonStats,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub enum ProtoError {
    /// Payload ended before a field could be read.
    Truncated { want: usize, have: usize },
    /// Frame length outside `[4, MAX_MSG_SIZE]`.
    BadFrameLen(u32),
    /// Peer advertised a version we refuse to speak.
    UnsupportedVersion(u32),
    /// Peer did not echo back the agreed version.
    HandshakeMismatch { agreed: u32, got: u32 },
    /// A string field was not valid UTF-8.
    BadUtf8,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { want, have } => {
                write!(f, "truncated payload: need {want} more bytes, have {have}")
            }
            Self::BadFrameLen(n) => write!(f, "invalid frame length {n}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported peer protocol version {v}"),
            Self::HandshakeMismatch { agreed, got } => {
                write!(f, "handshake mismatch: agreed {agreed}, peer echoed {got}")
            }
            Self::BadUtf8 => write!(f, "string field is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ProtoError {}

pub type Result<T> = std::result::Result<T, ProtoError>;

/// Cursor over a message payload. All scalars are big-endian.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(ProtoError::Truncated {
                want: n,
                have: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Optional trailing field: present only when the negotiated protocol is new
    /// enough *and* the payload actually has bytes left. Upstream relies on
    /// `operator>>` yielding 0 past the end, so a short frame is not an error.
    pub fn u32_opt(&mut self) -> Option<u32> {
        if self.remaining() < 4 {
            None
        } else {
            self.u32().ok()
        }
    }

    /// String as written by `MsgChannel::operator<<(const std::string&)`:
    /// `u32 len = 1 + s.length()`, then `len` bytes holding the text plus its
    /// NUL terminator. Content is everything up to the first NUL.
    pub fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        if len == 0 {
            // Upstream yields "" for a zero length rather than failing.
            return Ok(String::new());
        }
        let raw = self.take(len)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        std::str::from_utf8(&raw[..end])
            .map(|s| s.to_owned())
            .map_err(|_| ProtoError::BadUtf8)
    }
}

/// Encode a complete frame: `[u32 len][u32 type][payload]`, where `len` covers
/// the type word as well.
pub fn encode_frame(ty: MsgType, payload: &[u8]) -> Vec<u8> {
    let len = 4 + payload.len() as u32;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&(ty as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// The empty `MON_LOGIN` frame that turns a connection into a monitor.
pub fn mon_login_frame() -> Vec<u8> {
    encode_frame(MsgType::MonLogin, &[])
}

/// A clean `END`, so the scheduler drops us without waiting for a write to fail.
pub fn end_frame() -> Vec<u8> {
    encode_frame(MsgType::End, &[])
}

/// Validate a frame length word and return the payload size.
pub fn payload_len(frame_len: u32) -> Result<usize> {
    if !(4..=MAX_MSG_SIZE).contains(&frame_len) {
        return Err(ProtoError::BadFrameLen(frame_len));
    }
    Ok((frame_len - 4) as usize)
}

/// Version handshake, byte level. The four steps are split out so both the live
/// connection and the offline tests drive identical logic.
pub struct Handshake {
    ours: u32,
    agreed: Option<u32>,
}

impl Handshake {
    pub fn new(ours: u32) -> Self {
        Self { ours, agreed: None }
    }

    /// Step 1: what we send immediately on connect. Little-endian raw bytes.
    pub fn greeting(&self) -> [u8; 4] {
        (self.ours).to_le_bytes()
    }

    /// Step 2/3: given the peer's version, settle on `min` and return the bytes
    /// to echo back.
    pub fn accept_peer(&mut self, peer: [u8; 4]) -> Result<[u8; 4]> {
        let peer = u32::from_le_bytes(peer);
        if !(MIN_PROTOCOL_VERSION..=(1 << 20)).contains(&peer) {
            return Err(ProtoError::UnsupportedVersion(peer));
        }
        let agreed = peer.min(self.ours);
        self.agreed = Some(agreed);
        Ok(agreed.to_le_bytes())
    }

    /// Step 4: the peer echoes the agreed version; anything else is a mismatch.
    pub fn confirm(&self, echo: [u8; 4]) -> Result<u32> {
        let got = u32::from_le_bytes(echo);
        let agreed = self.agreed.expect("confirm before accept_peer");
        if got != agreed {
            return Err(ProtoError::HandshakeMismatch { agreed, got });
        }
        Ok(agreed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_settles_on_the_lower_version() {
        let mut hs = Handshake::new(44);
        assert_eq!(hs.greeting(), [44, 0, 0, 0]);
        // Live scheduler 1.4 answered 43, so 43 is the agreed version.
        assert_eq!(hs.accept_peer([43, 0, 0, 0]).unwrap(), [43, 0, 0, 0]);
        assert_eq!(hs.confirm([43, 0, 0, 0]).unwrap(), 43);
    }

    #[test]
    fn handshake_caps_at_our_own_version() {
        let mut hs = Handshake::new(44);
        assert_eq!(hs.accept_peer([99, 0, 0, 0]).unwrap(), [44, 0, 0, 0]);
    }

    #[test]
    fn handshake_rejects_ancient_and_absurd_peers() {
        assert!(matches!(
            Handshake::new(44).accept_peer([20, 0, 0, 0]),
            Err(ProtoError::UnsupportedVersion(20))
        ));
        assert!(matches!(
            Handshake::new(44).accept_peer([0, 0, 0, 0xff]),
            Err(ProtoError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn handshake_detects_a_bad_echo() {
        let mut hs = Handshake::new(44);
        hs.accept_peer([43, 0, 0, 0]).unwrap();
        assert!(matches!(
            hs.confirm([42, 0, 0, 0]),
            Err(ProtoError::HandshakeMismatch {
                agreed: 43,
                got: 42
            })
        ));
    }

    #[test]
    fn mon_login_is_a_bare_type_word() {
        assert_eq!(mon_login_frame(), vec![0, 0, 0, 4, 0, 0, 0, 82]);
    }

    #[test]
    fn frame_length_covers_the_type_word() {
        assert_eq!(payload_len(4).unwrap(), 0);
        assert_eq!(payload_len(12).unwrap(), 8);
        assert!(matches!(payload_len(3), Err(ProtoError::BadFrameLen(3))));
        assert!(payload_len(MAX_MSG_SIZE + 1).is_err());
    }

    #[test]
    fn strings_carry_a_nul_inside_their_length() {
        // "hi" is written as len=3 followed by b"hi\0".
        let buf = [0, 0, 0, 3, b'h', b'i', 0];
        assert_eq!(Reader::new(&buf).string().unwrap(), "hi");
    }

    #[test]
    fn zero_length_string_is_empty_not_an_error() {
        let buf = [0, 0, 0, 0];
        assert_eq!(Reader::new(&buf).string().unwrap(), "");
    }

    #[test]
    fn truncated_payload_is_reported() {
        let buf = [0, 0, 0, 9, b'x'];
        assert!(matches!(
            Reader::new(&buf).string(),
            Err(ProtoError::Truncated { .. })
        ));
    }
}
