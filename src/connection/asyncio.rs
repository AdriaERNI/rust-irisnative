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
