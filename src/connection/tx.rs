//! Transactions (ledger rows 30–32).
//!
//! ref: go-irisnative src/connection/mod.go:BeginTx / Commit / Rollback and
//! src/connection/transaction.go.
//!
//! `begin` executes `START TRANSACTION` through `DU` (go `BeginTx` — live
//! probe confirms no autocommit toggle is required first, settling Q4);
//! commit/rollback are the single-frame `TC` / `TR` messages.

use super::commands::{build_commit, build_rollback};
use super::driver::Transport;
use super::session::Session;
use super::sql::direct_update;
use crate::{Error, Result};

/// Begin a transaction; a second begin on an open transaction is refused
/// client-side the way go's `errMultipleTx` refuses it.
///
/// ref: go mod.go:BeginTx (tx flag guard).
pub fn begin<T: Transport>(t: &mut T, s: &mut Session, open: &mut bool) -> Result<()> {
    if *open {
        return Err(Error::Protocol(
            "multiple transactions: a transaction is already open".into(),
        ));
    }
    if !s.info.durable_transactions() {
        return Err(Error::Protocol(
            "server did not negotiate DurableTransactions (featureOptions bit 4)".into(),
        ));
    }
    direct_update(t, s, "START TRANSACTION", &[])?;
    *open = true;
    Ok(())
}

/// Commit with `TC`; consumes one reply.
///
/// ref: go mod.go:Commit.
pub fn commit<T: Transport>(t: &mut T, s: &mut Session, open: &mut bool) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_commit(c), c)?;
    if reply.status() != 0 && reply.sysio_code() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: super::session::first_text(&reply.body).unwrap_or_default(),
        });
    }
    *open = false;
    Ok(())
}

/// Rollback with `TR`; consumes one reply.
///
/// ref: go mod.go:Rollback.
pub fn rollback<T: Transport>(t: &mut T, s: &mut Session, open: &mut bool) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_rollback(c), c)?;
    if reply.status() != 0 && reply.sysio_code() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: super::session::first_text(&reply.body).unwrap_or_default(),
        });
    }
    *open = false;
    Ok(())
}
