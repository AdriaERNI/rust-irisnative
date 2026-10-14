# Quick start

!!! info "Pre-release"
    The wire layer is under construction; this page documents the target API
    (as shipped in `src/lib.rs` stubs) so early readers can review the shape.
    Every call that touches the server currently returns
    `Error::NotImplemented` — no code on this page runs against IRIS yet.
    Executable behavior arrives with the protocol port.

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

Today step 3 prints nothing: `connect` returns `Err(NotImplemented)`.

## 4. Try the surface

Target shape — each bullet returns `Error::NotImplemented` until the wire
layer lands:

- SQL: `conn.query(...)` → `ResultSet` (`fetch_next` + `get::<T>(col)`)
- Globals: `global!` macro, `get`/`set`/`kill`/`next`
- Tools: `conn.class_method("Tool.X", "Run", &[...])` — including server-side
  Python (`[Language = python]`) methods

`examples/connect.rs` wires 1–3 together: `cargo run --example connect`
(env vars `IRISHOST`/`IRISPORT`/`IRISUSER`/`IRISPASSWORD`/`IRISNS` override
the dev-container defaults).

## 5. Run the live smoke suite

```bash
cargo test --features async -- --ignored --test-threads=1
```

Pre-wire this asserts only that port 1972 answers TCP and that `connect()`
fails gracefully — see `tests/live.rs`. No latency or throughput figures
exist anywhere in this project yet; if benchmarks ever land, they will be
measured on the CI container and dated.
