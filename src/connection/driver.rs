//! Shared IO driver logic: given a socket-like transport, run commands.
//!
//! Kept transport-generic so the `async` feature can drive the *same* code
//! with a tokio socket (acceptance A-6.2 — async wraps IO, never forks the
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
    // HS (ledger rows 6-7)
    let c0 = session.next_count();
    let reply = t.roundtrip(&build_handshake(), c0)?;
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

    // CN (ledger rows 8-9)
    let c1 = session.next_count();
    let reply = t.roundtrip(&build_connect(p, c1), c1)?;
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

fn map_frame_err(e: crate::wire::message::FrameError) -> Error {
    use crate::wire::message::FrameError;
    match e {
        FrameError::Io(e) => Error::Io(e),
        FrameError::Oversized => Error::Protocol("frame exceeds safety bound".into()),
        FrameError::Desync { want, got } => {
            Error::Protocol(format!("message count desync: expected {want}, got {got}"))
        }
    }
}

/// sysio replies: 0 is success; any other code maps to `Error::Server`
/// with the trailing message when present (`<UNDEFINED>` is the notable
/// one — surfaced as `Error::Server` per A-2.1).
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
