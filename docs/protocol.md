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

## What the wire looks like

Nothing on this page is verified yet: **no probe has been run against a live
server in this repository's history**, so every section below states the
target structure and cites the probe that will fill it. Until then treat all
of it as TBD.

### Framing

_TBD — probe planned: deeper byte-dump of a request/reply pair
(`probes/handshake_dump.py` currently only observes the passive connection
window; a request-sending framing probe is part of the protocol mission)._

### Authentication

_The reference drivers (`go-irisnative`, the pure-Python client in
`iris-driver-distribution`) are reported to implement a salted exchange in
which the password itself does not cross the network in plaintext — this is
an attributed claim about those implementations, **not** a verified fact
about this crate or a probe result. TBD — probe required._

!!! abstract "Security posture today"
    This crate is a pre-release scaffold: `connect()` performs no IO at all.
    When the wire layer lands, the first versions speak **plaintext TCP** —
    no TLS on `%Service_CP` by default. Keep credentials in environment
    variables or a secret store (as `tests/live.rs` and CI do), never in
    source or committed files, and do not trust this client on untrusted
    networks until TLS support is documented here as landed.

### Commands

_TBD._

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
| 2025.3 | ⏳ | ⏳ | ⏳ | ⏳ | live-smoke job |
| 2024.1 | ⏳ | | | | manual |

⏳ = untested. A cell turns ✅/❌ only via a green/red run of
`tests/live.rs --ignored` against that version (today that suite asserts
TCP reachability and graceful `NotImplemented` only — no matrix cell can
change from it); date every verdict.
