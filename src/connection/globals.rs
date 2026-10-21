//! Global (M) commands.
//!
//! ref: go-irisnative src/connection/globals.go.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use super::commands::{
    build_global_data, build_global_get, build_global_kill, build_global_lock, build_global_order,
    build_global_set, build_global_unlock, build_increment, build_unlock_all,
};
use super::driver::{Transport, check_sysio, first_item};
use super::session::Session;
use crate::wire::list::Item;
use crate::wire::message::Reply;
use crate::{Error, Result};

/// Direction flags for the order command.
///
/// ref: go globals.go:GlobalNext (`Set(3)`) / `GlobalPrev` (`Set(7)`).
pub const DIR_NEXT: i64 = 3;
/// Previous direction flag.
pub const DIR_PREV: i64 = 7;

/// Decode a `GLOBAL_GET` reply: `None` for an undefined node (sysio code
/// 1009 = `<UNDEFINED>`, py-iris `_IRIS.py:ERROR_UNDEFINED`), the node text
/// otherwise. Pure — shared by the sync and async drivers.
///
/// ref: go globals.go:GlobalGet; py _IRIS.py:GlobalGet.
pub fn decode_get_reply(reply: &Reply) -> Result<Option<String>> {
    if reply.sysio_code() == ERROR_UNDEFINED {
        return Ok(None);
    }
    check_sysio(reply)?;
    Ok(first_item(&reply.body)?.map(|i| i.as_text()))
}

/// `GLOBAL_GET` 0xC241: value text, or `None` for an undefined node
/// (sysio code 1009 = `<UNDEFINED>`, py-iris `_IRIS.py:ERROR_UNDEFINED`).
pub fn get<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
) -> Result<Option<String>> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_get(name, subs, c), c)?;
    decode_get_reply(&reply)
}

/// Decode a `GLOBAL_ORDER` reply: the next subscript, `None` at the end of
/// the walk (empty result — go checks `result != ""`). Pure — shared by the
/// sync and async drivers.
///
/// ref: go globals.go:GlobalNext / `GlobalPrev`.
pub fn decode_order_reply(reply: &Reply) -> Result<Option<String>> {
    check_sysio(reply)?;
    match first_item(&reply.body)? {
        Some(Item::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(i) if !matches!(i, Item::Null | Item::Undefined) && !i.as_text().is_empty() => {
            Ok(Some(i.as_text()))
        }
        _ => Ok(None),
    }
}

/// `GLOBAL_SET` 0xC242.
pub fn set<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
    value: &str,
) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_set(name, subs, value, c), c)?;
    check_sysio(&reply)
}

/// `GLOBAL_KILL` 0xC243.
pub fn kill<T: Transport>(t: &mut T, s: &mut Session, name: &str, subs: &[String]) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_kill(name, subs, c), c)?;
    check_sysio(&reply)
}

/// `GLOBAL_ORDER` 0xC245 result: the next subscript, `None` at the end of
/// the walk (empty result — go checks `result != ""`).
pub fn order<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
    ind: &str,
    dir: i64,
) -> Result<Option<String>> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_order(name, subs, ind, dir, c), c)?;
    decode_order_reply(&reply)
}

/// Data/definedness result of `GLOBAL_DATA` 0xC249.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Data {
    /// The node itself is defined (`r % 10 == 1`).
    pub value: bool,
    /// The node has a subtree (`r >= 10`).
    pub subtree: bool,
}

/// Decode a `GLOBAL_DATA` reply into definedness. Pure — shared by the
/// sync and async drivers.
///
/// ref: go globals.go:GlobalIsDefined.
pub fn decode_data_reply(reply: &Reply) -> Result<Data> {
    check_sysio(reply)?;
    let r = match first_item(&reply.body)? {
        Some(Item::Int(v)) => v,
        Some(other) => other
            .as_text()
            .parse::<i64>()
            .map_err(|e| Error::Protocol(e.to_string()))?,
        None => 0,
    };
    Ok(Data {
        value: r % 10 == 1,
        subtree: r >= 10,
    })
}

/// `GLOBAL_DATA` 0xC249.
///
/// ref: go globals.go:GlobalIsDefined.
pub fn data<T: Transport>(t: &mut T, s: &mut Session, name: &str, subs: &[String]) -> Result<Data> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_data(name, subs, c), c)?;
    decode_data_reply(&reply)
}

/// sysio `<UNDEFINED>` code.
/// ref: py-iris iris/_`IRIS.py:ERROR_UNDEFINED` = 1009.
pub const ERROR_UNDEFINED: i32 = 1009;

/// sysio `<TIMEOUT>` code returned by a LOCK whose wait window elapsed
/// (live capture 2026-10-21: contention reply = 1240 with `<TIMEOUT>`
/// text; both reference drivers raise the same condition).
/// ref: py-iris-legacy iris/_IRIS.py:lock (`RuntimeError` `<TIMEOUT>`).
pub const ERROR_LOCK_TIMEOUT: i32 = 1240;

/// Decode an `INCREMENT` reply: the node's new value text. Pure — shared by
/// both drivers.
///
/// ref: py-iris-legacy iris/_IRIS.py:increment (returns the new value).
pub fn decode_increment_reply(reply: &Reply) -> Result<String> {
    check_sysio(reply)?;
    Ok(first_item(&reply.body)?
        .map(|i| i.as_text())
        .unwrap_or_default())
}

/// `INCREMENT` 0xC246: add `amount` to the node server-side (atomic — the
/// get+set composition it replaces would race) and return the new value.
pub fn increment<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
    amount: &str,
) -> Result<String> {
    let c = s.next_count();
    let compact = s.info.compact_double;
    let reply = t.roundtrip(&build_increment(name, subs, amount, compact, c), c)?;
    decode_increment_reply(&reply)
}

/// Decode a `LOCK` reply; success is sysio 0, a timed-out wait surfaces
/// `ERROR_LOCK_TIMEOUT` with the server's `<TIMEOUT>` text. Pure.
pub fn decode_lock_reply(reply: &Reply) -> Result<()> {
    check_sysio(reply)
}

/// `LOCK` 0xC24E: lock a global node (`mode`: "" exclusive, "S" shared,
/// "E" escalate; `timeout_secs`: negative waits forever, 0 tries once).
/// The server blocks the *request* while it waits.
pub fn lock<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
    mode: &str,
    timeout_secs: i64,
) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_lock(name, subs, mode, timeout_secs, c), c)?;
    decode_lock_reply(&reply)
}

/// `UNLOCK` 0xC24F: release one lock count (`mode` may add `I`/`D`).
pub fn unlock<T: Transport>(
    t: &mut T,
    s: &mut Session,
    name: &str,
    subs: &[String],
    mode: &str,
) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_unlock(name, subs, mode, c), c)?;
    decode_lock_reply(&reply)
}

/// `UNLOCK_ALL` 0xC250: release every lock this connection holds
/// (body-less frame, live-confirmed 2026-10-21).
pub fn release_all_locks<T: Transport>(t: &mut T, s: &mut Session) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_unlock_all(c), c)?;
    decode_lock_reply(&reply)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod lock_increment_tests {
    use super::*;
    use crate::wire::header::Header;
    use crate::wire::message::Reply;

    fn reply_with(status: u16, body: Vec<u8>) -> Reply {
        Reply {
            header: Header {
                length: body.len() as u32,
                count: 0,
                statement_id: 0,
                type_word: status.to_le_bytes(),
            },
            body,
        }
    }

    #[test]
    fn lock_timeout_surfaces_as_a_server_error() {
        // captured contention reply: sysio 1240 + "<TIMEOUT>" text
        let mut w = crate::wire::list::ListWriter::new();
        w.set_str("<TIMEOUT>");
        let r = reply_with(0xCA00 + 1240, w.buf);
        let e = decode_lock_reply(&r).expect_err("1240 fails");
        assert!(
            matches!(&e, crate::Error::Server { code, message }
                if *code == 1240 && message == "<TIMEOUT>"),
            "{e:?}"
        );
    }

    #[test]
    fn increment_value_decodes_through_the_item_text() {
        let mut w = crate::wire::list::ListWriter::new();
        w.set_int(5);
        assert_eq!(
            decode_increment_reply(&reply_with(0xCA00, w.buf)).unwrap(),
            "5"
        );
        // 8.5 came back as a short DOUBLE item in the live capture
        let r = reply_with(0xCA00, vec![0x04, 0x08, 0x08, 0x41]);
        assert_eq!(decode_increment_reply(&r).unwrap(), "8.5");
    }
}
