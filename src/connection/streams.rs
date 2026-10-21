//! Stream commands: the `JS` read plus transparent SQL stream columns.
//!
//! Two user stories, both proven against the live drivers:
//!
//! 1. *Transparent stream columns.* A `CLOB`/`BLOB` column comes back in a
//!    result row as a `%List` item whose payload is a nested descriptor
//!    list `[id, class-name, location]`. The official Python driver fetches
//!    the content transparently (live: a 40 KB CLOB and a 40 KB BLOB both
//!    round-trip value-for-value); the Go driver fires one `READ_STREAM`
//!    (`JS`) per stream column and does the same for character columns
//!    (its binary branch is commented out — sql.go:328). Here BOTH kinds
//!    decode transparently: for every stream-typed column, the descriptor
//!    is handed to `JS` with size `-1` and the raw reply replaces the
//!    value. Live capture 2026-10-21: one `JS` frame served the whole
//!    40 000-byte value; the reply body is the content itself, not a
//!    `%List` stream.
//! 2. *Stream objects.* Creation and every operation except the content
//!    read ride the already-porting class-method/OREF commands
//!    (`%New`, `Write`, `Read`, `Rewind`, `%Close`, the `Size` property);
//!    `read` uses `JS(handle, size)`. `Truncate`/`Erase` are NOT
//!    implemented by the server's concrete stream classes on IRIS 2025.3
//!    (live probe: sysio 1148 `<UNDEFINED>` — they exist only on the
//!    `%Stream.Interface` contract): `truncate` is surfaced and answers
//!    that 1148 where the class lacks it, `Erase` and the `Position`
//!    property getter are not surfaced over the wire (sysio 1196).
//!
//! ref: go-irisnative src/connection/sql.go:fetchStream (JS handle + (-1))
//! ref: py-iris-legacy iris/_InStream.py/_OutStream.py (stream semantics)

use super::commands::build_read_stream;
use super::driver::Transport;
use super::session::Session;
use super::sql::Column;
use crate::wire::list::{Item, ListReader};
use crate::wire::message::Reply;
use crate::{Error, Result};

/// SQL type codes of stream columns: `LONGVARCHAR` (`CLOB`) and
/// `LONGVARBINARY` (`BLOB`).
///
/// ref: go-irisnative src/connection/sql.go SQLTYPE (the `fetchStream`
/// switch); a live DQ metadata reply reports exactly these codes for a
/// `CLOB`/`BLOB` pair (probe 2026-10-21).
#[must_use]
pub const fn is_stream_column(type_code: i32) -> bool {
    type_code == -1 || type_code == -4
}

/// Extract the `JS` handle from a row item of a stream column: the item's
/// raw payload bytes, provided the payload parses as a `%List` whose second
/// element names a `%Stream.*` class. `None` keeps the value verbatim
/// (SQL NULL / empty stream — a live probe shows NULL stream columns arrive
/// as an empty payload, which the references hand back as the value).
#[must_use]
pub fn stream_handle(item: &Item) -> Option<Vec<u8>> {
    match item {
        Item::String(_) | Item::Oref(_) => {}
        _ => return None,
    }
    let raw = item.as_bytes();
    if raw.is_empty() {
        return None;
    }
    let mut r = ListReader::new(&raw);
    let _id = r.next()?.ok()?;
    let class = r.next()?.ok()?;
    if class.as_text().starts_with("%Stream.") {
        Some(raw)
    } else {
        None
    }
}

/// Send `JS(handle, size)` and return the raw content bytes. The reply
/// carries a plain status word (0 on success); failures such as a corrupt
/// handle surface the server code verbatim (probe: status 412).
pub fn read_stream<T: Transport>(
    t: &mut T,
    s: &mut Session,
    handle: &[u8],
    size: i64,
) -> Result<Vec<u8>> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_read_stream(handle, size, c), c)?;
    check_stream_reply(&reply)
}

/// Pure `JS` reply check shared by both drivers: raw status, not the
/// sysio-biased word (documented divergence from the globals decoders —
/// stream replies use the SQL-style status word).
pub fn check_stream_reply(reply: &Reply) -> Result<Vec<u8>> {
    if reply.status() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: String::from("stream read failed"),
        });
    }
    Ok(reply.body.clone())
}

/// Replace every stream-column descriptor in a decoded row with its
/// content (sync driver; the async driver calls [`stream_handle`] +
/// [`check_stream_reply`] around its own IO so no logic duplicates).
pub fn resolve_row<T: Transport>(
    t: &mut T,
    s: &mut Session,
    columns: &[Column],
    row: &mut [Item],
) -> Result<()> {
    for (i, item) in row.iter_mut().enumerate() {
        let stream = columns
            .get(i)
            .is_some_and(|c| is_stream_column(c.type_code));
        if !stream {
            continue;
        }
        if let Some(handle) = stream_handle(item) {
            let data = read_stream(t, s, &handle, -1)?;
            *item = Item::String(data.iter().map(|b| char::from(*b)).collect());
        }
    }
    Ok(())
}

/// Decode a stream `Size` property reply (text → integer).
pub fn decode_size(text: &str) -> Result<i64> {
    text.trim()
        .parse::<i64>()
        .map_err(|e| Error::Protocol(e.to_string()))
}

/// Interpret a `%Status` integer returned by stream methods: `1` is OK,
/// anything else is the server's status code.
///
/// ref: py-iris-legacy iris/_IRISObject.py status handling (`$$OK`).
#[must_use]
pub fn status_is_ok(text: &str) -> bool {
    matches!(text.trim(), "1" | "OK")
}

/// Map a failing `%Status` reply to a crate error.
#[must_use]
pub fn status_error(text: &str) -> Error {
    Error::Server {
        code: text.trim().parse::<i64>().unwrap_or(i64::MIN),
        message: "stream operation failed".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::wire::list::ListWriter;

    fn descriptor(class: &str) -> Item {
        // the live row-item shape for a stream column: a %List wrapped in
        // one ASCII item, [id, class-name, location]
        let mut inner = ListWriter::new();
        inner.set_int(1);
        inner.set_str(class);
        inner.set_str("^poCN.Be3M.S");
        let mut w = ListWriter::new();
        w.set_bytes(&inner.buf);
        ListReader::new(&w.buf).next().unwrap().unwrap()
    }

    #[test]
    fn stream_column_codes_are_the_odbc_long_types() {
        assert!(is_stream_column(-1));
        assert!(is_stream_column(-4));
        assert!(!is_stream_column(12)); // VARCHAR
        assert!(!is_stream_column(4)); // INTEGER
    }

    #[test]
    fn descriptors_round_trip_the_handle_bytes() {
        let item = descriptor("%Stream.GlobalCharacter");
        let handle = stream_handle(&item).expect("handle extracted");
        // the handle is the descriptor payload verbatim (what go sends)
        assert_eq!(handle, item.as_bytes());
        let mut r = ListReader::new(&handle);
        assert_eq!(r.next().unwrap().unwrap().as_text(), "1");
        assert!(r.next().unwrap().unwrap().as_text().starts_with("%Stream."));
    }

    #[test]
    fn non_descriptors_are_left_alone() {
        assert!(stream_handle(&Item::Null).is_none());
        assert!(stream_handle(&Item::String("plain text".into())).is_none());
        assert!(stream_handle(&Item::Int(7)).is_none());
        // a two-item list naming a non-stream class is not a handle
        let mut inner = ListWriter::new();
        inner.set_int(3);
        inner.set_str("%Persistent");
        let mut w = ListWriter::new();
        w.set_bytes(&inner.buf);
        let item = ListReader::new(&w.buf).next().unwrap().unwrap();
        assert!(stream_handle(&item).is_none());
    }

    #[test]
    fn status_decodes_like_the_server_shows_it() {
        assert!(status_is_ok("1"));
        assert!(status_is_ok("OK"));
        assert!(!status_is_ok("0"));
        assert!(matches!(
            status_error("8128"),
            Error::Server { code: 8128, .. }
        ));
    }

    #[test]
    fn size_decoding_rejects_non_numeric_text() {
        assert_eq!(decode_size("40000").unwrap(), 40000);
        assert!(decode_size("<UNDEFINED>").is_err());
    }
}
