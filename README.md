<div align="center">

  # irisnative

  **InterSystems IRIS from Rust — one socket, no SDK.**

  [![CI](https://github.com/AdriaERNI/rust-irisnative/actions/workflows/ci.yml/badge.svg)](https://github.com/AdriaERNI/rust-irisnative/actions/workflows/ci.yml)
  ![MSRV](https://img.shields.io/badge/rust-1.85%2B-blue)
  [![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
  ![IRIS](https://img.shields.io/badge/IRIS-2025.3-orange)

</div>

> **Pre-release scaffold.** This crate ships its public API surface, docs and
> probe infrastructure. The Superserver wire layer is **not implemented yet**:
> `connect()` returns `Error::NotImplemented`, so nothing below runs against a
> live server today. The protocol port is the next mission (see
> [probes/](probes/)).

`irisnative` aims to be a pure-Rust client for **InterSystems IRIS** that
speaks the native **Superserver wire protocol** on TCP port **1972** — the
protocol family InterSystems documents their own JDBC/Python/Node drivers as
using. No vendor shared libraries, no ODBC driver, no web gateway: one TCP
socket. Pure Rust means binaries stay static and small, and run on the
platforms Rust and this crate's CI cover (Linux and Windows today).

Because IRIS ships multi-language runtimes in its kernel (ObjectScript,
**Embedded Python** — vendor-documented since 2022.1 [^epython] — and
JavaScript), a native connection turns an IRIS instance into a **remote
compute engine**: deploy your internal tool once as a class method — in
Python if you like — and every Rust service on the network can invoke it,
next to the data, with zero HTTP plumbing in between.

## Why this exists

| Surface | What it needs | Where it fits |
|---|---|---|
| Atelier REST (e.g. [Rism]) | web gateway, HTTP/WS port | dev tooling, MCP/agents |
| **irisnative (this crate)** | **port 1972 only** | **libraries, services, CLI tools** |
| Vendor SDK bindings | `libiscapi`/`.dll` on the host | legacy integration |

Rust prior art is effectively none: [daimor's `rust-irisnative`][daimor]
powered a VS Code language server in 2021 and was never published; the
reference for the wire format in another language is
[`go-irisnative`][go-irisnative] (Go, MIT). This crate fills that hole for
Rust, with a deliberately small, typed API.

## Features (target surface)

None of the unchecked boxes works yet — every method returns
`Error::NotImplemented` until the protocol port lands.

- [ ] Native handshake: username + auth (salted exchange per the reference drivers — see [the protocol doc](docs/protocol.md)), namespace attach
- [ ] Globals: `get` / `set` / `kill` / `next` / subscript walks (`global!` macro)
- [ ] SQL: statement prepare/execute/fetch, positional parameters
- [ ] Objects: class-property access and class-method invocation — including `[Language = python]` methods
- [ ] Sync API on `std::net`; `async` feature (tokio) behind a flag — **the flag exists in Cargo.toml but gates no code yet**
- [ ] Small core: no runtime required for the sync path

## Quick start (API preview — does not run yet)

The types and signatures below are what `src/lib.rs` actually exposes; the
calls fail with `Error::NotImplemented` until the wire layer lands.

```rust,no_run
use irisnative::{connect, global, ConnectionOptions};

let mut conn = connect(ConnectionOptions::new("dbhost", "USER", "appuser", "secret")).unwrap();

// SQL
let mut rs = conn.query("SELECT Name FROM %Dictionary.ClassDefinition WHERE Super = 'Ens.Production'").unwrap();
while rs.fetch_next().unwrap() {
    let name: String = rs.get(0).unwrap();
    println!("{name}");
}

// Globals (M-level)
conn.set(&global!(Demo(1)), "hello").unwrap();
let v: String = conn.get(&global!(Demo(1))).unwrap();

// Call a server-side Python tool
let out = conn.class_method("Tool.Report", "Monthly", &["2026-09".into()]).unwrap();
```

## The protocol, honestly

The Superserver protocol is **undocumented and proprietary**. This project
derives its implementation from clean-room readable sources: InterSystems's
own pure-Python client shipped in `iris-driver-distribution` and the
`go-irisnative` Go driver — no decompilation of InterSystems binaries.
Practical consequences you should weigh before production use:

- Connections are **plaintext TCP today and until TLS support lands**: no
  encryption by default. Password handling will follow the salted scheme the
  reference drivers implement — verified by probe, not yet by this crate.
  Credentials travel via `ConnectionOptions` / environment variables in our
  examples; never hardcode them.
- InterSystems may change the protocol without notice; expect to pin IRIS
  versions and re-run the compatibility suite on upgrades.
- It is not, as far as publicly stated, licensed for third-party products —
  the community ships drivers on this basis anyway; your risk assessment is
  yours. For officially supported paths use the vendor SDKs or the REST APIs.
- TLS support on the native port (`%Service_CP`) depends on server
  configuration; the crate will support it when the handshake analysis lands.

## Development

```bash
git clone https://github.com/AdriaERNI/rust-irisnative
cd rust-irisnative
docker compose up -d iris          # IRIS Community 2025.3, exposes 1972
cargo test                         # unit tests (no server needed)
cargo test --features async -- --ignored --test-threads=1   # live smoke (needs compose)
```

The live suite (`tests/live.rs`) is env-driven — `IRISHOST` (127.0.0.1),
`IRISPORT` (1972), `IRISUSER` (_SYSTEM), `IRISPASSWORD` (SYS), `IRISNS`
(USER) — the same variables `examples/connect.rs` reads. Against the current
stubs it asserts only TCP reachability and that `connect()` fails
gracefully; it pins no protocol behavior.

- `documentation/` (git-ignored) holds the working protocol notes and raw
  probe output.
- Wire changes require a probe first: settle every question against a live
  server (`probes/`, or a `#[ignore]`d probe test) before it reaches `src/`.

## MSRV & versions

Rust **1.85** or newer — that is the first release stabilizing
[edition 2024][edition-2024], which the crate is written in. CI stands up
IRIS Community **2025.3** for the live-smoke leg; the protocol
compatibility range is TBD pre-release (see
[docs/protocol.md](docs/protocol.md)).

## License

MIT — see [LICENSE](LICENSE).

[Rism]: https://github.com/AdriaERNI/Rism
[daimor]: https://github.com/daimor/rust-irisnative
[go-irisnative]: https://github.com/caretdev/go-irisnative
[edition-2024]: https://doc.rust-lang.org/edition-guide/rust-2024/index.html
[^epython]: InterSystems IRIS 2022.1 release notes, "Embedded Python …
  fully integrated into the kernel":
  <https://docs.intersystems.com/irislatest/csp/docbook/DocBook.UI.Page.cls?KEY=GCRN_new20221>
