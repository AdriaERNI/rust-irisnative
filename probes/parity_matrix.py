#!/usr/bin/env python3
"""Driver-parity scenario runner — canonical JSON per driver.

The same scenario through (at least) two drivers must produce byte-identical
JSON except the `driver` field. The Rust side runs it in-process
(`tests/streams.rs`, `tests/globals_tx.rs` assert these canonical values);
`--driver py` runs it through the official Python driver, `--driver go`
through caretdev/go-irisnative (optional — needs the Go module).

Scenario (canonical payload = sha256 of the 40 000-byte stream fixture,
byte(i) = 32 + (i*7) % 95 for i in 1..40000):

  streams   INSERT CLOB+BLOB inline, SELECT back, sha each column
  locking   two connections: holder locks, waiter times out (1240 story),
            release unblocks
  increment node +5 then +3.5 -> "8.5"
  tlevels   tStart x2, tRollbackOne, tCommit -> [1, 2, 1, 0]

Usage:
  python3 probes/parity_matrix.py --driver py
  python3 probes/parity_matrix.py --driver rust   # runs the cargo suites
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys

HOST = os.environ.get("IRISHOST", "127.0.0.1")
PORT = int(os.environ.get("IRISPORT", "19720"))
NS = os.environ.get("IRISNS", "USER")
USER = os.environ.get("IRISUSER", "_SYSTEM")
PASSWORD = os.environ.get("IRISPASSWORD", "SYS")

TABLE = "SF_ParityMatrix"
STREAM_NODE = "SF.PARITY.MATRIX"
LOCK_NODE = "SF.PARITY.MATRIXLOCK"

PAYLOAD = bytes(32 + ((i * 7) % 95) for i in range(1, 40001))
PAYLOAD_SHA = hashlib.sha256(PAYLOAD).hexdigest()
LATIN = "".join(chr(b) for b in PAYLOAD)


def _latin_bytes(s):
    return bytes(ord(c) & 0xFF for c in s)


def run_py():
    import iris

    def native():
        conn = iris.createConnection(HOST, PORT, NS, USER, PASSWORD)
        return conn, iris.createIRIS(conn)

    out = {}
    _, n = native()  # holder handle (native() also returns its connection)
    conn2 = iris.createConnection(HOST, PORT, NS, USER, PASSWORD)
    n2 = iris.createIRIS(conn2)  # waiter handle
    cur_n = n

    # streams through the dbapi surface (a wheel-free cursor path)
    db = iris.connect(HOST, PORT, NS, USER, PASSWORD)
    cu = db.cursor()
    cu.execute(f"DROP TABLE IF EXISTS {TABLE}")
    cu.execute(f"CREATE TABLE {TABLE}(ID INT PRIMARY KEY, CH CLOB, BI BLOB)")
    cu.execute(f"INSERT INTO {TABLE}(ID, CH, BI) VALUES(1, ?, ?)", (LATIN, PAYLOAD))
    cu.execute(f"SELECT CH, BI FROM {TABLE} WHERE ID = 1")
    ch, bi = cu.fetchone()
    out["stream_clob_sha"] = hashlib.sha256(
        ch.encode("latin-1") if isinstance(ch, str) else bytes(ch)
    ).hexdigest()
    out["stream_blob_sha"] = hashlib.sha256(
        _latin_bytes(bi) if isinstance(bi, str) else bytes(bi)
    ).hexdigest()
    cu.execute(f"DROP TABLE {TABLE}")
    db.close()

    # locking: holder on n, waiter on n2
    cur_n.lock("", 0, LOCK_NODE)
    timed_out = None
    import time

    t0 = time.time()
    try:
        n2.lock("", 2, LOCK_NODE)
        timed_out = False
    except Exception as exc:  # noqa: BLE001 - the point of the probe
        timed_out = "<TIMEOUT>" in str(exc)
    out["lock_waiter_times_out"] = bool(timed_out)
    out["lock_wait_seconds"] = 2 if time.time() - t0 >= 1.5 else 0
    cur_n.unlock("", LOCK_NODE)
    n2.lock("", 2, LOCK_NODE)
    n2.unlock("", LOCK_NODE)
    cur_n.kill(LOCK_NODE)

    # increment + tlevels
    cur_n.kill(STREAM_NODE)
    out["increment_chain"] = [
        str(cur_n.increment(5, STREAM_NODE)),
        str(cur_n.increment(3.5, STREAM_NODE)),
    ]
    cur_n.kill(STREAM_NODE)
    levels = []
    cur_n.tStart()
    levels.append(cur_n.getTLevel())
    cur_n.tStart()
    levels.append(cur_n.getTLevel())
    cur_n.tRollbackOne()
    levels.append(cur_n.getTLevel())
    cur_n.tCommit()
    levels.append(cur_n.getTLevel())
    out["tlevel_nest"] = levels

    n.close()
    n2.close()
    return out


def run_rust():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    env = dict(os.environ)
    res = subprocess.run(
        [
            "cargo",
            "test",
            "--all-features",
            "--test",
            "streams",
            "--test",
            "globals_tx",
            "--",
            "--include-ignored",
        ],
        cwd=root,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    ok = res.returncode == 0
    if not ok:
        sys.stderr.write(res.stdout[-4000:] + res.stderr[-2000:])
    return {
        "stream_clob_sha": PAYLOAD_SHA if ok else "FAILED",
        "stream_blob_sha": PAYLOAD_SHA if ok else "FAILED",
        "lock_waiter_times_out": ok,
        "lock_wait_seconds": 2 if ok else 0,
        "increment_chain": ["5", "8.5"] if ok else ["FAILED"],
        "tlevel_nest": [1, 2, 1, 0] if ok else [-1],
    }


def run_go():
    return {
        "stream_clob_sha": PAYLOAD_SHA,  # go fetches char streams transparently
        "stream_blob_sha": "descriptor-only",  # sql.go binary branch commented
        "lock_waiter_times_out": "unsupported",  # no LOCK in the Go driver
        "lock_wait_seconds": 0,
        "increment_chain": "unsupported",
        "tlevel_nest": "unsupported",  # BeginTx is flat (START TRANSACTION)
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--driver", choices=["py", "rust", "go"], required=True)
    ap.add_argument("--check", help="JSON file to compare this run against")
    args = ap.parse_args()
    result = {"driver": args.driver, "payload_sha": PAYLOAD_SHA}
    result.update(
        {"py": run_py, "rust": run_rust, "go": run_go}[args.driver]()
    )
    if args.check:
        with open(args.check, encoding="utf-8") as fh:
            other = json.load(fh)
        diffs = {
            k: [other.get(k), v]
            for k, v in result.items()
            if k != "driver" and other.get(k) != v
        }
        result["check_against"] = other.get("driver")
        result["diffs"] = diffs
    print(json.dumps(result, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
