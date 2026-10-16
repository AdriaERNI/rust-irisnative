//! SQL commands: DQ/DU/FD/OE.
//!
//! Parsing lives in the pure `decode_*` helpers below so the sync and async
//! drivers share one source of truth.
//!
//! ref: go-irisnative src/connection/sql.go (`DirectQuery`, `DirectUpdate`,
//! getColumns, fetchMoreData) and rows.go (Next).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use super::commands::{build_direct_query, build_direct_update, build_error_info, build_fetch};
use super::driver::{Transport, first_item};
use super::session::Session;
use crate::wire::list::{Item, ListReader};
use crate::wire::message::Reply;
use crate::{Error, Result};

/// A result column as reported in DQ metadata.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// ODBC type code (see go sql.go SQLTYPE).
    pub type_code: i32,
    /// Index of this column's item inside a `FastSelect` packed row
    /// (`slot_position - 1`; equals the column index when unset — go
    /// sql.go:getColumns).
    pub slot: usize,
}

/// DQ metadata reply decoded to its essentials.
#[derive(Debug)]
pub struct Meta {
    /// `FastSelect` featureOption from the statement feature block.
    pub feature_option: i64,
    /// Packed row item count (`FastSelect`).
    pub max_row_items: usize,
    /// Result columns.
    pub columns: Vec<Column>,
}

/// State for one executed query.
#[derive(Debug)]
pub struct QueryState {
    /// Statement id this query owns.
    pub statement_id: u32,
    /// Result columns.
    pub columns: Vec<Column>,
    /// `FastSelect` wire form (featureOption == 1 packs rows differently).
    pub fast_select: bool,
    /// Max items per packed row (`FastSelect`).
    pub max_row_items: usize,
    /// Buffer of undecoded rows (data messages).
    pub data: Vec<u8>,
    /// Read cursor inside `data`.
    pub offset: usize,
    /// Last data-message SQLCODE.
    pub sqlcode: i16,
}

/// Cursor over a reply body that latches on the first unreadable or absent
/// item, so a server-declared count can never spin a decode loop past the
/// bytes that actually exist.
struct ItemCursor<'a> {
    reader: ListReader<'a>,
    body_len: usize,
    done: bool,
}

impl<'a> ItemCursor<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            reader: ListReader::new(body),
            body_len: body.len(),
            done: false,
        }
    }

    fn next(&mut self) -> Option<Item> {
        if self.done {
            return None;
        }
        if let Some(Ok(item)) = self.reader.next() {
            return Some(item);
        }
        self.done = true;
        None
    }

    /// Whether the walk can no longer advance.
    fn stopped(&self) -> bool {
        self.done
    }

    /// Upper bound on items still declarable from the bytes that remain
    /// (the smallest complete item is a two-byte prefix).
    fn item_capacity(&self) -> usize {
        self.body_len.saturating_sub(self.reader.offset()) / 2
    }
}

/// Decode the DQ metadata message body.
///
/// ref: go sql.go:statementFeature + getColumns + parameterInfo.
#[must_use]
pub fn decode_meta(body: &[u8]) -> Meta {
    let mut r = ItemCursor::new(body);
    let next_item = |r: &mut ItemCursor| -> Option<Item> { r.next() };
    let feature_option = match next_item(&mut r) {
        Some(Item::Int(v)) => v,
        _ => 0,
    };
    if feature_option == 2 {
        let _msg_count = next_item(&mut r);
    }
    let mut max_row_items = 0usize;
    if feature_option == 1 || feature_option == 2 {
        if let Some(Item::Int(v)) = next_item(&mut r) {
            max_row_items = usize::try_from(v).unwrap_or(0);
        }
    }
    let col_count = match next_item(&mut r) {
        Some(Item::Int(v)) => usize::try_from(v).unwrap_or(0),
        _ => 0,
    };
    // A server-supplied count must never drive a raw reservation: each
    // declared column costs body bytes, so reserve by what could actually
    // be read and stop the walk when the items run out (go getColumns does
    // `make([]Column, cnt)` unchecked — documented divergence, hygiene
    // rule: validate before allocating).
    let mut columns = Vec::with_capacity(col_count.min(r.item_capacity() / 2));
    for _ in 0..col_count {
        if r.stopped() {
            break;
        }
        let name = next_item(&mut r).map(|i| i.as_text()).unwrap_or_default();
        let type_code = match next_item(&mut r) {
            Some(Item::Int(v)) => v as i32,
            Some(other) => other.as_text().parse().unwrap_or(12),
            None => 12,
        };
        // precision, scale, nullable, label, table, schema, catalog, flags
        for _ in 0..8 {
            let _ = next_item(&mut r);
        }
        // slot_position is only present in FastSelect mode (go
        // sql.go:getColumns: slot -= 1; otherwise the column index).
        let slot = if feature_option & 1 == 1 {
            match next_item(&mut r) {
                Some(Item::Int(v)) => usize::try_from(v - 1).unwrap_or(0),
                _ => 0,
            }
        } else {
            columns.len()
        };
        columns.push(Column {
            name,
            type_code,
            slot,
        });
    }
    Meta {
        feature_option,
        max_row_items,
        columns,
    }
}

/// Decode a DU reply body to rows-affected.
///
/// ref: go sql.go:getParameterInfo + `msg.Get(&batchRows)`; the exact item
/// layout between those was settled against a live 2025.3 capture (probes).
#[must_use]
pub fn decode_du_affected(body: &[u8]) -> i64 {
    let mut r = ItemCursor::new(body);
    let n_params = match r.next() {
        Some(Item::Int(v)) => v.max(0) as usize,
        _ => 0,
    };
    // The same hostile-count guard as decode_meta: a server-declared
    // parameter count must not spin the walk past the bytes that exist.
    for _ in 0..n_params.saturating_mul(4).min(body.len()) {
        if r.stopped() {
            break;
        }
        let _ = r.next();
    }
    let _cache_flag = r.next();
    match r.next() {
        Some(Item::Int(v)) => v,
        Some(other) => other.as_text().parse().unwrap_or(0),
        None => 0,
    }
}

/// `OE` `GET_SERVER_ERROR`: server text for an SQLCODE.
///
/// ref: go sql.go:getErrorInfo.
pub fn error_text<T: Transport>(t: &mut T, s: &mut Session, sqlcode: i16) -> Result<String> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_error_info(sqlcode, c), c)?;
    Ok(first_item(&reply.body)?
        .map(|i| i.as_text())
        .unwrap_or_default())
}

/// SQLCODE ∈ {0, 100} continues; anything else is an error whose text the
/// caller fetches via `OE`. Pure rule, shared by the sync and async drivers
/// so the {0, 100} exception set lives in one place.
#[must_use]
pub const fn sqlcode_is_ok(code: i16) -> bool {
    code == 0 || code == 100
}

/// Build the `Error::Server` a failing SQLCODE maps to.
#[must_use]
pub fn sqlcode_error(code: i16, message: String) -> Error {
    Error::Server {
        code: i64::from(code),
        message,
    }
}

/// SQLCODE ∈ {0, 100} continues; anything else errors with the `OE`
/// message.
pub fn check_sqlcode<T: Transport>(t: &mut T, s: &mut Session, code: i16) -> Result<()> {
    if sqlcode_is_ok(code) {
        return Ok(());
    }
    let message = error_text(t, s, code)?;
    Err(sqlcode_error(code, message))
}

/// Send `DQ` and read the metadata + first pushed data frame (sync driver).
///
/// ref: go sql.go:DirectQuery.
pub fn direct_query<T: Transport>(
    t: &mut T,
    s: &mut Session,
    sql: &str,
    params: &[Option<String>],
    max_rows: i64,
) -> Result<QueryState> {
    let statement_id = s.next_statement();
    let c = s.next_count();
    let reply = t.roundtrip(
        &build_direct_query(sql, params, max_rows, statement_id, c),
        c,
    )?;
    let code = i16::try_from(reply.status()).unwrap_or(i16::MAX);
    check_sqlcode(t, s, code)?;
    let meta = decode_meta(&reply.body);

    // first data message arrives as a server push (go sql.go:DirectQuery
    // ReadMessage after metadata; the pushed frame echoes the DQ count).
    let reply = t.read_push(c)?;
    let sqlcode = i16::try_from(reply.status()).unwrap_or(i16::MAX);
    check_sqlcode(t, s, sqlcode)?;
    Ok(QueryState {
        statement_id,
        columns: meta.columns,
        fast_select: meta.feature_option == 1,
        max_row_items: meta.max_row_items,
        data: reply.body,
        offset: 0,
        sqlcode,
    })
}

/// Pure row decoder: pull one row's items from the current data buffer.
/// Returns `None` when the buffer is exhausted.
///
/// ref: go sql.go:ResultSet.Next. In `FastSelect` mode the server packs each
/// row into ONE ASCII wrapper item whose payload is an inner `%List` of
/// `maxRowItemCount` slot items; columns pick their value by slot position
/// (live 2025.3 negotiates featureOption=1 for ordinary SELECTs — probe
/// 2026-10-15, `probes/refclient.py`; the earlier "defensive parity"
/// comment claiming featureOption 0 was wrong and broke multi-column
/// result sets).
pub fn decode_row(st: &mut QueryState) -> Result<Option<Vec<Item>>> {
    if st.offset >= st.data.len() {
        return Ok(None);
    }
    if st.fast_select {
        let mut rr = ListReader::new(&st.data[st.offset..]);
        let Some(res) = rr.next() else {
            return Ok(None);
        };
        let wrapper = match res {
            Ok(i) => i,
            Err(e) => return Err(super::session::list_err(&e)),
        };
        st.offset += rr.offset();
        let inner = wrapper.as_bytes();
        let slot_count = st
            .max_row_items
            .max(1)
            .max(st.columns.iter().map(|c| c.slot + 1).max().unwrap_or(0));
        let mut vals = Vec::with_capacity(slot_count);
        let mut ir = ListReader::new(&inner);
        for _ in 0..slot_count {
            match ir.next() {
                Some(Ok(i)) => vals.push(i),
                Some(Err(e)) => return Err(super::session::list_err(&e)),
                None => break,
            }
        }
        let mut row = Vec::with_capacity(st.columns.len());
        for c in &st.columns {
            row.push(vals.get(c.slot).cloned().unwrap_or(Item::Null));
        }
        return Ok(Some(row));
    }
    let mut row = Vec::with_capacity(st.columns.len());
    let mut rr = ListReader::new(&st.data[st.offset..]);
    for _ in 0..st.columns.len() {
        match rr.next() {
            Some(Ok(i)) => row.push(i),
            Some(Err(e)) => return Err(super::session::list_err(&e)),
            None => break,
        }
    }
    st.offset += rr.offset();
    Ok(Some(row))
}

/// Advance to the next row, sending `FD` when a batch ends (sync driver).
///
/// ref: go sql.go:fetchMoreData; py _DBAPI _`switch_buffer` (FD only after a
/// SQLCODE-0 data message — 100 means the rowset is exhausted; live-
/// confirmed: an FD after SQLCODE 100 errors with -400).
pub fn next_row<T: Transport>(
    t: &mut T,
    s: &mut Session,
    st: &mut QueryState,
) -> Result<Option<Vec<Item>>> {
    loop {
        if st.sqlcode != 0 && st.sqlcode != 100 {
            return Ok(None);
        }
        if let Some(row) = decode_row(st)? {
            return Ok(Some(row));
        }
        if st.sqlcode == 100 {
            return Ok(None);
        }
        let c = s.next_count();
        let reply: Reply = t.roundtrip(&build_fetch(st.statement_id, c), c)?;
        st.sqlcode = i16::try_from(reply.status()).unwrap_or(i16::MAX);
        check_sqlcode(t, s, st.sqlcode)?;
        st.data = reply.body;
        st.offset = 0;
    }
}

/// `DU` direct update; returns rows affected (sync driver).
///
/// ref: go sql.go:DirectUpdate (single batch path).
pub fn direct_update<T: Transport>(
    t: &mut T,
    s: &mut Session,
    sql: &str,
    params: &[Option<String>],
) -> Result<i64> {
    let statement_id = s.next_statement();
    let c = s.next_count();
    let reply = t.roundtrip(&build_direct_update(sql, params, statement_id, c), c)?;
    let code = i16::try_from(reply.status()).unwrap_or(i16::MAX);
    check_sqlcode(t, s, code)?;
    Ok(decode_du_affected(&reply.body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
            .collect()
    }

    #[test]
    fn decode_meta_handles_a_live_dq_metadata_body() {
        // captured 2026-10-15 (probes/captures/typed_query.json): body of
        // the DQ metadata message for a five-column SELECT.
        let body = hex(
            "02040304050401545303040b0304130204030402040154530201020102040e0100000001000001010000\
             00000301440304020304050304020304020301440201020102040e0100000001000001010000000003014e\
             03040c03040a020403040203014e0201020102040e0100000001000001010000000003014903040403040a\
             02040304020301490201020102040e01000000010000010100000000030146030402030403030402020403\
             01460201020102040e010000000100000100000000000204030401",
        );
        let meta = decode_meta(&body);
        assert_eq!(meta.feature_option, 0);
        let names: Vec<&str> = meta.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["TS", "D", "N", "I", "F"]);
    }

    #[test]
    fn decode_meta_never_panics_on_truncated_metadata() {
        // hygiene: garbage in, error (or partial) out — never a panic
        let full = hex("0204030501545303040b03040a0304000304010501435453040153595305014e554c0204");
        for n in 0..full.len() {
            let _ = decode_meta(&full[..n]);
        }
    }

    #[test]
    fn metadata_decoders_survive_garbage_bodies() {
        // Fuzz the two body decoders over pseudo-random garbage and every
        // truncation of it: no panics, no unbounded work (bounded run time
        // is implicit — a hang would fail the test harness).
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for round in 0..64usize {
            let mut body = Vec::with_capacity(48);
            for _ in 0..48 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                body.push((state >> 24) as u8);
            }
            if round % 2 == 0 {
                // bias toward plausible item prefixes
                body[0] = 0x02;
                body[1] = 0x04;
            }
            for n in (0..=body.len()).step_by(3) {
                let _ = decode_meta(&body[..n]);
                let _ = decode_du_affected(&body[..n]);
            }
        }
    }

    #[test]
    fn decode_meta_bounds_a_hostile_column_count() {
        // A tiny body declaring ~1e9 columns must not reserve (or build)
        // anything near that count: each declared column costs at least two
        // body bytes, so the reservation follows the bytes actually present.
        let mut w = crate::wire::list::ListWriter::new();
        w.set_int(0); // feature_option
        w.set_int(1_000_000_000i64); // declared column count
        let after_count = w.buf.len();
        w.set_str("x"); // one token column
        let meta = decode_meta(&w.buf);
        assert!(
            meta.columns.len() <= (w.buf.len() - after_count) / 2 + 1,
            "columns {} far exceeds the bytes that declare them",
            meta.columns.len()
        );
    }

    #[test]
    fn du_affected_decodes_probe_shapes() {
        // live DU reply item shapes (probes, IRIS 2025.3):
        // [paramscnt] [(type,prec,scale,flag) × n] [cacheflag] [rows]
        let body = hex("0204030401030405");
        assert_eq!(decode_du_affected(&body), 5);
        let mut w = crate::wire::list::ListWriter::new();
        w.set_int(1); // paramscnt
        w.set_int(12);
        w.set_int(50);
        w.set_int(0);
        w.set_int(1); // (type, precision, scale, nullable)
        w.set_int(5); // cache flag
        w.set_int(2); // rows affected
        assert_eq!(decode_du_affected(&w.buf), 2);
    }

    #[test]
    fn row_decode_walks_items_to_buffer_end() {
        let mut w = crate::wire::list::ListWriter::new();
        w.set_str("1");
        w.set_str("x");
        let mut st = QueryState {
            statement_id: 0,
            columns: vec![
                Column {
                    name: "A".into(),
                    type_code: 12,
                    slot: 0,
                },
                Column {
                    name: "B".into(),
                    type_code: 12,
                    slot: 1,
                },
            ],
            fast_select: false,
            max_row_items: 0,
            data: w.buf.clone(),
            offset: 0,
            sqlcode: 100,
        };
        let row = decode_row(&mut st).unwrap().unwrap();
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].as_text(), "1");
        assert_eq!(row[1].as_text(), "x");
        assert!(decode_row(&mut st).unwrap().is_none());
    }
}
