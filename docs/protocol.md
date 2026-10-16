# The Superserver protocol

!!! warning "Undocumented by design"
    InterSystems's native Superserver protocol (`%Service_CP`, TCP **1972**
    by default) is proprietary and unpublished. This crate implements it
    from clean-room readable sources only:

    - `intersystems-community/iris-driver-distribution` — InterSystems's own
      pure-Python client inside their driver wheels;
    - `caretdev/go-irisnative` — a Go driver built the same way (MIT).

    No InterSystems binary was disassembled or decompiled to build this.
    InterSystems may change the protocol without notice; the compatibility
    matrix below exists because of that risk.

Everything on this page is verified against a live IRIS Community
**2025.3** (protocol 69) with `probes/refclient.py` — a stdlib
re-implementation of the reference drivers' wire rules that produced the
golden captures under `tests/fixtures/` — and date-stamped
**verified 2026-10-15**.

## What the wire looks like

### Framing

Every message is a 14-byte fixed header plus a body, and every body field
is either a raw byte run or a `%List` item (`wire::header`, `wire::list`):

- The header carries the body length, the message count, the statement id
  (its high bit marks a continuation chunk: the body continues in another
  frame and the reader coalesces until the bit clears), and a type word in
  bytes 12–14 — the message type on requests, a status code on replies.
  Anything allocated from a server-supplied length is bounded first
  (`wire::message`), and a count mismatch is rejected as a desync.
- The client increments a message counter per request and the server
  **echoes that count** in the reply header — there is no separate reply
  id for sysio commands.
- SQL `DQ` (direct query) carries the statement id in the header and the
  **first data frame arrives with the reply** (no second round-trip to
  start fetching); `DU` (direct update) returns rows-affected. Later
  batches come from `FD` (fetch data) against the same statement id.
- For sysio replies (globals and object commands) the status word at
  header bytes 12–14 is `0xCA00 + code` little-endian — e.g. `<UNDEFINED>`
  reads as `0xCDF1` (= 1009). SQL replies carry the bare signed SQLCODE.

### Authentication

Probe truth (2026-10-15, `probes/refclient.py` against IRIS 2025.3,
matching go `mod.go:encode` and py `_IRISConnection.py:__encode`): there is
**no salt and no challenge-response**. After the greeting and the `HS`
protocol packet (raw LE u16 `69`), a single `CN` packet completes auth.
Username and password are transformed per position — XOR `0xA7`, add the
destination index `n - 1 - i`, rotate left 5 bits (`temp << 5 | temp >> 3`,
i.e. `rotate_right(3)` in Rust) — and embedded as
`%List` byte items. That is an obfuscation, not a hash: the password is
recoverable from the frame, so **the network must be treated as knowing
it**. Both reference drivers do exactly this; MD5/SHA appear nowhere on the
connect path (the SHA-256 in py `_InStream._check_sheader` guards gateway
reentrancy, not auth).

The `CN` field order is pinned by golden capture: namespace, encoded user,
encoded password, machine user, machine name, application, unused,
**licensing host**, event class, autocommit, isolation, feature options.
The licensing host field **must be non-empty** — sending an empty string
gets `CN` status 98 "Unable to allocate a license" on 2025.3 Community.
Failures map to `Error::Auth` (bad credentials) or `Error::Server`.

!!! abstract "Security posture today"
    Connections are plaintext TCP — no TLS on `%Service_CP` by default.
    Keep credentials in environment variables or a secret store (as
    `tests/protocol.rs` and CI do), never in source or committed files, and
    do not trust this client on untrusted networks until TLS support is
    documented here as landed.

### Commands

| Family | Types (header bytes 12–14) | Notes (all live-verified 2025.3) |
|---|---|---|
| Globals | `0xC241`–`0xC249` get/set/kill/order/data | subscripts as `%List` items — canonical integers use `POS_INT`/`NEG_INT` so `^x(1)` and `^x("1")` hit the same node; order walks are cursor-style (reply updates the caller's node) |
| SQL | `DQ` query, `DU` update, `FD` fetch | `DQ`/`DU` reuse the direct protocol with the statement id in the header; positional `?` parameters are rewritten server-side to `:%qpar(n)` (go `formatQuery` parity); metadata comes from a binary layout on the `DQ` reply; SQLCODE errors surface as `Error::Server` |
| Objects | `0xC24B`/`0xC24C` class-method value/void, `0xC25B`/`0xC25C` OREF method, `0xC25D`/`0xC25E` property | OREFs are `%List` items of type `0x19` (e.g. `5@%Net.HttpRequest`); server errors from XDCall (1148 method not found, 1150 class not found, 1197 private) map through the same status decode |
| Transactions | `TC` / `TR` plus `DU` for `START TRANSACTION` | gated on the negotiated `DurableTransactions` feature bit; nested begin is refused client-side |
| Session | `HS` `CN` `DC` | `DC` teardown: `Connection::close()` sends and flushes it on both drivers (sync and async; the authoritative disconnect, go `Disconnect()` / py `close()` parity) — `Drop` is the backstop: sync writes it straight to the socket (verified by watching the server job disappear in `%SYS`), async `Drop` is best-effort without a runtime. TCP close is the final backstop either way |

**Embedded Python caveat.** A `[Language = python]` class method, once it
exists on the server, is invoked exactly like any class method through
`0xC24B` — the kernel runs the Python code. But the native protocol has no
command that loads or compiles class **source**, so deploying such a class
requires another surface (Studio/Atelier REST, or an admin). This is a
property of the protocol, not of the crate: neither reference driver ships
one either.

## Legal & operational posture

- Officially supported access paths are the vendor SDKs (Call-in/Java/.NET)
  and the REST/SQL interfaces. Choose those if a supported contract matters
  more than a small static binary.
- Community drivers over this port are widespread, and to the best of public
  knowledge have not drawn objections; that is community experience, not a
  license grant.

## Compatibility matrix

| IRIS version | Handshake | Globals | SQL | Objects | CI |
|---|---|---|---|---|---|
| 2025.3 | ✅ | ✅ | ✅ | ✅ | live-suite job |
| 2024.1 | ⏳ | | | | manual |

✅ = `tests/protocol.rs --include-ignored` green against that version
(2025.3 verified 2026-10-15, protocol 69 negotiated). ⏳ = untested. A cell
turns ✅/❌ only via a green/red run of the live suite against that version;
date every result.

The `latest` image tag (2026.1 at the time of writing) has NOT been
validated: a boot attempt with the plain `IRIS_PASSWORD` env recipe hit the
same unreliable startup path CI works around, so no conclusion about this
client against 2026.1 exists either way — the matrix stays honest and does
not list it.
