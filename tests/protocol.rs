//! Live protocol suite — real round-trips against a reachable IRIS.
//!
//! Run against the dev container:
//! `IRISPORT=19720 cargo test --features async -- --include-ignored`.
//! Defaults match the compose dev server (`docker compose up -d iris`):
//! `IRISHOST`=127.0.0.1, `IRISPORT`=1972, `IRISUSER`=_SYSTEM,
//! `IRISPASSWORD`=SYS, `IRISNS`=USER. On shared hosts the container may
//! map a different host port (our dev box uses 19720).
//!
//! Parity layer:
//! `parity_matches_reference_scenario` runs the SAME end-to-end scenario
//! twice — once through this crate, once through `probes/parity_scenario.py`
//! (the stdlib re-implementation of go-irisnative's wire rules; the pure-
//! Python wheel in intersystems-community/iris-driver-distribution
//! speaks protocol 65 and is denied
//! by 2025.3 — the version trap, probed 2026-10-15) — and asserts equal
//! outcomes value-for-value, SQLCODE-for-SQLCODE.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::items_after_statements,
    clippy::many_single_char_names,
    clippy::map_unwrap_or,
    clippy::redundant_closure,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use irisnative::{ConnectionOptions, Error, connect, global};
use std::process::Command;

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

fn connected() -> irisnative::Connection {
    connect(options()).expect("connect to live IRIS")
}

/// Minimal JSON subset reader (objects/arrays/strings/ints/bools) — the
/// same no-dependency approach as tests/golden.rs.
mod json {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Val {
        Null,
        Bool(bool),
        Int(i64),
        Str(String),
        Arr(Vec<Val>),
        Obj(Vec<(String, Val)>),
    }

    impl Val {
        pub fn get(&self, key: &str) -> Option<&Val> {
            match self {
                Val::Obj(kvs) => kvs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
    }

    pub fn parse(src: &str) -> Val {
        let b: Vec<u8> = src.bytes().collect();
        let mut p = 0usize;
        skip(&b, &mut p);
        value(&b, &mut p)
    }

    fn skip(b: &[u8], p: &mut usize) {
        while *p < b.len() && (b[*p] as char).is_whitespace() {
            *p += 1;
        }
    }

    fn value(b: &[u8], p: &mut usize) -> Val {
        skip(b, p);
        match b[*p] {
            b'{' => obj(b, p),
            b'[' => arr(b, p),
            b'"' => Val::Str(string(b, p)),
            b't' | b'f' => {
                let lit = if b[*p] == b't' { "true" } else { "false" };
                assert!(b[*p..].starts_with(lit.as_bytes()));
                *p += lit.len();
                Val::Bool(lit == "true")
            }
            b'n' => {
                assert!(b[*p..].starts_with(b"null"));
                *p += 4;
                Val::Null
            }
            _ => number(b, p),
        }
    }

    fn string(b: &[u8], p: &mut usize) -> String {
        assert_eq!(b[*p], b'"');
        *p += 1;
        let mut out = String::new();
        while b[*p] != b'"' {
            if b[*p] == b'\\' {
                *p += 1;
                out.push(match b[*p] {
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'"' => '"',
                    b'\\' => '\\',
                    b'u' => {
                        let hex = String::from_utf8_lossy(&b[*p + 1..*p + 5]).into_owned();
                        *p += 4;
                        char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap_or('\u{fffd}')
                    }
                    c => c as char,
                });
            } else {
                out.push(b[*p] as char);
            }
            *p += 1;
        }
        *p += 1;
        out
    }

    fn number(b: &[u8], p: &mut usize) -> Val {
        let start = *p;
        while *p < b.len()
            && (b[*p].is_ascii_digit() || matches!(b[*p], b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            *p += 1;
        }
        let s = std::str::from_utf8(&b[start..*p]).unwrap();
        s.parse::<i64>()
            .map(Val::Int)
            .unwrap_or_else(|_| Val::Str(s.to_string()))
    }

    fn arr(b: &[u8], p: &mut usize) -> Val {
        assert_eq!(b[*p], b'[');
        *p += 1;
        let mut items = Vec::new();
        skip(b, p);
        if b[*p] == b']' {
            *p += 1;
            return Val::Arr(items);
        }
        loop {
            items.push(value(b, p));
            skip(b, p);
            match b[*p] {
                b',' => {
                    *p += 1;
                }
                b']' => {
                    *p += 1;
                    return Val::Arr(items);
                }
                c => panic!("bad array byte {c}"),
            }
        }
    }

    fn obj(b: &[u8], p: &mut usize) -> Val {
        assert_eq!(b[*p], b'{');
        *p += 1;
        let mut kvs = Vec::new();
        skip(b, p);
        if b[*p] == b'}' {
            *p += 1;
            return Val::Obj(kvs);
        }
        loop {
            skip(b, p);
            let k = string(b, p);
            skip(b, p);
            assert_eq!(b[*p], b':');
            *p += 1;
            let v = value(b, p);
            kvs.push((k, v));
            skip(b, p);
            match b[*p] {
                b',' => {
                    *p += 1;
                }
                b'}' => {
                    *p += 1;
                    return Val::Obj(kvs);
                }
                c => panic!("bad object byte {c}"),
            }
        }
    }
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn connect_reports_banner_and_negotiated_facts() {
    let mut conn = connected();
    let banner = conn.server_version().expect("server_version");
    assert!(
        banner.contains("IRIS"),
        "banner looks like a version: {banner}"
    );
    let info = conn.info();
    assert_eq!(info.protocol, 69, "go negotiates PROTOCOL 69");
    assert!(!info.info.is_empty(), "CN reply carries an info banner");
    assert!(
        !info.server_job.is_empty(),
        "CN reply carries the job number"
    );
    assert!(info.durable_transactions(), "we requested DurableTx bit 4");
}

#[test]
#[ignore = "needs a live IRIS"]
fn bad_password_maps_to_auth_error() {
    // CN status 417 -> Error::Auth (driver.rs; refclient probe 2026-10-15
    // shows the same 417 "Access Denied" from the live server).
    let opts = ConnectionOptions::new(
        env_or("IRISHOST", "127.0.0.1"),
        env_or("IRISNS", "USER"),
        env_or("IRISUSER", "_SYSTEM"),
        "definitely-not-the-password",
    )
    .port(env_or("IRISPORT", "1972").parse().unwrap_or(1972));
    match connect(opts) {
        Err(Error::Auth) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[test]
#[ignore = "needs a live IRIS"]
fn globals_roundtrip_walk_and_definedness() {
    let mut conn = connected();
    conn.kill(&global!(SFPG)).expect("kill root");
    for i in 1..=3 {
        conn.set(&global!(SFPG(i)), &format!("v{i}")).expect("set");
    }
    for i in 1..=3 {
        let v: String = conn.get(&global!(SFPG(i))).expect("get");
        assert_eq!(v, format!("v{i}"));
    }
    assert!(conn.is_defined(&global!(SFPG(2))).expect("defined"));
    assert!(
        conn.is_defined(&global!(SFPG)).expect("subtree defined"),
        "root is defined by its subtree"
    );

    // order walk, cursor style (go GlobalNext semantics)
    let mut node = global!(SFPG(0));
    let mut seen = Vec::new();
    while let Some(sub) = conn.next(&mut node).expect("next") {
        assert_eq!(
            sub.as_str(),
            node.subscripts.last().expect("cursor updated").as_str()
        );
        seen.push(sub);
    }
    assert_eq!(seen, vec!["1".to_string(), "2".into(), "3".into()]);

    conn.kill(&global!(SFPG)).expect("kill subtree");
    assert!(!conn.is_defined(&global!(SFPG(1))).expect("gone"));
}

#[test]
#[ignore = "needs a live IRIS"]
fn undefined_global_is_server_error() {
    // <UNDEFINED> surfaces as Error::Server with code -1009
    // (1009 is $ZOS status 11.9.1 -> the port normalizes the sign).
    let mut conn = connected();
    conn.kill(&global!(SFPU(1))).ok();
    match conn.get::<String>(&global!(SFPU(1))) {
        Err(Error::Server { code, message }) => {
            assert_eq!(code.abs(), 1009, "code {code}");
            assert!(message.contains("UNDEFINED"), "message {message:?}");
        }
        other => panic!("expected Server UNDEFINED, got {other:?}"),
    }
}

#[test]
#[ignore = "needs a live IRIS"]
fn sql_lifecycle_with_parameters_and_errors() {
    let mut conn = connected();
    conn.execute("DROP TABLE IF EXISTS irisnative_live_sql", &[])
        .expect("drop-if-exists");
    conn.execute(
        "CREATE TABLE irisnative_live_sql (id INT PRIMARY KEY, name VARCHAR(50))",
        &[],
    )
    .expect("create");

    let n = conn
        .execute(
            "INSERT INTO irisnative_live_sql (id, name) VALUES (1, NULL)",
            &[],
        )
        .expect("insert null literal");
    assert_eq!(n, 1);
    let n = conn
        .execute(
            "INSERT INTO irisnative_live_sql (id, name) VALUES (?, ?)",
            &[Some("2".into()), Some("bob".into())],
        )
        .expect("insert bound");
    assert_eq!(n, 1);
    let n = conn
        .execute(
            "INSERT INTO irisnative_live_sql (id, name) VALUES (?, ?)",
            &[Some("3".into()), None],
        )
        .expect("insert bound NULL");
    assert_eq!(n, 1);

    let mut rs = conn
        .query("SELECT id, name FROM irisnative_live_sql ORDER BY id")
        .expect("select");
    assert_eq!(rs.column_count(), 2);
    assert_eq!(rs.column_name(1), Some("name"));
    let mut rows = Vec::new();
    while rs.fetch_next().expect("fetch") {
        let id: i64 = rs.get(0).expect("id");
        let name: irisnative::value::Nullable<String> = rs.get(1).expect("name");
        rows.push((id, name.0));
    }
    assert_eq!(
        rows,
        vec![(1, None), (2, Some("bob".into())), (3, None)],
        "SQL NULL decodes as None (py empty-ASCII convention)"
    );

    match conn.query("SELECT * FROM irisnative_no_such_table") {
        Err(Error::Server { code, message }) => {
            assert_eq!(code.abs(), 30, "SQLCODE -30 table not found: {code}");
            assert!(
                message.contains("not found"),
                "OE text present: {message:?}"
            );
        }
        other => panic!("expected Server SQLCODE 30, got {other:?}"),
    }

    conn.execute("DROP TABLE irisnative_live_sql", &[])
        .expect("drop");
}

#[test]
#[ignore = "needs a live IRIS"]
fn transactions_commit_and_rollback() {
    let mut conn = connected();
    conn.execute("DROP TABLE IF EXISTS irisnative_live_tx", &[])
        .expect("drop");
    conn.execute("CREATE TABLE irisnative_live_tx (id INT PRIMARY KEY)", &[])
        .expect("create");

    conn.begin().expect("begin");
    assert!(conn.in_transaction());
    conn.execute("INSERT INTO irisnative_live_tx (id) VALUES (10)", &[])
        .expect("insert in tx");
    conn.commit().expect("commit");
    assert!(!conn.in_transaction());

    conn.begin().expect("begin 2");
    conn.execute("INSERT INTO irisnative_live_tx (id) VALUES (11)", &[])
        .expect("insert in tx 2");
    assert!(conn.in_transaction());
    conn.rollback().expect("rollback");
    assert!(!conn.in_transaction());

    let mut rs = conn
        .query("SELECT id FROM irisnative_live_tx ORDER BY id")
        .expect("select");
    let mut ids = Vec::new();
    while rs.fetch_next().expect("fetch") {
        ids.push(rs.get::<i64>(0).expect("id"));
    }
    assert_eq!(ids, vec![10], "commit persists, rollback erases");

    // nested begin refused (go mod.go:BeginTx parity)
    conn.begin().expect("begin again");
    match conn.begin() {
        Err(Error::Protocol(msg)) => assert!(msg.contains("already open"), "{msg}"),
        other => panic!("expected Protocol nesting error, got {other:?}"),
    }
    conn.commit().expect("commit after nesting check");
    conn.execute("DROP TABLE irisnative_live_tx", &[])
        .expect("drop");
}

#[test]
#[ignore = "needs a live IRIS"]
fn class_method_invocation_round_trips() {
    // objects acceptance (go classes.go CLASSMETHOD_VALUE).
    let mut conn = connected();
    let v = conn
        .class_method("%SYSTEM.Version", "GetVersion", &[])
        .expect("GetVersion");
    assert!(v.contains("IRIS"), "{v}");
    // Probe 2026-10-15 on the dev Community 2025.3: `%SYSTEM.Info` is not
    // installed there (server answers SQLCODE 1150 "<CLASS DOES NOT
    // EXIST>"), so the second call targets a method the edition ships.
    let build = conn
        .class_method("%SYSTEM.Version", "GetBuildNumber", &[])
        .expect("GetBuildNumber");
    assert!(
        build.chars().all(|c| c.is_ascii_digit()) && !build.is_empty(),
        "numeric build number: {build}"
    );
}

#[test]
#[ignore = "needs a live IRIS"]
fn oref_method_and_property_round_trip() {
    // OREF instance methods + property get/set
    // (go classes.go METHOD_VALUE / PROPERTY, py _IRISObject.py). The
    // `%Net.HttpRequest` property accessors are generated instance
    // methods, so AcceptGzipGet proves the OREF really dispatched; the
    // Location set/get proves PROPERTY_VALUE round-trips the same object.
    let mut conn = connected();
    let oref = conn
        .class_method("%Net.HttpRequest", "%New", &[])
        .expect("%New returns an OREF");
    assert!(oref.contains('@'), "OREF shape {oref}");
    let v = conn
        .method(&oref, "AcceptGzipGet", &[])
        .expect("instance method on OREF");
    assert_ne!(v, "");
    conn.property_set(&oref, "Location", "/probe")
        .expect("property set");
    assert_eq!(
        conn.property_get(&oref, "Location").expect("property get"),
        "/probe"
    );
    // a private method must surface the server's <PRIVATE METHOD> error,
    // not a silent success (error-path parity, go XDCall semantics).
    assert!(
        matches!(
            conn.method(&oref, "%OnClose", &[]),
            Err(Error::Server { code, .. }) if code.abs() == 1197
        ),
        "private method rejected by the server"
    );
}

#[test]
#[ignore = "needs a live IRIS"]
fn dropping_the_connection_disconnects_the_server_job() {
    // Hygiene: Drop sends DC; the server job must go away. Verified
    // through %SYS.ProcessQuery on a second connection (the JobNumber field
    // of the USER job is its Pid — probe 2026-10-15).
    let mut watcher = connect(
        ConnectionOptions::new(
            env_or("IRISHOST", "127.0.0.1"),
            "%SYS",
            env_or("IRISUSER", "_SYSTEM"),
            env_or("IRISPASSWORD", "SYS"),
        )
        .port(env_or("IRISPORT", "1972").parse().unwrap_or(1972)),
    )
    .expect("watcher connects to %SYS");

    let job = {
        let conn = connected();
        let job: i64 = conn
            .info()
            .server_job
            .parse()
            .expect("job number is numeric");
        let alive = |w: &mut irisnative::Connection| -> bool {
            let mut rs = w
                .query(&format!(
                    "SELECT Pid FROM %SYS.ProcessQuery WHERE Pid = {job}"
                ))
                .expect("process query");
            rs.fetch_next().expect("fetch")
        };
        assert!(alive(&mut watcher), "job {job} alive while held");
        drop(conn); // sends DC
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while alive(&mut watcher) {
            assert!(
                std::time::Instant::now() < deadline,
                "job {job} survived DC — Drop did not disconnect"
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        job
    };
    println!("job {job} terminated after Drop sent DC");
}

#[test]
#[ignore = "needs a live IRIS"]
fn large_global_value_round_trips() {
    // Payloads past 65 534 bytes take the %List 4-byte size prefix; the
    // server rejects a mis-framed one with status 1139 <LIST> (probe
    // 2026-10-15), so a green round-trip pins the wire format end-to-end.
    let mut conn = connected();
    let big = "Z".repeat(70_000);
    conn.set(&global!(SFBIG(1)), &big).expect("set 70 KiB");
    let v: String = conn.get(&global!(SFBIG(1))).expect("get 70 KiB");
    assert_eq!(v.len(), big.len());
    assert_eq!(v, big);
    conn.kill(&global!(SFBIG)).expect("kill subtree");
    assert!(!conn.is_defined(&global!(SFBIG(1))).expect("gone"));
}

#[test]
#[ignore = "needs a live IRIS"]
fn sql_error_code_is_the_wire_magnitude() {
    // SQL-family `Error::Server.code` is the magnitude the wire reports
    // (IRIS's negative SQLCODE travels unsigned; the OE text names the
    // condition). Pinned here so the documented shape cannot drift.
    let mut conn = connected();
    match conn.query("SELECT * FROM irisnative.sign_check") {
        Err(Error::Server { code, message }) => {
            assert_eq!(code, 30, "table-not-found magnitude, not signed -30");
            assert!(message.contains("not found"), "OE text: {message:?}");
        }
        other => panic!("expected Server 30, got {other:?}"),
    }
}

/// Collect the scenario through THIS crate (mirrors parity_scenario.py).
fn rust_scenario() -> json::Val {
    use json::Val;
    let mut conn = connected();
    let mut out: Vec<(String, Val)> = Vec::new();

    // globals: set 1..3, order walk from subscript "1"
    conn.kill(&global!(SFPARITY)).ok();
    for i in 1..=3 {
        conn.set(&global!(SFPARITY(i)), &format!("v{i}"))
            .expect("set");
    }
    let mut walk = Vec::new();
    let mut node = global!(SFPARITY(1));
    while let Some(sub) = conn.next(&mut node).expect("next") {
        walk.push(Val::Str(sub));
    }
    // python walks from ind "1" the same way; cursor start node is ("1")
    out.push(("global_walk".into(), Val::Arr(walk)));
    let undef_is_none = matches!(
        conn.get::<String>(&global!(SFPARITY(999))),
        Err(Error::Server { code, .. }) if code.abs() == 1009
    );
    out.push(("undefined".into(), Val::Bool(undef_is_none)));
    conn.kill(&global!(SFPARITY)).ok();

    conn.execute(&format!("DROP TABLE IF EXISTS {PARITY_TABLE}"), &[])
        .expect("drop-if");
    conn.execute(
        &format!("CREATE TABLE {PARITY_TABLE} (id INT PRIMARY KEY, name VARCHAR(50))"),
        &[],
    )
    .expect("create");
    let i1 = conn
        .execute(
            &format!("INSERT INTO {PARITY_TABLE} (id, name) VALUES (1, NULL)"),
            &[],
        )
        .expect("insert 1");
    out.push(("insert_1".into(), Val::Int(i1)));
    let i2 = conn
        .execute(
            &format!("INSERT INTO {PARITY_TABLE} (id, name) VALUES (?, ?)"),
            &[Some("2".into()), Some("bob".into())],
        )
        .expect("insert 2");
    out.push(("insert_2".into(), Val::Int(i2)));

    let mut rows: Vec<Val> = Vec::new();
    let mut rs = conn
        .query(&format!("SELECT id, name FROM {PARITY_TABLE} ORDER BY id"))
        .expect("select");
    while rs.fetch_next().expect("fetch") {
        let id: i64 = rs.get(0).expect("id");
        let name: irisnative::value::Nullable<String> = rs.get(1).expect("name");
        rows.push(Val::Arr(vec![
            Val::Str(id.to_string()),
            Val::Str(name.0.unwrap_or_default()),
        ]));
    }
    out.push(("select_rows".into(), Val::Arr(rows)));

    let mut rows: Vec<Val> = Vec::new();
    let mut rs = conn
        .query_params(
            &format!("SELECT id, name FROM {PARITY_TABLE} WHERE id = ?"),
            &[Some("2".into())],
        )
        .expect("param select");
    while rs.fetch_next().expect("fetch") {
        rows.push(Val::Arr(vec![
            Val::Str(rs.get::<i64>(0).expect("id").to_string()),
            Val::Str(rs.get::<String>(1).expect("name")),
        ]));
    }
    out.push(("param_row".into(), Val::Arr(rows)));

    let bad = match conn.query("SELECT * FROM irisnative_missing_table") {
        Err(Error::Server { code, .. }) => Some(code.abs()),
        _ => None,
    };
    out.push((
        "bad_sql_code".into(),
        bad.map_or(Val::Null, |c| Val::Int(c)),
    ));

    // transactions: commit persists (id 10), rollback erases (id 11)
    conn.begin().expect("begin");
    conn.execute(
        &format!("INSERT INTO {PARITY_TABLE} (id, name) VALUES (10, ?)"),
        &[Some("committed".into())],
    )
    .expect("tx insert");
    conn.commit().expect("commit");
    let mut rs = conn
        .query(&format!("SELECT name FROM {PARITY_TABLE} WHERE id = 10"))
        .expect("select 10");
    let mut found = false;
    while rs.fetch_next().expect("fetch") {
        found = true;
    }
    out.push(("commit_visible".into(), Val::Bool(found)));

    conn.begin().expect("begin 2");
    conn.execute(
        &format!("INSERT INTO {PARITY_TABLE} (id, name) VALUES (11, ?)"),
        &[Some("rolled".into())],
    )
    .expect("tx insert 2");
    conn.rollback().expect("rollback");
    let mut rs = conn
        .query(&format!("SELECT name FROM {PARITY_TABLE} WHERE id = 11"))
        .expect("select 11");
    let mut found = false;
    while rs.fetch_next().expect("fetch") {
        found = true;
    }
    out.push(("rollback_visible".into(), Val::Bool(found)));

    conn.execute(&format!("DROP TABLE {PARITY_TABLE}"), &[])
        .expect("drop");

    let version = conn
        .class_method("%SYSTEM.Version", "GetVersion", &[])
        .expect("GetVersion");
    out.push((
        "version_prefix".into(),
        Val::Str(version.chars().take(14).collect()),
    ));

    // job is informational only for this comparison; parity fields above
    // are the contract.
    out.push((
        "job".into(),
        Val::Int(conn.info().server_job.parse().unwrap_or_default()),
    ));
    Val::Obj(out)
}

const PARITY_TABLE: &str = "irisnative_parity_p";

#[test]
#[ignore = "needs a live IRIS"]
fn parity_matches_reference_scenario() {
    // The go/py comparison requirement: identical wire semantics must
    // produce identical observable outcomes. Python side runs
    // probes/parity_scenario.py (Go wire rules, protocol 69 — see module
    // docs for why the py wheel itself is not runnable against 2025.3).
    let py = env_or("PYTHON", "python3");
    let script = [
        "probes/parity_scenario.py".to_string(),
        format!("{}/probes/parity_scenario.py", env!("CARGO_MANIFEST_DIR")),
    ];
    let mut output = None;
    for candidate in &script {
        let ok = std::path::Path::new(candidate).exists();
        if !ok {
            continue;
        }
        let out = Command::new(py)
            .arg(candidate)
            .env("IRISHOST", env_or("IRISHOST", "127.0.0.1"))
            .env("IRISPORT", env_or("IRISPORT", "1972"))
            .env("IRISUSER", env_or("IRISUSER", "_SYSTEM"))
            .env("IRISPASSWORD", env_or("IRISPASSWORD", "SYS"))
            .env("IRISNS", env_or("IRISNS", "USER"))
            .output()
            .expect("run parity scenario through the reference-semantics client");
        assert!(
            out.status.success(),
            "reference scenario failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        output = Some(out.stdout);
        break;
    }
    let reference = json::parse(
        std::str::from_utf8(&output.expect("found probes/parity_scenario.py")).expect("utf8"),
    );
    let ours = rust_scenario();

    // Compare every field except job (server-assigned, differs per run).
    use json::Val;
    let Val::Obj(ours_kv) = &ours else {
        panic!("scenario builds an object")
    };
    let mut mismatches = Vec::new();
    for (k, ours_v) in ours_kv {
        if k == "job" {
            continue;
        }
        match reference.get(k) {
            Some(ref_v) if values_agree(ref_v, ours_v) => {}
            Some(ref_v) => mismatches.push(format!("{k}: reference={ref_v:?} ours={ours_v:?}")),
            None => mismatches.push(format!("{k}: missing in reference")),
        }
    }
    assert!(
        mismatches.is_empty(),
        "parity drift vs the reference-semantics client:\n{}",
        mismatches.join("\n")
    );
}

/// Flatten a scalar to its comparable text form: `null` and `""` are the
/// same value at this boundary (py's `_as_text` renders undefined/None as
/// the empty string; the Rust side uses Option), and captured wire values
/// may carry trailing padding/newlines the other side trims.
fn norm(v: &json::Val) -> Option<String> {
    use json::Val;
    match v {
        Val::Null => Some(String::new()),
        Val::Str(s) => Some(s.trim().to_string()),
        Val::Bool(x) => Some(x.to_string()),
        Val::Int(i) => Some(i.to_string()),
        Val::Arr(items) => {
            let parts: Vec<String> = items.iter().map(|i| norm(i).unwrap_or_default()).collect();
            Some(parts.join(","))
        }
        Val::Obj(_) => None,
    }
}

fn values_agree(a: &json::Val, b: &json::Val) -> bool {
    use json::Val;
    match (a, b) {
        (Val::Arr(x), Val::Arr(y)) if x.len() == y.len() => {
            // nested arrays (rows of rows) need recursive agreement
            let nested = x.iter().all(|i| matches!(i, Val::Arr(_)));
            if nested {
                return x.len() == y.len() && x.iter().zip(y).all(|(i, j)| values_agree(i, j));
            }
            norm(a) == norm(b)
        }
        _ => norm(a) == norm(b),
    }
}

#[cfg(feature = "async")]
#[tokio::test]
#[ignore = "needs a live IRIS"]
async fn async_close_disconnects_the_server_job() {
    // close() is the authoritative async disconnect: the server job must
    // disappear the same way the sync Drop test proves for DC.
    let mut conn = irisnative::r#async::Connection::connect(options())
        .await
        .expect("async connect");
    let job: i64 = conn
        .info()
        .server_job
        .parse()
        .expect("job number is numeric");
    conn.close().await.expect("async close sends DC");
    drop(conn);
    let mut watcher = connect(
        ConnectionOptions::new(
            env_or("IRISHOST", "127.0.0.1"),
            "%SYS",
            env_or("IRISUSER", "_SYSTEM"),
            env_or("IRISPASSWORD", "SYS"),
        )
        .port(env_or("IRISPORT", "1972").parse().unwrap_or(1972)),
    )
    .expect("watcher connects to %SYS");
    let alive = |w: &mut irisnative::Connection| -> bool {
        let mut rs = w
            .query(&format!(
                "SELECT Pid FROM %SYS.ProcessQuery WHERE Pid = {job}"
            ))
            .expect("process query");
        rs.fetch_next().expect("fetch")
    };
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    while alive(&mut watcher) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "job {job} survived close() — async DC did not disconnect"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

#[cfg(feature = "async")]
#[tokio::test]
#[ignore = "needs a live IRIS"]
async fn async_connection_drives_the_same_wire() {
    // the async variant shares codecs and builders; behavior
    // must match the sync path on the same server.
    let mut conn = irisnative::r#async::Connection::connect(options())
        .await
        .expect("async connect");
    let banner = conn.server_version().await.expect("async server_version");
    assert!(banner.contains("IRIS"));
    conn.global_set("SFPA", &[7.to_string()], "seven")
        .await
        .expect("async set");
    let v = conn
        .global_get("SFPA", &[7.to_string()])
        .await
        .expect("get");
    assert_eq!(v.as_deref(), Some("seven"));
    conn.global_kill("SFPA", &[7.to_string()])
        .await
        .expect("kill");
    let st = conn
        .query("SELECT 40 + 2 AS N", &[])
        .await
        .expect("async query");
    assert_eq!(st.columns.len(), 1);
    let mut st = st;
    let row = conn
        .next_row(&mut st)
        .await
        .expect("async row")
        .expect("one row");
    assert_eq!(row[0].as_text(), "42");

    // Full-surface parity with the sync live tests: definedness, the
    // order walk (both directions), OREF method + property, and the
    // void class-method frame all have async twins through the shared
    // builders/decoders.
    conn.global_kill("SFPA", &[]).await.expect("kill root");
    for i in 1..=3 {
        conn.global_set("SFPA", &[i.to_string()], &format!("v{i}"))
            .await
            .expect("async set");
    }
    assert!(
        conn.is_defined("SFPA", &["2".into()])
            .await
            .expect("defined")
    );
    assert!(
        conn.is_defined("SFPA", &[]).await.expect("subtree"),
        "root is defined by its subtree"
    );
    let mut walk = Vec::new();
    let mut ind = String::new();
    while let Some(sub) = conn
        .global_next("SFPA", &[], &ind)
        .await
        .expect("async next")
    {
        walk.push(sub.clone());
        ind = sub;
    }
    assert_eq!(walk, ["1", "2", "3"]);
    assert_eq!(
        conn.global_prev("SFPA", &[], "2")
            .await
            .expect("async prev"),
        Some("1".into())
    );
    let oref = conn
        .class_method("%Net.HttpRequest", "%New", &[])
        .await
        .expect("async %New");
    assert!(oref.contains('@'), "OREF shape {oref}");
    assert_ne!(
        conn.method(&oref, "AcceptGzipGet", &[])
            .await
            .expect("async OREF method"),
        ""
    );
    conn.property_set(&oref, "Location", "/probe")
        .await
        .expect("async property set");
    assert_eq!(
        conn.property_get(&oref, "Location")
            .await
            .expect("async property get"),
        "/probe"
    );
    conn.class_method_void("%SYSTEM.Version", "GetVersion", &[])
        .await
        .expect("async class_method_void");
    conn.global_kill("SFPA", &[]).await.expect("kill subtree");
    assert!(!conn.is_defined("SFPA", &["1".into()]).await.expect("gone"));
    conn.close().await.expect("async close sends DC");
}
