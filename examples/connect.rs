//! Connect to a local IRIS (compose) and print the server banner.
//! `docker compose up -d iris` first; defaults target the dev container.

use irisnative::{ConnectionOptions, connect};

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
    println!("{}", conn.server_version()?);
    Ok(())
}
