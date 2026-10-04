//! user-service-switcher — shared logic for the `uss` CLI and the `ussd` daemon.
//!
//! The two binaries (`src/bin/uss.rs`, `src/bin/ussd.rs`) stay thin;
//! everything they share lives in this library, per spec §12: the protocol
//! types, the group state model + file IO, name validation, and the
//! exit-code/error model.

#![forbid(unsafe_code)]

pub mod error;
pub mod names;
pub mod protocol;
pub mod state;
pub mod systemdctl;

pub use error::{Error, ErrorCode, OpError, OpVerb};
