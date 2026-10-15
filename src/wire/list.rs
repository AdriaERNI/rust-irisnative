//! `%List` item codec (ledger rows 3 and 4).
//!
//! Item framing (both references agree): a size/type prefix followed by the
//! payload. `prefix = payload_len + 2` and type byte for payloads under 253
//! bytes; otherwise a `0x00` escape introduces a little-endian u16 of
//! `payload_len + 1`. The single byte `0x01` is *undefined*; the two bytes
//! `0x02 0x01` are *null* (an empty ASCII item).
//!
//! ref: go-irisnative src/list/listitem.go:NewListItem / Dump / `GetListItem`
//! ref: py-iris iris/_DBList.py:__`set_list_length_type` / _`get_list_element`
//!
//! Where go and py disagree (go's short-form threshold overflows at
//! payloads 254–255), this codec follows py's `_DBList` arithmetic, which the
//! live server is known to accept (probe: `probes/refclient.py`).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

/// Item type bytes as they appear on the wire (second byte of the prefix).
///
/// ref: go-irisnative src/list/listitem.go const block; py-iris
/// iris/_DBList.py ITEM_* constants.
pub mod item_type {
    /// 8-bit string.
    pub const ASCII: u8 = 0x01;
    /// UTF-16LE string.
    pub const UNICODE: u8 = 0x02;
    /// Positive integer, little-endian magnitude bytes.
    pub const POS_INT: u8 = 0x04;
    /// Negative integer, complement bytes (see decode).
    pub const NEG_INT: u8 = 0x05;
    /// Positive decimal: scale byte + LE magnitude.
    pub const POS_DECIMAL: u8 = 0x06;
    /// Negative decimal: scale byte + complement magnitude.
    pub const NEG_DECIMAL: u8 = 0x07;
    /// IEEE double (variable length when compact).
    pub const DOUBLE: u8 = 0x08;
    /// Compact double: LE double minus leading zero bytes.
    pub const COMPACT_DOUBLE: u8 = 0x09;
    /// Object reference (ASCII form).
    pub const OREF: u8 = 0x19;
    /// Object reference (unicode form).
    pub const OREF_UNICODE: u8 = 0x1A;
    /// Pass-by-reference flag added to the item type (both refs).
    pub const BY_REF: u8 = 0x20;
}

/// A decoded `%List` item.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// Undefined node (`0x01` / `$o`-style absent value).
    Undefined,
    /// SQL NULL — the empty ASCII item.
    Null,
    /// String payload (types 0x01 / 0x02), decoded lossily.
    String(String),
    /// Integer (types 0x04 / 0x05).
    Int(i64),
    /// Decimal (types 0x06 / 0x07) kept as its scale + mantissa parts.
    Decimal {
        /// Scale (power-of-ten exponent).
        scale: i8,
        /// Unscaled value.
        value: i128,
    },
    /// Binary float (types 0x08 / 0x09).
    Double(f64),
    /// Object reference (types 0x19 / 0x1A).
    Oref(String),
}

impl Item {
    /// Raw payload bytes for a string-carrying item: latin-1 bytes when
    /// every char fits one byte (the ASCII case is byte-preserving by
    /// construction in `decode`), UTF-16LE for unicode items. Used by the
    /// `FastSelect` row unpacker to hand the server's inner `%List` buffer
    /// back to a `ListReader`.
    ///
    /// ref: go listitem.go Get(&[]byte) / py `_grab_ascii_string(asBytes=True)`.
    #[must_use]
    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            Item::String(s) | Item::Oref(s) => {
                if s.chars().all(|c| (c as u32) <= 0xFF) {
                    s.chars().map(|c| c as u8).collect()
                } else {
                    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
                }
            }
            Item::Undefined | Item::Null => Vec::new(),
            other => other.as_text().into_bytes(),
        }
    }

    /// Best-effort text rendering used by the global/SQL value decoders.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Item::Undefined | Item::Null => String::new(),
            Item::String(s) | Item::Oref(s) => s.clone(),
            Item::Int(i) => i.to_string(),
            Item::Decimal { scale, value } => {
                let s = value.abs().to_string();
                let (sign, digits) = if *value < 0 {
                    ("-", s.as_str())
                } else {
                    ("", s.as_str())
                };
                match (*scale).signum() {
                    0 => format!("{sign}{digits}"),
                    1 => format!("{sign}{digits}{}", "0".repeat(*scale as usize)),
                    _ => {
                        let n = (-*scale) as usize;
                        let padded = format!("{:0>width$}", digits, width = n + 1);
                        let (a, b) = padded.split_at(padded.len() - n);
                        format!("{sign}{a}.{b}")
                    }
                }
            }
            Item::Double(f) => {
                // exact check intended: compact form is bit-for-bit lossless
                #[allow(clippy::float_cmp)]
                let single_exact = *f == f64::from(*f as f32);
                if single_exact {
                    format!("{}", *f as f32)
                } else {
                    f.to_string()
                }
            }
        }
    }
}

/// Writer that appends `%List` items to a byte buffer.
#[derive(Debug, Default)]
pub struct ListWriter {
    /// Accumulated body bytes.
    pub buf: Vec<u8>,
}

impl ListWriter {
    /// A new empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn prefix(&mut self, payload_len: usize, type_byte: u8) {
        // ref: py-iris iris/_DBList.py:__set_list_length_type
        let total = payload_len + 1; // data bytes + type byte
        if total < 0xFF {
            self.buf.push((total + 1) as u8);
            self.buf.push(type_byte);
        } else if total <= 0xFFFF {
            self.buf.push(0);
            self.buf.extend_from_slice(&(total as u16).to_le_bytes());
            self.buf.push(type_byte);
        } else {
            self.buf.extend_from_slice(&[0, 0]);
            self.buf.extend_from_slice(&(total as u32).to_le_bytes());
            self.buf.push(type_byte);
        }
    }

    /// Write a raw payload item.
    fn raw(&mut self, type_byte: u8, payload: &[u8]) {
        self.prefix(payload.len(), type_byte);
        self.buf.extend_from_slice(payload);
    }

    /// Write a string: 8-bit item when every char fits a byte, else
    /// UTF-16LE (`0x02`).
    ///
    /// ref: go-irisnative src/list/listitem.go (string case); py-iris
    /// iris/_DBList.py:_`stuff_str` / __`stuff_8bit_ascii`.
    pub fn set_str(&mut self, s: &str) {
        if s.is_empty() {
            // go mod.go Set("") on the wire == empty ASCII item (probe).
            self.raw(item_type::ASCII, &[]);
            return;
        }
        if s.chars().all(|c| (c as u32) <= 0xFF) {
            self.raw(item_type::ASCII, s.as_bytes());
        } else {
            let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            self.raw(item_type::UNICODE, &utf16);
        }
    }

    /// Write a SQL parameter string: the empty string is sent as a
    /// one-byte NUL payload per both refs (`go sql.go toODBC`,
    /// `py __stuff_empty_string`) so it cannot collide with NULL.
    pub fn set_sql_str(&mut self, s: &str) {
        if s.is_empty() {
            self.raw(item_type::ASCII, b"\x00");
        } else {
            self.set_str(s);
        }
    }

    /// Write a global subscript: canonical integers go as `POS_INT`/`NEG_INT`
    /// items the way both references pass them (go `msg.Set(1)`, py
    /// `_stuff_int`); everything else stays an 8-bit/16-bit string item.
    /// In M, `^x("1")` and `^x(1)` denote the same node, so the numeric
    /// encoding is the collation-correct choice.
    pub fn set_subscript(&mut self, s: &str) {
        if let Ok(v) = s.parse::<i64>() {
            // canonical M numeric form: no leading zeros, no '+', minimal
            if v.to_string() == s {
                self.set_int(v);
                return;
            }
        }
        self.set_str(s);
    }

    /// Write bytes as an 8-bit string item.
    pub fn set_bytes(&mut self, b: &[u8]) {
        self.raw(item_type::ASCII, b);
    }

    /// Write an OREF (`0x19`) item from its text form.
    ///
    /// ref: go-irisnative src/list/listitem.go (iris.Oref case; OREFs are
    /// carried by value here — the by-ref flag is a stored-procedure
    /// feature outside this crate's surface).
    pub fn set_oref(&mut self, oref: &str) {
        self.raw(item_type::OREF, oref.as_bytes());
    }

    /// Write an integer (types 0x04 / 0x05).
    ///
    /// ref: go-irisnative src/list/listitem.go int case; py-iris
    /// iris/_DBList.py:_`stuff_int` (zero-length encoding for -1).
    pub fn set_int(&mut self, v: i64) {
        match v.signum() {
            0 => self.raw(item_type::POS_INT, &[]),
            1 => {
                let mut t = v;
                let mut data = Vec::new();
                while t > 0 {
                    data.push((t & 0xFF) as u8);
                    t >>= 8;
                }
                self.raw(item_type::POS_INT, &data);
            }
            _ => {
                let mut t = -v - 1;
                let mut data = Vec::new();
                while t > 0 {
                    data.push(((!t) & 0xFF) as u8);
                    t >>= 8;
                }
                self.raw(item_type::NEG_INT, &data);
            }
        }
    }

    /// Write a float: compact (variable-length, leading zeros skipped) when
    /// `compact` is negotiated, else a full 8-byte IEEE double (type 0x08).
    ///
    /// ref: py-iris iris/_DBList.py:_`stuff_double` (authoritative for the
    /// 8/9 type split); go listitem.go writes 0x09 for all f64s — py's
    /// choice is used since the live server round-trips it (probe).
    pub fn set_f64(&mut self, v: f64, compact: bool) {
        let bytes = v.to_le_bytes();
        if compact {
            let start = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
            if start == bytes.len() {
                self.raw(item_type::DOUBLE, &[]); // zero
            } else {
                self.raw(item_type::COMPACT_DOUBLE, &bytes[start..]);
            }
        } else {
            self.raw(item_type::DOUBLE, &bytes);
        }
    }

    /// Write SQL NULL.
    ///
    /// ref: py-iris iris/_DBList.py:_`stuff_null` (`0x02 0x01`).
    pub fn set_null(&mut self) {
        self.raw(item_type::ASCII, &[]);
    }
}

/// A cursor over a body buffer reading `%List` items.
#[derive(Debug)]
pub struct ListReader<'a> {
    data: &'a [u8],
    offset: usize,
}

/// Decoding failure kinds (never a panic on server data — hygiene row 34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// Ran past the end of the buffer while reading a prefix or payload.
    Truncated,
    /// A payload/item type the codec does not know.
    UnknownType(u8),
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "%List item truncated"),
            Self::UnknownType(t) => write!(f, "unknown %List item type 0x{t:02X}"),
        }
    }
}

impl std::error::Error for ListError {}

impl<'a> ListReader<'a> {
    /// A reader positioned at the start of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    /// Bytes consumed so far.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offset >= self.data.len()
    }

    /// Read one item, or `None` when the buffer is exhausted (the name
    /// mirrors go's `GetListItem` call sites, not [`std::iter::Iterator`];
    /// items carry errors, not `Option`.)
    ///
    /// Returns `None` when the buffer is exhausted (go returns a
    /// null item past the end; treating that as end-of-stream is stricter
    /// and lets callers loop).
    ///
    /// ref: go-irisnative src/list/listitem.go:GetListItem; py-iris
    /// iris/_DBList.py:_`get_list_element`.
    #[allow(clippy::should_implement_trait)] // see doc: cursor API, fallible
    pub fn next(&mut self) -> Option<Result<Item, ListError>> {
        if self.offset >= self.data.len() {
            return None;
        }
        let (ty, payload_len) = match self.read_prefix() {
            Ok(UNDEFINED_MARK) => return Some(Ok(Item::Undefined)),
            Ok(pair) => pair,
            Err(e) => return Some(Err(e)),
        };
        let Some(end) = self.offset.checked_add(payload_len) else {
            return Some(Err(ListError::Truncated));
        };
        let Some(payload) = self.data.get(self.offset..end) else {
            return Some(Err(ListError::Truncated));
        };
        self.offset = end;
        Some(Self::decode(ty, payload))
    }

    /// Parse the size+type prefix and advance to the payload start.
    fn read_prefix(&mut self) -> Result<(u8, usize), ListError> {
        let b0 = self.data[self.offset];
        if b0 == 1 {
            self.offset += 1;
            return Ok(UNDEFINED_MARK);
        }
        let (ty, len);
        if b0 == 0 {
            // extended form: u16 length at +1..+3; when that u16 is 0, a
            // u32 length follows instead (4-byte form).
            // ref: py-iris iris/_DBList.py:_get_list_element
            if self.offset + 3 > self.data.len() {
                return Err(ListError::Truncated);
            }
            let mut inner =
                u16::from_le_bytes([self.data[self.offset + 1], self.data[self.offset + 2]])
                    as usize;
            self.offset += 3;
            if inner == 0 {
                if self.offset + 4 > self.data.len() {
                    return Err(ListError::Truncated);
                }
                inner = u32::from_le_bytes([
                    self.data[self.offset],
                    self.data[self.offset + 1],
                    self.data[self.offset + 2],
                    self.data[self.offset + 3],
                ]) as usize;
                self.offset += 4;
            }
            let type_byte = *self.data.get(self.offset).ok_or(ListError::Truncated)?;
            self.offset += 1;
            len = inner.checked_sub(1).ok_or(ListError::Truncated)?;
            ty = type_byte;
        } else {
            let payload_len = usize::from(b0).checked_sub(2).ok_or(ListError::Truncated)?;
            let type_byte = *self.data.get(self.offset + 1).ok_or(ListError::Truncated)?;
            self.offset += 2;
            ty = type_byte;
            len = payload_len;
        }
        let ty = if (item_type::BY_REF..0x40).contains(&ty) {
            ty - item_type::BY_REF
        } else {
            ty
        };
        Ok((ty, len))
    }

    /// Decode one item's payload by type byte.
    fn decode(ty: u8, payload: &[u8]) -> Result<Item, ListError> {
        Ok(match ty {
            item_type::ASCII => {
                if payload.is_empty() {
                    Item::Null
                } else if payload == b"\x00" {
                    Item::String(String::new())
                } else {
                    // Byte-preserving latin-1, not UTF-8 lossy: py
                    // `_grab_ascii_string` decodes with the connection
                    // locale ("latin-1") and go's string(li.data) is
                    // byte-preserving, so payloads with bytes >= 0x80 must
                    // survive round-trip (one char per byte) to stay in
                    // parity — and so FastSelect row wrappers can be
                    // re-encoded losslessly when unpacked.
                    Item::String(payload.iter().map(|b| char::from(*b)).collect::<String>())
                }
            }
            item_type::UNICODE => Item::String(decode_utf16(payload)),
            item_type::POS_INT => Item::Int(le_magnitude_u64(payload) as i64),
            item_type::NEG_INT => {
                // ref: py _grab_neg_integer: byte-wise complement within the
                // payload width, then -(v+1). go's getNegInt agrees.
                let raw = le_magnitude_u64(payload);
                let bits = payload.len().saturating_mul(8).min(64);
                let mask = if bits >= 64 {
                    u64::MAX
                } else {
                    (1u64 << bits) - 1
                };
                let v = (!raw) & mask;
                Item::Int(wrapping_neg(v.wrapping_add(1)))
            }
            item_type::POS_DECIMAL | item_type::NEG_DECIMAL => {
                if payload.is_empty() {
                    return Err(ListError::Truncated);
                }
                let scale = payload[0] as i8;
                let raw = le_magnitude_u64(&payload[1..]);
                let value = if ty == item_type::NEG_DECIMAL {
                    // complement within the payload width, then -(v+1)
                    let bits = (payload.len() - 1).saturating_mul(8).min(64);
                    let mask = if bits >= 64 {
                        u128::MAX
                    } else {
                        (128u128 - 1) >> (64 - bits).min(127)
                    };
                    let v = ((!u128::from(raw)) & mask).wrapping_add(1);
                    i128::try_from(v).map_or(i128::MIN, |x| -x)
                } else {
                    i128::from(raw)
                };
                Item::Decimal { scale, value }
            }
            item_type::DOUBLE => {
                if payload.len() == 8 {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(payload);
                    Item::Double(f64::from_le_bytes(b))
                } else {
                    if payload.len() > 4 {
                        return Err(ListError::Truncated);
                    }
                    // compact single: right-aligned f32 bits
                    let mut b = [0u8; 4];
                    b[4 - payload.len()..].copy_from_slice(payload);
                    Item::Double(f64::from(f32::from_le_bytes(b)))
                }
            }
            item_type::COMPACT_DOUBLE => {
                if payload.len() > 8 {
                    return Err(ListError::Truncated);
                }
                let mut b = [0u8; 8];
                b[8 - payload.len()..].copy_from_slice(payload);
                Item::Double(f64::from_le_bytes(b))
            }
            item_type::OREF | item_type::OREF_UNICODE => {
                let s = if ty == item_type::OREF_UNICODE {
                    decode_utf16(payload)
                } else {
                    String::from_utf8_lossy(payload).into_owned()
                };
                Item::Oref(s)
            }
            _ => return Err(ListError::UnknownType(ty)),
        })
    }
}

/// Sentinel returned by `read_prefix` for the undefined marker byte.
const UNDEFINED_MARK: (u8, usize) = (u8::MAX, 0);

fn le_magnitude_u64(payload: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for (i, b) in payload.iter().take(8).enumerate() {
        v |= u64::from(*b) << (8 * i);
    }
    v
}

/// `-(v)` with `i64::MIN` wrapped by two's complement (py's arbitrary ints).
fn wrapping_neg(v: u64) -> i64 {
    (v as i64).wrapping_neg()
}

fn decode_utf16(payload: &[u8]) -> String {
    // ref: go listitem.go getString (UTF-16LE pairs); odd tails fall back to lossy bytes.
    let mut units = Vec::with_capacity(payload.len() / 2);
    for pair in payload.chunks(2) {
        if pair.len() == 2 {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        } else {
            return String::from_utf8_lossy(payload).into_owned();
        }
    }
    String::from_utf16_lossy(&units)
}

/// SQL statement text chunking: a non-empty statement is written as
/// `chunk_count` followed by that many ≤31904-byte pieces; the empty
/// statement is a single empty item.
///
/// ref: go-irisnative src/connection/message.go:SetSQLText.
pub const SQL_CHUNK_SIZE: usize = 31_904;

/// Append statement text (chunk-count + chunks) to `w`.
pub fn write_sql_text(w: &mut ListWriter, sql: &str) {
    if sql.is_empty() {
        w.set_str(sql);
        return;
    }
    let chunks = sql.len().div_ceil(SQL_CHUNK_SIZE);
    w.set_int(chunks as i64);
    for i in 0..chunks {
        let begin = i * SQL_CHUNK_SIZE;
        let end = ((i + 1) * SQL_CHUNK_SIZE).min(sql.len());
        // byte slices can split UTF-8; the reference drivers only ever send
        // latin-1 SQL, and so do we via set_bytes to stay byte-exact.
        w.set_bytes(&sql.as_bytes()[begin..end]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn short_prefix_arithmetic() {
        // ref: py _DBList.__set_list_length_type: "ab" -> [04 01 'a' 'b']
        let mut w = ListWriter::new();
        w.set_str("ab");
        assert_eq!(w.buf, vec![0x04, 0x01, b'a', b'b']);
    }

    #[test]
    fn extended_prefix_above_253_payload() {
        let mut w = ListWriter::new();
        w.set_bytes(&[0x41u8; 300]);
        assert_eq!(w.buf[0], 0);
        assert_eq!(u16::from_le_bytes([w.buf[1], w.buf[2]]), 301);
        assert_eq!(w.buf[3], 0x01);
    }

    #[test]
    fn int_roundtrip_including_negatives_and_zero() {
        for v in [
            0i64,
            1,
            255,
            256,
            70_000,
            -1,
            -255,
            -256,
            -70_000,
            i64::MIN + 1,
        ] {
            let mut w = ListWriter::new();
            w.set_int(v);
            let item = ListReader::new(&w.buf)
                .next()
                .and_then(std::result::Result::ok);
            assert_eq!(item, Some(Item::Int(v)), "roundtrip {v}");
        }
    }

    #[test]
    fn null_undefined_and_empty_are_distinct_bytes() {
        // go Set("") == py _stuff_null bytes: 02 01
        let mut w = ListWriter::new();
        w.set_str("");
        assert_eq!(&w.buf[..2], &[0x02, 0x01]);
        // SQL empty string: 03 01 00 (py __stuff_empty_string)
        let mut w2 = ListWriter::new();
        w2.set_sql_str("");
        assert_eq!(&w2.buf, &[0x03, 0x01, 0x00]);
        // SQL NULL binds identically to go's toODBC(nil) -> "" empty item
        let mut w3 = ListWriter::new();
        w3.set_null();
        assert_eq!(&w3.buf[..2], &[0x02, 0x01]);
    }

    #[test]
    fn unicode_item_selects_0x02() {
        // Q3: any codepoint > 0xFF flips the item to UTF-16LE
        let mut w = ListWriter::new();
        w.set_str("a\u{e9}\u{4e2d}");
        assert_eq!(w.buf[1], item_type::UNICODE);
        let item = ListReader::new(&w.buf)
            .next()
            .and_then(std::result::Result::ok);
        assert_eq!(item, Some(Item::String("a\u{e9}\u{4e2d}".into())));
    }

    #[test]
    fn compact_double_skips_leading_zeros() {
        // ref: py _DBList._stuff_double compact path
        let mut w = ListWriter::new();
        w.set_f64(1.5, true);
        assert_eq!(w.buf[1], item_type::COMPACT_DOUBLE);
        let back = ListReader::new(&w.buf)
            .next()
            .and_then(std::result::Result::ok);
        assert!(matches!(back, Some(Item::Double(f)) if (f - 1.5).abs() < f64::EPSILON));
    }

    #[test]
    fn sql_text_chunking_matches_go_arithmetic() {
        // ref: go message.go SetSQLText chunksize 31904
        let long = "x".repeat(SQL_CHUNK_SIZE + 7);
        let mut w = ListWriter::new();
        write_sql_text(&mut w, &long);
        let mut r = ListReader::new(&w.buf);
        assert_eq!(
            r.next().and_then(std::result::Result::ok),
            Some(Item::Int(2))
        );
        assert_eq!(
            r.next()
                .and_then(std::result::Result::ok)
                .map(|i| i.as_text().len()),
            Some(SQL_CHUNK_SIZE)
        );
        assert_eq!(
            r.next()
                .and_then(std::result::Result::ok)
                .map(|i| i.as_text().len()),
            Some(7)
        );
    }

    #[test]
    fn truncated_item_errors_not_panics() {
        // hygiene: server data that runs past the buffer is an error
        let bad = [0x40u8, 0x01];
        let r = ListReader::new(&bad).next();
        assert!(matches!(r, Some(Err(ListError::Truncated))));
    }

    #[test]
    fn by_ref_flag_stripped_from_type() {
        // ref: go GetListItem (itemType 32..64 -> byRef)
        let body = [0x03, 0x21, b'x']; // type 0x01 + BY_REF
        let item = ListReader::new(&body)
            .next()
            .and_then(std::result::Result::ok);
        assert_eq!(item, Some(Item::String("x".into())));
    }
}
