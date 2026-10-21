//! Live stream parity suite: transparent SQL stream columns, the stream
//! object family, and the `CALL` no-rowset read-loop guard.
//!
//! Differential oracle: the official Python driver (5.2.1) round-trips
//! CLOB + BLOB columns transparently and the Go driver fetches each stream
//! column with exactly one `JS` frame — both proven by live capture
//! (2026-10-21). These tests pin the same outcomes for this crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use irisnative::{ConnectionOptions, Error, StreamKind, connect};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn options() -> ConnectionOptions {
    // A bounded per-read timeout turns any "server never replied" hang into
    // an Error::Timeout the harness can fail on instead of parking forever.
    ConnectionOptions {
        timeout_ms: Some(30_000),
        ..ConnectionOptions::new(
            env_or("IRISHOST", "127.0.0.1"),
            env_or("IRISNS", "USER"),
            env_or("IRISUSER", "_SYSTEM"),
            env_or("IRISPASSWORD", "SYS"),
        )
        .port(env_or("IRISPORT", "1972").parse().unwrap_or(1972))
    }
}

fn connected() -> irisnative::Connection {
    connect(options()).expect("connect to live IRIS")
}

/// The capture fixture payload: deterministic, 40 000 bytes (above the
/// 32 KB single-frame bound every reference probe used).
fn payload() -> Vec<u8> {
    (1..=40_000u32)
        .map(|i| u8::try_from(32 + ((i * 7) % 95)).expect("in printable 32..126"))
        .collect()
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

fn cleanup(conn: &mut irisnative::Connection) {
    conn.execute("DROP TABLE IF EXISTS SF_ParityStream", &[])
        .expect("drop stream table");
    conn.kill(&irisnative::Global {
        name: "SF.PARITY".to_string(),
        subscripts: Vec::new(),
    })
    .ok();
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn sql_stream_columns_round_trip_transparently() {
    let mut conn = connected();
    conn.execute("DROP TABLE IF EXISTS SF_ParityStream", &[])
        .ok();
    conn.execute(
        "CREATE TABLE SF_ParityStream(ID INT PRIMARY KEY, CH CLOB, BI BLOB)",
        &[],
    )
    .expect("create stream table");

    let p = payload();
    let affected = conn
        .execute(
            "INSERT INTO SF_ParityStream(ID, CH, BI) VALUES(?, ?, ?)",
            &[Some("1".into()), Some(latin1(&p)), Some(latin1(&p))],
        )
        .expect("40 KB inline stream insert");
    assert_eq!(affected, 1);
    conn.execute(
        "INSERT INTO SF_ParityStream(ID, CH, BI) VALUES(?, NULL, NULL)",
        &[Some("2".into())],
    )
    .expect("NULL stream row");

    let mut rs = conn
        .query("SELECT ID, CH, BI FROM SF_ParityStream WHERE ID = 1")
        .expect("select stream columns");
    assert!(rs.fetch_next().expect("fetch row"));
    let text: String = rs.get(1).expect("CLOB value");
    assert_eq!(text.len(), p.len(), "CLOB decoded transparently");
    assert_eq!(text, latin1(&p));
    let blob: Vec<u8> = rs.get(2).expect("BLOB value");
    assert_eq!(blob, p, "BLOB bytes survive the descriptor fetch");

    // NULL stream columns stay NULL — the empty item must NOT be JS-read.
    let mut rs = conn
        .query("SELECT CH, BI FROM SF_ParityStream WHERE ID = 2")
        .expect("select null streams");
    assert!(rs.fetch_next().expect("fetch null row"));
    let s: String = rs.get(0).expect("null CLOB");
    assert!(s.is_empty());

    cleanup(&mut conn);
}

#[test]
#[ignore = "needs a live IRIS"]
fn stream_object_lifecycle() {
    let mut conn = connected();
    for kind in [StreamKind::TempBinary, StreamKind::TempCharacter] {
        let mut stream = conn.create_stream(kind).expect("create stream");
        let p = latin1(&payload());
        stream.write(&p).expect("write 40 KB");
        assert_eq!(stream.size().expect("size"), 40_000, "{kind:?}");
        assert_eq!(stream.read_all().expect("read_all"), payload(), "{kind:?}");
        let head = stream.read(100).expect("read prefix");
        assert_eq!(head, payload()[..100], "sized JS read, {kind:?}");
        // The JS command is position-less (live capture): a second full
        // read still starts at byte 0.
        assert_eq!(stream.read_all().expect("re-read_all").len(), 40_000);
        // The object Read method advances a server-side cursor.
        stream.rewind().expect("rewind");
        let chunk = stream.read_text(10).expect("Read(10)");
        assert_eq!(chunk, p[..10]);
        // Truncate exists only on %Stream.Interface in this server build
        // (dictionary check 2026-10-21): either the content is cut, or the
        // server's 1148 must surface as an honest error — never a hang.
        match stream.truncate(4_096) {
            Ok(()) => assert_eq!(stream.size().expect("size after truncate"), 4_096),
            Err(Error::Server { code, .. }) if code.abs() == 1148 => {}
            Err(other) => panic!("unexpected truncate outcome: {other:?}"),
        }
        stream.close().expect("close frees the object");
    }
    // A corrupt stream handle errors — it must never hang or return data.
    let err = conn.read_stream(b"no-such-handle", -1);
    assert!(
        matches!(err, Err(Error::Server { .. })),
        "bad handle must surface the server status: {err:?}"
    );
}

/// Whether the parity procedure fixture (`ParityCall_Ping`) is registered
/// in this namespace — see probes/paritycall.cls.
fn procedure_fixture_present(conn: &mut irisnative::Connection) -> bool {
    let mut rs = conn
        .query(
            "SELECT ROUTINE_NAME FROM INFORMATION_SCHEMA.ROUTINES \
             WHERE ROUTINE_NAME = 'ParityCall_Ping'",
        )
        .expect("routines probe");
    rs.fetch_next().expect("routines rowset")
}

#[test]
#[ignore = "needs a live IRIS"]
fn call_procedures_answer_fast_on_both_paths() {
    // Live framing rule (captures 2026-10-21): a `CALL` whose statement
    // declares no result columns answers `DQ` with OK metadata followed
    // immediately by a terminal push (SQLCODE -400 = no rowset), and `DU`
    // with a plain status 0 — both fast, never a hang. The documented
    // execution path for procedures is `execute()`. An unknown procedure
    // errors via its metadata SQLCODE. (The reference harnesses'
    // "hang on LAST_IDENTITY" stories live in their own read loops; a
    // SELECT of the identity stays in sync through this driver — checked
    // below.)
    let mut conn = connected();
    match conn.query("CALL %Atelier_v1_Utils.Extension_GetStatus()") {
        Ok(mut rs) => {
            assert!(!rs.fetch_next().expect("rowset-backed CALL, no hang"));
        }
        Err(Error::Server { code: 400, .. }) => {}
        Err(other) => panic!("unexpected CALL verdict: {other:?}"),
    }
    conn.execute("CALL %Atelier_v1_Utils.Extension_GetStatus()", &[])
        .expect("CALL via execute must succeed");
    // a real SqlProc procedure (fixture: probes/paritycall.cls, loaded with
    // `do $system.OBJ.Load("paritycall.cls","ck")`): `CALL` answers via the
    // no-rowset verdict, and the function form returns its value. The
    // `User` schema is a SQL reserved word, hence the SQLUser qualifier.
    if procedure_fixture_present(&mut conn) {
        match conn.query("CALL SQLUser.ParityCall_Ping()") {
            Ok(mut rs) => {
                assert!(!rs.fetch_next().expect("rowset-backed CALL, no hang"));
            }
            Err(Error::Server { code: 400, .. }) => {}
            Err(other) => panic!("unexpected CALL verdict: {other:?}"),
        }
        conn.execute("CALL SQLUser.ParityCall_Ping()", &[])
            .expect("procedure CALL via execute");
        let mut rs = conn
            .query("SELECT SQLUser.ParityCall_Echo('hi')")
            .expect("scalar procedure call");
        assert!(rs.fetch_next().expect("echo row"));
        assert_eq!(rs.get::<String>(0).expect("echo value"), "hi");
    } else {
        eprintln!("skipping procedure assertions: load probes/paritycall.cls first");
    }
    match conn.query("CALL User.ParityNoSuchProcedure()") {
        Err(Error::Server { .. }) => {}
        other => panic!("unknown CALL must error server-side, got {other:?}"),
    }
    // identity query stays in sync (PULL item on the reference loops)
    let mut rs = conn
        .query("SELECT LAST_IDENTITY()")
        .expect("LAST_IDENTITY must answer");
    assert!(rs.fetch_next().expect("identity row, no hang"));

    // the connection stayed usable (no desync from the CALL paths)
    let mut rs = conn
        .query("SELECT 1")
        .expect("connection usable after CALL");
    assert!(rs.fetch_next().expect("keepalive row"));
}

#[cfg(feature = "async")]
#[tokio::test]
#[ignore = "needs a live IRIS"]
async fn async_stream_columns_round_trip_transparently() {
    let mut conn = irisnative::r#async::Connection::connect(options())
        .await
        .expect("async connect");
    // distinct table name: the sync suite runs concurrently in-process
    conn.execute("DROP TABLE IF EXISTS SF_ParityStreamA", &[])
        .await
        .ok();
    conn.execute(
        "CREATE TABLE SF_ParityStreamA(ID INT PRIMARY KEY, CH CLOB, BI BLOB)",
        &[],
    )
    .await
    .expect("create");
    let p = payload();
    conn.execute(
        "INSERT INTO SF_ParityStreamA(ID, CH, BI) VALUES(?, ?, ?)",
        &[Some("1".into()), Some(latin1(&p)), Some(latin1(&p))],
    )
    .await
    .expect("async 40 KB insert");

    let mut st = conn
        .query("SELECT ID, CH, BI FROM SF_ParityStreamA", &[])
        .await
        .expect("async select");
    let row = conn.next_row(&mut st).await.expect("row").expect("present");
    assert_eq!(row[1].as_text(), latin1(&p), "async CLOB decode");
    let bytes =
        <Vec<u8> as irisnative::value::Value>::decode(&row[2].as_text()).expect("async BLOB bytes");
    assert_eq!(bytes, p, "async BLOB decode");

    let mut stream = conn
        .create_stream(StreamKind::TempBinary)
        .await
        .expect("async temp stream");
    stream.write(&latin1(&p)).await.expect("async write");
    assert_eq!(stream.size().await.expect("async size"), 40_000);
    assert_eq!(stream.read_all().await.expect("async read_all"), p);
    stream.close().await.expect("async close");

    conn.execute("DROP TABLE SF_ParityStreamA", &[])
        .await
        .expect("async drop");
}
