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
use connection::{globals, object, sql, tx};

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
    /// Per-operation socket timeout hint; `None` = blocking default.
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
            opened.ok_or(Error::Io(last_err))?
        }
        None => std::net::TcpStream::connect((opts.host.as_str(), opts.port))?,
    };
    let mut stream = stream;
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
    /// undefined (acceptance A-2.1 documents this behavior).
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

    /// Next subscript in collation order (cursor-style subtree walk),
    /// updating `node.subscripts` in place.
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

    /// Borrow stream + session together (split-borrow helper for the
    /// command drivers).
    pub(crate) fn parts(&mut self) -> (&mut std::net::TcpStream, &mut Session) {
        (&mut self.stream, &mut self.session)
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

    #[test]
    fn is_update_query_routes_like_go() {
        // ref: go sql.go isUpdateQuery
        assert!(is_update_query("  insert into T values (1)"));
        assert!(is_update_query("DELETE FROM T"));
        assert!(!is_update_query("SELECT 1"));
    }
}
