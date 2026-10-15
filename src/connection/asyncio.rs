//! Tokio-backed connection variant (feature `async`; ledger row 33).
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

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream as TcpStreamAsync;

use super::commands::{self, ConnectParams};
use super::session::{Session, first_text};
use super::sql::{self as sqlcmd, QueryState};
use super::{MIN_PROTOCOL, globals};
use crate::wire::list::{Item, ListReader};
use crate::wire::message::Reply;
use crate::{ConnectionOptions, Error, Result};

/// Async transport state: buffered tokio socket + the same session struct
/// the sync `Connection` maintains.
#[derive(Debug)]
pub struct Connection {
    stream: BufStream<TcpStreamAsync>,
    session: Session,
    tx_open: bool,
}

impl Connection {
    /// Dial, handshake (`HS`), and login (`CN`) — async counterpart of
    /// [`crate::connect`].
    ///
    /// # Errors
    /// As [`crate::connect`].
    pub async fn connect(opts: ConnectionOptions) -> Result<Self> {
        let stream = TcpStreamAsync::connect((opts.host.as_str(), opts.port))
            .await
            .map_err(Error::Io)?;
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
        establish_async(&mut stream, &mut session, &params).await?;
        Ok(Self {
            stream,
            session,
            tx_open: false,
        })
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
        if reply.sysio_code() == globals::ERROR_UNDEFINED {
            return Ok(None);
        }
        super::driver::check_sysio(&reply)?;
        first_item_text(&reply.body)
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
        super::driver::check_sysio(&reply)
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
        super::driver::check_sysio(&reply)
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
        super::driver::check_sysio(&reply)?;
        Ok(first_item_text(&reply.body)?.unwrap_or_default())
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
        check_sqlcode_async(self, code).await?;
        let meta = sqlcmd::decode_meta(&reply.body);
        let reply = self.read_push(c).await?;
        let sqlcode = i16::try_from(reply.status()).unwrap_or(i16::MAX);
        check_sqlcode_async(self, sqlcode).await?;
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
            if st.sqlcode != 0 && st.sqlcode != 100 {
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
            if st.sqlcode != 0 && st.sqlcode != 100 {
                let message = self.error_text(st.sqlcode).await?;
                return Err(Error::Server {
                    code: i64::from(st.sqlcode),
                    message,
                });
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
        check_sqlcode_async(self, code).await?;
        Ok(sqlcmd::decode_du_affected(&reply.body))
    }

    /// `OE` server error text for an SQLCODE.
    async fn error_text(&mut self, sqlcode: i16) -> Result<String> {
        let c = self.session.next_count();
        let reply = self
            .roundtrip(&commands::build_error_info(sqlcode, c), c)
            .await?;
        Ok(first_item_text(&reply.body)?.unwrap_or_default())
    }

    /// `TC` commit.
    ///
    /// # Errors
    /// Transport/server failures.
    pub async fn commit(&mut self) -> Result<()> {
        let c = self.session.next_count();
        let reply = self.roundtrip(&commands::build_commit(c), c).await?;
        if reply.status() != 0 && reply.sysio_code() != 0 {
            return Err(Error::Server {
                code: i64::from(reply.status()),
                message: first_text(&reply.body).unwrap_or_default(),
            });
        }
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
        if reply.status() != 0 && reply.sysio_code() != 0 {
            return Err(Error::Server {
                code: i64::from(reply.status()),
                message: first_text(&reply.body).unwrap_or_default(),
            });
        }
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
        self.stream.write_all(frame).await.map_err(Error::Io)?;
        self.stream.flush().await.map_err(Error::Io)?;
        self.read_reply(Some(count)).await
    }

    async fn read_push(&mut self, count: u32) -> Result<Reply> {
        self.read_reply(Some(count)).await
    }

    async fn read_reply(&mut self, expect_count: Option<u32>) -> Result<Reply> {
        read_reply_async(&mut self.stream, expect_count).await
    }
}

impl Drop for Connection {
    /// Best-effort `DC` before the socket closes. `Drop` cannot await, so
    /// the frame goes into the `BufStream` buffer (`write` under a runtime
    /// context) and out with the stream flush at buffer teardown — the
    /// sync path is the authoritative disconnect (ledger row 11).
    fn drop(&mut self) {
        let count = self.session.next_count();
        let frame = commands::build_disconnect(count);
        let fut = tokio::io::AsyncWriteExt::write(&mut self.stream, &frame);
        tokio::pin!(fut);
        // BufStream::write completes synchronously into its buffer; poll
        // once with a no-op waker (no runtime needed at drop time).
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let _ = std::future::Future::poll(fut.as_mut(), &mut cx);
    }
}

async fn read_exact_async<S: AsyncReadExt + Unpin>(s: &mut S, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match s.read(&mut buf[filled..]).await.map_err(Error::Io)? {
            0 => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                )));
            }
            n => filled += n,
        }
    }
    Ok(())
}

fn first_item_text(body: &[u8]) -> Result<Option<String>> {
    match ListReader::new(body).next() {
        Some(Ok(item)) => Ok(Some(item.as_text())),
        Some(Err(e)) => Err(Error::Protocol(e.to_string())),
        None => Ok(None),
    }
}

/// Handshake + connect over an async transport, filling `session` with the
/// same step sequence as the sync `establish`.
async fn establish_async(
    stream: &mut BufStream<TcpStreamAsync>,
    session: &mut Session,
    p: &ConnectParams<'_>,
) -> Result<()> {
    // HS
    let c0 = session.next_count();
    let reply = roundtrip_stream(stream, &commands::build_handshake(), c0).await?;
    if reply.status() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: first_text(&reply.body).unwrap_or_default(),
        });
    }
    let proto = read_u16_at(&reply.body, 0)?;
    if proto < MIN_PROTOCOL {
        return Err(Error::Protocol(format!(
            "server protocol {proto} is below the minimum supported {MIN_PROTOCOL}"
        )));
    }
    let feature_bits = read_u16_at(&reply.body, 2)?;
    let locale = ListReader::new(reply.body.get(4..).unwrap_or(&[]))
        .next()
        .and_then(std::result::Result::ok)
        .map(|i| i.as_text())
        .ok_or_else(|| Error::Protocol("HS reply missing locale".into()))?;
    session.info.protocol = proto;
    session.info.unicode = feature_bits & 1 > 0;
    session.info.compact_double = proto >= 65 && feature_bits & 2 > 0;
    session.info.locale = locale;

    // CN
    let c1 = session.next_count();
    let reply = roundtrip_stream(stream, &commands::build_connect(p, c1), c1).await?;
    match reply.status() {
        0 => {}
        417 => return Err(Error::Auth),
        s => {
            return Err(Error::Server {
                code: i64::from(s),
                message: first_text(&reply.body).unwrap_or_default(),
            });
        }
    }
    // connect reply order (go mod.go:connect): info, delimited-ids, ignored,
    // isolation level, server job number, sql empty string, server features.
    let mut r = ListReader::new(&reply.body);
    session.info.info = r
        .next()
        .and_then(std::result::Result::ok)
        .map(|i| i.as_text())
        .unwrap_or_default();
    for _ in 0..3 {
        let _ = r.next();
    }
    session.info.server_job = r
        .next()
        .and_then(std::result::Result::ok)
        .map(|i| i.as_text())
        .unwrap_or_default();
    let _ = r.next(); // sql empty-string convention
    session.info.feature_options = match r.next().and_then(std::result::Result::ok) {
        Some(Item::Int(v)) => u32::try_from(v).unwrap_or(0),
        _ => 0,
    };
    Ok(())
}

async fn roundtrip_stream(
    stream: &mut BufStream<TcpStreamAsync>,
    frame: &[u8],
    count: u32,
) -> Result<Reply> {
    stream.write_all(frame).await.map_err(Error::Io)?;
    stream.flush().await.map_err(Error::Io)?;
    read_reply_async(stream, Some(count)).await
}

async fn read_reply_async(
    stream: &mut BufStream<TcpStreamAsync>,
    expect_count: Option<u32>,
) -> Result<Reply> {
    let mut head = [0u8; crate::wire::header::HEADER_SIZE];
    read_exact_async(stream, &mut head).await?;
    let header = crate::wire::header::Header::parse(&head);
    if let Some(want) = expect_count {
        if header.count != want {
            return Err(Error::Protocol(format!(
                "message count desync: expected {want}, got {}",
                header.count
            )));
        }
    }
    if header.length > crate::wire::message::MAX_FRAME_BODY {
        return Err(Error::Protocol("frame exceeds safety bound".into()));
    }
    let mut body = vec![0u8; header.length as usize];
    if header.length > 0 {
        read_exact_async(stream, &mut body).await?;
    }
    let mut more = header.is_continuation();
    while more {
        let mut next_head = [0u8; crate::wire::header::HEADER_SIZE];
        read_exact_async(stream, &mut next_head).await?;
        let nh = crate::wire::header::Header::parse(&next_head);
        if nh.length > crate::wire::message::MAX_FRAME_BODY {
            return Err(Error::Protocol("frame exceeds safety bound".into()));
        }
        let mut chunk = vec![0u8; nh.length as usize];
        if nh.length > 0 {
            read_exact_async(stream, &mut chunk).await?;
        }
        if body.len() + chunk.len() > crate::wire::message::MAX_TOTAL_BODY {
            return Err(Error::Protocol("coalesced body exceeds bound".into()));
        }
        body.extend_from_slice(&chunk);
        more = nh.is_continuation();
    }
    Ok(Reply { header, body })
}

/// Async twin of `sql::check_sqlcode` (same rule, OE fetch awaited).
async fn check_sqlcode_async(conn: &mut Connection, code: i16) -> Result<()> {
    if code == 0 || code == 100 {
        return Ok(());
    }
    let message = conn.error_text(code).await?;
    Err(Error::Server {
        code: i64::from(code),
        message,
    })
}

fn read_u16_at(body: &[u8], off: usize) -> Result<u16> {
    let pair = body
        .get(off..off + 2)
        .ok_or_else(|| Error::Protocol("HS reply truncated".into()))?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}
