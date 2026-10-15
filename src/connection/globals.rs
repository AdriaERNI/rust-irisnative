//! Global (M) commands (ledger rows 13–17).
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
    build_global_data, build_global_get, build_global_kill, build_global_order, build_global_set,
};
use super::driver::{Transport, check_sysio, first_item};
use super::session::Session;
use crate::wire::list::Item;
use crate::{Error, Result};

/// Direction flags for the order command.
///
/// ref: go globals.go:GlobalNext (`Set(3)`) / `GlobalPrev` (`Set(7)`).
pub const DIR_NEXT: i64 = 3;
/// Previous direction flag.
pub const DIR_PREV: i64 = 7;

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
    if reply.sysio_code() == ERROR_UNDEFINED {
        return Ok(None);
    }
    check_sysio(&reply)?;
    Ok(first_item(&reply.body)?.map(|i| i.as_text()))
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
    check_sysio(&reply)?;
    match first_item(&reply.body)? {
        Some(Item::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(i) if !matches!(i, Item::Null | Item::Undefined) && !i.as_text().is_empty() => {
            Ok(Some(i.as_text()))
        }
        _ => Ok(None),
    }
}

/// Data/definedness result of `GLOBAL_DATA` 0xC249.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Data {
    /// The node itself is defined (`r % 10 == 1`).
    pub value: bool,
    /// The node has a subtree (`r >= 10`).
    pub subtree: bool,
}

/// `GLOBAL_DATA` 0xC249.
///
/// ref: go globals.go:GlobalIsDefined.
pub fn data<T: Transport>(t: &mut T, s: &mut Session, name: &str, subs: &[String]) -> Result<Data> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_global_data(name, subs, c), c)?;
    check_sysio(&reply)?;
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

/// sysio `<UNDEFINED>` code.
/// ref: py-iris iris/_`IRIS.py:ERROR_UNDEFINED` = 1009.
pub const ERROR_UNDEFINED: i32 = 1009;
