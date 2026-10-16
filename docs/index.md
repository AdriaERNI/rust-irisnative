# irisnative — public docs

Home of the published documentation (GitHub Pages:
<https://adriaerni.github.io/rust-irisnative/>).

- `protocol.md` — what the Superserver protocol *is*, honestly stated:
  undocumented, proprietary, implemented from clean-room readable sources.
  Every claim must cite the probe that established it
  (probe scripts live in `probes/`, committed).
- `quickstart.md` — workflow against a running IRIS: globals, SQL, one
  Embedded-Python class-method call. Live against IRIS 2025.3 (protocol 69);
  no latency/throughput figures exist yet, and any that land
  will be measured on the CI container and dated.
- `api.md` — public-surface tour; rustdoc is generated separately
  (`cargo doc`) and linked, not duplicated.

Protocol **working notes** are NOT docs: `documentation/` (git-ignored) is
the scratch space where raw frame dumps live until they are distilled here.
A finding becomes documentation only when a byte-level test pins it.
