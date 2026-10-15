#!/usr/bin/env python3
"""Minimal reference client speaking the Superserver protocol per go-irisnative.

This is the capture harness for the golden-frame tests: it implements the
wire rules of `.sf-local/ref/go-irisnative/src/` (protocol 69) with pure
stdlib Python, and every exchange it performs can be logged frame-by-frame
(`log_frames=True`) for replay in `tests/golden.rs`. It is a probe tool, not
production code — error handling is deliberately thin.

Rule sources (cite, never invented):
  header/frame     go-irisnative src/connection/message_header.go, message.go
  handshake/connect go-irisnative src/connection/mod.go (PROTOCOL=69, encode)
  %List items      py-iris iris/_DBList.py (_stuff_*, __set_list_length_type)
                   + go src/list/listitem.go (Dump/GetListItem)
  globals          go src/connection/globals.go
  SQL              go src/connection/sql.go (DirectQuery/DirectUpdate,
                   FormatQuery, writeParameters, SetSQLText)
  objects          go src/connection/classes.go
  transactions     go src/connection/mod.go (Commit/Rollback/BeginTx)

Empty-string convention: go's `Set("")` emits item [02 01]; py's
_DBList._stuff_empty_string emits [03 01 00]. The server accepts both; the
capture pins go's choice. SQL NULL parameters use py's [02 01] convention
(go's toODBC maps None -> "" and "" -> "\\x00" == py empty string — the two
agree on the wire for both cases).

Usage as a library:
    from refclient import Conn
    c = Conn("127.0.0.1", 19720, "USER", "_SYSTEM", "SYS")
    c.gset("Demo", "1", [1]); print(c.gget("Demo", [1]))
    print(c.class_method("%SYSTEM.Version", "GetVersion"))
    c.disconnect()
"""
from __future__ import annotations

import socket
import struct

PROTOCOL = 69  # go mod.go VERSION_PROTOCOL

# message types (go message_header.go); header bytes 12/13 in wire order
HS = b"HS"
CN = b"CN"
DC = b"DC"
GLOBAL_GET = b"\x41\xc2"
GLOBAL_SET = b"\x42\xc2"
GLOBAL_KILL = b"\x43\xc2"
GLOBAL_ORDER = b"\x45\xc2"
GLOBAL_DATA = b"\x49\xc2"
CLASSMETHOD_VALUE = b"\x4b\xc2"
CLASSMETHOD_VOID = b"\x4c\xc2"
METHOD_VALUE = b"\x5b\xc2"
METHOD_VOID = b"\x5c\xc2"
PROPERTY_GET = b"\x5d\xc2"
PROPERTY_SET = b"\x5e\xc2"
DQ = b"DQ"
DU = b"DU"
FD = b"FD"
TC = b"TC"
TR = b"TR"
OE = b"OE"

CHUNKSIZE = 31904  # go message.go SetSQLText


# ---------------------------------------------------------------- %List items

def item(value, compact_double=False) -> bytes:
    """Encode one %List item (go listitem.go NewListItem+Dump / py _DBList)."""
    if value is None:
        return b"\x02\x01"  # SQL NULL: py _stuff_null == go empty-string dump
    if isinstance(value, bool):
        value = 1 if value else 0
    if isinstance(value, int):
        if value == 0:
            return b"\x02\x04"
        if value > 0:
            data = b""
            t = value
            while t > 0:
                data += bytes([t & 0xFF])
                t >>= 8
            return _head(len(data), 0x04) + data
        if value == -1:
            return b"\x02\x05"
        data = b""
        t = -value - 1
        while t > 0:
            data += bytes([(t ^ 0xFF) & 0xFF])
            t >>= 8
        if not data:
            data = b"\x00"
        return _head(len(data), 0x05) + data
    if isinstance(value, float):
        raw = struct.pack("<d", value)
        if compact_double:
            i = 0
            while i < len(raw) and raw[i] == 0:
                i += 1
            return _head(len(raw) - i, 0x09) + raw[i:]
        return _head(8, 0x09) + raw
    if isinstance(value, (bytes, bytearray)):
        return _head(len(value), 0x01) + bytes(value)
    if isinstance(value, str):
        if any(ord(ch) > 0xFF for ch in value):
            raw = value.encode("utf-16-le")
            return _head(len(raw), 0x02) + raw
        if value == "":
            return b"\x02\x01"
        return _head(len(value), 0x01) + value.encode("latin-1")
    raise TypeError(f"unencodable item {value!r}")


def _head(payload_len: int, type_byte: int) -> bytes:
    n = payload_len + 1
    if n < 0xFF:
        return bytes([n + 1, type_byte])
    if n <= 0xFFFF:
        return b"\x00" + struct.pack("<H", n) + bytes([type_byte])
    return b"\x00\x00" + struct.pack("<I", n) + bytes([type_byte])


# ------------------------------------------------------------- auth obfuscation

def encode_credentials(value: str) -> bytes:
    """go mod.go encode(): reverse-position XOR 0xA7 + index, rot by 5."""
    inb = value.encode("latin-1")
    n = len(inb)
    out = bytearray(n)
    for i, b in enumerate(inb):
        j = n - 1 - i
        temp = ((b ^ 0xA7) + j) & 0xFF
        out[j] = ((temp << 5) | (temp >> 3)) & 0xFF
    return bytes(out)


# ------------------------------------------------------------------ frame layer

def read_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"server closed at {len(buf)}/{n} bytes")
        buf += chunk
    return bytes(buf)


class Conn:
    def __init__(self, host, port, namespace, user, password, *,
                 machine="rust-machine", application="libirisnative",
                 os_user="rust", host_ip="127.0.0.1", compact_double=False,
                 log_frames=False):
        # NOTE (probe finding): an empty host-IP field in CN gets status 98
        # "Unable to allocate a license" from IRIS 2025.3 — the field must be
        # non-empty (go sends "go"; py sends the resolved IP).
        self.sock = socket.create_connection((host, port), timeout=10)
        self.count = 0
        self.stmt = 0
        self.frames = []  # (dir, hex(header+body)) when log_frames
        self.log = log_frames
        self.version = 0
        self.unicode = False
        self.locale = ""
        self.info = ""
        self.job = ""
        self.feature_options = 0
        self.compact_double = compact_double
        self._handshake()
        self._connect(namespace, user, password, machine, application,
                      os_user, host_ip)

    # ------------------------------------------------------------- plumbing
    def _send(self, mtype: bytes, body: bytes, stmt_id: int = 0) -> None:
        c = self.count
        self.count += 1  # go mod.go count(): +1 per message (Q2: not py's +2)
        header = struct.pack("<III", len(body), c, stmt_id) + mtype
        frame = header + body
        if self.log:
            self.frames.append(("c2s", frame.hex()))
        self.sock.sendall(frame)

    CONTINUATION = 0x80000000  # py _InStream: high bit of stmt-id word

    def _recv(self):
        """Read one reply; coalesce continuation frames (py __read_message_internal:
        while high bit of the statement-id word: extend body, keep reading)."""
        header = read_exact(self.sock, 14)
        length, _count, stmt = struct.unpack("<III", header[:12])
        body = read_exact(self.sock, length) if length else b""
        if self.log:
            self.frames.append(("s2c", (header + body).hex()))
        while stmt & self.CONTINUATION:
            h2 = read_exact(self.sock, 14)
            l2, _c, _s = struct.unpack("<III", h2[:12])
            b2 = read_exact(self.sock, l2) if l2 else b""
            if self.log:
                self.frames.append(("s2c-cont", (h2 + b2).hex()))
            body += b2
        # header[12:14] semantics (py _InStream, go GetStatus):
        #  * SQL/DQ/FD/DU/OE replies: u16 LE == SQLCODE
        #  * sysio replies (globals/classes/props): u16 LE == 0xCA00 + code
        #  * handshake replies: status 0/100 like SQL
        status = struct.unpack("<H", header[12:14])[0]
        return header, status, body

    @staticmethod
    def _sysio_ok(status: int) -> int:
        return status - 0xCA00

    @staticmethod
    def _get_item(body: bytes, off: int):
        """Decode one %List item; returns (value, next_offset) (go GetListItem)."""
        b0 = body[off]
        if b0 == 0:
            n = struct.unpack_from("<H", body, off + 1)[0] - 1
            t = body[off + 3]
            data = body[off + 4: off + 4 + n]
            return data, off + 4 + n
        if b0 == 1:
            return None, off + 1  # undefined/null
        n = b0 - 2
        t = body[off + 1]
        data = body[off + 2: off + 2 + n]
        if 32 <= t < 64:
            t -= 32
        if t in (0x01, 0x19):
            return data, off + 2 + n  # raw bytes; caller interprets
        if t == 0x02:
            return data, off + 2 + n
        if t == 0x04:
            return int.from_bytes(data, "little"), off + 2 + n
        if t == 0x05:
            if not data:
                return -1, off + 2
            v = int.from_bytes(data, "little") ^ ((1 << (8 * len(data))) - 1)
            return -(v + 1), off + 2 + n
        if t in (0x06, 0x07):
            scale = data[0] - (256 if data[0] > 127 else 0)
            if t == 0x06:
                v = int.from_bytes(data[1:], "little")
            else:
                v = -(int.from_bytes(data[1:], "little") ^ ((1 << (8 * (len(data) - 1))) - 1)) - 1
            return v * (10.0 ** scale), off + 2 + n
        if t == 0x08:
            if len(data) == 8:
                return struct.unpack("<d", data)[0], off + 2 + n
            return struct.unpack("<f", data + b"\x00" * (4 - len(data)))[0], off + 2 + n
        if t == 0x09:
            return struct.unpack("<d", b"\x00" * (8 - len(data)) + data)[0], off + 2 + n
        return data, off + 2 + n

    # -------------------------------------------------------------- handshake
    def _handshake(self):
        self._send(HS, struct.pack("<H", PROTOCOL))
        _, status, body = self._recv()
        if status not in (0, 100):
            raise RuntimeError(f"HS status {status}: {body[:64]!r}")
        self.version = struct.unpack_from("<H", body, 0)[0]
        feature_bits = struct.unpack_from("<H", body, 2)[0]
        self.unicode = (feature_bits & 1) > 0
        self._feature_bits = feature_bits
        raw, _ = self._get_item(body, 4)
        self.locale = raw.decode("latin-1") if isinstance(raw, bytes) else str(raw)

    def _connect(self, namespace, user, password, machine, application,
                 os_user, host_ip):
        body = b"".join([
            item(namespace),
            item(encode_credentials(user)),
            item(encode_credentials(password)),
            item(os_user),
            item(machine),
            item(application),
            item(""),                    # go: "?" field
            item(host_ip),               # go: "go" here; empty is accepted
            item(""),                    # event class
            item(1),                     # autocommit
            item(0),                     # isolation level
            item(1 + 4 + 32),            # featureOptions: FastSelect|DurableTx|Redirect
        ])
        self._send(CN, body)
        _, status, body = self._recv()
        if status != 0:
            raw, _ = self._get_item(body, 0)
            msg = raw.decode("latin-1", "replace") if isinstance(raw, bytes) else str(raw)
            raise AuthError(f"status {status}: {msg}")
        off = 0
        v, off = self._get_item(body, off)
        self.info = self._as_text(v)
        _delimited, off = self._get_item(body, off)
        _ignored, off = self._get_item(body, off)
        _isolation, off = self._get_item(body, off)
        v, off = self._get_item(body, off)
        self.job = self._as_text(v)
        _sql_empty, off = self._get_item(body, off)
        feats, off = self._get_item(body, off)
        self.feature_options = feats if isinstance(feats, int) else 0

    @staticmethod
    def _as_text(v):
        if isinstance(v, bytes):
            return v.decode("latin-1", "replace")
        return "" if v is None else str(v)

    # ------------------------------------------------------------------ ops
    @staticmethod
    def _sub_item(x):
        # canonical numeric subscripts ride as POS/NEG_INT items (both refs
        # pass native ints; ^x(1) and ^x("1") are the same M node)
        if isinstance(x, (int,)) and not isinstance(x, bool):
            return item(x)
        try:
            i = int(x)
            if str(i) == str(x):
                return item(i)
        except (ValueError, TypeError):
            pass
        return item(x)

    def _subs(self, subs):
        return b"".join(self._sub_item(s) for s in subs)

    def global_get(self, name, subs):
        body = item(name) + item(len(subs)) + self._subs(subs)
        self._send(GLOBAL_GET, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code not in (0, 1009):  # py ERROR_UNDEFINED = 1009
            msg = self._as_text(self._get_item(body, 0)[0])
            raise ServerError(f"GLOBAL_GET code {code}: {msg}")
        v, _ = self._get_item(body, 0)
        if code == 1009:
            return None
        return self._as_text(v)

    def global_set(self, name, value, subs):
        body = item(name) + item(len(subs)) + self._subs(subs) + item(value)
        self._send(GLOBAL_SET, body)
        _, status, _ = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"GLOBAL_SET code {code}")

    def global_kill(self, name, subs):
        body = item(name) + item(len(subs)) + self._subs(subs)
        self._send(GLOBAL_KILL, body)
        _, status, _ = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"GLOBAL_KILL code {code}")

    def global_data(self, name, subs):
        body = item(name) + item(len(subs)) + self._subs(subs) + item(0)
        self._send(GLOBAL_DATA, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"GLOBAL_DATA code {code}")
        r, _ = self._get_item(body, 0)
        if isinstance(r, bytes):
            r = int.from_bytes(r, "little")
        return {"value": r % 10 == 1, "subtree": r >= 10, "raw": r}

    def global_order(self, name, ind, subs, direction=3):
        # go GlobalNext: subs..., *ind as an extra subscript item, then flag
        body = (item(name) + item(len(subs) + 1)
                + self._subs(subs) + item(ind) + item(direction))
        self._send(GLOBAL_ORDER, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"GLOBAL_ORDER code {code}")
        v, _ = self._get_item(body, 0)
        return self._as_text(v)

    def class_method(self, class_name, method, args=()):
        body = item(class_name) + item(method) + item(len(args)) + b"".join(item(a) for a in args)
        self._send(CLASSMETHOD_VALUE, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"CLASSMETHOD code {code}")
        v, _ = self._get_item(body, 0)
        return self._as_text(v)

    def class_method_void(self, class_name, method, args=()):
        body = item(class_name) + item(method) + item(len(args)) + b"".join(item(a) for a in args)
        self._send(CLASSMETHOD_VOID, body)
        _, status, _ = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"CLASSMETHOD_VOID code {code}")

    def method(self, oref, method, args=()):
        # OREF travels as %List item type 0x19 (go listitem LISTITEM_OREF)
        o = oref.encode("latin-1") if isinstance(oref, str) else oref
        body = _head(len(o), 0x19) + o + item(method) + item(len(args)) + b"".join(item(a) for a in args)
        self._send(METHOD_VALUE, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"METHOD code {code}")
        v, _ = self._get_item(body, 0)
        return self._as_text(v)

    def prop_get(self, oref, prop):
        o = oref.encode("latin-1") if isinstance(oref, str) else oref
        body = _head(len(o), 0x19) + o + item(prop)
        self._send(PROPERTY_GET, body)
        _, status, body = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"PROPERTY_GET code {code}")
        v, _ = self._get_item(body, 0)
        return self._as_text(v)

    def prop_set(self, oref, prop, value):
        o = oref.encode("latin-1") if isinstance(oref, str) else oref
        body = _head(len(o), 0x19) + o + item(prop) + item(value)
        self._send(PROPERTY_SET, body)
        _, status, _ = self._recv()
        code = self._sysio_ok(status)
        if code != 0:
            raise ServerError(f"PROPERTY_SET code {code}")

    @staticmethod
    def format_query(sql, n_params):
        """go sql.go FormatQuery: first `n_params` '?' -> ' :%qpar(k) '."""
        out = sql
        for k in range(1, n_params + 1):
            out = out.replace("?", f" :%qpar({k}) ", 1)
        return out

    def _sql_text(self, sql):
        if not sql:
            return item(sql)
        chunks = (len(sql) + CHUNKSIZE - 1) // CHUNKSIZE
        body = item(chunks)
        for i in range(chunks):
            body += item(sql[i * CHUNKSIZE:(i + 1) * CHUNKSIZE])
        return body

    def query(self, sql, params=(), max_rows=0):
        stmt = self.stmt
        self.stmt += 1
        text = self.format_query(sql, len(params))
        body = self._sql_text(text)
        body += item(len(params))
        for _ in params:
            body += item(99) + item(4)
        body += item(1) + item(len(params))
        for p in params:
            body += item(p)
        body += item(0)        # query timeout
        body += item(max_rows) # maxRows per fetch
        self._send(DQ, body, stmt_id=stmt)
        _, status, body = self._recv()
        if status not in (0, 100):
            raise self.sql_error(status)
        off = 0
        fo, off = self._get_item(body, off)
        if fo == 2:
            _msg_count, off = self._get_item(body, off)
        mri = 0
        if fo in (1, 2):
            mri, off = self._get_item(body, off)
        col_count, off = self._get_item(body, off)
        cols = []
        for i in range(col_count):
            col = {}
            for key in ("name", "type", "precision", "scale", "nullable",
                        "label", "table", "schema", "catalog", "additional"):
                col[key], off = self._get_item(body, off)
            if isinstance(col["name"], bytes):
                col["name"] = col["name"].decode("latin-1", "replace")
            if isinstance(col["type"], bytes):
                col["type"] = int.from_bytes(col["type"], "little")
            # go sql.go:getColumns: slot_position rides only in FastSelect
            if isinstance(fo, int) and fo & 1 == 1:
                slot_raw, off = self._get_item(body, off)
                col["slot"] = int.from_bytes(slot_raw, "little") if isinstance(slot_raw, bytes) else int(slot_raw or 0)
                col["slot"] -= 1
            else:
                col["slot"] = i
            cols.append(col)
        p_count, off = self._get_item(body, off)
        _p_flag, off = self._get_item(body, off)
        _, status, data = self._recv()  # first data message
        return {"cols": cols, "feature_option": fo, "max_row_items": mri,
                "data": data, "status": status, "stmt": stmt}

    def fetch_rows(self, rs, limit=100000):
        # Batch rule (py _DBAPI fetch path, go rows.Next/fetchMoreData):
        # rows are present while status in {0,100} and the data message is
        # non-empty; a status-100 FD reply with empty data ends the rowset.
        # SQLCODE in a data message: 0 = batch complete, MORE pending (send FD);
        # 100 = rowset exhausted (py _switch_buffer gates FD on _sqlcode == 0).
        # FastSelect (featureOption=1, live default on 2025.3): each row is
        # ONE wrapper item whose payload is an inner %List of maxRowItemCount
        # slot items (go sql.go:ResultSet.Next; py _ResultSetRow.indexRow).
        rows = []
        data, sqlcode = rs["data"], rs["status"]
        fo = rs["feature_option"]
        while sqlcode in (0, 100):
            off = 0
            while off < len(data):
                if fo == 1:
                    wrapper, new_off = self._get_item(data, off)
                    off = new_off
                    inner = wrapper if isinstance(wrapper, (bytes, bytearray)) else str(wrapper).encode("latin-1")
                    io = 0
                    slots = []
                    while io < len(inner):
                        v, io = self._get_item(inner, io)
                        slots.append(v)
                    row = [slots[col["slot"]] if col["slot"] < len(slots) else None
                           for col in rs["cols"]]
                else:
                    row = []
                    for col in rs["cols"]:
                        v, off = self._get_item(data, off)
                        row.append(v)
                rows.append(row)
                if len(rows) >= limit:
                    return rows
            if sqlcode == 100:
                return rows
            # py _DBAPI._switch_buffer: FD carries the statement id
            self._send(FD, b"", stmt_id=rs["stmt"])
            _, sqlcode, data = self._recv()
            if sqlcode not in (0, 100):
                raise self.sql_error(sqlcode)
        return rows

    def update(self, sql, params=()):
        stmt = self.stmt
        self.stmt += 1
        text = self.format_query(sql, len(params))
        body = self._sql_text(text)
        body += item(0)  # batchSize (go: FormatQuery returns param count only via ';' path)
        body += item("") + item(0) + item(1) + item(len(params))
        for p in params:
            body += item(p)
        self._send(DU, body, stmt_id=stmt)
        _, status, body = self._recv()
        if status not in (0, 100):
            raise self.sql_error(status)
        # go DirectUpdate reply: paramscnt, 4 items per param, cache flag,
        # rowsAffected (captured 2026-10-15; getParameterInfo + msg.Get)
        off = 0
        n_params, off = self._int_item(body, off)
        for _ in range(n_params):
            for _ in range(4):
                _, off = self._get_item(body, off)
        _flag, off = self._get_item(body, off)
        affected, off = self._int_item(body, off)
        return affected

    @staticmethod
    def _int_item(body, off):
        v, off = Conn._get_item(body, off)
        if isinstance(v, (bytes, bytearray)):
            v = int.from_bytes(v, "little")
        return int(v or 0), off

    def sql_error(self, sqlcode):
        self._send(OE, item(sqlcode))
        _, _status, body = self._recv()
        v, _ = self._get_item(body, 0)
        return ServerError(f"SQLCODE {sqlcode}: {self._as_text(v)}")

    def begin_tx(self):
        self.update("START TRANSACTION")

    def commit(self):
        self._send(TC, b"")
        self._recv()

    def rollback(self):
        self._send(TR, b"")
        self._recv()

    def disconnect(self):
        self._send(DC, b"")
        self.sock.close()


class AuthError(Exception):
    pass


class ServerError(Exception):
    pass
