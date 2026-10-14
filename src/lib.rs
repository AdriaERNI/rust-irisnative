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
//! - `async` — planned surface, currently a no-op: the flag is declared in
//!   `Cargo.toml` and pulls in `tokio`, but no code in the crate is
//!   feature-gated yet, so enabling it changes nothing. The sync API uses
//!   `std::net` only, whatever flags are on.
//!
//! ## Status
//!
//! **Pre-release.** The wire layer is being ported against a live IRIS
//! (Community 2025.3) with `go-irisnative` as the readable reference
//! implementation. See `docs/protocol.md` for the compatibility matrix and
//! `README.md` for the protocol's legal posture before using this in
//! production.
//!
//! [Embedded Python]: https://docs.intersystems.com/irislatest/csp/docbook/DocBook.UI.Page.cls?KEY=GEPYTHON

// Module scaffolding: the wire layer lands here (frame codec, auth handshake,
// command dispatch) followed by the typed surface (globals, SQL, objects).

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
pub struct Connection;

impl Connection {
    /// Server product/version banner (`$zversion` style), once the handshake
    /// lands.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn server_version(&mut self) -> Result<String> {
        Err(Error::NotImplemented)
    }

    /// Run a SQL statement and return a forward-only row cursor.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn query(&mut self, _sql: &str) -> Result<ResultSet> {
        Err(Error::NotImplemented)
    }

    /// Set a global node.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn set(&mut self, _node: &Global, _value: &str) -> Result<()> {
        Err(Error::NotImplemented)
    }

    /// Get a global node's value.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported, or
    /// [`Error::Server`] when the node is undefined.
    pub fn get<T: crate::value::Value>(&mut self, _node: &Global) -> Result<T> {
        Err(Error::NotImplemented)
    }

    /// Kill a global node (and its subtree).
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn kill(&mut self, _node: &Global) -> Result<()> {
        Err(Error::NotImplemented)
    }

    /// Next subscript in collation order (cursor-style subtree walk).
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn next(&mut self, _node: &mut Global) -> Option<String> {
        None
    }

    /// Invoke `##class(class).method(args...)` server-side (including
    /// `[Language = python]` methods — the internal-tools pattern).
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn class_method(
        &mut self,
        _class: &str,
        _method: &str,
        _args: &[String],
    ) -> Result<String> {
        Err(Error::NotImplemented)
    }
}

/// Forward-only SQL row cursor.
#[derive(Debug)]
pub struct ResultSet;

impl ResultSet {
    /// Advance to the next row; `false` at end of result.
    ///
    /// Named `fetch_next` (not `next`) deliberately: this is a cursor
    /// positioner, not an [`std::iter::Iterator`] — rows are pulled by index
    /// after each fetch.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn fetch_next(&mut self) -> Result<bool> {
        Err(Error::NotImplemented)
    }

    /// Column `i` (0-based) of the current row, converted to `T`.
    ///
    /// # Errors
    /// [`Error::NotImplemented`] until the wire layer is ported.
    pub fn get<T: crate::value::Value>(&mut self, _i: usize) -> Result<T> {
        Err(Error::NotImplemented)
    }
}

/// Value conversions for wire payloads.
pub mod value {
    /// A type decodable from an IRIS wire string value.
    ///
    /// IRIS is string-typed on the wire; numeric conversion follows M
    /// canonical rules once implemented.
    pub trait Value: Sized {
        /// Decode from the raw wire representation.
        ///
        /// # Errors
        /// [`Error::Server`](crate::Error::Server) on a type/convert failure.
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
}

/// Placeholder constructor — the wire handshake lands in `connection.rs`.
///
/// # Errors
/// [`Error::Io`] on connect failure; protocol/auth errors once implemented.
pub fn connect(_opts: ConnectionOptions) -> Result<Connection> {
    Err(Error::NotImplemented)
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Crate errors. The precise protocol variants are added as the wire layer is
/// ported; consumers must not match on exhaustiveness yet.
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
        /// Raw IRIS `$status` value.
        code: i64,
        /// Decoded message text.
        message: String,
    },
    /// Frame desynchronization or an unexpected reply shape.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Functionality not yet implemented (pre-release surface).
    #[error("not implemented yet")]
    NotImplemented,
}
