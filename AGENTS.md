# AGENTS.md — rust-irisnative

Rust client for InterSystems IRIS over the native Superserver wire protocol
(TCP 1972). Crate name `irisnative`, repo `rust-irisnative`. MIT. MSRV 1.85,
edition 2024.

## Branch model (Git Flow, no exceptions)

- `main` — released, protected. `development` — integration, protected.
- Feature work: branch `feat/<topic>` or `fix/<topic>` from `development`,
  PR back to `development`. The USER merges in the web UI; agents NEVER
  `gh pr merge` and NEVER approve their own PRs.
- Release: cut `release/vX.Y.Z` from `development` tip → PR to `main` →
  user merges → tag on the NEW main tip → CI publishes → sync `main`→
  `development` (hard-reset when trees are identical, else sync PR with a
  merge commit). Pre-releases: tag on `development` only.
- Tags carry a `v` prefix. Never `gh release create` — CI does it.

## Commands

| Task | Command |
|---|---|
| Lint gate (must be silent) | `cargo fmt --check && cargo clippy --all-targets -- -D warnings` |
| Tests (no server needed) | `cargo test` |
| Live integration | `docker compose up -d iris`, then `cargo test --features async -- --ignored --test-threads=1`. Env-driven: `IRISHOST` (default 127.0.0.1), `IRISPORT` (1972), `IRISUSER` (`_SYSTEM`), `IRISPASSWORD` (`SYS`), `IRISNS` (`USER`) — same vars `examples/connect.rs` reads. Pre-wire, `tests/live.rs` asserts TCP reachability + graceful `NotImplemented` only. |
| Docs (strict) | `uv run --with 'mkdocs-material[imaging]' mkdocs build --strict` |
| Publish dry-run | `cargo package --list` (check contents, license, no leaks) |

## The golden rule of this repo: probe before you code

The wire protocol is undocumented. NOTHING goes into `src/` from memory or
from a guess. Settle every frame, command id, and field against a live IRIS
first — a ~30-line Python socket script that dumps the raw request/reply
bytes, or a `#[ignore]`d probe test — then write the Rust against the dump.
`documentation/` (git-ignored) holds the raw protocol notes; every stable
finding that consumers of the code need gets distilled into `docs/`.
Reference implementations to cross-read (do NOT copy without the probe):
`caretdev/go-irisnative` (Go) and the pure-Python client inside
`intersystems-community/iris-driver-distribution` wheels.

## Dev server

`docker compose up -d iris` → IRIS Community 2025.3 on **1972** (superserver)
and 52773 (web, probe cross-checks only). Credentials `_SYSTEM`/`SYS` are set
by `docker/init-password.sh` on first boot (compose `IRIS_PASSWORD` env is
unreliable on this tag). Long sessions: the container can exit — check
`docker inspect irisnative-iris --format '{{.State.Status}}'` and restart
before debugging a "protocol bug". Never commit real server credentials
anywhere beyond this compose pattern.

## Layout

- `src/` — framework-free library core. `lib.rs` public API; wire codec
  (`wire/`), session/connection, typed surface modules (globals, sql,
  objects). No clap, no MCP, no binaries — this repo ships a LIBRARY; tools
  consuming it live elsewhere.
- `tests/` — `wire.rs` (today: pure-logic macro tests; byte-level codec
  round-trips are the target and land with the probe mission, each quirk
  pinned by a test citing the probe that discovered it) and `live.rs`
  (`#[ignore]`d live smoke — TCP reachability + graceful NotImplemented,
  env-driven, see Commands).
- `probes/` — the Python/socket probe scripts the tests cite. A protocol
  claim without a probe file is not a claim. `handshake_dump.py` (passive
  byte dump) exists; deeper probes land with the probe mission.
- `examples/` — compile and run against compose, but stop at `connect()`
  with `NotImplemented` until the wire layer lands (globals walk, SQL,
  class-method / Embedded-Python call are the target).
- `docs/` + `mkdocs.yml` — GitHub Pages. Every public item documented; every
  data point in docs explains where it came from.

## Style & policy

- `#![deny(unsafe_code)]` — enforced as `unsafe_code = "deny"` in the
  workspace lints (`Cargo.toml [lints.rust]`, not a crate attribute); lints
  deny `unwrap`/`expect` — test mods carry module-level
  `#[allow(clippy::unwrap_used, clippy::expect_used)]`.
- Public API is fallible: `Result<_, Error>`, `#[non_exhaustive]` enums, no
  panics on any server-supplied path. Errors carry the raw IRIS status code
  plus the decoded message.
- Docs on every public item (`missing_docs` warns); rustdoc examples
  `no_run` unless they work headless.
- Server-supplied lengths are validated BEFORE allocating — the codec must
  never trust a frame header (DoS + desync class).

## Wire-layer rules (append ONLY with probe evidence)

- (none yet — first probe session fills this section; each rule cites the
  probe script/output that established it)

## CI (GitHub Actions)

lint → test → Windows compile-check → live-smoke against a fresh
`intersystemsdc/iris-community:2025.3` service container (port 1972 exposed);
`v*` tags: git-cliff release notes + `cargo package` gate + GitHub release
(.crate asset); `cargo publish` only once `CARGO_REGISTRY_TOKEN` exists —
never claim "published to crates.io" before that. NO macOS targets anywhere,
per owner policy (no comments about it either). CI stable can lint NEWER than
local — before declaring done, run clippy under the toolchain CI reports
(`rustup run <v> cargo clippy --all-targets -- -D warnings`).

## Commit messages

Conventional commits (git-cliff generates the changelog): `feat:`, `fix:`,
`docs:`, `refactor:`, `test:`, `ci:`. One line, no body. Scope = module
(`wire`, `globals`, `sql`, `objects`). Never commit `documentation/` or other
local working notes.
