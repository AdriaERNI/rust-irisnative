# Quick start

!!! info "Pre-release"
    The wire layer is implemented and verified against IRIS Community
    2025.3 (protocol 69, suite green 2026-10-15). There is no TLS — run it
    on trusted networks only (see [the protocol doc](protocol.md)).

## 1. Start a dev IRIS

```bash
docker compose up -d iris     # Community Edition 2025.3
# superserver on :1972, web (probes only) on :52773
```

Credentials after `docker/init-password.sh` runs on first boot:
`_SYSTEM` / `SYS` (dev container only — never a production value).

## 2. Add the crate

The crate is **not published to crates.io yet** (CI's publish step runs only
once `CARGO_REGISTRY_TOKEN` exists), so depend on it by git:

```toml
[dependencies]
irisnative = { git = "https://github.com/AdriaERNI/rust-irisnative" }
```

## 3. Connect

```rust,no_run
use irisnative::{connect, ConnectionOptions};

fn main() -> irisnative::Result<()> {
    let opts = ConnectionOptions::new("127.0.0.1", "USER", "_SYSTEM", "SYS");
    let mut conn = connect(opts)?;
    println!("{}", conn.server_version()?);
    Ok(())
}
```

Against the dev container this prints the full banner, e.g. `IRIS for UNIX
(… Containers) 2025.3 (Build 226U) …`.

## 4. Try the surface

- SQL: `conn.query(...)` → `ResultSet` (`fetch_next` + `get::<T>(col)`),
  `conn.execute(sql, params)` for updates (`?` placeholders, `None` = NULL)
- Globals: `global!` macro, `get`/`set`/`kill`/`next`/`prev`/`is_defined`
- Tools: `conn.class_method("Tool.X", "Run", &[...])` — including server-side
  Python (`[Language = python]`) methods; see
  [the protocol doc](protocol.md#commands) for how such a class gets
  deployed (the native port carries no source loader).
- Transactions: `conn.begin()` / `commit()` / `rollback()`
- Async: enable the `async` feature for a tokio variant of the same surface.

`examples/connect.rs` wires 1–3 together and `examples/demo.rs` walks the
whole surface: `cargo run --example demo` (env vars
`IRISHOST`/`IRISPORT`/`IRISUSER`/`IRISPASSWORD`/`IRISNS` override the
dev-container defaults).

## 5. Run the live suite

```bash
cargo test --features async -- --include-ignored
```

End-to-end behavior against the server: handshake/auth, global walks, SQL
lifecycle with parameters and errors, class-method/property calls,
transactions, Drop-sends-Disconnect, and a parity scenario compared
value-for-value against the reference wire rules (`tests/protocol.rs`,
`probes/parity_scenario.py`). No latency or throughput figures exist
anywhere in this project yet; if benchmarks ever land, they will be
measured on the CI container and dated.
