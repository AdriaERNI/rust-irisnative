//! 14-byte message header (ledger row 1).
//!
//! ref: go-irisnative `src/connection/message_header.go:NewMessageHeader` /
//! `SetLength` / `SetCount` / `SetStatementId` — little-endian u32 fields at
//! byte offsets 0 (body length), 4 (message count), 8 (statement id), and a
//! two-byte type word at 12..14.
//!
//! ref: py-iris iris/_MessageHeader.py:_`get_function_code` — on replies the
//! type word doubles as a status: raw SQLCODE for SQL messages, or
//! `0xCA00 + code` for sysio (global/class) messages, confirmed live by
//! `probes/refclient.py` against IRIS 2025.3 (protocol 69).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

/// Wire size of the fixed header.
///
/// ref: py-iris iris/_`MessageHeader.py:HEADER_SIZE` = 14.
pub const HEADER_SIZE: usize = 14;

/// High bit of the statement-id word marks a continuation chunk: the body
/// continues in another frame (coalesce until the bit clears).
///
/// ref: py-iris iris/_InStream.py:__`read_message_internal` (`high_bit` loop).
pub const CONTINUATION_BIT: u32 = 0x8000_0000;

/// Bias of the type word on sysio (globals/classes) replies.
///
/// ref: py-iris iris/_InStream.py:_`read_message_sysio` (`code - 51712`).
pub const SYSIO_STATUS_BIAS: u16 = 0xCA00;

/// A two-byte message type as it appears in header bytes 12..14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageType(pub [u8; 2]);

impl MessageType {
    /// Constant constructor for a two-ASCII-byte type.
    #[must_use]
    pub const fn new(a: u8, b: u8) -> Self {
        Self([a, b])
    }
}

macro_rules! msg_types {
    ($($name:ident = $a:literal, $b:literal;)*) => {
        $(
            #[doc = concat!("Message type `", stringify!($name), "` (header bytes 12..14).")]
            pub const $name: MessageType = MessageType::new($a, $b);
        )*
    };
}

// ref: go-irisnative src/connection/message_header.go const block.
msg_types! {
    HANDSHAKE = b'H', b'S';
    CONNECT = b'C', b'N';
    DISCONNECT = b'D', b'C';
    GLOBAL_GET = 0x41, 0xC2;
    GLOBAL_SET = 0x42, 0xC2;
    GLOBAL_KILL = 0x43, 0xC2;
    GLOBAL_ORDER = 0x45, 0xC2;
    GLOBAL_DATA = 0x49, 0xC2;
    CLASSMETHOD_VALUE = 0x4B, 0xC2;
    CLASSMETHOD_VOID = 0x4C, 0xC2;
    METHOD_VALUE = 0x5B, 0xC2;
    METHOD_VOID = 0x5C, 0xC2;
    PROPERTY_GET = 0x5D, 0xC2;
    PROPERTY_SET = 0x5E, 0xC2;
    DIRECT_QUERY = b'D', b'Q';
    DIRECT_UPDATE = b'D', b'U';
    FETCH_DATA = b'F', b'D';
    COMMIT = b'T', b'C';
    ROLLBACK = b'T', b'R';
    GET_SERVER_ERROR = b'O', b'E';
}

/// An encoded or decoded message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Body length that follows the header.
    pub length: u32,
    /// Message count (request: sequence position; reply: server echo).
    pub count: u32,
    /// Statement id; high bit is the continuation flag (see
    /// [`CONTINUATION_BIT`]).
    pub statement_id: u32,
    /// Type word (request) / status word (reply).
    pub type_word: [u8; 2],
}

impl Header {
    /// Header for a request of `message_type` carrying `body_len` bytes.
    ///
    /// ref: go-irisnative src/connection/message.go:Dump — length = body
    /// length, count/statement id stamped by the caller.
    #[must_use]
    pub fn request(
        message_type: MessageType,
        body_len: usize,
        count: u32,
        statement_id: u32,
    ) -> Self {
        Self {
            length: body_len as u32,
            count,
            statement_id,
            type_word: message_type.0,
        }
    }

    /// Parse a 14-byte header buffer.
    ///
    /// ref: go-irisnative src/connection/message.go:ReadMessage.
    #[must_use]
    pub fn parse(buf: &[u8; HEADER_SIZE]) -> Self {
        Self {
            length: u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4])),
            count: u32::from_le_bytes(buf[4..8].try_into().unwrap_or([0; 4])),
            statement_id: u32::from_le_bytes(buf[8..12].try_into().unwrap_or([0; 4])),
            type_word: [buf[12], buf[13]],
        }
    }

    /// Decode from a slice at least [`HEADER_SIZE`] long.
    #[must_use]
    pub fn parse_slice(buf: &[u8]) -> Self {
        let mut fixed = [0u8; HEADER_SIZE];
        fixed.copy_from_slice(buf.get(..HEADER_SIZE).unwrap_or(&[0; HEADER_SIZE]));
        Self::parse(&fixed)
    }

    /// Encode to 14 wire bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0u8; HEADER_SIZE];
        out[0..4].copy_from_slice(&self.length.to_le_bytes());
        out[4..8].copy_from_slice(&self.count.to_le_bytes());
        out[8..12].copy_from_slice(&self.statement_id.to_le_bytes());
        out[12..14].copy_from_slice(&self.type_word);
        out
    }

    /// Type word as a little-endian u16 status.
    #[must_use]
    pub fn status(&self) -> u16 {
        u16::from_le_bytes(self.type_word)
    }

    /// Whether this frame's body continues in the next frame.
    #[must_use]
    pub fn is_continuation(&self) -> bool {
        self.statement_id & CONTINUATION_BIT != 0
    }

    /// Statement id with the continuation bit masked off.
    #[must_use]
    pub fn plain_statement_id(&self) -> u32 {
        self.statement_id & !CONTINUATION_BIT
    }

    /// sysio reply code: `status - 0xCA00` (SQLCODE-shaped for SQL replies;
    /// callers pick the right reader per command).
    ///
    /// ref: py-iris iris/_InStream.py:_`read_message_sysio`.
    #[must_use]
    pub fn sysio_code(&self) -> i32 {
        i32::from(self.status()) - i32::from(SYSIO_STATUS_BIAS)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_matches_go_field_offsets() {
        // ref: go-irisnative src/connection/message.go:Dump
        let h = Header::request(GLOBAL_SET, 0x1234, 9, 2);
        let bytes = h.encode();
        assert_eq!(&bytes[0..4], &0x1234u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &9u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &2u32.to_le_bytes());
        assert_eq!(&bytes[12..14], &[0x42, 0xC2]);
        assert_eq!(Header::parse(&bytes), h);
    }

    #[test]
    fn continuation_bit_masks_off_statement_id() {
        // ref: py-iris iris/_InStream.py:__read_message_internal high_bit
        let mut raw = [0u8; HEADER_SIZE];
        raw[8..12].copy_from_slice(&CONTINUATION_BIT.to_le_bytes());
        let h = Header::parse(&raw);
        assert!(h.is_continuation());
        assert_eq!(h.plain_statement_id(), 0);
    }

    #[test]
    fn sysio_bias_decodes_like_python_reader() {
        // ref: py-iris iris/_InStream.py:_read_message_sysio
        let mut raw = [0u8; HEADER_SIZE];
        raw[12] = 0xF1;
        raw[13] = 0xCD; // LE 0xCDF1 - 0xCA00 = 1009 (<UNDEFINED>)
        assert_eq!(Header::parse(&raw).sysio_code(), 1009);
    }
}
