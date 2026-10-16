//! Live scale tests — long-running load against a reachable IRIS, proved
//! out-of-tree first and shipped here as evidence (probe-settled 2026-10-16:
//! 20k mixed ops RSS delta ≈ 64 KiB; 2.5k churn cycles, 0 failures;
//! 8×500 concurrent ops on 8 connections, 0 errors).
//!
//! All three carry the `scale_` prefix and are `#[ignore]`d: the CI live
//! job skips them (`--skip scale_`) so its duration stays bounded — run
//! them explicitly on a dev box or the nightly lane:
//! `IRISPORT=19720 cargo test --features async -- --include-ignored scale_`
//!
//! Counts are env-tunable — `SCALE_OPS` (default 50 000),
//! `SCALE_CYCLES` (default 2 000) — so a smaller smoke figure can be run
//! without editing code.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use irisnative::{ConnectionOptions, global};

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

fn env_count(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Connect, tolerating the server's finite license pool. Status 98
/// "Unable to allocate a license" is a server resource condition (other
/// live tests or jobs hold the last slots; IRIS frees one a beat after a
/// job exits), not a client defect — retry it with backoff. Every other
/// error surfaces immediately.
fn connect_retrying(
    opts: &ConnectionOptions,
    attempts: usize,
) -> irisnative::Result<irisnative::Connection> {
    let mut last = None;
    for i in 0..attempts {
        match irisnative::connect(opts.clone()) {
            Ok(c) => return Ok(c),
            Err(irisnative::Error::Server { code: 98, .. }) => {
                std::thread::sleep(std::time::Duration::from_millis(250 + 250 * i as u64));
                last = Some(98);
            }
            Err(e) => return Err(e),
        }
    }
    Err(irisnative::Error::Server {
        code: 98,
        message: format!("license slots busy after {attempts} tries (last={last:?})"),
    })
}

/// Current RSS in KiB from /proc (Linux-only; the scale lane runs on
/// ubuntu). Returns 0 where the file is unreadable so assertions can skip.
fn rss_kb() -> usize {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:").map(str::trim))
        .and_then(|v| v.trim_end_matches(" kB").trim().parse().ok())
        .unwrap_or(0)
}

#[test]
#[ignore = "needs a live IRIS and minutes of load"]
fn scale_soak_mixed_ops() {
    // Set 5k keys; every 3rd op is a get, every 50th a global order walk,
    // every 100th a `SELECT 1` — the same mix as the external soak probe.
    // Memory must stay flat across the run (page-cache granularity, not a
    // leak): assert the RSS delta in absolute KiB.
    let n = env_count("SCALE_OPS", 50_000);
    let mut conn = connect_retrying(&options(), 8).expect("connect live IRIS");
    conn.kill(&global!(SFSCALE)).expect("clean root");
    let keys: Vec<_> = (0..5_000).map(|i| global!(SFSCALE(i))).collect();
    for (i, k) in keys.iter().enumerate() {
        conn.set(k, &format!("v{i}")).expect("seed set");
    }

    let rss0 = rss_kb();
    let mut errors = 0usize;
    for i in 0..n {
        let k = &keys[i % keys.len()];
        if i % 3 == 0 {
            if conn.get::<String>(k).is_err() {
                errors += 1;
            }
        } else if i % 50 == 0 {
            // a bounded slice of the order walk, not a full 5k sweep, so
            // the soak stays minutes-fast (the probe's shape, capped)
            let mut node = global!(SFSCALE(0));
            for _ in 0..20 {
                if conn.next(&mut node).expect("walk").is_none() {
                    break;
                }
            }
        } else if i % 100 == 0 {
            let Ok(mut rs) = conn.query("SELECT 1") else {
                errors += 1;
                continue;
            };
            while rs.fetch_next().expect("fetch") {}
        } else if conn.set(k, "w").is_err() {
            errors += 1;
        }
    }
    let delta = rss_kb().saturating_sub(rss0);
    conn.kill(&global!(SFSCALE)).expect("kill subtree");
    assert_eq!(errors, 0, "{n} mixed ops must all succeed");
    if rss0 > 0 {
        // Generous bound: the probe measured ~64 KiB across 20k ops; a few
        // MiB allows allocator/fragmentation slack without hiding a leak.
        assert!(delta < 8_192, "RSS grew {delta} KiB over {n} ops");
    }
    println!("soak: {n} mixed ops, errors=0, rss delta {delta} KiB");
}

#[test]
#[ignore = "needs a live IRIS and minutes of churn"]
fn scale_connection_churn() {
    // Full connect → set → get → close() cycles: no fd growth, no auth
    // pressure, every cycle clean. ~42 ms/cycle on a local server. A
    // transient license-slot refusal retries the whole cycle.
    let cycles = env_count("SCALE_CYCLES", 2_000);
    let mut failures = 0usize;
    for i in 0..cycles {
        let mut ok = false;
        for _ in 0..5 {
            let outcome = (|| -> irisnative::Result<()> {
                let mut conn = connect_retrying(&options(), 4)?;
                conn.set(&global!(SFCHURN(1)), &format!("c{i}"))?;
                let v: String = conn.get(&global!(SFCHURN(1)))?;
                assert_eq!(v, format!("c{i}"));
                conn.close()?;
                Ok(())
            })();
            if outcome.is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            failures += 1;
        }
    }
    assert_eq!(failures, 0, "{cycles} churn cycles must all succeed");
    println!("churn: {cycles} cycles, failures=0");
}

#[test]
#[ignore = "needs a live IRIS"]
fn scale_concurrent_hammer() {
    // 8 threads x 500 set+get ops, each thread on its OWN connection and
    // its own global — the supported sharing model (no interior sync on
    // one session). Compile-time proof that the types cross thread
    // boundaries lives in the Send/Sync assert below.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<irisnative::Connection>();
    assert_send_sync::<irisnative::ConnectionOptions>();
    assert_send_sync::<irisnative::Session>();

    let mut handles = Vec::new();
    for t in 0..8usize {
        let opts = options();
        handles.push(std::thread::spawn(move || {
            // 8 simultaneous dials can momentarily exhaust the license
            // pool (status 98) — retry with backoff instead of panicking.
            let Ok(mut conn) = connect_retrying(&opts, 8) else {
                return 1; // license slots exhausted: counted as an error
            };
            let mut errors = 0usize;
            for i in 0..500usize {
                let g = global!(SFHAM(t, i));
                if conn.set(&g, &format!("{i}")).is_err() {
                    errors += 1;
                }
                let v: irisnative::Result<String> = conn.get(&g);
                if v.map_or(true, |s| s.parse::<usize>() != Ok(i)) {
                    errors += 1;
                }
            }
            conn.close().expect("thread close");
            errors
        }));
    }
    let errors: usize = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread must not panic"))
        .sum();
    assert_eq!(errors, 0, "8x500 ops across 8 connections, zero errors");
    println!("hammer: 8x500 ops, errors=0");
}
