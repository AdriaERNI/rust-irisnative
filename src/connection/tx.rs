//! Transactions.
//!
//! ref: go-irisnative src/connection/mod.go:BeginTx / Commit / Rollback and
//! src/connection/transaction.go.
//!
//! `begin` executes `START TRANSACTION` through `DU` (go `BeginTx` — live
//! probe confirms no autocommit toggle is required first, settling Q4);
//! commit/rollback are the single-frame `TC` / `TR` messages.

use super::commands::{build_commit, build_rollback, build_tlevel_command};
use super::driver::Transport;
use super::session::Session;
use super::sql::direct_update;
use crate::wire::header::{TX_COMMIT_LEVEL, TX_LEVEL, TX_ROLLBACK_ALL, TX_ROLLBACK_ONE, TX_START};
use crate::wire::list::ListReader;
use crate::wire::message::Reply;
use crate::{Error, Result};

/// Pure `TC`/`TR` reply check shared by the sync and async drivers: the
/// reply counts as success when either the raw status word or the
/// sysio-decoded word is 0 (one place, both drivers).
///
/// ref: go-irisnative src/connection/mod.go:Commit / Rollback.
pub fn check_tx_reply(reply: &Reply) -> Result<()> {
    if reply.status() != 0 && reply.sysio_code() != 0 {
        return Err(Error::Server {
            code: i64::from(reply.status()),
            message: super::session::first_text(&reply.body).unwrap_or_default(),
        });
    }
    Ok(())
}

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
    check_tx_reply(&reply)?;
    *open = false;
    Ok(())
}

/// Rollback with `TR`; consumes one reply.
///
/// ref: go mod.go:Rollback.
pub fn rollback<T: Transport>(t: &mut T, s: &mut Session, open: &mut bool) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_rollback(c), c)?;
    check_tx_reply(&reply)?;
    *open = false;
    Ok(())
}

/// Decode a body-less transaction-level sysio reply (success = 0).
/// Pure — shared by both drivers.
pub fn check_tlevel_reply(reply: &Reply) -> Result<()> {
    super::globals::decode_lock_reply(reply)
}

/// Decode a `GET_TLEVEL` reply: the current nesting depth.
pub fn decode_tlevel_reply(reply: &Reply) -> Result<u32> {
    check_tlevel_reply(reply)?;
    match ListReader::new(&reply.body).next() {
        Some(Ok(crate::wire::list::Item::Int(v))) => u32::try_from(v)
            .map_err(|_| Error::Protocol(format!("negative TLEVEL {v} from the server"))),
        Some(Ok(other)) => {
            other.as_text().trim().parse::<u32>().map_err(|e| {
                Error::Protocol(format!("unparsable TLEVEL {:?}: {e}", other.as_text()))
            })
        }
        Some(Err(e)) => Err(super::session::list_err(&e)),
        None => Ok(0),
    }
}

/// `TSTART` 0xC251: open one nested transaction level (`$TLEVEL` + 1).
/// Repeated calls nest — unlike [`begin`], which the SQL `START
/// TRANSACTION` path refuses to nest.
///
/// ref: py-iris-legacy iris/_IRIS.py:tStart.
pub fn tstart<T: Transport>(t: &mut T, s: &mut Session) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_tlevel_command(TX_START, c), c)?;
    check_tlevel_reply(&reply)
}

/// `TCOMMIT` 0xC252: end one transaction level; when the last closes, the
/// whole transaction commits. At level 0 the server answers 1125 (live
/// capture), surfaced as `Error::Server`.
///
/// ref: py-iris-legacy iris/_IRIS.py:tCommit.
pub fn tcommit<T: Transport>(t: &mut T, s: &mut Session) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_tlevel_command(TX_COMMIT_LEVEL, c), c)?;
    check_tlevel_reply(&reply)
}

/// `TROLLBACK` 0xC253: roll back every open level at once.
///
/// ref: py-iris-legacy iris/_IRIS.py:tRollback.
pub fn trollback_all<T: Transport>(t: &mut T, s: &mut Session) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_tlevel_command(TX_ROLLBACK_ALL, c), c)?;
    check_tlevel_reply(&reply)
}

/// `TROLLBACK_ONE` 0xC254: roll back the innermost level only, keeping the
/// outer levels' changes.
///
/// ref: py-iris-legacy iris/_IRIS.py:tRollbackOne.
pub fn trollback_one<T: Transport>(t: &mut T, s: &mut Session) -> Result<()> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_tlevel_command(TX_ROLLBACK_ONE, c), c)?;
    check_tlevel_reply(&reply)
}

/// `GET_TLEVEL` 0xC255: the current nesting depth (0 outside any
/// transaction).
///
/// ref: py-iris-legacy iris/_IRIS.py:getTLevel.
pub fn tlevel<T: Transport>(t: &mut T, s: &mut Session) -> Result<u32> {
    let c = s.next_count();
    let reply = t.roundtrip(&build_tlevel_command(TX_LEVEL, c), c)?;
    decode_tlevel_reply(&reply)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tlevel_tests {
    use super::*;
    use crate::wire::header::Header;
    use crate::wire::list::ListWriter;

    fn sysio_reply(code: i32, body: Vec<u8>) -> Reply {
        let status = u16::try_from(0xCA00i32 + code).unwrap_or(0);
        Reply {
            header: Header {
                length: u32::try_from(body.len()).unwrap_or_default(),
                count: 0,
                statement_id: 0,
                type_word: status.to_le_bytes(),
            },
            body,
        }
    }

    #[test]
    fn tlevel_decodes_int_and_text_items() {
        let mut w = ListWriter::new();
        w.set_int(2);
        assert_eq!(decode_tlevel_reply(&sysio_reply(0, w.buf)).unwrap(), 2);
        let mut w = ListWriter::new();
        w.set_str("1");
        assert_eq!(decode_tlevel_reply(&sysio_reply(0, w.buf)).unwrap(), 1);
        // empty body reads as level 0 (a bare sysio-0 reply)
        assert_eq!(decode_tlevel_reply(&sysio_reply(0, Vec::new())).unwrap(), 0);
    }

    #[test]
    fn tlevel_errors_surface_the_server_code() {
        // live capture: TCOMMIT at level 0 answers sysio 1125
        let e = check_tlevel_reply(&sysio_reply(1125, Vec::new())).expect_err("fails");
        assert!(matches!(e, Error::Server { code: 1125, .. }), "{e:?}");
    }
}
