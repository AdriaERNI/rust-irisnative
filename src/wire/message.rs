//! Message framing: build request frames, read reply frames (ledger rows 2,
//! 11, 18, 34).
//!
//! ref: go-irisnative src/connection/message.go:ReadMessage / Message.Dump

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use std::io::Read;

use super::header::{HEADER_SIZE, Header, MessageType};
use super::list::{ListWriter, write_sql_text};

/// Upper bound for a single frame body before it is allocated. The
/// references allocate `length` bytes unchecked (go message.go:ReadMessage —
/// documented divergence, ledger row 34); a hostile 4 GiB header must not
/// turn into an allocation attempt.
///
/// IRIS never sends SQL data frames this large in practice: reply bodies
/// arrive continuation-chunked (see [`Header::is_continuation`]) with each
/// chunk well under 1 MiB in captures (`probes/captures/fetch_batch.json`).
pub const MAX_FRAME_BODY: u32 = 64 * 1024 * 1024;

/// Hard cap on a coalesced multi-chunk reply (continuation frames are
/// concatenated until the high bit clears).
pub const MAX_TOTAL_BODY: usize = 512 * 1024 * 1024;

/// A decoded reply frame.
#[derive(Debug, Clone)]
pub struct Reply {
    /// Reply header; type word carries SQLCODE / sysio status.
    pub header: Header,
    /// Body bytes (continuation chunks already coalesced).
    pub body: Vec<u8>,
}

impl Reply {
    /// Raw type word as u16 (SQL replies: SQLCODE).
    #[must_use]
    pub fn status(&self) -> u16 {
        self.header.status()
    }

    /// sysio reply code (globals/classes): type word minus `0xCA00`.
    #[must_use]
    pub fn sysio_code(&self) -> i32 {
        self.header.sysio_code()
    }
}

/// Encode a complete request frame.
///
/// ref: go-irisnative src/connection/message.go:Dump.
#[must_use]
pub fn encode_frame(
    message_type: MessageType,
    count: u32,
    statement_id: u32,
    body: &[u8],
) -> Vec<u8> {
    let header = Header::request(message_type, body.len(), count, statement_id);
    let mut frame = header.encode().to_vec();
    frame.extend_from_slice(body);
    frame
}

/// Read one reply (with coalesced continuation chunks) from a blocking
/// reader. The `Read` bound keeps this pure-IO code out of `wire/` logic —
/// callers supply the socket.
///
/// ref: go-irisnative src/connection/message.go:ReadMessage;
/// py-iris iris/_InStream.py:__`read_message_internal` (continuation loop).
///
/// # Errors
/// [`FrameError::Io`] on transport failure; [`FrameError::Oversized`] for a
/// header length beyond [`MAX_FRAME_BODY`] (hygiene row 34);
/// [`FrameError::Desync`] when the server echoes a count we did not send.
pub fn read_reply<R: Read>(reader: &mut R, expect_count: Option<u32>) -> Result<Reply, FrameError> {
    let mut head = [0u8; HEADER_SIZE];
    let header = read_one(reader, &mut head, expect_count)?;
    let mut body = checked_body(header.length)?;
    reader.read_exact(&mut body).map_err(FrameError::Io)?;
    // Continuation chunks each carry the high bit in their own header; keep
    // reading until a chunk arrives without it (py-iris
    // __read_message_internal high_bit loop).
    let mut more = header.is_continuation();
    while more {
        let mut next_head = [0u8; HEADER_SIZE];
        reader.read_exact(&mut next_head).map_err(FrameError::Io)?;
        let nh = Header::parse(&next_head);
        if body
            .len()
            .saturating_add(usize::try_from(nh.length).unwrap_or(usize::MAX))
            > MAX_TOTAL_BODY
        {
            return Err(FrameError::Oversized);
        }
        let mut chunk = checked_body(nh.length)?;
        reader.read_exact(&mut chunk).map_err(FrameError::Io)?;
        body.extend_from_slice(&chunk);
        more = nh.is_continuation();
    }
    Ok(Reply { header, body })
}

fn read_one<R: Read>(
    reader: &mut R,
    head: &mut [u8; HEADER_SIZE],
    expect_count: Option<u32>,
) -> Result<Header, FrameError> {
    reader.read_exact(head).map_err(FrameError::Io)?;
    let header = Header::parse(head);
    if let Some(want) = expect_count {
        // py-iris _InStream raises "Invalid Message Count" on echo mismatch.
        if header.count != want {
            return Err(FrameError::Desync {
                want,
                got: header.count,
            });
        }
    }
    Ok(header)
}

fn checked_body(length: u32) -> Result<Vec<u8>, FrameError> {
    if length > MAX_FRAME_BODY {
        return Err(FrameError::Oversized);
    }
    Ok(vec![0u8; length as usize])
}

/// Framing failures.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Transport failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A server-supplied length exceeded [`MAX_FRAME_BODY`] /
    /// [`MAX_TOTAL_BODY`].
    #[error("frame length exceeds the safety bound")]
    Oversized,
    /// The server echoed a message count that does not match our sequence.
    #[error("protocol desync: expected count {want}, got {got}")]
    Desync {
        /// Count we expected.
        want: u32,
        /// Count the server echoed.
        got: u32,
    },
}

/// A request-under-construction: message type + statement id + body writer.
#[derive(Debug)]
pub struct MessageBuilder {
    /// Message type for the header.
    pub message_type: MessageType,
    /// Statement id for the header (0 for non-SQL messages).
    pub statement_id: u32,
    /// Body writer accumulating `%List` items.
    pub writer: ListWriter,
}

impl MessageBuilder {
    /// Start a message.
    #[must_use]
    pub fn new(message_type: MessageType) -> Self {
        Self {
            message_type,
            statement_id: 0,
            writer: ListWriter::new(),
        }
    }

    /// Set the header statement id.
    #[must_use]
    pub fn with_statement(mut self, statement_id: u32) -> Self {
        self.statement_id = statement_id;
        self
    }

    /// Append SQL text with the chunk-count framing.
    ///
    /// ref: go-irisnative src/connection/message.go:SetSQLText.
    pub fn set_sql_text(&mut self, sql: &str) {
        write_sql_text(&mut self.writer, sql);
    }

    /// Finish with the sequence `count` → wire bytes.
    #[must_use]
    pub fn dump(self, count: u32) -> Vec<u8> {
        encode_frame(
            self.message_type,
            count,
            self.statement_id,
            &self.writer.buf,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::wire::header::CONTINUATION_BIT;

    #[test]
    fn encode_frame_field_order_matches_go_dump() {
        // ref: go message.go Dump — count and length are stamped last.
        let body = [1u8, 2, 3];
        let frame = encode_frame(MessageType::new(b'D', b'Q'), 7, 2, &body);
        assert_eq!(&frame[0..4], &3u32.to_le_bytes());
        assert_eq!(&frame[4..8], &7u32.to_le_bytes());
        assert_eq!(&frame[8..12], &2u32.to_le_bytes());
        assert_eq!(&frame[12..14], *b"DQ");
        assert_eq!(&frame[14..], &body);
    }

    #[test]
    fn huge_length_header_is_rejected_before_alloc() {
        // hygiene row 34: a hostile 0xFFFFFFF0 length must error, not OOM.
        let mut fake = vec![0u8; HEADER_SIZE];
        fake[0..4].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        fake[4..8].copy_from_slice(&0u32.to_le_bytes());
        let mut cur = std::io::Cursor::new(fake);
        let result = read_reply(&mut cur, Some(0));
        assert!(matches!(result, Err(FrameError::Oversized)));
    }

    #[test]
    fn count_mismatch_is_desync() {
        let frame = encode_frame(MessageType::new(b'F', b'D'), 5, 0, &[]);
        let mut cur = std::io::Cursor::new(frame);
        let result = read_reply(&mut cur, Some(9));
        assert!(matches!(
            result,
            Err(FrameError::Desync { want: 9, got: 5 })
        ));
    }

    #[test]
    fn eof_mid_body_is_io_error() {
        let mut frame = encode_frame(MessageType::new(b'F', b'D'), 0, 0, &[]);
        frame[0..4].copy_from_slice(&10u32.to_le_bytes()); // promise 10 bytes
        let mut cur = std::io::Cursor::new(frame);
        let result = read_reply(&mut cur, None);
        assert!(matches!(result, Err(FrameError::Io(_))));
    }

    #[test]
    fn continuation_chunks_coalesce() {
        // py-iris _InStream.py __read_message_internal high_bit loop: the
        // first chunk carries the continuation bit in the stmt-id word and
        // the body continues in the next frame.
        let mut chunk1 = Header::request(MessageType::new(0, 0), 2, 3, CONTINUATION_BIT)
            .encode()
            .to_vec();
        chunk1.extend_from_slice(b"aa");
        let chunk2 = encode_frame(MessageType::new(0, 0), 3, 0, b"bb");
        let mut stream = chunk1;
        stream.extend_from_slice(&chunk2);
        let mut cur = std::io::Cursor::new(stream);
        let reply = read_reply(&mut cur, Some(3)).expect("coalesced reply");
        assert_eq!(reply.body, b"aabb");
    }
}
