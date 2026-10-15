//! Pure wire codecs: message framing and the `%List` item encoding.
//!
//! No IO lives here — everything is byte-in / byte-out, so a probe capture
//! can be replayed as a golden-frame test. The shapes are pinned against the
//! vendored reference drivers:
//!
//! - header/message framing: `go-irisnative src/connection/message_header.go`
//!   and `message.go`
//! - `%List` items: `go-irisnative src/list/listitem.go` plus
//!   `py-iris iris/_DBList.py` for the size-prefix and item-type rules

pub mod header;
pub mod list;
pub mod message;
