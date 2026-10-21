//! Live global-lock, increment, and nested-transaction parity suite.
//!
//! Differential reference: the official Python driver exhibits exactly
//! these semantics on this server (live capture 2026-10-21): a legacy
//! `LOCK` holder blocks another driver's timed lock until it answers
//! `<TIMEOUT>` (sysio 1240) and a release unblocks the waiter; `INCREMENT`
//! returns the new value atomically; `tStart`/`tCommit`/`tRollbackOne`
//! nest transaction levels independent of any other connection.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use irisnative::{ConnectionOptions, Error, Global, connect, global};
use std::time::Instant;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn options() -> ConnectionOptions {
    ConnectionOptions {
        // bounded reads: a desync hangs as Error::Timeout, never forever
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

fn lock_node() -> Global {
    Global {
        name: "SF.PARITYLOCK.SYNC".to_string(),
        subscripts: Vec::new(),
    }
}

fn counter_node(tag: &str) -> Global {
    Global {
        name: format!("SF.PARITYINC.{tag}"),
        subscripts: Vec::new(),
    }
}

fn tx_table(conn: &mut irisnative::Connection, name: &str) {
    conn.execute(&format!("DROP TABLE IF EXISTS {name}"), &[])
        .ok();
    conn.execute(
        &format!("CREATE TABLE {name} (id INT PRIMARY KEY, tag VARCHAR(20))"),
        &[],
    )
    .expect("create tx table");
}

#[test]
#[ignore = "needs a live IRIS: docker compose up -d iris (see README)"]
fn global_lock_contention_blocks_then_releases() {
    let mut holder = connected();
    let mut waiter = connected();
    let node = lock_node();
    holder.kill(&node).ok();
    holder.lock(&node, "", 0).expect("holder takes lock");

    // The waiter's two-second timed lock must actually wait server-side
    // (same story the reference-driver differential demo shows) and then
    // answer the `<TIMEOUT>` status, not a protocol error.
    let started = Instant::now();
    let blocked = waiter.lock(&node, "", 2);
    let elapsed = started.elapsed();
    match blocked {
        Err(Error::Server { code, .. }) => assert_eq!(code.abs(), 1240),
        other => panic!("locked waiter must time out with 1240, got {other:?}"),
    }
    assert!(
        elapsed.as_millis() >= 1500,
        "the 2 s wait is server-side: only {elapsed:?} elapsed"
    );

    // releasing unblocks the next attempt
    holder.unlock(&node, "").expect("holder releases");
    waiter
        .lock(&node, "", 2)
        .expect("waiter acquires after release");
    waiter.unlock(&node, "").expect("waiter releases");

    // release_all_locks frees everything this connection holds
    holder.lock(&node, "", 0).expect("relock");
    holder
        .lock(&global!(SFPARITYINNER), "", 0)
        .expect("second lock");
    holder.release_all_locks().expect("release all");
    waiter
        .lock(&node, "", 0)
        .expect("free after release_all_locks");
    waiter.unlock(&node, "").ok();
    holder.kill(&node).ok();
    holder.kill(&global!(SFPARITYINNER)).ok();
}

#[test]
#[ignore = "needs a live IRIS"]
fn increment_returns_the_new_value_atomically() {
    let mut conn = connected();
    let node = counter_node("a");
    conn.kill(&node).ok();
    // from undefined, the first increment answers the amount itself
    assert_eq!(
        conn.increment::<String>(&node, "5").expect("inc 5"),
        "5",
        "increment from undefined"
    );
    assert_eq!(
        conn.increment::<String>(&node, "3.5").expect("inc 3.5"),
        "8.5",
        "5 → 8.5 chain (ledger scenario)"
    );
    assert_eq!(
        conn.increment::<String>(&node, "-2").expect("dec 2"),
        "6.5",
        "negative amount"
    );
    assert_eq!(conn.get::<String>(&node).expect("get"), "6.5");
    // concurrent increments never lose a count: 2×10 of +1 → 26.5
    let mut other = connected();
    std::thread::scope(|scope| {
        for handle in [&mut conn, &mut other] {
            scope.spawn(|| {
                for _ in 0..10 {
                    handle.increment::<String>(&node, "1").expect("race inc");
                }
            });
        }
    });
    assert_eq!(conn.get::<String>(&node).expect("post-race"), "26.5");
    conn.kill(&node).ok();
}

#[test]
#[ignore = "needs a live IRIS"]
fn nested_transaction_levels_compose() {
    let mut conn = connected();
    let mut peer = connected();
    tx_table(&mut conn, "irisnative_parity_t");

    assert_eq!(conn.tlevel().expect("base level"), 0);
    conn.tstart().expect("tstart level 1");
    assert_eq!(conn.tlevel().expect("level 1"), 1);
    conn.execute("INSERT INTO irisnative_parity_t VALUES (21, 'outer')", &[])
        .expect("outer insert");
    conn.tstart().expect("tstart level 2");
    assert_eq!(conn.tlevel().expect("level 2"), 2);
    // transaction levels are per-connection state
    assert_eq!(peer.tlevel().expect("peer level"), 0);

    // trollback_one undoes the innermost level only (savepoint story)
    conn.execute("INSERT INTO irisnative_parity_t VALUES (22, 'inner')", &[])
        .expect("inner insert");
    conn.trollback_one().expect("trollback one");
    assert_eq!(conn.tlevel().expect("back to 1"), 1);
    let mut rs = conn
        .query("SELECT id FROM irisnative_parity_t WHERE id = 22")
        .expect("select inner");
    assert!(!rs.fetch_next().expect("inner row rolled back"));
    let mut rs = conn
        .query("SELECT id FROM irisnative_parity_t WHERE id = 21")
        .expect("select outer");
    assert!(rs.fetch_next().expect("outer row kept"));

    // the level-1 commit closes the whole transaction (capture: last
    // tcommit drops level 1 → 0 and persists everything still open)
    conn.execute("INSERT INTO irisnative_parity_t VALUES (23, 'level1')", &[])
        .expect("level-1 insert");
    conn.tcommit().expect("commit the last level");
    assert_eq!(conn.tlevel().expect("out of tx"), 0);
    let mut rs = peer
        .query("SELECT COUNT(*) FROM irisnative_parity_t")
        .expect("peer count");
    assert!(rs.fetch_next().expect("count row"));
    assert_eq!(rs.get::<i64>(0).expect("count"), 2);

    // trollback erases every open level
    conn.tstart().expect("restart");
    conn.execute("INSERT INTO irisnative_parity_t VALUES (24, 'gone')", &[])
        .expect("tx insert");
    conn.trollback().expect("trollback all");
    assert_eq!(conn.tlevel().expect("rolled back out"), 0);
    let mut rs = conn
        .query("SELECT id FROM irisnative_parity_t WHERE id = 24")
        .expect("select 24");
    assert!(!rs.fetch_next().expect("24 erased"));

    // committing with no open transaction is the server's 1125
    match conn.tcommit() {
        Err(Error::Server { code, .. }) => assert_eq!(code.abs(), 1125),
        other => panic!("tcommit at level 0 must answer 1125, got {other:?}"),
    }
    conn.execute("DROP TABLE irisnative_parity_t", &[])
        .expect("drop");
}

#[cfg(feature = "async")]
#[tokio::test]
#[ignore = "needs a live IRIS"]
async fn async_locks_increments_and_levels_match_sync() {
    use irisnative::r#async::Connection;
    let mut holder = Connection::connect(options()).await.expect("holder");
    let mut waiter = Connection::connect(options()).await.expect("waiter");
    holder.global_kill("SF.PARITYLOCK.ASYNC", &[]).await.ok();
    holder
        .lock("SF.PARITYLOCK.ASYNC", &[], "", 0)
        .await
        .expect("async lock");
    let started = Instant::now();
    let blocked = waiter.lock("SF.PARITYLOCK.ASYNC", &[], "", 2).await;
    match blocked {
        Err(Error::Server { code, .. }) => assert_eq!(code.abs(), 1240),
        other => panic!("async waiter must time out with 1240, got {other:?}"),
    }
    assert!(started.elapsed().as_millis() >= 1500, "server-side wait");
    holder
        .unlock("SF.PARITYLOCK.ASYNC", &[], "")
        .await
        .expect("async unlock");
    waiter
        .lock("SF.PARITYLOCK.ASYNC", &[], "", 2)
        .await
        .expect("async acquire after release");
    waiter
        .release_all_locks()
        .await
        .expect("async release_all_locks");

    assert_eq!(
        holder
            .increment("SF.PARITYINC.ASYNC", &[], "5")
            .await
            .expect("async inc 5"),
        "5"
    );
    assert_eq!(
        holder
            .increment("SF.PARITYINC.ASYNC", &[], "3.5")
            .await
            .expect("async inc 3.5"),
        "8.5"
    );
    holder
        .global_kill("SF.PARITYINC.ASYNC", &[])
        .await
        .expect("kill counter");

    let base = holder.tlevel().await.expect("async base level");
    holder.tstart().await.expect("async tstart");
    holder.tstart().await.expect("async nested tstart");
    assert_eq!(holder.tlevel().await.expect("async level 2"), base + 2);
    holder.trollback_one().await.expect("async rollback one");
    assert_eq!(holder.tlevel().await.expect("async level 1"), base + 1);
    holder.tcommit().await.expect("async tcommit");
    assert_eq!(holder.tlevel().await.expect("async out"), base);
}
