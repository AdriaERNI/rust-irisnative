//! Connection layer: session state, handshake/auth, and command families.
//!
//! The command request builders are pure functions shared by the sync and
//! async IO drivers, so the `async` feature wraps IO only and never forks
//! protocol logic.

pub mod auth;
pub mod commands;
pub mod driver;
pub mod session;
pub mod sql;

pub mod globals;
pub mod object;
pub mod tx;

#[cfg(feature = "async")]
pub mod asyncio;

/// Negotiated protocol version sent in the handshake.
///
/// ref: go-irisnative `src/connection/mod.go:VERSION_PROTOCOL` = 69.
/// (py-iris negotiates 65 — the version trap; go's value is authoritative
/// for IRIS 2024+ servers and the probe confirmed 69 is accepted.)
pub const VERSION_PROTOCOL: u16 = 69;

/// Minimum protocol version the reference clients accept.
///
/// ref: py-iris iris/_IRISConnection.py:_connect (`if protocol < 61: raise`).
pub const MIN_PROTOCOL: u16 = 61;
