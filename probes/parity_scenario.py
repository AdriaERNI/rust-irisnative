#!/usr/bin/env python3
"""Run the live parity scenario through refclient.py and print JSON.

The scenario mirrors `tests/protocol.rs::live_parity_vs_reference`: the Rust
client and this script must report IDENTICAL outcomes (values, SQLCODE
behavior, rows affected) against the same IRIS server. Reference rules come
from github.com/caretdev/go-irisnative (src/connection/*.go); the
pure-Python wheel in intersystems-community/iris-driver-distribution
speaks protocol 65 and is denied
by modern servers (version trap — probed 2026-10-15), so the Go wire
semantics via refclient.py are the parity oracle.

Env: IRISHOST, IRISPORT, IRISUSER, IRISPASSWORD, IRISNS.
"""
from __future__ import annotations

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from refclient import Conn  # noqa: E402

TABLE = "irisnative_parity_p"


def main() -> int:
    host = os.environ.get("IRISHOST", "127.0.0.1")
    port = int(os.environ.get("IRISPORT", "1972"))
    user = os.environ.get("IRISUSER", "_SYSTEM")
    password = os.environ.get("IRISPASSWORD", "SYS")
    ns = os.environ.get("IRISNS", "USER")
    c = Conn(host, port, ns, user, password)
    out = {}

    def text(v):
        if v is None:
            return ""
        if isinstance(v, (bytes, bytearray)):
            return v.decode("latin-1")
        return str(v)

    # globals: set 1..3, order walk from subscript "1"
    c.global_kill("SFPARITY", [])
    for i in (1, 2, 3):
        c.global_set("SFPARITY", "v%d" % i, [i])
    walk = []
    ind = c.global_order("SFPARITY", "1", [], 3)
    while ind:
        walk.append(ind)
        ind = c.global_order("SFPARITY", ind, [], 3)
    out["global_walk"] = walk
    # undefined node -> code 1009 mapped to -1009 by the Rust side
    out["undefined"] = c.global_get("SFPARITY", ["999"]) is None
    c.global_kill("SFPARITY", [])

    # SQL: table lifecycle, NULL round-trip, positional parameter
    c.update("DROP TABLE IF EXISTS %s" % TABLE)
    c.update("CREATE TABLE %s (id INT PRIMARY KEY, name VARCHAR(50))" % TABLE)
    out["insert_1"] = c.update(
        "INSERT INTO %s (id, name) VALUES (1, NULL)" % TABLE)
    out["insert_2"] = c.update(
        "INSERT INTO %s (id, name) VALUES (?, ?)" % TABLE,
        params=["2", "bob"])
    rs = c.query("SELECT id, name FROM %s ORDER BY id" % TABLE)
    out["select_rows"] = [[text(v) for v in row] for row in c.fetch_rows(rs)]
    rs = c.query("SELECT id, name FROM %s WHERE id = ?" % TABLE, params=["2"])
    out["param_row"] = [[text(v) for v in row] for row in c.fetch_rows(rs)]

    # bad SQL -> SQLCODE 30 (unsigned of -30 per py _DBAPI header status)
    try:
        c.query("SELECT * FROM %s_missing_table" % TABLE)
        out["bad_sql_code"] = None
    except Exception as e:  # noqa: BLE001
        out["bad_sql_code"] = int(str(e).split("SQLCODE ")[1].split(":")[0])

    # transactions: commit persists, rollback erases
    c.begin_tx()
    c.update("INSERT INTO %s (id, name) VALUES (10, ?)" % TABLE,
             params=["committed"])
    c.commit()
    rs = c.query("SELECT name FROM %s WHERE id = 10" % TABLE)
    out["commit_visible"] = len(c.fetch_rows(rs)) == 1
    c.begin_tx()
    c.update("INSERT INTO %s (id, name) VALUES (11, ?)" % TABLE,
             params=["rolled"])
    c.rollback()
    rs = c.query("SELECT name FROM %s WHERE id = 11" % TABLE)
    out["rollback_visible"] = len(c.fetch_rows(rs)) == 1

    c.update("DROP TABLE %s" % TABLE)

    # class method (objects surface, go classes.go)
    out["version_prefix"] = c.class_method(
        "%SYSTEM.Version", "GetVersion")[:14]

    # job number for the Drop-sends-Disconnect sanity check
    out["job"] = int(c.job)
    c.disconnect()
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
