//! Golden-frame tests: byte-exact replay of a live capture produced by
//! `probes/refclient.py` (a stdlib re-implementation of the reference
//! drivers' wire rules, protocol 69) against the live IRIS 2025.3 container
//! on 2026-10-15. Any change that shifts request bytes fails here.
//!
//! Fixture: `tests/fixtures/frames.json` (regenerate with the snippet in
//! `probes/README.md`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::items_after_statements
)]

use irisnative::wire::list::{Item, ListReader, ListWriter};

fn fixture() -> JsonDoc {
    JsonDoc::parse(include_str!("fixtures/frames.json"))
}

/// Minimal object-of-arrays-of-strings reader (no dev-dependencies).
struct JsonDoc(std::collections::BTreeMap<String, Vec<String>>);

impl JsonDoc {
    fn parse(src: &str) -> Self {
        let bytes: Vec<u8> = src.bytes().collect();
        let mut pos = 0usize;
        fn skip(b: &[u8], p: &mut usize) {
            while *p < b.len() && (b[*p] as char).is_whitespace() {
                *p += 1;
            }
        }
        fn string(b: &[u8], p: &mut usize) -> String {
            skip(b, p);
            assert_eq!(b[*p], b'"');
            *p += 1;
            let start = *p;
            while b[*p] != b'"' {
                *p += 1;
            }
            let s = String::from_utf8_lossy(&b[start..*p]).into_owned();
            *p += 1;
            s
        }
        fn array(b: &[u8], p: &mut usize) -> Vec<String> {
            skip(b, p);
            assert_eq!(b[*p], b'[');
            *p += 1;
            let mut out = Vec::new();
            loop {
                skip(b, p);
                if b[*p] == b']' {
                    *p += 1;
                    break;
                }
                out.push(string(b, p));
                skip(b, p);
                if b[*p] == b',' {
                    *p += 1;
                }
            }
            out
        }
        skip(&bytes, &mut pos);
        assert_eq!(bytes[pos], b'{');
        pos += 1;
        let mut map = std::collections::BTreeMap::new();
        loop {
            skip(&bytes, &mut pos);
            if bytes[pos] == b'}' {
                break;
            }
            let k = string(&bytes, &mut pos);
            skip(&bytes, &mut pos);
            assert_eq!(bytes[pos], b':');
            pos += 1;
            let v = array(&bytes, &mut pos);
            map.insert(k, v);
            skip(&bytes, &mut pos);
            if bytes[pos] == b',' {
                pos += 1;
            }
        }
        Self(map)
    }

    fn get(&self, key: &str) -> &[String] {
        self.0.get(key).map_or(&[][..], |v| v.as_slice())
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex fixture"))
        .collect()
}

#[test]
fn golden_connect_path_encoder_vectors() {
    // ref: probes/refclient.py encode_credentials (live-accepted login)
    assert_eq!(
        irisnative::connection::auth::encode_credentials("SYS"),
        vec![0x9E, 0xFF, 0xDE]
    );
}

#[test]
fn golden_request_frames_replay_capture() {
    let doc = fixture();
    let reqs = doc.get("requests");
    assert!(!reqs.is_empty(), "requests present");
    // capture sequence (counts continue after HS=0/CN=1; a KILL ran at 2):
    // 0: GS SFDEMO(1,"x")="v1"   count 3
    let gs = hex(&reqs[0]);
    assert_eq!(
        irisnative::connection::commands::build_global_set(
            "SFDEMO",
            &["1".into(), "x".into()],
            "v1",
            3,
        ),
        gs
    );
    // 1: GG SFDEMO(1,"x")        count 4
    let gg = hex(&reqs[1]);
    assert_eq!(
        irisnative::connection::commands::build_global_get("SFDEMO", &["1".into(), "x".into()], 4),
        gg
    );
    // 2: GG SFDEMO(999)          count 5
    assert_eq!(
        irisnative::connection::commands::build_global_get("SFDEMO", &["999".into()], 5),
        hex(&reqs[2])
    );
    // 3: GD SFDEMO(1)            count 6
    assert_eq!(
        irisnative::connection::commands::build_global_data("SFDEMO", &["1".into()], 6),
        hex(&reqs[3])
    );
    // 4: GLOBAL_ORDER next, ind="1" count 7
    assert_eq!(
        irisnative::connection::commands::build_global_order("SFDEMO", &[], "1", 3, 7),
        hex(&reqs[4])
    );
    // 5: CLASSMETHOD Version     count 8
    assert_eq!(
        irisnative::connection::commands::build_class_method(
            "%SYSTEM.Version",
            "GetVersion",
            &[],
            8,
        ),
        hex(&reqs[5])
    );
    // 6: DQ with one bound param count 9 stmt 0
    assert_eq!(
        irisnative::connection::commands::build_direct_query(
            "SELECT ID, NAME FROM SFDEMO.T3 WHERE NAME = ?",
            &[Some("committed".into())],
            0,
            0,
            9,
        ),
        hex(&reqs[6])
    );
    // 7: DU insert               count 10 stmt 1
    assert_eq!(
        irisnative::connection::commands::build_direct_update(
            "INSERT INTO SFDEMO.T3(NAME) VALUES (?)",
            &[Some("golden".into())],
            1,
            10,
        ),
        hex(&reqs[7])
    );
    // 8: TC commit               count 11
    assert_eq!(
        irisnative::connection::commands::build_commit(11),
        hex(&reqs[8])
    );
}

#[test]
fn golden_reply_decoders_replay_capture() {
    let doc = fixture();
    let reps = doc.get("replies");
    assert!(!reps.is_empty(), "replies present");
    // replies interleave pushes: [0]=GS ack, [1]=GG value, [2]=GG undef,
    // [3]=GD, [4]=GO, [5]=CM, [6]=DQ metadata, [7]=DQ data push, [8]=DU,
    // [9]=TC
    let r0 = hex(&reps[0]);
    assert_eq!(
        irisnative::wire::header::Header::parse_slice(&r0).sysio_code(),
        0
    );
    let r1 = hex(&reps[1]);
    let item = ListReader::new(&r1[14..]).next().and_then(Result::ok);
    assert_eq!(item.map(|i| i.as_text()), Some("v1".into()));
    let r2 = hex(&reps[2]);
    let h2 = irisnative::wire::header::Header::parse_slice(&r2);
    assert_eq!(h2.sysio_code(), 1009, "<UNDEFINED>");
    let body2 = &r2[14..];
    let msg = ListReader::new(body2).next().and_then(Result::ok);
    assert_eq!(msg.map(|i| i.as_text()), Some("<UNDEFINED>".into()));
    // GLOBAL_DATA on ^SFDEMO(1) which holds only a subtree: raw 10
    // (value = 10 % 10 == 1 -> false; subtree = 10 >= 10 -> true)
    let r3 = hex(&reps[3]);
    assert_eq!(
        ListReader::new(&r3[14..]).next().and_then(Result::ok),
        Some(Item::Int(10))
    );
    // DQ metadata reply decodes its columns
    let r6 = hex(&reps[6]);
    let meta = irisnative::connection::sql::decode_meta(&r6[14..]);
    let names: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, ["ID", "NAME"]);
}

#[test]
fn writer_produces_the_documented_item_shapes() {
    // py unit vectors (_DBList) pinned from the reference wheel:
    let mut w = ListWriter::new();
    w.set_str("");
    assert_eq!(&w.buf[..2], &[0x02, 0x01]);
    w = ListWriter::new();
    w.set_int(1);
    assert_eq!(&w.buf, &[0x03, 0x04, 0x01]);
    w = ListWriter::new();
    w.set_int(300);
    assert_eq!(&w.buf, &[0x04, 0x04, 0x2C, 0x01]);
    // py wire: Decimal("1.5") -> 04 06 FF 0F (scale -1, mantissa 15)
    let body = [0x04, 0x06, 0xFF, 0x0F];
    let item = ListReader::new(&body)
        .next()
        .and_then(Result::ok)
        .expect("decimal");
    match item {
        Item::Decimal { scale, value } => {
            assert_eq!(scale, -1);
            assert_eq!(value, 15);
        }
        other => panic!("expected decimal, got {other:?}"),
    }
    // py float 1.5 (non-compact) -> 0a 08 + IEEE LE bytes
    let mut b = vec![0x0A, 0x08];
    b.extend_from_slice(&1.5f64.to_le_bytes());
    let item = ListReader::new(&b)
        .next()
        .and_then(Result::ok)
        .expect("double");
    assert!(matches!(item, Item::Double(f) if (f - 1.5).abs() < f64::EPSILON));
    // SQL empty string (py __stuff_empty_string / go toODBC(""))
    let mut w = ListWriter::new();
    w.set_sql_str("");
    assert_eq!(&w.buf, &[0x03, 0x01, 0x00]);
    // OREF item type 0x19 (py ITEM_OREF_ASCII / go LISTITEM_OREF)
    let mut w = ListWriter::new();
    w.set_oref("5@%Dictionary.ClassDefinition");
    assert_eq!(w.buf[1], 0x19);
}

#[test]
fn header_vectors_replay_the_handshake_bytes() {
    // HS request from the 2026-10-15 capture: raw LE 69 after the header
    let frame = irisnative::connection::commands::build_handshake();
    assert_eq!(&frame[12..14], b"HS");
    assert_eq!(&frame[14..], &69u16.to_le_bytes());
    assert_eq!(
        u32::from_le_bytes(frame[0..4].try_into().unwrap_or([0; 4])),
        2
    );
}
