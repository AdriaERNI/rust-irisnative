//! Byte-level wire codec tests. Every case cites the live probe that
//! established the expectation; until probes run, the module pins only the
//! pure-logic surface (subscript encoding, option parsing).

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::float_cmp,
    clippy::semicolon_if_nothing_returned
)]
mod wire {
    use irisnative::{ConnectionOptions, Error, global};

    #[test]
    fn global_macro_captures_name_and_subs() {
        let g = global!(Demo(1, "x", 2));
        assert_eq!(g.name, "Demo");
        assert_eq!(
            g.subscripts,
            vec!["1".to_string(), "x".to_string(), "2".to_string()]
        );
    }

    #[test]
    fn global_macro_bare() {
        let g = global!(A);
        assert_eq!(g.name, "A");
        assert!(
            g.subscripts.is_empty(),
            "bare global! must carry no subscripts"
        );
    }

    #[test]
    fn global_subscript_order_is_source_order() {
        // The macro stringifies left-to-right; position is contractual for
        // later collation work, so pin it (docs/api.md "subscripts stringify").
        let g = global!(Idx("a", 3, 1.5));
        assert_eq!(g.subscripts, vec!["a", "3", "1.5"]);
    }

    #[test]
    fn global_clone_is_independent_and_equal() {
        let g = global!(Demo(1, "x"));
        let h = g.clone();
        assert_eq!(g.name, h.name);
        assert_eq!(g.subscripts, h.subscripts);
    }

    #[test]
    fn options_defaults_and_port_override() {
        let o = ConnectionOptions::new("h", "NS", "u", "p");
        assert_eq!(
            (o.host.as_str(), o.port, o.namespace.as_str()),
            ("h", 1972, "NS")
        );
        assert_eq!((o.username.as_str(), o.password.as_str()), ("u", "p"));
        assert_eq!(o.timeout_ms, None);
        assert_eq!(o.clone().port(1973).port, 1973);
        // builder returns self by value; the override replaces, not augments
        assert_eq!(o.port(2000).port(2001).port, 2001);
    }

    #[test]
    fn options_debug_redacts_the_password() {
        // A `{opts:?}` in a log line must never carry the credential.
        let o = ConnectionOptions::new("db.example", "USER", "appuser", "hunter2")
            .port(19720)
            .clone();
        let text = format!("{o:?}");
        assert!(!text.contains("hunter2"), "password leaked in {text}");
        assert!(text.contains("\"***\""), "marker missing in {text}");
        // everything else stays readable for debugging
        assert!(
            text.contains("db.example") && text.contains("appuser") && text.contains("19720"),
            "fields went opaque: {text}"
        );
        // the struct shape is contractual: a field-name audit tool parses it
        assert!(text.starts_with("ConnectionOptions {"), "{text}");
    }

    #[test]
    fn error_display_per_variant() {
        // Every Display string is part of the surface consumers log; pin one
        // rendering per variant (docs/api.md "Errors").
        assert_eq!(Error::Auth.to_string(), "authentication failed");
        assert_eq!(
            Error::Server {
                code: -1,
                message: "<APP>err".into()
            }
            .to_string(),
            "iris status -1: <APP>err"
        );
        assert_eq!(
            Error::Protocol("bad frame".into()).to_string(),
            "protocol error: bad frame"
        );
        assert_eq!(Error::Timeout.to_string(), "operation timed out");
        let io = Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(
            matches!(&io, Error::Io(_)) && io.to_string().starts_with("io error: "),
            "io display prefix is contractual: {io}"
        );
    }

    // Probe-pinned framing cases land here (docs/protocol.md cites probes/).

    // ------------------------------------------------------------------
    // Property corpus (hand-rolled xorshift, same pattern as the in-repo
    // fuzz in src/connection/sql.rs — no proptest dev-dep by decision).
    // Ported from the external probe that found the boundaries
    // (2026-10-16: 500k+ rounds, zero defects). Iteration counts are
    // sized so the whole block stays a few seconds in debug; raise
    // IRISNATIVE_PROPSEED to re-run the corpus with a different stream.
    // ------------------------------------------------------------------

    /// xorshift64* — deterministic, seedable from the environment.
    struct Xor(u64);
    impl Xor {
        fn seeded(default: u64) -> Self {
            let s = std::env::var("IRISNATIVE_PROPSEED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default);
            Self(if s == 0 { default } else { s })
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
    }

    #[test]
    fn prop_header_roundtrip() {
        use irisnative::wire::header::Header;
        // 50k random field combinations: encode -> parse must be identity.
        let mut rng = Xor::seeded(0x9E37_79B9_7F4A_7C15);
        for _ in 0..50_000 {
            let h = Header {
                length: rng.next() as u32,
                count: rng.next() as u32,
                statement_id: rng.next() as u32,
                type_word: [rng.byte(), rng.byte()],
            };
            let back = Header::parse(&h.encode());
            assert_eq!(back.length, h.length);
            assert_eq!(back.count, h.count);
            assert_eq!(back.statement_id, h.statement_id);
            assert_eq!(back.type_word, h.type_word);
        }
    }

    #[test]
    fn prop_list_string_roundtrip() {
        use irisnative::wire::list::{Item, ListReader, ListWriter};
        // Random lengths spanning the 2-byte and 4-byte size-prefix
        // boundaries (255, 65536); payloads below the boundary window are
        // capped at 300 chars so the corpus stays cheap, while ~7% of
        // draws land above 65k and exercise the 8-byte prefix form.
        let mut rng = Xor::seeded(0xDEAD_BEEF_CAFE_0001);
        for _ in 0..5_000 {
            let n = (rng.next() % 70_000) as usize;
            let len = if n > 65_000 { n } else { n.min(300) };
            let s: String = (0..len)
                .map(|_| (b'a' + (rng.byte() % 26)) as char)
                .collect();
            let mut w = ListWriter::new();
            w.set_str(&s);
            // frame size must match the declared prefix class exactly
            let total = s.len() + 1;
            let prefix = if total < 0xFF {
                2
            } else if total <= 0xFFFF {
                4
            } else {
                8
            };
            assert_eq!(w.buf.len(), prefix + s.len(), "prefix class {total}");
            let mut r = ListReader::new(&w.buf);
            let got = r.next().unwrap().unwrap();
            match got {
                Item::String(t) => assert_eq!(t, s),
                other => panic!("expected string, got {other:?}"),
            }
            assert!(r.next().is_none(), "one item per frame");
        }
    }

    #[test]
    fn prop_list_int_roundtrip() {
        use irisnative::wire::list::{Item, ListReader, ListWriter};
        // Full-width i64s across every encoded width class.
        let mut rng = Xor::seeded(0x0123_4567_89AB_CDEF);
        for _ in 0..20_000 {
            let v = rng.next() as i64;
            let mut w = ListWriter::new();
            w.set_int(v);
            let got = ListReader::new(&w.buf).next().unwrap().unwrap();
            match got {
                Item::Int(t) => assert_eq!(t, v, "int {v} mistranscoded"),
                other => panic!("int came back {other:?}"),
            }
        }
    }

    #[test]
    fn prop_list_double_roundtrip() {
        use irisnative::wire::list::{Item, ListReader, ListWriter};
        // Random bit patterns (finite ones) in both compact and wide form.
        let mut rng = Xor::seeded(0xFEED_FACE_1234_5678);
        for _ in 0..10_000 {
            let bits = rng.next();
            let f = f64::from_bits(bits);
            if !f.is_finite() {
                continue;
            }
            for compact in [false, true] {
                let mut w = ListWriter::new();
                w.set_f64(f, compact);
                let got = ListReader::new(&w.buf).next().unwrap().unwrap();
                match got {
                    Item::Double(t) => {
                        assert_eq!(t, f, "double bits {bits:#x} compact={compact}")
                    }
                    other => panic!("double came back {other:?}"),
                }
            }
        }
    }

    #[test]
    fn prop_list_reader_survives_garbage() {
        use irisnative::wire::list::ListReader;
        // Random byte streams: every item must be Ok or Err — never panic.
        // Complements the truncated-metadata fuzz in src/connection/sql.rs.
        let mut rng = Xor::seeded(0xABCD_EF01_2345_6789);
        for _ in 0..50_000 {
            let n = (rng.next() % 40) as usize;
            let buf: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            let mut r = ListReader::new(&buf);
            while let Some(item) = r.next() {
                if item.is_err() {
                    break;
                }
            }
        }
    }
}
