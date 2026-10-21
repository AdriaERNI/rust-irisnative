//! # irisnative
//!
//! A pure-Rust client for **`InterSystems IRIS`** that speaks the native
//! **Superserver wire protocol** directly — the same protocol `InterSystems`'s
//! own JDBC/Python/Node drivers use on TCP port **1972**. No vendor SDK, no
//! shared libraries, no web gateway, no HTTP: one TCP socket and this crate.
//!
//! ## What it is for
//!
//! IRIS is a multi-model database with an embedded Python and JavaScript
//! runtime in its kernel. `irisnative` lets any Rust program treat an IRIS
//! instance as a **remote compute engine**: SQL and global (M) access are
//! first-class, and class-method invocation — including methods written in
//! [Embedded Python] — runs trusted tool code server-side, next to the data.
//!
//! ## Quick start
//!
//! ```no_run
//! use irisnative::{connect, global, ConnectionOptions};
//!
//! # fn main() -> irisnative::Result<()> {
//! let mut conn = connect(ConnectionOptions::new("dbhost", "USER", "appuser", "secret"))?;
//!
//! // SQL
//! let mut rs = conn.query("SELECT Name FROM %Dictionary.ClassDefinition WHERE Super = 'Ens.Production'")?;
//! while rs.fetch_next()? {
//!     let name: String = rs.get(0)?;
//!     println!("{name}");
//! }
//!
//! // Globals (M-level access)
//! conn.set(&global!(Demo(1)), "hello")?;
//! let v: String = conn.get(&global!(Demo(1)))?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Feature flags
//!
//! - `async` — a tokio-backed `async::Connection` variant that shares the
//!   wire codec and command builders with the sync path (identical request
//!   bytes, proven by a fixture test). The consumer supplies the runtime;
//!   the sync API uses `std::net` only, whatever flags are on.
//!
//! ## Status
//!
//! **Pre-release.** The wire layer is ported against a live IRIS
//! (Community 2025.3) with `go-irisnative` as the readable reference
//! implementation. See `docs/protocol.md` for the compatibility matrix and
//! `README.md` for the protocol's legal posture before using this in
//! production.
//!
//! [Embedded Python]: https://docs.intersystems.com/irislatest/csp/docbook/DocBook.UI.Page.cls?KEY=GEPYTHON

pub mod wire;

/// Internal protocol layer, exposed for golden-frame tests and the async
/// bridge only; the stable surface is the crate root. Not a stability
/// guarantee: names here may move as the port evolves.
#[doc(hidden)]
pub mod connection;

use std::io::Write as _;

pub use connection::session::{ConnectionInfo, Session};

use connection::commands::ConnectParams;
use connection::driver::establish;
use connection::{globals, object, sql, streams, tx};

/// Connection options: host, port (default 1972), namespace, credentials.
#[derive(Debug, Clone)]
pub struct ConnectionOptions {
    /// IRIS host name or address.
    pub host: String,
    /// Superserver TCP port (`%Service_CP`), 1972 by default.
    pub port: u16,
    /// Target namespace, e.g. `USER`.
    pub namespace: String,
    /// IRIS username.
    pub username: String,
    /// IRIS password.
    pub password: String,
    /// Dial + per-read socket timeout; `None` = block forever. Applies to
    /// every socket read (not per-operation), matching how py's connect
    /// timeout and go's deadline behave. The async variant honors it as of
    /// this release.
    pub timeout_ms: Option<u64>,
}

impl ConnectionOptions {
    /// Options with the default superserver port.
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        namespace: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port: 1972,
            namespace: namespace.into(),
            username: username.into(),
            password: password.into(),
            timeout_ms: None,
        }
    }

    /// Override the superserver port.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

/// A global reference: `^Name(subscript...)`. Produced by [`global!`].
#[derive(Debug, Clone)]
pub struct Global {
    /// Global name without the leading caret.
    pub name: String,
    /// Subscript tree, each subscript a string (IRIS canonical form).
    pub subscripts: Vec<String>,
}

/// `global!(A)` / `global!(A(1, "x", 2))` — construct a [`Global`] reference.
#[macro_export]
macro_rules! global {
    ($name:ident) => {
        $crate::Global {
            name: stringify!($name).to_string(),
            subscripts: ::std::vec::Vec::new(),
        }
    };
    ($name:ident( $($sub:expr),* $(,)? )) => {
        $crate::Global {
            name: stringify!($name).to_string(),
            subscripts: ::std::vec![ $( ::std::format!("{}", $sub) ),* ],
        }
    };
}

/// A live IRIS native connection.
#[derive(Debug)]
pub struct Connection {
    stream: std::net::TcpStream,
    session: Session,
    tx_open: bool,
}

/// Client identity sent in the `CN` packet.
///
/// ref: go mod.go:connect sends ("go", "go-machine", "libirisnative");
/// we identify the crate honestly.
pub(crate) fn identity_for(host: &str) -> (String, String) {
    let machine = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "rust-machine".to_string());
    let ip = std::net::ToSocketAddrs::to_socket_addrs(&(host.to_string(), 0u16))
        .ok()
        .and_then(|mut it| it.next().map(|a| a.ip().to_string()))
        // An empty licensing host is rejected with CN status 98 on live
        // IRIS 2025.3 (probe-settled), so fall back to loopback.
        .unwrap_or_else(|| "127.0.0.1".to_string());
    (machine, ip)
}

/// Open a native connection: TCP dial, `HS` handshake, `CN` login.
///
/// # Errors
/// [`Error::Io`] on dial failure; [`Error::Auth`] on rejected credentials;
/// [`Error::Server`] / [`Error::Protocol`] on handshake/connect failures.
#[allow(clippy::needless_pass_by_value)] // owned options is the stable public signature
pub fn connect(opts: ConnectionOptions) -> Result<Connection> {
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(opts.host.as_str(), opts.port))
        .map_err(Error::Io)?;
    let stream = match opts.timeout_ms {
        Some(ms) => {
            let timeout = std::time::Duration::from_millis(ms);
            let mut last_err =
                std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no address resolved");
            let mut opened = None;
            for addr in addrs {
                match std::net::TcpStream::connect_timeout(&addr, timeout) {
                    Ok(s) => {
                        opened = Some(s);
                        break;
                    }
                    Err(e) => last_err = e,
                }
            }
            opened.ok_or_else(|| match last_err.kind() {
                std::io::ErrorKind::TimedOut => Error::Timeout,
                _ => Error::Io(last_err),
            })?
        }
        None => std::net::TcpStream::connect((opts.host.as_str(), opts.port))?,
    };
    let mut stream = stream;
    // Strict ping-pong request/response is exactly what Nagle taxes; go's
    // net.DialTCP gets TCP_NODELAY from Go's TCPConn default, so dial with
    // it too (parity: go mod.go Dial). Best-effort like the read timeout
    // below: a platform that refuses the option still has a working socket.
    let _ = stream.set_nodelay(true);
    if let Some(ms) = opts.timeout_ms {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(ms)));
    }
    let (machine, host_ip) = identity_for(&opts.host);
    let mut session = Session::default();
    let params = ConnectParams {
        namespace: &opts.namespace,
        user: &opts.username,
        password: &opts.password,
        machine: &machine,
        application: "libirisnative",
        host_ip: &host_ip,
    };
    establish(&mut stream, &mut session, &params)?;
    Ok(Connection {
        stream,
        session,
        tx_open: false,
    })
}

impl Connection {
    /// Negotiated connection facts (protocol, locale, feature gating).
    #[must_use]
    pub fn info(&self) -> &ConnectionInfo {
        &self.session.info
    }

    /// Server product/version banner (`$zversion` style) via
    /// `%SYSTEM.Version.GetVersion`.
    ///
    /// # Errors
    /// Transport, protocol, or server errors from the class-method call.
    pub fn server_version(&mut self) -> Result<String> {
        object::server_version(&mut self.stream, &mut self.session)
    }

    /// Run a SQL statement and return a forward-only row cursor.
    ///
    /// Statements starting with a DML/DDL keyword route to `DU` and return
    /// an empty cursor whose `rows_affected()` reports the count (go
    /// sql.go:Query/isUpdateQuery split).
    ///
    /// # Errors
    /// [`Error::Server`] with the SQLCODE and server text on failure.
    pub fn query(&mut self, sql_text: &str) -> Result<ResultSet<'_>> {
        self.query_params(sql_text, &[])
    }

    /// Run a parameterized SQL statement (`?` placeholders).
    ///
    /// Values are passed as canonical M strings; the server infers each
    /// parameter's type (bind type 99, as go's `toODBC` path does), so
    /// `"42"` binds into an INT column exactly. Binary/temporal typed
    /// binding is not exposed.
    ///
    /// # Errors
    /// As [`Connection::query`].
    pub fn query_params<'a>(
        &'a mut self,
        sql_text: &str,
        params: &[Option<String>],
    ) -> Result<ResultSet<'a>> {
        if is_update_query(sql_text) {
            let affected =
                sql::direct_update(&mut self.stream, &mut self.session, sql_text, params)?;
            return Ok(ResultSet {
                conn: self,
                state: None,
                row: Vec::new(),
                affected,
            });
        }
        let state = sql::direct_query(&mut self.stream, &mut self.session, sql_text, params, 0)?;
        Ok(ResultSet {
            conn: self,
            state: Some(state),
            row: Vec::new(),
            affected: 0,
        })
    }

    /// Run a DML/DDL statement; returns rows affected.
    ///
    /// # Errors
    /// [`Error::Server`] with the SQLCODE and server text on failure.
    pub fn execute(&mut self, sql_text: &str, params: &[Option<String>]) -> Result<i64> {
        sql::direct_update(&mut self.stream, &mut self.session, sql_text, params)
    }

    /// Set a global node.
    ///
    /// Global names and subscripts are not validated client-side: the
    /// server reports `<SYNTAX>`/`<UNDEFINED>` as [`Error::Server`],
    /// mirroring both reference drivers (go passes `Set(global)` verbatim;
    /// py forwards `globalName`).
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn set(&mut self, node: &Global, value: &str) -> Result<()> {
        globals::set(
            &mut self.stream,
            &mut self.session,
            &node.name,
            &node.subscripts,
            value,
        )
    }

    /// Get a global node's value.
    ///
    /// # Errors
    /// [`Error::Server`] (code 1009, `<UNDEFINED>`) when the node is
    /// undefined (this is deliberate behavior, matching the drivers).
    pub fn get<T: crate::value::Value>(&mut self, node: &Global) -> Result<T> {
        let raw = globals::get(
            &mut self.stream,
            &mut self.session,
            &node.name,
            &node.subscripts,
        )?;
        match raw {
            Some(s) => T::decode(&s),
            None => Err(Error::Server {
                code: -1009,
                message: "<UNDEFINED>".to_string(),
            }),
        }
    }

    /// Kill a global node (and its subtree).
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn kill(&mut self, node: &Global) -> Result<()> {
        globals::kill(
            &mut self.stream,
            &mut self.session,
            &node.name,
            &node.subscripts,
        )
    }

    /// Previous subscript in collation order (mirror of [`Connection::next`]).
    ///
    /// ref: go-irisnative src/connection/globals.go:GlobalPrev (direction 7).
    ///
    /// # Errors
    /// Transport/server failures; the walk ends with `Ok(None)`.
    #[must_use = "returns None at the start of the walk"]
    pub fn prev(&mut self, node: &mut Global) -> Result<Option<String>> {
        let parent_subs = if node.subscripts.is_empty() {
            Vec::new()
        } else {
            node.subscripts[..node.subscripts.len() - 1].to_vec()
        };
        let ind = node.subscripts.last().cloned().unwrap_or_default();
        let (stream, session) = self.parts();
        let found = globals::order(
            stream,
            session,
            &node.name,
            &parent_subs,
            &ind,
            globals::DIR_PREV,
        )?;
        if let Some(sub) = &found {
            let mut v = parent_subs.clone();
            v.push(sub.clone());
            node.subscripts = v;
        }
        Ok(found)
    }

    /// Subscript presence/`data` result for a node.
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn is_defined(&mut self, node: &Global) -> Result<bool> {
        let d = globals::data(
            &mut self.stream,
            &mut self.session,
            &node.name,
            &node.subscripts,
        )?;
        Ok(d.value || d.subtree)
    }

    /// Next subscript in collation order, updating `node.subscripts` in
    /// place. This is a sibling cursor at M `$ORDER` level (identical to
    /// go `GlobalNext`/`GlobalPrev`); a subtree-descent walk composes
    /// `next` + recursion at the call site.
    ///
    /// # Errors
    /// Transport/server failures; the walk ends with `Ok(None)`.
    #[must_use = "returns None at the end of the walk"]
    pub fn next(&mut self, node: &mut Global) -> Result<Option<String>> {
        let parent_subs = if node.subscripts.is_empty() {
            Vec::new()
        } else {
            node.subscripts[..node.subscripts.len() - 1].to_vec()
        };
        let ind = node.subscripts.last().cloned().unwrap_or_default();
        let found = globals::order(
            &mut self.stream,
            &mut self.session,
            &node.name,
            &parent_subs,
            &ind,
            globals::DIR_NEXT,
        )?;
        match found {
            Some(sub) => {
                if parent_subs.is_empty() {
                    node.subscripts = vec![sub.clone()];
                } else {
                    let mut v = parent_subs;
                    v.push(sub.clone());
                    node.subscripts = v;
                }
                Ok(Some(sub))
            }
            None => Ok(None),
        }
    }

    /// Invoke `##class(class).method(args...)` server-side (including
    /// `[Language = python]` methods — the internal-tools pattern).
    ///
    /// # Errors
    /// Transport/server failures; sysio error codes surface as
    /// [`Error::Server`].
    pub fn class_method(&mut self, class: &str, method: &str, args: &[String]) -> Result<String> {
        object::class_method(&mut self.stream, &mut self.session, class, method, args)
    }

    /// Invoke a class method and discard its result.
    ///
    /// # Errors
    /// As [`Connection::class_method`].
    pub fn class_method_void(&mut self, class: &str, method: &str, args: &[String]) -> Result<()> {
        object::class_method_void(&mut self.stream, &mut self.session, class, method, args)
    }

    /// Invoke an instance method on an object reference (OREF).
    ///
    /// # Errors
    /// As [`Connection::class_method`].
    pub fn method(&mut self, oref: &str, name: &str, args: &[String]) -> Result<String> {
        object::method(&mut self.stream, &mut self.session, oref, name, args, false)
    }

    /// Read a property of an OREF.
    ///
    /// # Errors
    /// As [`Connection::class_method`].
    pub fn property_get(&mut self, oref: &str, prop: &str) -> Result<String> {
        object::property_get(&mut self.stream, &mut self.session, oref, prop)
    }

    /// Write a property of an OREF.
    ///
    /// # Errors
    /// As [`Connection::class_method`].
    pub fn property_set(&mut self, oref: &str, prop: &str, value: &str) -> Result<()> {
        object::property_set(&mut self.stream, &mut self.session, oref, prop, value)
    }

    /// Begin a transaction (`START TRANSACTION` via `DU`). A nested begin is
    /// refused; requires the server-negotiated `DurableTransactions` bit.
    ///
    /// # Errors
    /// [`Error::Protocol`] on nesting or lack of server support.
    pub fn begin(&mut self) -> Result<()> {
        tx::begin(&mut self.stream, &mut self.session, &mut self.tx_open)
    }

    /// Commit with `TC`.
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn commit(&mut self) -> Result<()> {
        tx::commit(&mut self.stream, &mut self.session, &mut self.tx_open)
    }

    /// Rollback with `TR`.
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn rollback(&mut self) -> Result<()> {
        tx::rollback(&mut self.stream, &mut self.session, &mut self.tx_open)
    }

    /// Whether a transaction is currently open on this connection.
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.tx_open
    }

    /// Lock a global node server-side (`LOCK` 0xC24E). `mode` takes the M
    /// lock modifiers (`""` exclusive, `"S"` shared, `"E"` escalation);
    /// `timeout_secs` negative waits forever, `0` tries once, `n` waits up
    /// to `n` seconds. While the server waits, **this request blocks** —
    /// like the reference drivers, the wait is bounded by the timeout, not
    /// by spinning: a timed-out lock returns
    /// `Error::Server { code: 1240, .. }` with `<TIMEOUT>` text (live
    /// differential, same semantics as the official Python driver).
    /// Locks are per-connection server state and visible to **every**
    /// driver speaking the protocol.
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:lock
    ///
    /// # Errors
    /// Transport/server failures; [`Error::Server`] 1240 on timeout.
    pub fn lock(&mut self, node: &Global, mode: &str, timeout_secs: i64) -> Result<()> {
        let (stream, session) = self.parts();
        globals::lock(
            stream,
            session,
            &node.name,
            &node.subscripts,
            mode,
            timeout_secs,
        )
    }

    /// Release one lock count on a global node (`UNLOCK` 0xC24F; `mode`
    /// may add `I` immediate or `D` deferred).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:unlock
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn unlock(&mut self, node: &Global, mode: &str) -> Result<()> {
        let (stream, session) = self.parts();
        globals::unlock(stream, session, &node.name, &node.subscripts, mode)
    }

    /// Release every lock this connection holds (`UNLOCK_ALL` 0xC250).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:releaseAllLocks
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn release_all_locks(&mut self) -> Result<()> {
        let (stream, session) = self.parts();
        globals::release_all_locks(stream, session)
    }

    /// Atomically add `amount` to a global node server-side and return the
    /// new value (`INCREMENT` 0xC246) — the one wire command that makes
    /// counters race-free across connections, where a get-then-set
    /// composition would not. `amount` is a canonical numeric string
    /// (`"5"`, `"-2"`, `"3.5"`); other text is sent as a string the server
    /// coerces, mirroring the Python driver's typed marshalling.
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:increment
    ///
    /// # Errors
    /// Transport/server failures; [`Error::Server`] on a non-numeric node.
    pub fn increment<T: crate::value::Value>(&mut self, node: &Global, amount: &str) -> Result<T> {
        let (stream, session) = self.parts();
        let text = globals::increment(stream, session, &node.name, &node.subscripts, amount)?;
        T::decode(&text)
    }

    /// Open one nested transaction level (`TSTART` 0xC251). Unlike
    /// [`Connection::begin`], levels nest: each call raises `$TLEVEL`,
    /// [`Connection::tcommit`] drops one, and the outermost close commits
    /// everything (live-proven nesting 0→1→2, and one-level rollback keeps
    /// the outer level's writes).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:tStart
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn tstart(&mut self) -> Result<()> {
        let (stream, session) = self.parts();
        tx::tstart(stream, session)
    }

    /// End one nested transaction level (`TCOMMIT` 0xC252); at the last
    /// level this commits the whole transaction. Committing at level 0 is
    /// the server's error 1125, surfaced as [`Error::Server`] (capture).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:tCommit
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn tcommit(&mut self) -> Result<()> {
        let (stream, session) = self.parts();
        tx::tcommit(stream, session)
    }

    /// Roll back **all** open transaction levels (`TROLLBACK` 0xC253).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:tRollback
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn trollback(&mut self) -> Result<()> {
        let (stream, session) = self.parts();
        tx::trollback_all(stream, session)
    }

    /// Roll back only the innermost transaction level (`TROLLBACK_ONE`
    /// 0xC254) — the savepoint-style undo the SQL `TR` frame cannot
    /// express.
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:tRollbackOne
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn trollback_one(&mut self) -> Result<()> {
        let (stream, session) = self.parts();
        tx::trollback_one(stream, session)
    }

    /// Current transaction nesting depth (`GET_TLEVEL` 0xC255; 0 outside
    /// any transaction).
    ///
    /// ref: py-iris-legacy iris/_IRIS.py:getTLevel
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn tlevel(&mut self) -> Result<u32> {
        let (stream, session) = self.parts();
        tx::tlevel(stream, session)
    }

    /// Create a server-side stream object (`%Stream.*` classes' `%New`).
    /// SQL stream columns do **not** need this: `SELECT` results decode
    /// them transparently into [`Connection::query`] values.
    ///
    /// ref: go-irisnative src/connection/`message_header.go` (stream table);
    /// py-iris-legacy iris/_OutStream.py (object stream semantics)
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn create_stream(&mut self, kind: StreamKind) -> Result<Stream<'_>> {
        let oref = {
            let (stream, session) = self.parts();
            object::class_method(stream, session, kind.class_name(), "%New", &[])?
        };
        Ok(Stream {
            conn: self,
            oref,
            kind,
        })
    }

    /// Read raw stream content for a stream descriptor/handle bytewise
    /// (`JS` `READ_STREAM`, the reference drivers' fetch command). SQL stream
    /// columns are resolved automatically; this covers handles obtained
    /// elsewhere (an object reference, a stored descriptor). `size` of `-1`
    /// reads the whole stream. The server does not advance any position
    /// through this command (live capture).
    ///
    /// ref: go-irisnative src/connection/sql.go:fetchStream
    ///
    /// # Errors
    /// Transport/server failures; a bad handle surfaces the server status
    /// as [`Error::Server`].
    pub fn read_stream(&mut self, handle: &[u8], size: i64) -> Result<Vec<u8>> {
        let (stream, session) = self.parts();
        streams::read_stream(stream, session, handle, size)
    }

    /// Borrow stream + session together (split-borrow helper for the
    /// command drivers).
    pub(crate) fn parts(&mut self) -> (&mut std::net::TcpStream, &mut Session) {
        (&mut self.stream, &mut self.session)
    }
}

/// Which server-side stream class a [`Stream`] runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Persistent 8-bit stream (`%Stream.GlobalBinary`).
    Binary,
    /// Persistent text stream (`%Stream.GlobalCharacter`).
    Character,
    /// Job-scoped temporary 8-bit stream (`%Stream.TmpBinary`) — erased
    /// when the server job ends.
    TempBinary,
    /// Job-scoped temporary text stream (`%Stream.TmpCharacter`).
    TempCharacter,
}

impl StreamKind {
    /// The backing class name.
    #[must_use]
    pub const fn class_name(self) -> &'static str {
        match self {
            Self::Binary => "%Stream.GlobalBinary",
            Self::Character => "%Stream.GlobalCharacter",
            Self::TempBinary => "%Stream.TmpBinary",
            Self::TempCharacter => "%Stream.TmpCharacter",
        }
    }
}

/// A live server-side stream object, created by
/// [`Connection::create_stream`].
///
/// `write`/`read` are byte-preserving: pass binary content as a
/// byte-per-char string (`(0..=255u8).map(char::from)`, exactly how
/// global values already travel), and `read` hands bytes back.
/// `read(size)` fetches that many bytes **from the start** of the stream —
/// the wire `JS` command is position-less (live capture) — while `read`
/// with the server's own `Read` method advances the stream cursor
/// ([`Stream::read_text`], [`Stream::rewind`]).
#[derive(Debug)]
pub struct Stream<'a> {
    conn: &'a mut Connection,
    oref: String,
    kind: StreamKind,
}

impl Stream<'_> {
    /// The OREF text the server knows this stream by.
    #[must_use]
    pub fn oref(&self) -> &str {
        &self.oref
    }

    /// The backing class kind.
    #[must_use]
    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    /// Append `data` to the stream (`Write`); errors surface the `%Status`
    /// code the server returned.
    ///
    /// # Errors
    /// Transport/server failures; stream `%Status` errors.
    pub fn write(&mut self, data: &str) -> Result<()> {
        let (stream, session) = self.conn.parts();
        let status = object::method(
            stream,
            session,
            &self.oref,
            "Write",
            &[data.to_string()],
            false,
        )?;
        if streams::status_is_ok(&status) {
            Ok(())
        } else {
            Err(streams::status_error(&status))
        }
    }

    /// Read up to `size` bytes from the start of the stream via the wire
    /// `READ_STREAM` command (`JS`, the Go reference's fetch path —
    /// sql.go:fetchStream).
    ///
    /// # Errors
    /// Transport/server failures; nonzero stream status surfaces as
    /// [`Error::Server`].
    pub fn read(&mut self, size: u64) -> Result<Vec<u8>> {
        let (stream, session) = self.conn.parts();
        let size = i64::try_from(size)
            .map_err(|_| Error::Protocol("stream read size out of range".into()))?;
        streams::read_stream(stream, session, self.oref.as_bytes(), size)
    }

    /// Read the whole stream content.
    ///
    /// # Errors
    /// As [`Stream::read`].
    pub fn read_all(&mut self) -> Result<Vec<u8>> {
        let (stream, session) = self.conn.parts();
        streams::read_stream(stream, session, self.oref.as_bytes(), -1)
    }

    /// Read `count` characters through the server's `Read` method,
    /// advancing the stream cursor.
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn read_text(&mut self, count: u64) -> Result<String> {
        let (stream, session) = self.conn.parts();
        object::method(
            stream,
            session,
            &self.oref,
            "Read",
            &[count.to_string()],
            false,
        )
    }

    /// Rewind the server-side cursor to the start (`Rewind`).
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn rewind(&mut self) -> Result<()> {
        let (stream, session) = self.conn.parts();
        let status = object::method(stream, session, &self.oref, "Rewind", &[], false)?;
        if streams::status_is_ok(&status) {
            Ok(())
        } else {
            Err(streams::status_error(&status))
        }
    }

    /// Cut the stored content to `length` bytes/characters (the stream
    /// `Truncate` method).
    ///
    /// ref: go-irisnative src/connection/`message_header.go` (stream table);
    /// py-iris-legacy iris/_OutStream.py
    ///
    /// # Errors
    /// Transport/server failures; a bad length surfaces the server status.
    pub fn truncate(&mut self, length: i64) -> Result<()> {
        let (stream, session) = self.conn.parts();
        let status = object::method(
            stream,
            session,
            &self.oref,
            "Truncate",
            &[length.to_string()],
            false,
        )?;
        if streams::status_is_ok(&status) {
            Ok(())
        } else {
            Err(streams::status_error(&status))
        }
    }

    /// Total bytes/characters currently stored (the `Size` property).
    ///
    /// # Errors
    /// Transport/server failures.
    pub fn size(&mut self) -> Result<i64> {
        let (stream, session) = self.conn.parts();
        let text = object::property_get(stream, session, &self.oref, "Size")?;
        streams::decode_size(&text)
    }

    /// Persist a (non-temporary) stream so it survives this session
    /// (`%Save`).
    ///
    /// # Errors
    /// Transport/server failures; stream `%Status` errors.
    pub fn save(&mut self) -> Result<String> {
        let (stream, session) = self.conn.parts();
        let status = object::method(stream, session, &self.oref, "%Save", &[], false)?;
        if streams::status_is_ok(&status) {
            Ok(status)
        } else {
            Err(streams::status_error(&status))
        }
    }

    /// Release the server-side object (`%Close`). Temporary streams vanish
    /// with the connection anyway; closing frees the storage early.
    ///
    /// # Errors
    /// Transport/server failures; stream `%Status` errors.
    pub fn close(&mut self) -> Result<()> {
        let (stream, session) = self.conn.parts();
        let status = object::method(stream, session, &self.oref, "%Close", &[], false)?;
        if streams::status_is_ok(&status) {
            Ok(())
        } else {
            Err(streams::status_error(&status))
        }
    }
}

/// DML/DDL keyword routing for the `query()` entry point.
///
/// ref: go-irisnative src/connection/sql.go:isUpdateQuery.
#[must_use]
pub fn is_update_query(sql_text: &str) -> bool {
    let upper = sql_text.trim().to_uppercase();
    [
        "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT", "REVOKE",
        "MERGE", "REPLACE",
    ]
    .iter()
    .any(|kw| upper.starts_with(kw))
}

impl Drop for Connection {
    /// Send a best-effort `DC` disconnect before the socket closes.
    ///
    /// ref: go-irisnative src/connection/mod.go:Disconnect.
    fn drop(&mut self) {
        let count = self.session.next_count();
        let frame = connection::commands::build_disconnect(count);
        let _ = self.stream.write_all(&frame);
    }
}

/// Forward-only SQL row cursor.
#[derive(Debug)]
pub struct ResultSet<'a> {
    conn: &'a mut Connection,
    state: Option<sql::QueryState>,
    row: Vec<wire::list::Item>,
    affected: i64,
}

impl ResultSet<'_> {
    /// Advance to the next row; `false` at end of result.
    ///
    /// Named `fetch_next` (not `next`) deliberately: this is a cursor
    /// positioner, not an [`std::iter::Iterator`] — rows are pulled by index
    /// after each fetch.
    ///
    /// # Errors
    /// [`Error::Server`] when an SQLCODE outside {0, 100} appears in a data
    /// message, with the server text via `OE`.
    pub fn fetch_next(&mut self) -> Result<bool> {
        let Some(state) = self.state.as_mut() else {
            return Ok(false);
        };
        let (stream, session) = self.conn.parts();
        let row = sql::next_row(stream, session, state)?;
        if let Some(items) = row {
            self.row = items;
            // Stream columns arrive as `%List` descriptors; fetch their
            // content like both reference drivers do (transparent on the
            // wire, so a CLOB reads back as its text and a BLOB as bytes).
            let (stream, session) = self.conn.parts();
            streams::resolve_row(stream, session, &state.columns, &mut self.row)?;
            Ok(true)
        } else {
            self.row = Vec::new();
            Ok(false)
        }
    }

    /// Column `i` (0-based) of the current row, converted to `T`.
    ///
    /// # Errors
    /// [`Error::Protocol`] for out-of-range indices or unconvertible text.
    pub fn get<T: crate::value::Value>(&mut self, i: usize) -> Result<T> {
        let item = self
            .row
            .get(i)
            .ok_or_else(|| Error::Protocol(format!("column {i} out of range")))?;
        T::decode(&item.as_text())
    }

    /// Column count of the result (0 for DML cursors).
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.state.as_ref().map_or(0, |s| s.columns.len())
    }

    /// Column `i`'s reported name (empty for DML cursors).
    #[must_use]
    pub fn column_name(&self, i: usize) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|s| s.columns.get(i))
            .map(|c| c.name.as_str())
    }

    /// Rows affected for DML statements run through [`Connection::query`].
    #[must_use]
    pub fn rows_affected(&self) -> i64 {
        self.affected
    }
}

/// Value conversions for wire payloads.
pub mod value {
    /// A type decodable from an IRIS wire string value.
    ///
    /// IRIS values arrive typed on the wire; the text form (M canonical
    /// rendering) is the conversion base, matching the reference drivers'
    /// `asString` behavior.
    pub trait Value: Sized {
        /// Decode from the raw wire representation.
        ///
        /// # Errors
        /// [`Error::Protocol`](crate::Error::Protocol) on a convert failure.
        fn decode(raw: &str) -> crate::Result<Self>;
    }

    impl Value for String {
        fn decode(raw: &str) -> crate::Result<Self> {
            Ok(raw.to_string())
        }
    }

    impl Value for f64 {
        fn decode(raw: &str) -> crate::Result<Self> {
            raw.parse::<f64>()
                .map_err(|e| crate::Error::Protocol(e.to_string()))
        }
    }

    impl Value for i64 {
        fn decode(raw: &str) -> crate::Result<Self> {
            raw.parse::<i64>()
                .map_err(|e| crate::Error::Protocol(e.to_string()))
        }
    }

    impl Value for bool {
        fn decode(raw: &str) -> crate::Result<Self> {
            Ok(!raw.is_empty() && raw != "0")
        }
    }

    /// Byte-preserving decode for binary payloads (BLOB stream columns,
    /// binary globals): every char of the wire text must fit one byte, the
    /// same latin-1 discipline the codec's string items use.
    impl Value for Vec<u8> {
        fn decode(raw: &str) -> crate::Result<Self> {
            let mut out = Vec::with_capacity(raw.len());
            for ch in raw.chars() {
                let b = u8::try_from(ch).map_err(|_| {
                    crate::Error::Protocol(format!("byte-decode: char {ch:?} above 0xFF"))
                })?;
                out.push(b);
            }
            Ok(out)
        }
    }

    /// SQL NULL / absent value: `None` when the text is empty, `Some` else.
    pub struct Nullable<T>(pub Option<T>);

    impl<T: Value> Value for Nullable<T> {
        fn decode(raw: &str) -> crate::Result<Self> {
            if raw.is_empty() {
                Ok(Self(None))
            } else {
                T::decode(raw).map(|v| Self(Some(v)))
            }
        }
    }
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Crate errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying TCP/IO failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A dial or socket read exceeded `ConnectionOptions::timeout_ms`.
    ///
    /// py-iris raises `socket.timeout` and go surfaces
    /// `os.ErrDeadlineExceeded` distinctly; this is the same separation.
    /// The deadline applies to each socket read, not to a whole operation;
    /// a read cancelled by the deadline mid-reply can leave part of a frame
    /// unread, so the connection's reply stream is no longer dependable
    /// after this error — the next request normally surfaces it as
    /// `Error::Protocol` (reply-count desync); treat a timed-out connection
    /// as one to close and reconnect, as with any IO error.
    #[error("operation timed out")]
    Timeout,
    /// Server refused or terminated the authentication handshake.
    #[error("authentication failed")]
    Auth,
    /// A server status code with its IRIS message.
    #[error("iris status {code}: {message}")]
    Server {
        /// Raw IRIS `$status` / SQLCODE value.
        code: i64,
        /// Decoded message text.
        message: String,
    },
    /// Frame desynchronization or an unexpected reply shape.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// The tokio-backed connection variant (feature `async`).
#[cfg(feature = "async")]
pub use connection::asyncio as r#async;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn is_update_query_routes_like_go() {
        // ref: go sql.go isUpdateQuery
        assert!(is_update_query("  insert into T values (1)"));
        assert!(is_update_query("DELETE FROM T"));
        assert!(!is_update_query("SELECT 1"));
    }

    fn read_frame(s: &mut TcpStream) -> Option<([u8; 2], [u8; 4], Vec<u8>)> {
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
    fn write_reply(s: &mut TcpStream, type_word: [u8; 2], count: [u8; 4], body: &[u8]) {
        let mut frame = Vec::with_capacity(14 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&count);
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&type_word);
        frame.extend_from_slice(body);
        let _ = s.write_all(&frame);
        let _ = s.flush();
    }

    /// Answer one `HS` + `CN` pair like the live server (golden capture
    /// shapes), then serve `GLOBAL_GET`/`GLOBAL_SET` from a fixed value
    /// until the client sends `DC`. Offline socket-option/timeout probe.
    fn serve_mock(mut client: TcpStream) {
        let _ = client.set_nodelay(true);
        let Some((type_word, count, _)) = read_frame(&mut client) else {
            return;
        };
        if type_word != *b"HS" {
            return;
        }
        // protocol 69 + feature bits 3 + locale item "Unicode"
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
            let body: &[u8] = if type_word == [0x41, 0xC2] {
                &[0x04, 0x01, b'v', b'1'] // GLOBAL_GET -> "v1"
            } else {
                &[]
            };
            write_reply(&mut client, [0, 0xCA], count, body);
        }
    }

    fn mock_server() -> (u16, std::sync::mpsc::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                serve_mock(stream);
                let _ = tx.send(());
            }
        });
        (port, rx)
    }

    fn mock_options(port: u16) -> ConnectionOptions {
        ConnectionOptions::new("127.0.0.1", "USER", "_SYSTEM", "SYS").port(port)
    }

    #[test]
    fn sync_dial_sets_tcp_nodelay() {
        // The superserver protocol is strict ping-pong; Nagle must be off
        // like go's TCPConn default (go mod.go Dial via net.DialTCP).
        let (port, done) = mock_server();
        let mut conn = connect(mock_options(port)).expect("mock connect");
        assert!(
            conn.stream.nodelay().expect("nodelay queryable"),
            "the client socket must run TCP_NODELAY"
        );
        let v: String = conn.get(&global!(MOCK(1))).expect("mock get");
        assert_eq!(v, "v1");
        drop(conn); // sends DC; the mock exits its loop
        done.recv().expect("mock saw DC and closed");
    }

    #[test]
    fn stalled_read_maps_to_timeout_error() {
        // An expired per-read timeout must be its own error kind, not an
        // opaque WouldBlock IO error (py socket.timeout / go deadline).
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
        let err = connect(opts).expect_err("stalled HS reply must fail");
        assert!(matches!(err, Error::Timeout), "got {err:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }
}
