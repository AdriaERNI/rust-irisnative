//! End-to-end example: connect, globals, SQL, class methods.
//! Run against a live server: `IRISPORT=19720 cargo run --example demo`
//! (env names match the tests: `IRISHOST`, `IRISPORT`, `IRISUSER`,
//! `IRISPASSWORD`, `IRISNS`; defaults target the compose dev server).
#![allow(missing_docs)]

use irisnative::{ConnectionOptions, connect, global};

fn main() -> irisnative::Result<()> {
    let opts = ConnectionOptions::new(
        std::env::var("IRISHOST").unwrap_or_else(|_| "127.0.0.1".into()),
        std::env::var("IRISNS").unwrap_or_else(|_| "USER".into()),
        std::env::var("IRISUSER").unwrap_or_else(|_| "_SYSTEM".into()),
        std::env::var("IRISPASSWORD").unwrap_or_else(|_| "SYS".into()),
    )
    .port(
        std::env::var("IRISPORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(1972),
    );
    let mut conn = connect(opts)?;
    println!("connected: {}", conn.info().info);
    println!("version: {}", conn.server_version()?);

    conn.set(&global!(Demo(1)), "hello")?;
    let v: String = conn.get(&global!(Demo(1)))?;
    println!("^Demo(1) = {v}");

    let mut rs = conn.query("SELECT 40 + 2 AS N")?;
    while rs.fetch_next()? {
        println!("SELECT 40+2 -> {}", rs.get::<i64>(0)?);
    }

    println!(
        "GetVersion: {}",
        conn.class_method("%SYSTEM.Version", "GetVersion", &[])?
    );
    conn.kill(&global!(Demo(1)))?;
    Ok(())
}
