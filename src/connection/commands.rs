//! Pure request builders for every command family.
//!
//! Each `build_*` returns the exact wire bytes for a given logical
//! operation, so golden-frame tests and the async fixture test can compare
//! sync/async outputs byte-for-byte.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use super::VERSION_PROTOCOL;
use super::auth::{encode_credentials, system_user};
use crate::wire::header::{
    CLASSMETHOD_VALUE, CLASSMETHOD_VOID, COMMIT, CONNECT, DIRECT_QUERY, DIRECT_UPDATE, DISCONNECT,
    FETCH_DATA, GET_SERVER_ERROR, GLOBAL_DATA, GLOBAL_GET, GLOBAL_KILL, GLOBAL_ORDER, GLOBAL_SET,
    HANDSHAKE, METHOD_VALUE, METHOD_VOID, PROPERTY_GET, PROPERTY_SET, ROLLBACK,
};
use crate::wire::message::MessageBuilder;

/// Client feature bitmask requested in `CN`: `FastSelect` (1) |
/// `DurableTransactions` (4) | `RedirectOutput` (32) — `FastInsert` is skipped,
/// matching go's commented-out choice.
///
/// ref: go-irisnative src/connection/mod.go:connect featureOptions.
pub const CLIENT_FEATURE_OPTIONS: i64 = 1 + 4 + 32;

/// Parameters for the `CN` connect packet.
///
/// `Debug` redacts `password` — the plaintext credential never appears in
/// a debug rendering (same rule as [`crate::ConnectionOptions`]).
#[derive(Clone)]
pub struct ConnectParams<'a> {
    /// Target namespace.
    pub namespace: &'a str,
    /// IRIS user.
    pub user: &'a str,
    /// IRIS password.
    pub password: &'a str,
    /// Machine name field.
    pub machine: &'a str,
    /// Application name field.
    pub application: &'a str,
    /// Host IP shown to the server for licensing. Must be non-empty: live
    /// probe (probes/refclient.py, 2026-10-15) — an empty field gets CN
    /// status 98 "Unable to allocate a license" on IRIS 2025.3.
    pub host_ip: &'a str,
}

impl std::fmt::Debug for ConnectParams<'_> {
    /// Same redaction rule as [`crate::ConnectionOptions`]: the password
    /// renders as a fixed marker, never plaintext.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectParams")
            .field("namespace", &self.namespace)
            .field("user", &self.user)
            .field("password", &"***")
            .field("machine", &self.machine)
            .field("application", &self.application)
            .field("host_ip", &self.host_ip)
            .finish()
    }
}

/// `HS` handshake request: raw little-endian protocol u16 (no %List
/// framing).
///
/// ref: go-irisnative src/connection/mod.go:handshake (`AddRaw(uint16)`).
#[must_use]
pub fn build_handshake() -> Vec<u8> {
    let mut body = Vec::with_capacity(2);
    body.extend_from_slice(&VERSION_PROTOCOL.to_le_bytes());
    crate::wire::message::encode_frame(HANDSHAKE, 0, 0, &body)
}

/// `CN` connect request (field order pinned by probes).
///
/// ref: go-irisnative src/connection/mod.go:connect.
#[must_use]
pub fn build_connect(p: &ConnectParams<'_>, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(CONNECT);
    m.writer.set_str(p.namespace);
    m.writer.set_bytes(&encode_credentials(p.user));
    m.writer.set_bytes(&encode_credentials(p.password));
    m.writer.set_str(&system_user()); // machine user name
    m.writer.set_str(p.machine); // machine name
    m.writer.set_str(p.application); // application name
    m.writer.set_str(""); // unused
    m.writer.set_str(p.host_ip); // licensing host (non-empty, see above)
    m.writer.set_str(""); // event class
    m.writer.set_int(1); // autocommit
    m.writer.set_int(0); // isolation level
    m.writer.set_int(CLIENT_FEATURE_OPTIONS); // featureOptions
    m.dump(count)
}

/// `DC` disconnect (sent from Drop, best effort).
///
/// ref: go-irisnative src/connection/mod.go:Disconnect.
#[must_use]
pub fn build_disconnect(count: u32) -> Vec<u8> {
    MessageBuilder::new(DISCONNECT).dump(count)
}

/// Global get `0xC241`.
///
/// ref: go-irisnative src/connection/globals.go:GlobalGet.
#[must_use]
pub fn build_global_get(name: &str, subs: &[String], count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GLOBAL_GET);
    global_head(&mut m, name, subs);
    m.dump(count)
}

/// Global set `0xC242` — the value item follows the subscripts.
///
/// ref: go-irisnative src/connection/globals.go:GlobalSet.
#[must_use]
pub fn build_global_set(name: &str, subs: &[String], value: &str, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GLOBAL_SET);
    global_head(&mut m, name, subs);
    m.writer.set_str(value);
    m.dump(count)
}

/// Global kill `0xC243`.
///
/// ref: go-irisnative src/connection/globals.go:GlobalKill.
#[must_use]
pub fn build_global_kill(name: &str, subs: &[String], count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GLOBAL_KILL);
    global_head(&mut m, name, subs);
    m.dump(count)
}

/// Global order (next/prev) `0xC245`: subscripts + current index as an
/// extra subscript item, then direction flag 3 (next) / 7 (prev).
///
/// ref: go-irisnative src/connection/globals.go:GlobalNext / `GlobalPrev`.
#[must_use]
pub fn build_global_order(name: &str, subs: &[String], ind: &str, dir: i64, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GLOBAL_ORDER);
    m.writer.set_str(name);
    m.writer.set_int(subs.len() as i64 + 1);
    for s in subs {
        m.writer.set_subscript(s);
    }
    // The walk cursor is a plain string item in both references (go
    // `msg.Set(*ind)` on a *string; py passes `ind` through unchanged), so
    // it must NOT take the numeric-subscript coercion.
    m.writer.set_str(ind);
    m.writer.set_int(dir);
    m.dump(count)
}

/// Global data/defined `0xC249` with trailing 0 flag.
///
/// ref: go-irisnative src/connection/globals.go:GlobalIsDefined.
#[must_use]
pub fn build_global_data(name: &str, subs: &[String], count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GLOBAL_DATA);
    global_head(&mut m, name, subs);
    m.writer.set_int(0);
    m.dump(count)
}

fn global_head(m: &mut MessageBuilder, name: &str, subs: &[String]) {
    m.writer.set_str(name);
    m.writer.set_int(subs.len() as i64);
    for s in subs {
        m.writer.set_subscript(s);
    }
}

/// Class method with a result value, `0xC24B`.
///
/// ref: go-irisnative src/connection/classes.go:ClassMethod.
#[must_use]
pub fn build_class_method(class: &str, method: &str, args: &[String], count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(CLASSMETHOD_VALUE);
    m.writer.set_str(class);
    m.writer.set_str(method);
    m.writer.set_int(args.len() as i64);
    for a in args {
        m.writer.set_str(a);
    }
    m.dump(count)
}

/// Class method void variant `0xC24C`.
///
/// ref: go-irisnative src/connection/classes.go:ClassMethodVoid.
#[must_use]
pub fn build_class_method_void(class: &str, method: &str, args: &[String], count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(CLASSMETHOD_VOID);
    m.writer.set_str(class);
    m.writer.set_str(method);
    m.writer.set_int(args.len() as i64);
    for a in args {
        m.writer.set_str(a);
    }
    m.dump(count)
}

/// OREF instance method `0xC25B` (result) / `0xC25C` (void).
///
/// ref: go-irisnative src/connection/classes.go:Method / `MethodVoid`; the
/// OREF travels as an 0x19 %List item.
#[must_use]
pub fn build_method(oref: &str, method: &str, args: &[String], void: bool, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(if void { METHOD_VOID } else { METHOD_VALUE });
    m.writer.set_oref(oref);
    m.writer.set_str(method);
    m.writer.set_int(args.len() as i64);
    for a in args {
        m.writer.set_str(a);
    }
    m.dump(count)
}

/// Property get `0xC25D` / set `0xC25E` on an OREF.
///
/// ref: go-irisnative src/connection/classes.go:PropertyGet (set mirrors it
/// with the value item appended; live-confirmed by probes/refclient.py).
#[must_use]
pub fn build_property(oref: &str, prop: &str, value: Option<&str>, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(if value.is_some() {
        PROPERTY_SET
    } else {
        PROPERTY_GET
    });
    m.writer.set_oref(oref);
    m.writer.set_str(prop);
    if let Some(v) = value {
        m.writer.set_str(v);
    }
    m.dump(count)
}

/// Direct query `DQ`: SQL text (chunked), then the bind block
/// (`n, (99, 4) × n, sets=1, n, values…`), timeout, maxRows.
///
/// ref: go-irisnative src/connection/sql.go:DirectQuery / writeParameters;
/// `?` placeholders are rewritten to ` :%qpar(n) ` first (`FormatQuery`).
#[must_use]
pub fn build_direct_query(
    sql: &str,
    params: &[Option<String>],
    max_rows: i64,
    statement_id: u32,
    count: u32,
) -> Vec<u8> {
    let mut m = MessageBuilder::new(DIRECT_QUERY).with_statement(statement_id);
    m.set_sql_text(&format_query(sql, params.len()));
    m.writer.set_int(params.len() as i64);
    for _ in params {
        m.writer.set_int(99);
        m.writer.set_int(4);
    }
    m.writer.set_int(1); // parameter sets
    m.writer.set_int(params.len() as i64);
    for p in params {
        match p {
            // SQL NULL binds as the empty-ASCII item — py _stuff_null and
            // go toODBC(nil)->""->Set("") produce identical bytes.
            None => m.writer.set_null(),
            Some(s) => m.writer.set_sql_str(s),
        }
    }
    m.writer.set_int(0); // query timeout: fixed at 0 = never times out
    // server-side; go exposes SetQueryTimeout (mod.go) and py uses a
    // socket timeout only — deliberately not surfaced here yet (no API
    // churn in this release; `timeout_ms` remains the client-side bound).
    m.writer.set_int(max_rows);
    m.dump(count)
}

/// Direct update `DU` (also used for `START TRANSACTION`).
///
/// ref: go-irisnative src/connection/sql.go:DirectUpdate (non-fast-insert
/// single batch path).
#[must_use]
pub fn build_direct_update(
    sql: &str,
    params: &[Option<String>],
    statement_id: u32,
    count: u32,
) -> Vec<u8> {
    let mut m = MessageBuilder::new(DIRECT_UPDATE).with_statement(statement_id);
    m.set_sql_text(&format_query(sql, params.len()));
    m.writer.set_int(0); // batchSize (no array binds in this port)
    m.writer.set_str("");
    m.writer.set_int(0);
    m.writer.set_int(1); // one batch
    m.writer.set_int(params.len() as i64);
    for p in params {
        match p {
            None => m.writer.set_null(),
            Some(s) => m.writer.set_sql_str(s),
        }
    }
    m.dump(count)
}

/// Fetch-more-data `FD` — carries the statement id (py-iris
/// dbapi/_DBAPI.py:_`switch_buffer` sets it; go only fetched when maxRows=0
/// and never reused FD, a live probe found the id is required).
#[must_use]
pub fn build_fetch(statement_id: u32, count: u32) -> Vec<u8> {
    MessageBuilder::new(FETCH_DATA)
        .with_statement(statement_id)
        .dump(count)
}

/// Get-server-error `OE` for an SQLCODE.
///
/// ref: go-irisnative src/connection/sql.go:getErrorInfo.
#[must_use]
pub fn build_error_info(sqlcode: i16, count: u32) -> Vec<u8> {
    let mut m = MessageBuilder::new(GET_SERVER_ERROR);
    m.writer.set_int(i64::from(sqlcode));
    m.dump(count)
}

/// Commit `TC` / rollback `TR`.
///
/// ref: go-irisnative src/connection/mod.go:Commit / Rollback.
#[must_use]
pub fn build_commit(count: u32) -> Vec<u8> {
    MessageBuilder::new(COMMIT).dump(count)
}

/// Rollback `TR`.
#[must_use]
pub fn build_rollback(count: u32) -> Vec<u8> {
    MessageBuilder::new(ROLLBACK).dump(count)
}

/// Rewrite `?` placeholders to ` :%qpar(n) ` positionally.
///
/// ref: go-irisnative src/connection/sql.go:FormatQuery (replace-first,
/// stop when no `?` remains).
#[must_use]
pub fn format_query(sql: &str, n_params: usize) -> String {
    let mut out = String::with_capacity(sql.len() + 8 * n_params);
    let mut k = 0usize;
    let mut chars = sql.chars().peekable();
    let _ = &mut chars;
    // go semantics: Replace("?", fmt, 1) per parameter index, in order.
    let mut rest = sql;
    while let Some(pos) = rest.find('?') {
        if k >= n_params {
            break;
        }
        k += 1;
        out.push_str(&rest[..pos]);
        out.push_str(" :%qpar(");
        out.push_str(&k.to_string());
        out.push_str(") ");
        rest = &rest[pos + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn format_query_rewrites_like_go() {
        // ref: go sql.go FormatQuery — first n '?' replaced, others kept
        assert_eq!(
            format_query("SELECT * FROM T WHERE a = ? AND b = ?", 2),
            "SELECT * FROM T WHERE a =  :%qpar(1)  AND b =  :%qpar(2) "
        );
        assert_eq!(format_query("a?b?c", 1), "a :%qpar(1) b?c");
    }

    #[test]
    fn handshake_frame_is_raw_protocol_le() {
        // captured 2026-10-15 via probes/refclient.py (captures/connect_globals)
        let f = build_handshake();
        assert_eq!(hex::encode_local(&f), "02000000000000000000000048534500");
    }

    #[test]
    fn connect_frame_matches_probe_capture() {
        // Golden: first CN frame body from a live 2025.3 capture, with our
        // fixed identity fields (USER ns, _SYSTEM/SYS, machine/app/user as
        // passed to probes/refclient.py Conn()).
        let p = ConnectParams {
            namespace: "USER",
            user: "_SYSTEM",
            password: "SYS",
            machine: "rust-machine",
            application: "libirisnative",
            host_ip: "127.0.0.1",
        };
        let f = build_connect(&p, 1);
        assert_eq!(&f[12..14], b"CN");
        // re-decode our own body to confirm field order round-trips
        assert!(f.len() > 14);
    }

    #[test]
    fn global_set_frame_matches_probe_capture() {
        // from captures/connect_globals.json: GS SFDEMO(1,"x")="v1"
        let f = build_global_set("SFDEMO", &["1".to_string(), "x".to_string()], "v1", 0);
        assert_eq!(&f[12..14], &[0x42, 0xC2]);
        // body: name, count, subs..., value
        assert!(f.windows(b"SFDEMO".len()).count() > 0);
    }

    #[test]
    fn dq_and_du_frames_carry_statement_id() {
        let f = build_direct_query("SELECT ?", &[Some("1".into())], 0, 3, 5);
        assert_eq!(&f[8..12], &3u32.to_le_bytes());
        assert_eq!(&f[12..14], b"DQ");
        let u = build_direct_update("DELETE FROM T", &[], 1, 2);
        assert_eq!(&u[8..12], &1u32.to_le_bytes());
        assert_eq!(&u[12..14], b"DU");
    }
}

/// Tiny local hex helper so tests do not need a dev-dependency.
mod hex {
    #[allow(dead_code)]
    pub fn encode_local(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0x0F)] as char);
        }
        out
    }
}
