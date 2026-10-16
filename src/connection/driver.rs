//! Shared IO driver logic: given a socket-like transport, run commands.
//!
//! Kept transport-generic so the `async` feature can drive the *same* code
//! with a tokio socket (async wraps IO, never forks the
//! codec or the command sequence).
//!
//! ref: go-irisnative src/connection/mod.go (message send/read loop).

use std::io::Write;

use super::commands::{ConnectParams, build_connect, build_handshake};
use super::session::{Session, first_text};
use crate::connection::MIN_PROTOCOL;
use crate::wire::list::{Item, ListReader};
use crate::wire::message::{Reply, read_reply};
use crate::{Error, Result};

/// Anything that can push and pull whole frames.
pub trait Transport {
    /// Push a request frame and await its coalesced reply, expecting the
    /// server to echo `count`.
    fn roundtrip(&mut self, frame: &[u8], count: u32) -> Result<Reply>;

    /// Await a server-push frame the previous command's reply announced
    /// (DQ sends metadata, then *pushes* the first data message without a
    /// request — go sql.go:DirectQuery tail `ReadMessage`).
    fn read_push(&mut self, count: u32) -> Result<Reply>;
}

impl Transport for std::net::TcpStream {
    fn roundtrip(&mut self, frame: &[u8], count: u32) -> Result<Reply> {
        self.write_all(frame).map_err(Error::Io)?;
        // `&mut TcpStream` implements Read; no clone, no shared-fd subtlety.
        read_reply(self, Some(count)).map_err(map_frame_err)
    }

    fn read_push(&mut self, count: u32) -> Result<Reply> {
        read_reply(self, Some(count)).map_err(map_frame_err)
    }
}

/// Handshake + connect over a transport, filling `session`.
///
/// ref: go-irisnative src/connection/mod.go:handshake / connect.
pub fn establish<T: Transport>(
    t: &mut T,
    session: &mut Session,
    p: &ConnectParams<'_>,
) -> Result<()> {
    // HS
    let c0 = session.next_count();
    let reply = t.roundtrip(&build_handshake(), c0)?;
    apply_handshake(&reply, session)?;
    // CN
    let c1 = session.next_count();
    let reply = t.roundtrip(&build_connect(p, c1), c1)?;
    apply_connect(&reply, session)
}

/// Decode an `HS` reply into `session` — pure, shared by the sync and async
/// drivers so the negotiation rules live in exactly one place.
///
/// ref: go-irisnative src/connection/mod.go:handshake.
pub fn apply_handshake(reply: &Reply, session: &mut Session) -> Result<()> {
    if reply.status() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: first_text(&reply.body).unwrap_or_default(),
        });
    }
    let raw_u16 = |b: &[u8], off: usize| -> Result<u16> {
        let pair = b
            .get(off..off + 2)
            .ok_or_else(|| Error::Protocol("HS reply truncated".into()))?;
        Ok(u16::from_le_bytes([pair[0], pair[1]]))
    };
    let proto = raw_u16(&reply.body, 0)?;
    if proto < MIN_PROTOCOL {
        return Err(Error::Protocol(format!(
            "server protocol {proto} is below the minimum supported {MIN_PROTOCOL}"
        )));
    }
    let feature_bits = raw_u16(&reply.body, 2)?;
    // The two raw u16s sit before the %List stream (go reads them with
    // GetRaw, then reads the locale item). ref: go mod.go:handshake.
    let locale_item = ListReader::new(reply.body.get(4..).unwrap_or(&[]))
        .next()
        .and_then(std::result::Result::ok)
        .ok_or_else(|| Error::Protocol("HS reply missing locale".into()))?;
    session.info.protocol = proto;
    session.info.unicode = feature_bits & 1 > 0;
    // compact double honored when protocol >= 65 and feature bit 1
    // ref: py-iris iris/_IRISConnection.py:_connect
    session.info.compact_double = proto >= 65 && feature_bits & 2 > 0;
    session.info.locale = super::session::item_text(&locale_item);
    Ok(())
}

/// Decode a `CN` reply into `session` — pure, shared by the sync and async
/// drivers. Status 417 is Access Denied → [`Error::Auth`].
///
/// ref: go-irisnative src/connection/mod.go:connect.
pub fn apply_connect(reply: &Reply, session: &mut Session) -> Result<()> {
    match reply.status() {
        0 => {}
        417 => return Err(Error::Auth), // Access Denied
        s => {
            return Err(Error::Server {
                code: i64::from(s),
                message: first_text(&reply.body).unwrap_or_default(),
            });
        }
    }
    let mut r = ListReader::new(&reply.body);
    let take_text = |r: &mut ListReader| -> String {
        match r.next() {
            Some(Ok(item)) => super::session::item_text(&item),
            _ => String::new(),
        }
    };
    session.info.info = take_text(&mut r);
    let _delimited_ids = r.next().and_then(std::result::Result::ok);
    let _ignored = r.next().and_then(std::result::Result::ok);
    let _isolation = r.next().and_then(std::result::Result::ok);
    session.info.server_job = take_text(&mut r);
    let _sql_empty = r.next().and_then(std::result::Result::ok);
    session.info.feature_options = match r.next().and_then(std::result::Result::ok) {
        Some(Item::Int(v)) => u32::try_from(v).unwrap_or(0),
        _ => 0,
    };
    Ok(())
}

/// Maps a framing failure to the crate error type — one place, both
/// drivers (the async reader reuses the `wire` framing policy). An expired
/// socket read arrives here as `WouldBlock`/`TimedOut` from std or tokio;
/// it becomes [`Error::Timeout`] so callers can branch without inspecting
/// `io::Error::kind()` (py raises `socket.timeout`, go
/// `os.ErrDeadlineExceeded` — same distinct signal).
#[must_use]
pub fn map_frame_err(e: crate::wire::message::FrameError) -> Error {
    use crate::wire::message::FrameError;
    match e {
        FrameError::Io(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Error::Timeout
        }
        FrameError::Io(e) => Error::Io(e),
        FrameError::Oversized => Error::Protocol("frame exceeds safety bound".into()),
        FrameError::Desync { want, got } => {
            Error::Protocol(format!("message count desync: expected {want}, got {got}"))
        }
    }
}

/// sysio replies: 0 is success; any other code maps to `Error::Server`
/// with the trailing message when present (`<UNDEFINED>` is the notable
/// one — surfaced as `Error::Server`.
pub fn check_sysio(reply: &Reply) -> Result<()> {
    let code = reply.sysio_code();
    if code == 0 {
        return Ok(());
    }
    Err(Error::Server {
        code: i64::from(code),
        message: first_text(&reply.body).unwrap_or_default(),
    })
}

/// Decode the first body item for global/class commands.
pub fn first_item(body: &[u8]) -> Result<Option<Item>> {
    match ListReader::new(body).next() {
        Some(Ok(item)) => Ok(Some(item)),
        Some(Err(e)) => Err(super::session::list_err(&e)),
        None => Ok(None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::wire::message::FrameError;

    #[test]
    fn timeout_error_kinds_map_to_the_distinct_variant() {
        // Both shapes an expired socket read takes (std yields
        // WouldBlock on a blocking socket, tokio TimedOut) must land on
        // Error::Timeout, while every other IO error stays Error::Io.
        for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            let e = map_frame_err(FrameError::Io(std::io::Error::new(kind, "stale")));
            assert!(matches!(e, Error::Timeout), "{kind:?} -> {e:?}");
        }
        let e = map_frame_err(FrameError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "closed",
        )));
        assert!(matches!(e, Error::Io(_)), "got {e:?}");
    }

    #[test]
    fn auth_status_stays_auth_not_timeout() {
        // Negative: the distinct Error::Timeout must not swallow the
        // login-rejection mapping (status 417 -> Error::Auth).
        let mut session = Session::default();
        let reply = Reply {
            header: crate::wire::header::Header {
                length: 0,
                count: 0,
                statement_id: 0,
                type_word: 417u16.to_le_bytes(),
            },
            body: Vec::new(),
        };
        let e = apply_connect(&reply, &mut session).expect_err("417 rejects");
        assert!(matches!(e, Error::Auth), "got {e:?}");
    }
}
