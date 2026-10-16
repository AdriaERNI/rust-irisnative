//! Tokio-backed connection variant (feature `async`).
//!
//! Every wire fact comes from the shared `wire/` codecs and
//! `connection::commands` builders — this module contains IO sequencing
//! only. Request bytes are byte-identical to the sync path by construction;
//! `tests/golden.rs` pins the claim with a fixture comparison.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use std::future::Future;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream as TcpStreamAsync;

use super::commands::{self, ConnectParams};
use super::driver;
use super::globals;
use super::object;
use super::session::Session;
use super::sql::{self as sqlcmd, QueryState};
use super::tx;
use crate::wire::list::Item;
use crate::wire::{message, message::Reply};
use crate::{ConnectionOptions, Error, Result};

/// Async transport state: buffered tokio socket + the same session struct
/// the sync `Connection` maintains.
#[derive(Debug)]
pub struct Connection {
    stream: BufStream<TcpStreamAsync>,
    session: Session,
    tx_open: bool,
    timeout: Option<Duration>,
    /// Set once a `DC` has been queued/sent (by `close`), so `Drop` does
    /// not send a second disconnect for the same session.
    dc_sent: bool,
}

/// Apply `ConnectionOptions::timeout_ms` to one IO future — the async twin
/// of std's dial timeout + `set_read_timeout`. A missed deadline yields
/// [`Error::Timeout`] (py raises `socket.timeout`, go
/// `os.ErrDeadlineExceeded` — same distinct signal).
async fn with_timeout<T>(timeout: Option<Duration>, fut: T) -> Result<T::Output>
where
    T: Future,
{
    match timeout {
        None => Ok(fut.await),
        Some(d) => tokio::time::timeout(d, fut)
            .await
            .map_err(|_| Error::Timeout),
    }
}

impl Connection {
    /// Dial, handshake (`HS`), and login (`CN`) — async counterpart of
    /// [`crate::connect`]. `opts.timeout_ms` bounds the dial and every
    /// socket read on this connection, same as the sync path.
    ///
    /// # Errors
    /// As [`crate::connect`]; a dial deadline miss is [`Error::Timeout`].
    pub async fn connect(opts: ConnectionOptions) -> Result<Self> {
        let timeout = opts.timeout_ms.map(Duration::from_millis);
        let stream = with_timeout(
            timeout,
            TcpStreamAsync::connect((opts.host.as_str(), opts.port)),
        )
        .await??;
        // Nagle taxes this strict ping-pong protocol; go's TCPConn runs
        // TCP_NODELAY by default, so match it (ref: go mod.go Dial).
        let _ = stream.set_nodelay(true);
        let mut stream = BufStream::new(stream);
        let mut session = Session::default();
        let (machine, host_ip) = crate::identity_for(&opts.host);
        let params = ConnectParams {
            namespace: &opts.namespace,
            user: &opts.username,
            password: &opts.password,
            machine: &machine,
            application: "libirisnative",
            host_ip: &host_ip,
        };
        establish_async(&mut stream, &mut session, &params, timeout).await?;
        Ok(Self {
            stream,
            session,
            tx_open: false,
            timeout,
            dc_sent: false,
        })
    }

    /// Send `DC` and flush it — the authoritative async disconnect, the
    /// counterpart of go's `Disconnect()` / py's `close()`. Prefer this
    /// over relying on `Drop` (see its notes).
    ///
    /// # Errors
    /// [`Error::Io`] if the frame cannot reach the socket.
    pub async fn close(&mut self) -> Result<()> {
        if self.dc_sent {
            return Ok(());
        }
        self.dc_sent = true;
        let frame = commands::build_disconnect(self.session.next_count());
        with_timeout(self.timeout, self.stream.write_all(&frame))
            .await?
            .map_err(Error::Io)?;
        with_timeout(self.timeout, self.stream.flush())
            .await?
            .map_err(Error::Io)
    }

    /// Negotiated connection facts.
    #[must_use]
    pub fn info(&self) -> &super::session::ConnectionInfo {
        &self.session.info
    }

    /// Server banner via `%SYSTEM.Version.GetVersion`.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn server_version(&mut self) -> Result<String> {
        self.class_method("%SYSTEM.Version", "GetVersion", &[])
            .await
    }

    /// `GLOBAL_GET` (0xC241): the node's text, or [`None`] when undefined.
    ///
    /// Undefined reads return `Ok(None)` (go-style raw `GlobalGet`); the
    /// sync [`crate::Connection::get`] raises `Error::Server(-1009,
    /// "<UNDEFINED>")` instead (py-style). Same wire, by design.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn global_get(&mut self, name: &str, subs: &[String]) -> Result<Option<String>> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_global_get(name, subs, c), c)
            .await?;
        globals::decode_get_reply(&reply)
    }

    /// `GLOBAL_SET` (0xC242).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn global_set(&mut self, name: &str, subs: &[String], value: &str) -> Result<()> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_global_set(name, subs, value, c), c)
            .await?;
        driver::check_sysio(&reply)
    }

    /// `GLOBAL_KILL` (0xC243).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn global_kill(&mut self, name: &str, subs: &[String]) -> Result<()> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_global_kill(name, subs, c), c)
            .await?;
        driver::check_sysio(&reply)
    }

    /// `GLOBAL_DATA` (0xC249): whether the node or its subtree is defined,
    /// same rule as [`crate::Connection::is_defined`].
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn is_defined(&mut self, name: &str, subs: &[String]) -> Result<bool> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_global_data(name, subs, c), c)
            .await?;
        globals::decode_data_reply(&reply).map(|d| d.value || d.subtree)
    }

    /// `GLOBAL_ORDER` (0xC245) next: the following subscript in collation
    /// order, `None` at the end of the walk. Sibling cursor at M
    /// `$ORDER` level (see [`crate::Connection::next`]).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn global_next(
        &mut self,
        name: &str,
        subs: &[String],
        ind: &str,
    ) -> Result<Option<String>> {
        self.global_order(name, subs, ind, globals::DIR_NEXT).await
    }

    /// `GLOBAL_ORDER` (0xC245) previous: mirror of
    /// [`Connection::global_next`](Self::global_next).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn global_prev(
        &mut self,
        name: &str,
        subs: &[String],
        ind: &str,
    ) -> Result<Option<String>> {
        self.global_order(name, subs, ind, globals::DIR_PREV).await
    }

    async fn global_order(
        &mut self,
        name: &str,
        subs: &[String],
        ind: &str,
        dir: i64,
    ) -> Result<Option<String>> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_global_order(name, subs, ind, dir, c), c)
            .await?;
        globals::decode_order_reply(&reply)
    }

    /// `METHOD_VALUE` (0xC25B): invoke an instance method on an OREF.
    ///
    /// # Errors
    /// Transport/server failures; sysio errors surface as
    /// [`Error::Server`] like the sync path.
    pub async fn method(&mut self, oref: &str, name: &str, args: &[String]) -> Result<String> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_method(oref, name, args, false, c), c)
            .await?;
        object::decode_value_reply(&reply)
    }

    /// `PROPERTY_GET` (0xC25D).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn property_get(&mut self, oref: &str, prop: &str) -> Result<String> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_property(oref, prop, None, c), c)
            .await?;
        object::decode_value_reply(&reply)
    }

    /// `PROPERTY_SET` (0xC25E).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn property_set(&mut self, oref: &str, prop: &str, value: &str) -> Result<()> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_property(oref, prop, Some(value), c), c)
            .await?;
        driver::check_sysio(&reply)
    }

    /// `CLASSMETHOD_VOID` (0xC24C): invoke a class method, discarding any
    /// result.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn class_method_void(
        &mut self,
        class: &str,
        method: &str,
        args: &[String],
    ) -> Result<()> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(
                &commands::build_class_method_void(class, method, args, c),
                c,
            )
            .await?;
        driver::check_sysio(&reply)
    }

    /// `CLASSMETHOD_VALUE` (0xC24B).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn class_method(
        &mut self,
        class: &str,
        method: &str,
        args: &[String],
    ) -> Result<String> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_class_method(class, method, args, c), c)
            .await?;
        object::decode_value_reply(&reply)
    }

    /// Send `DQ` and read metadata + first data push (shared decoders).
    ///
    /// # Errors
    /// SQLCODE errors surface as [`Error::Server`] like the sync path.
    pub async fn query(&mut self, sql_text: &str, params: &[Option<String>]) -> Result<QueryState> {
        let statement_id = self.session.next_statement();
        let c = self.session.next_count();
        let reply = self
            .roundtrip(
                &commands::build_direct_query(sql_text, params, 0, statement_id, c),
                c,
            )
            .await?;
        let code = i16::try_from(reply.status()).unwrap_or(i16::MAX);
        if !sqlcmd::sqlcode_is_ok(code) {
            let message = self.error_text(code).await?;
            return Err(sqlcmd::sqlcode_error(code, message));
        }
        let meta = sqlcmd::decode_meta(&reply.body);
        let reply = self.read_push(c).await?;
        let sqlcode = i16::try_from(reply.status()).unwrap_or(i16::MAX);
        if !sqlcmd::sqlcode_is_ok(sqlcode) {
            let message = self.error_text(sqlcode).await?;
            return Err(sqlcmd::sqlcode_error(sqlcode, message));
        }
        Ok(QueryState {
            statement_id,
            columns: meta.columns,
            fast_select: meta.feature_option == 1,
            max_row_items: meta.max_row_items,
            data: reply.body,
            offset: 0,
            sqlcode,
        })
    }

    /// Next row of an active query (shared `decode_row` + async FD).
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn next_row(&mut self, st: &mut QueryState) -> Result<Option<Vec<Item>>> {
        loop {
            if !sqlcmd::sqlcode_is_ok(st.sqlcode) {
                return Ok(None);
            }
            if let Some(row) = sqlcmd::decode_row(st)? {
                return Ok(Some(row));
            }
            if st.sqlcode == 100 {
                return Ok(None);
            }
            let c = self.session.next_count();
            let reply = self
                .roundtrip(&commands::build_fetch(st.statement_id, c), c)
                .await?;
            st.sqlcode = i16::try_from(reply.status()).unwrap_or(i16::MAX);
            if !sqlcmd::sqlcode_is_ok(st.sqlcode) {
                let message = self.error_text(st.sqlcode).await?;
                return Err(sqlcmd::sqlcode_error(st.sqlcode, message));
            }
            st.data = reply.body;
            st.offset = 0;
        }
    }

    /// `DU` direct update; rows affected.
    ///
    /// # Errors
    /// SQLCODE errors surface as [`Error::Server`].
    pub async fn execute(&mut self, sql_text: &str, params: &[Option<String>]) -> Result<i64> {
        let statement_id = self.session.next_statement();
        let c = self.session.next_count();
        let reply = self
            .roundtrip(
                &commands::build_direct_update(sql_text, params, statement_id, c),
                c,
            )
            .await?;
        let code = i16::try_from(reply.status()).unwrap_or(i16::MAX);
        if !sqlcmd::sqlcode_is_ok(code) {
            let message = self.error_text(code).await?;
            return Err(sqlcmd::sqlcode_error(code, message));
        }
        Ok(sqlcmd::decode_du_affected(&reply.body))
    }

    /// `OE` server error text for an SQLCODE.
    async fn error_text(&mut self, sqlcode: i16) -> Result<String> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_error_info(sqlcode, c), c)
            .await?;
        Ok(driver::first_item(&reply.body)?
            .map(|i| i.as_text())
            .unwrap_or_default())
    }

    /// `TC` commit.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn commit(&mut self) -> Result<()> {
        let c = self.session.next_count();
        let reply = self.roundtrip(&commands::build_commit(c), c).await?;
        tx::check_tx_reply(&reply)?;
        self.tx_open = false;
        Ok(())
    }

    /// `TR` rollback.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn rollback(&mut self) -> Result<()> {
        let c = self.session.next_count();
        let reply = self.roundtrip(&commands::build_rollback(c), c).await?;
        tx::check_tx_reply(&reply)?;
        self.tx_open = false;
        Ok(())
    }

    /// `START TRANSACTION` via `DU` (go `BeginTx` parity).
    ///
    /// # Errors
    /// [`Error::Protocol`] when a transaction is already open.
    pub async fn begin(&mut self) -> Result<()> {
        if self.tx_open {
            return Err(Error::Protocol(
                "multiple transactions: a transaction is already open".into(),
            ));
        }
        self.execute("START TRANSACTION", &[]).await?;
        self.tx_open = true;
        Ok(())
    }

    async fn roundtrip(&mut self, frame: &[u8], count: u32) -> Result<Reply> {
        with_timeout(self.timeout, self.stream.write_all(frame))
            .await?
            .map_err(Error::Io)?;
        with_timeout(self.timeout, self.stream.flush())
            .await?
            .map_err(Error::Io)?;
        self.read_reply(Some(count)).await
    }

    async fn read_push(&mut self, count: u32) -> Result<Reply> {
        self.read_reply(Some(count)).await
    }

    async fn read_reply(&mut self, expect_count: Option<u32>) -> Result<Reply> {
        read_reply_async(&mut self.stream, expect_count, self.timeout).await
    }
}

impl Drop for Connection {
    /// Best-effort `DC` before the socket closes.
    ///
    /// `Drop` cannot await: the frame is written into the `BufStream`
    /// buffer and flushed once under a no-op waker, which reaches the wire
    /// when the socket is immediately writable and is dropped otherwise
    /// (a tokio `BufStream` has no flush-on-Drop). For an authoritative
    /// clean disconnect — the server-side job teardown the sync `Drop`
    /// performs — call [`Connection::close`] before releasing the
    /// connection, mirroring go `Disconnect()` / py `close()`. TCP close
    /// remains the backstop either way.
    fn drop(&mut self) {
        if self.dc_sent {
            return;
        }
        self.dc_sent = true;
        let frame = commands::build_disconnect(self.session.next_count());
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut fut = std::pin::pin!(self.stream.write_all(&frame));
        if std::future::Future::poll(fut.as_mut(), &mut cx).is_pending() {
            return;
        }
        let mut fut = std::pin::pin!(self.stream.flush());
        let _ = std::future::Future::poll(fut.as_mut(), &mut cx);
    }
}

async fn read_exact_async<S: AsyncReadExt + Unpin>(
    s: &mut S,
    buf: &mut [u8],
    timeout: Option<Duration>,
) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = with_timeout(timeout, s.read(&mut buf[filled..]))
            .await?
            .map_err(Error::Io)?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-frame",
            )));
        }
        filled += n;
    }
    Ok(())
}

/// Handshake + connect over an async transport. The reply *decoding* is the
/// shared pure seam in [`driver`] — this function only sequences IO, so the
/// negotiation rules cannot drift from the sync path.
async fn establish_async(
    stream: &mut BufStream<TcpStreamAsync>,
    session: &mut Session,
    p: &ConnectParams<'_>,
    timeout: Option<Duration>,
) -> Result<()> {
    let c0 = session.next_count();
    let reply = roundtrip_stream(stream, &commands::build_handshake(), c0, timeout).await?;
    driver::apply_handshake(&reply, session)?;
    let c1 = session.next_count();
    let reply = roundtrip_stream(stream, &commands::build_connect(p, c1), c1, timeout).await?;
    driver::apply_connect(&reply, session)
}

async fn roundtrip_stream(
    stream: &mut BufStream<TcpStreamAsync>,
    frame: &[u8],
    count: u32,
    timeout: Option<Duration>,
) -> Result<Reply> {
    with_timeout(timeout, stream.write_all(frame))
        .await?
        .map_err(Error::Io)?;
    with_timeout(timeout, stream.flush())
        .await?
        .map_err(Error::Io)?;
    read_reply_async(stream, Some(count), timeout).await
}

async fn read_reply_async<S: AsyncReadExt + Unpin>(
    s: &mut S,
    expect_count: Option<u32>,
    timeout: Option<Duration>,
) -> Result<Reply> {
    // Same sequence as wire::message::read_reply (count echo -> bounded
    // alloc -> continuation coalesce); only the `read` is async, so the
    // framing policy stays in `wire`.
    let mut head = [0u8; crate::wire::header::HEADER_SIZE];
    read_exact_async(s, &mut head, timeout).await?;
    let header = crate::wire::header::Header::parse(&head);
    message::check_reply_count(&header, expect_count).map_err(driver::map_frame_err)?;
    let mut body = message::body_with_bound(header.length).map_err(driver::map_frame_err)?;
    if header.length > 0 {
        read_exact_async(s, &mut body, timeout).await?;
    }
    let mut more = header.is_continuation();
    let mut chunks = 0u32;
    while more {
        chunks += 1;
        message::check_continuation_bound(chunks).map_err(driver::map_frame_err)?;
        let mut next_head = [0u8; crate::wire::header::HEADER_SIZE];
        read_exact_async(s, &mut next_head, timeout).await?;
        let nh = crate::wire::header::Header::parse(&next_head);
        message::check_reply_count(&nh, expect_count).map_err(driver::map_frame_err)?;
        message::check_total_bound(body.len(), nh.length).map_err(driver::map_frame_err)?;
        let mut chunk = message::body_with_bound(nh.length).map_err(driver::map_frame_err)?;
        if nh.length > 0 {
            read_exact_async(s, &mut chunk, timeout).await?;
        }
        body.extend_from_slice(&chunk);
        more = nh.is_continuation();
    }
    Ok(Reply { header, body })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::wire::header::{CONTINUATION_BIT, Header, MessageType};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    fn read_frame(s: &mut std::net::TcpStream) -> Option<([u8; 2], [u8; 4], Vec<u8>)> {
        let mut head = [0u8; 14];
        if s.read_exact(&mut head).is_err() {
            return None;
        }
        let length = u32::from_le_bytes(head[0..4].try_into().unwrap_or([0; 4])) as usize;
        let mut body = vec![0u8; length];
        if s.read_exact(&mut body).is_err() {
            return None;
        }
        Some((
            [head[12], head[13]],
            head[4..8].try_into().unwrap_or([0; 4]),
            body,
        ))
    }

    #[allow(clippy::cast_possible_truncation)] // fixture bodies are tiny
    fn write_reply(s: &mut std::net::TcpStream, type_word: [u8; 2], count: [u8; 4], body: &[u8]) {
        let mut frame = Vec::with_capacity(14 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&count);
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&type_word);
        frame.extend_from_slice(body);
        let _ = s.write_all(&frame);
        let _ = s.flush();
    }

    /// Answer `HS` + `CN` like the live server (same fixtures as the sync
    /// mock in lib.rs), then serve until `DC`. Offline probe server.
    fn serve_mock(mut client: std::net::TcpStream) {
        let Some((type_word, count, _)) = read_frame(&mut client) else {
            return;
        };
        if type_word != *b"HS" {
            return;
        }
        let mut hs = vec![0x45, 0x00, 0x03, 0x00, 0x09, 0x01];
        hs.extend_from_slice(b"Unicode");
        write_reply(&mut client, [0, 0], count, &hs);
        let Some((type_word, count, _)) = read_frame(&mut client) else {
            return;
        };
        if type_word != *b"CN" {
            return;
        }
        let cn = [
            0x04u8, 0x01, b'm', // info text
            0x02, 0x04, // delimited ids 0
            0x02, 0x04, // ignored 0
            0x02, 0x04, // isolation 0
            0x05, 0x01, b'1', b'2', b'3', // server job "123"
            0x02, 0x04, // sql empty 0
            0x03, 0x04, 37, // feature options
        ];
        write_reply(&mut client, [0, 0], count, &cn);
        loop {
            let Some((type_word, count, _)) = read_frame(&mut client) else {
                return;
            };
            if type_word == [0x44, 0x43] {
                return; // DC
            }
            write_reply(&mut client, [0, 0xCA], count, &[]);
        }
    }

    fn mock_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                serve_mock(stream);
            }
        });
        port
    }

    fn mock_options(port: u16) -> ConnectionOptions {
        ConnectionOptions::new("127.0.0.1", "USER", "_SYSTEM", "SYS").port(port)
    }

    #[tokio::test]
    async fn async_dial_sets_tcp_nodelay() {
        // Same claim the sync mock test pins, on the tokio path (go's
        // TCPConn runs NODELAY by default; Nagle taxes this ping-pong).
        let port = mock_server();
        let conn = Connection::connect(mock_options(port))
            .await
            .expect("mock async connect");
        assert!(
            conn.stream.get_ref().nodelay().expect("nodelay queryable"),
            "the async client socket must run TCP_NODELAY"
        );
    }

    #[tokio::test]
    async fn async_stalled_read_maps_to_timeout_error() {
        // timeout_ms must bound async reads too, as Error::Timeout — the
        // parity twin of the sync stalled-read test. A listener that
        // accepts and never replies parks the HS read; if with_timeout
        // were dead wiring this would hang until the harness kills it.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let mut parked = Vec::new();
            while let Ok(s) = listener.accept() {
                parked.push(s); // accept and never reply
            }
        });
        let opts = ConnectionOptions {
            timeout_ms: Some(150),
            ..mock_options(port)
        };
        let start = std::time::Instant::now();
        let err = Connection::connect(opts)
            .await
            .expect_err("stalled HS reply must fail");
        assert!(matches!(err, Error::Timeout), "got {err:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn async_stalled_dial_maps_to_timeout_error() {
        // Dial-side parity: an unroutable address (TEST-NET-1, RFC 5737)
        // with a short timeout must fail as Error::Timeout, not hang. If
        // the environment instead refuses the route, an immediate Io error
        // is the same contract, so only a hang would be a defect.
        let opts = ConnectionOptions {
            timeout_ms: Some(200),
            ..mock_options(9)
        };
        let mut opts = opts;
        opts.host = "192.0.2.1".into();
        let start = std::time::Instant::now();
        let result = Connection::connect(opts).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "dial hung {elapsed:?}"
        );
        match result {
            // no route at all: refused fast — either way, never a hang
            Err(Error::Timeout | Error::Io(_)) => {}
            other => panic!("unexpected dial outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_zero_length_continuations_are_bounded() {
        // The chunk-count guard must be on the async loop too: an endless
        // stream of length-0 continuation frames would otherwise spin
        // forever (probe 2026-10-15) because the byte cap never trips.
        struct EndlessZerosAsync {
            buf: Vec<u8>,
            pos: usize,
        }
        impl EndlessZerosAsync {
            fn new() -> Self {
                let header = Header::request(MessageType::new(0, 0), 0, 0, CONTINUATION_BIT)
                    .encode()
                    .to_vec();
                Self {
                    buf: header,
                    pos: 0,
                }
            }
        }
        impl tokio::io::AsyncRead for EndlessZerosAsync {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                dst: &mut tokio::io::ReadBuf,
            ) -> std::task::Poll<std::io::Result<()>> {
                let n = (self.buf.len() - self.pos).min(dst.remaining());
                dst.put_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos >= self.buf.len() {
                    self.pos = 0; // endless: rewind, never EOF
                }
                std::task::Poll::Ready(Ok(()))
            }
        }
        let result = read_reply_async(&mut EndlessZerosAsync::new(), Some(0), None).await;
        assert!(
            matches!(&result, Err(Error::Protocol(m)) if m.contains("bound")),
            "async loop must cap continuation chunks, got {result:?}"
        );
    }
}
