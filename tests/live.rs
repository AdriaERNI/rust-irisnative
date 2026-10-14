//! Live-server smoke tests — `#[ignore]`d, run only against a reachable IRIS.
//!
//! Bring a server up (`docker compose up -d iris`) and run:
//! `cargo test --features async -- --ignored --test-threads=1`.
//! Env vars (defaults match the compose dev container): `IRISHOST`
//! (127.0.0.1), `IRISPORT` (1972), `IRISUSER` (_SYSTEM), `IRISPASSWORD`
//! (SYS), `IRISNS` (USER).
//!
//! Scope of honesty: the wire layer is not implemented, so this suite
//! asserts ONLY (a) the superserver port accepts a TCP connection and
//! (b) `connect()` fails gracefully (no panic). It asserts NO protocol
//! behavior. When the handshake lands, upgrade the assertions to
//! `server_version()`/`query()` and the compatibility matrix in
//! `docs/protocol.md` gets its first real cells.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use irisnative::{ConnectionOptions, Error, connect};
use std::io::Read;
use std::net::TcpStream;
use std::time::Duration;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn options() -> ConnectionOptions {
    ConnectionOptions::new(
        env_or("IRISHOST", "127.0.0.1"),
        env_or("IRISNS", "USER"),
        env_or("IRISUSER", "_SYSTEM"),
        env_or("IRISPASSWORD", "SYS"),
    )
    .port(env_or("IRISPORT", "1972").parse().unwrap_or(1972))
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn superserver_port_is_reachable() {
    let opts = options();
    let stream = TcpStream::connect((opts.host.as_str(), opts.port));
    assert!(
        stream.is_ok(),
        "no TCP listener on {}:{} — is `docker compose up -d iris` healthy?",
        opts.host,
        opts.port
    );
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn stub_connect_fails_gracefully() {
    let opts = options();
    let host = opts.host.clone();
    let port = opts.port;
    match connect(opts) {
        Err(Error::NotImplemented) => {}
        Err(other) => panic!(
            "pre-protocol connect() to {host}:{port} must report NotImplemented, got {other:?}"
        ),
        Ok(_) => panic!(
            "connect() unexpectedly succeeded — the wire layer landed; \
             rewrite tests/live.rs to assert server_version()/query() instead"
        ),
    }
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn handshake_window_stays_quiet_without_a_request() {
    // Observation only, no invented protocol: IRIS is request-driven on
    // %Service_CP, so a fresh connection should not push unsolicited bytes.
    // If this ever sees data, that is a probe finding for documentation/ —
    // not something to silently assert away.
    let opts = options();
    let mut stream = TcpStream::connect((opts.host.as_str(), opts.port))
        .expect("TCP connect failed — is the IRIS container healthy?");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set_read_timeout");
    let mut buf = [0u8; 32];
    match stream.read(&mut buf) {
        Ok(0) | Err(_) => {}
        Ok(n) => {
            println!(
                "note: server sent {n} unsolicited greeting bytes: {:02x?}",
                &buf[..n]
            );
        }
    }
}
