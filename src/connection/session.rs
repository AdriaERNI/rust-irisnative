//! Session state shared by the sync and async IO drivers.
//!
//! The drivers below both keep: a message counter incremented once per
//! send, a statement-id counter, and the negotiated
//! connection facts. Counter semantics: **go parity (+1)** — py's `+2`
//! sequence is irrelevant because the server only echoes our count back
//! (live-confirmed in `probes/captures/connect_globals.json`).
//!
//! ref: go-irisnative src/connection/mod.go:count / statementId.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
// Wire codecs convert protocol-bounded counts (item lengths, chunk counts)
// between usize and fixed-width ints; these values come from buffers this
// process sized, never from hostile input at these sites (server-supplied
// lengths are validated in wire::message before allocation).

use crate::wire::list::{Item, ListError, ListReader};

/// Server-negotiated facts captured during the handshake + connect.
///
/// The `CN` reply carries several further fields (delimited-id support,
/// isolation level, an SQL-empty-string flag); go keeps only the banner
/// and feature options and py stores the extras without acting on them —
/// autocommit and isolation are fixed in our `CN` request, so the ignored
/// fields cannot change client behavior and are not surfaced.
#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    /// Negotiated protocol version (≥ 61 enforced).
    pub protocol: u16,
    /// Server speaks unicode (handshake feature bit 0).
    pub unicode: bool,
    /// Compact doubles honored: protocol ≥ 65 **and** feature bit 1.
    ///
    /// ref: py-iris iris/_IRISConnection.py:_connect.
    pub compact_double: bool,
    /// Server locale string from the handshake reply.
    pub locale: String,
    /// Info banner from the connect reply.
    pub info: String,
    /// Server job number.
    pub server_job: String,
    /// Server-negotiated featureOptions — governs gating.
    pub feature_options: u32,
}

impl ConnectionInfo {
    /// `FastSelect` bit (1).
    /// ref: go mod.go:IsOptionFastSelect.
    #[must_use]
    pub fn fast_select(&self) -> bool {
        self.feature_options & 1 == 1
    }

    /// `DurableTransactions` bit (4).
    #[must_use]
    pub fn durable_transactions(&self) -> bool {
        self.feature_options & 4 == 4
    }

    /// `RedirectOutput` bit (32).
    #[must_use]
    pub fn redirect_output(&self) -> bool {
        self.feature_options & 32 == 32
    }
}

/// Per-connection sequencing state (no IO).
#[derive(Debug, Default)]
pub struct Session {
    /// Next message count.
    pub count: u32,
    /// Next statement id.
    pub statement: u32,
    /// Negotiated info.
    pub info: ConnectionInfo,
}

impl Session {
    /// Take the next message count (post-increment, go `count()`).
    pub fn next_count(&mut self) -> u32 {
        let c = self.count;
        self.count = self.count.wrapping_add(1);
        c
    }

    /// Take the next statement id (go `statementId()`).
    pub fn next_statement(&mut self) -> u32 {
        let s = self.statement;
        self.statement = self.statement.wrapping_add(1);
        s
    }
}

/// Read a text-ish item for connect/global payloads: bytes surface as the
/// lossy string the references produce, integers canonicalize to decimal.
///
/// ref: go listitem.go getString / asString.
#[must_use]
pub fn item_text(item: &Item) -> String {
    item.as_text()
}

/// Convert a list-decode error into the crate error type.
#[must_use]
pub fn list_err(e: &ListError) -> crate::Error {
    crate::Error::Protocol(e.to_string())
}

/// First item of a reply body as text (error-message decoding, OE, and the
/// sysio `<UNDEFINED>` shape).
pub fn first_text(body: &[u8]) -> Result<String, crate::Error> {
    match ListReader::new(body).next() {
        Some(Ok(item)) => Ok(item_text(&item)),
        Some(Err(e)) => Err(list_err(&e)),
        None => Ok(String::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_by_one_like_go() {
        // Q2 settled: server echoes our count, so any strictly monotonic
        // sequence works; go's +1 is the port choice.
        let mut s = Session::default();
        assert_eq!(s.next_count(), 0);
        assert_eq!(s.next_count(), 1);
        assert_eq!(s.next_statement(), 0);
        assert_eq!(s.next_statement(), 1);
    }

    #[test]
    fn feature_gating_predicates() {
        // ref: go mod.go IsOption*
        let i = ConnectionInfo {
            feature_options: 37,
            ..Default::default()
        };
        assert!(i.fast_select() && i.durable_transactions() && i.redirect_output());
        assert!(!ConnectionInfo::default().durable_transactions());
    }
}
