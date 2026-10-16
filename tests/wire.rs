//! Byte-level wire codec tests. Every case cites the live probe that
//! established the expectation; until probes run, the module pins only the
//! pure-logic surface (subscript encoding, option parsing).

#[allow(clippy::unwrap_used, clippy::expect_used)]
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
}
