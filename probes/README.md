# probes

Python/socket probes that establish wire facts. Every wire-layer rule in
AGENTS.md / docs/protocol.md cites a probe file here. A protocol claim
without a probe is not a claim.

## Running a probe

- Python **3.9+**, standard library only — no `pip install`, no venv.
- Run from the repo root: `python3 probes/handshake_dump.py [host] [port]`
  (defaults: `127.0.0.1` `1972` — the compose dev container; start it with
  `docker compose up -d iris`).
- Output: raw dumps land in `documentation/probe-output/` (git-ignored).
  Distill findings into `docs/protocol.md` / AGENTS.md wire rules, citing
  the probe file and the dated dump.

## Inventory

- `handshake_dump.py` — passive TCP connect + byte dump. Does NOT
  authenticate; it only observes what the server sends unsolicited.
- `refclient.py` — the active capture harness: a stdlib re-implementation
  of the go-irisnative wire rules (protocol 69). Used to settle wire facts
  and to capture the frames replayed by `tests/golden.rs` (raw captures in
  `captures/`, distilled fixture in `tests/fixtures/frames.json`).
- `parity_scenario.py` — the live parity oracle: runs the end-to-end
  scenario from `tests/protocol.rs` through `refclient.py` and prints the
  outcome as JSON; the Rust test compares value-for-value.

Regenerating the golden fixture: run `refclient.py` against the dev
container with a capture wrapper around `_send`/`_recv` (see
`captures/*.json` shapes), then flatten to the `requests`/`replies`
frame-hex lists in `tests/fixtures/frames.json`.
