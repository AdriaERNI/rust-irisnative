# Driver parity: Rust vs the reference drivers

Can a user of this crate do what the official Python driver
(`iris` 5.2.1) and the community Go driver (`caretdev/go-irisnative`) can
do over the native port? This page records the answer **capability by
capability**, classed as:

- **parity** — same user story over the wire, proven by live differential
  tests (`tests/protocol.rs`, `tests/streams.rs`, `tests/globals_tx.rs`)
  against IRIS 2025.3;
- **ours-only** / **theirs-only** — one driver has it, the others do not;
- **equivalent** — different mechanism, same reachable outcome;
- **documented** — no dedicated API anywhere; the story is reachable
  through SQL text or server configuration (each row says how);
- **none** — not reachable over the native protocol in any driver.

Reproduce: `python3 probes/parity_matrix.py --driver py` (official Python
driver scenarios) and `cargo test --all-features -- --include-ignored`
(the Rust scenarios the same script drives expectations for); Go is
optional (`--driver go`, needs the `go` toolchain).

| Capability | Rust | Python 5.2.1 | Go | Class / note |
|---|---|---|---|---|
| connect / handshake / auth | `connect` | `iris.connect` | `Connect` | parity — same obfuscated `CN` packet |
| server version | `server_version` | `getServerVersion` | `ServerVersion` | parity |
| globals get/set/kill/isDefined/next/prev | `Global` + `global!` | `_IRIS` methods | `Global*` funcs | parity |
| typed value variants (long/bool/bytes…) | `value::Value` decode | typed getters | `interface{}` targets | parity — decode-side, same wire |
| class methods (value/void) | `class_method(_void)` | `classMethod` | `ClassMethod*` | parity — `0xC24B/4C` |
| OREF methods / properties | `method`, `property_get/set` | `IRISObject` | `ObjectMethod*` | parity |
| **streams: SQL CLOB/BLOB transparent** | **yes** (fetch resolves `JS`) | yes (both directions) | char columns only (`sql.go:328` binary branch commented out) | parity+ since 2026-10 — 40 KB BLOB+CLOB round-trip live-tested |
| **streams: stream objects** | `create_stream` + `write/read/size/rewind/truncate/close` | `_InStream`/`_OutStream`, `%Stream.*` objects | `Stream` char-family | parity — server-side in 2025.3, `Truncate`/`Erase` are declared only on the `%Stream.Interface` contract: `truncate` is surfaced and answers the server's 1148 where the concrete class lacks it, `Erase`/`Position` are not surfaced over the wire (probe); `rewind` + the object `Read` cursor cover the positioned story |
| **global lock / unlock / releaseAllLocks** | `lock` / `unlock` / `release_all_locks` | yes (`0xC24E` sysio LOCK) | **no lock at all** | theirs-only → parity now; Go cannot participate; locks interoperate server-side with every driver (cross-driver contention proven) |
| **global increment (atomic)** | `increment` | yes (`0xC246`) | no | theirs-only → parity now; the race-free counter `get`+`set` is not |
| **nested transaction levels** | `tstart`/`tcommit`/`trollback(_one)`/`tlevel` | yes (`0xC251`–`0xC255`) | no (flat begin/commit only) | theirs-only → parity now; `trollback_one` is the savepoint the SQL `TR` frame cannot express |
| flat transactions | `begin`/`commit`/`rollback` | `begin`/`commit` | `BeginTx` (`START TRANSACTION` via `DU`, `TC`/`TR`) | parity |
| **transaction isolation / autocommit toggles** | via SQL text | `connect(isolationLevel=)`, `autocommit` kwargs | constants exist, **zero send sites** (dead table) | documented — run `execute("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")` / `START TRANSACTION` yourself; no driver has a wire flag for it |
| SQL direct query/update + param bind | `query(_params)`/`execute` | `cursor.execute` | `Query`/`Exec` | parity |
| **prepared statements (`PP`/`PQ`)** | no | yes (dbapi executes route `PP`) | `Prepare`/`Stmt` | equivalent — `DQ` re-binds per call; same plans, same results, small per-call prepare overhead instead of a handle |
| **stored procedures (`CALL`)** | `execute("CALL SQLUser.P()")`; row-returning ones via `query` | `_IRIS.procedure()` (`0xC25A` legacy) | `SP/SQ/SU/SF` constants declared, **zero send sites** — Go calls via `DQ`/`Exec` too | documented — the native-procedure command family is dead code in the references; `CALL` via `DU`/`DQ` is the live path (fixture: `probes/paritycall.cls`; note `User` is a SQL reserved word — qualify `SQLUser` or omit) |
| **fast-insert flag / `executeMany` batching** | no flag; loop `execute` | dbapi `executemany` | `optFastInsert` extras | documented — a server-side hint for bulk loads; the `DU` loop reaches the same rows (perf knob, not capability) |
| **`LastInsertId` / identity** | `query("SELECT LAST_IDENTITY()")` | `cursor.lastidentity` | `rows.LastInsertId` | equivalent — no `GG` frame needed; identity is the connection's last insert, so read it right after inserting (or wrap it in a class method). The reference harnesses' read-loop hangs around this are artifacts of their own fetch loops, not driver-wire facts: this driver answers it bounded and in-sync, live-tested |
| **multi result sets (`MD`/`MR`)** | no | dbapi `nextset` declared | declared, no send sites | documented — neither reference exercises it on live servers; no user story to match yet |
| **routine function/procedure (`0xC248/4D/5A`)** | no | `function()`/`doMethod` legacy | no | equivalent — the modern story is `class_method` (what all current drivers compile `{fn}`/`{call}` down to anyway) |
| **subscript tree view / globals iterator** | `next`/`prev` walk | `IRISGlobalNodeView` | `GlobalNext/Prev` loop | equivalent — `$ORDER` composition gives every subscript; `0xC24A` (`GET_SUBNODES`) would only batch the round-trips |
| `%List` / `IRISList` | `wire::list` | `IRISList` | `%List` funcs | parity |
| SQL error text (`OE`) | decoded in `Error::Server` | `getSQLError` | via status | parity |
| async / cancellation | `r#async::Connection`, `timeout_ms` | GIL-bound | `context.Context` | ours: tokio feature; cancellation bounded by socket timeouts |
| TLS on the native port | no | `getSecureConnection` (gateway-only) | no | none over `%Service_CP` from any driver — use a TLS terminator or `%Service_GLW`; see README |
| shared memory / embedded | n/a | yes (Windows agent) | n/a | not the wire protocol — out of scope |
| DB-API 2.0 / `database/sql` layer | no (adapter-crate territory) | `iris.dbapi` | IS one | out of scope by design |
| **connection URI string** | `ConnectionOptions` builder | `connectionstr="iris://…"` kwarg | address string | documented — build options from any URI with a 3-line helper in your app; no wire difference |

## Divergences that are ours on purpose

- Stream columns decode **transparently in both directions** (the Python
  story), which puts us ahead of the Go driver's character-only fetch.
  `JS` is position-less server-side (capture): `Stream::read_all`/`read`
  re-read from byte 0, and the object cursor (`rewind` + `read_text`)
  provides the positioned reads — exactly the reference split, not a
  client-side emulation of one.
- `lock` surfaces `<TIMEOUT>` as `Error::Server { code: 1240 }` instead of
  Python's `RuntimeError` — the crate's uniform error channel.

## Evidence

Live differential evidence behind every row: the ignored-by-default suites
under `tests/` (every new sysio frame is exercised against the live server
there), the canonical scenario runner `probes/parity_matrix.py`
(`--driver py` / `--driver rust` / `--driver go`) which compares all
drivers value-for-value, the golden request frames in
`src/connection/commands.rs` (byte-for-byte from live captures, replayed in
CI), and the raw frame dumps under `probes/captures/`.
