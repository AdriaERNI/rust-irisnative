//! Class-method and OREF commands.
//!
//! ref: go-irisnative src/connection/classes.go.

use super::commands::{build_class_method, build_class_method_void, build_method, build_property};
use super::driver::{Transport, check_sysio, first_item};
use super::session::Session;
use crate::Result;
use crate::wire::message::Reply;

/// Decode a value-returning class/OREF method reply (sysio check then first
/// item as text). Pure — shared by the sync and async drivers.
///
/// ref: go classes.go:ClassMethod / Method.
pub fn decode_value_reply(reply: &Reply) -> Result<String> {
    check_sysio(reply)?;
    Ok(first_item(&reply.body)?
        .map(|i| i.as_text())
        .unwrap_or_default())
}

/// Invoke a class method and decode its result.
///
/// ref: go classes.go:ClassMethod.
pub fn class_method<T: Transport>(
    t: &mut T,
    s: &mut Session,
    class: &str,
    method: &str,
    args: &[String],
) -> Result<String> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_class_method(class, method, args, c), c)?;
    decode_value_reply(&reply)
}

/// Invoke a class method ignoring any result.
///
/// ref: go classes.go:ClassMethodVoid.
pub fn class_method_void<T: Transport>(
    t: &mut T,
    s: &mut Session,
    class: &str,
    method: &str,
    args: &[String],
) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_class_method_void(class, method, args, c), c)?;
    check_sysio(&reply)
}

/// Invoke an instance method on an OREF.
///
/// ref: go classes.go:Method / `MethodVoid`.
pub fn method<T: Transport>(
    t: &mut T,
    s: &mut Session,
    oref: &str,
    name: &str,
    args: &[String],
    void: bool,
) -> Result<String> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_method(oref, name, args, void, c), c)?;
    decode_value_reply(&reply)
}

/// Read a property of an OREF.
///
/// ref: go classes.go:PropertyGet.
pub fn property_get<T: Transport>(
    t: &mut T,
    s: &mut Session,
    oref: &str,
    prop: &str,
) -> Result<String> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_property(oref, prop, None, c), c)?;
    decode_value_reply(&reply)
}

/// Write a property of an OREF.
///
/// ref: py-iris `_IRISObject.py` property assignment (the live counterpart
/// of go's absent `PropertySet`; frame shape confirmed by probe).
pub fn property_set<T: Transport>(
    t: &mut T,
    s: &mut Session,
    oref: &str,
    prop: &str,
    value: &str,
) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_property(oref, prop, Some(value), c), c)?;
    check_sysio(&reply)
}

/// `$zversion`-style server banner via `%SYSTEM.Version.GetVersion`.
///
/// ref: go classes.go:ServerVersion.
pub fn server_version<T: Transport>(t: &mut T, s: &mut Session) -> Result<String> {
    class_method(t, s, "%SYSTEM.Version", "GetVersion", &[])
}
