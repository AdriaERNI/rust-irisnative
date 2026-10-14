#!/usr/bin/env python3
"""Open TCP 1972 and observe the raw connection — do NOT authenticate.

This is the first probe of the planned protocol work. It performs the most
passive experiment possible: make a TCP connection to the superserver
(%Service_CP), read whatever the server sends unsolicited (IRIS is
request-driven, so expect nothing until we send a request — that
expectation is itself the fact this probe tests), and dump every byte in
hex + ASCII side by side.

It does NOT send credentials, does NOT implement any login, and does NOT
invent frame bytes. Facts established here are written up in
`documentation/` (git-ignored working notes) and only then distilled into
`docs/protocol.md` with a citation to this file.

Usage:
    python3 probes/handshake_dump.py [host] [port]
    # defaults: 127.0.0.1 1972

Deps: Python 3.9+ standard library only.
"""

from __future__ import annotations

import datetime
import socket
import sys
from pathlib import Path

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 1972
READ_TIMEOUT_S = 5.0
MAX_BYTES = 4096
CHUNK = 64


def describe(data: bytes) -> str:
    """Hex + ASCII rail for a byte string."""
    lines = []
    for off in range(0, len(data), 16):
        row = data[off : off + 16]
        hexs = " ".join(f"{b:02x}" for b in row)
        ascii_ = "".join(chr(b) if 32 <= b < 127 else "." for b in row)
        lines.append(f"{off:08x}  {hexs:<47}  |{ascii_}|")
    return "\n".join(lines)


def main() -> int:
    host = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_HOST
    port = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_PORT

    stamp = datetime.datetime.now(datetime.timezone.utc)
    out_dir = Path(__file__).resolve().parent.parent / "documentation" / "probe-output"
    out_dir.mkdir(parents=True, exist_ok=True)
    out_path = out_dir / f"handshake_dump_{stamp:%Y%m%dT%H%M%SZ}_{host}_{port}.txt"

    report: list[str] = [
        f"# handshake_dump — {stamp.isoformat()}",
        f"target: {host}:{port} (%Service_CP)",
        "mode: PASSIVE TCP connect + read only; NO auth, NO request bytes sent",
        "",
    ]

    try:
        with socket.create_connection((host, port), timeout=READ_TIMEOUT_S) as sock:
            report.append("connect: OK")
            sock.settimeout(READ_TIMEOUT_S)
            received = bytearray()
            try:
                while len(received) < MAX_BYTES:
                    chunk = sock.recv(CHUNK)
                    if not chunk:
                        report.append(f"server closed after {len(received)} bytes")
                        break
                    received.extend(chunk)
            except socket.timeout:
                report.append(
                    "read: TIMEOUT with no more data — server is waiting for "
                    "a client request (IRIS is request-driven on this port)"
                )
            except OSError as exc:
                report.append(f"read error: {exc!r}")
            report.append("")
            report.append(f"unsolicited bytes from server: {len(received)}")
            if received:
                report.append("")
                report.append(describe(bytes(received)))
    except OSError as exc:
        report.append(f"connect: FAILED — {exc!r}")
        report.append("is the dev container up?  docker compose up -d iris")

    text = "\n".join(report)
    print(text)
    out_path.write_text(text + "\n", encoding="utf-8")
    print(f"\nwritten: {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
