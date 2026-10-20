//! The one error type every layer returns: a change that doesn't apply, something missing,
//! a log that doesn't verify, or what serialization, IO and SQLite report.
//!
//! tend: sylva/Foundation

use alloc::string::String;

/// Non-exhaustive: features add variants (`Json` with `log` or `knowledge`, `Io` with `std`, `Db`
/// with `sqlite`), and features unify across a build, so a match outside this crate needs a
/// catch-all arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The change doesn't apply (it would break a rule, or changes nothing).
    #[error("invalid operation: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// A stored log doesn't verify (a gap, a broken link, an edited op).
    #[error("integrity violation at seq {seq}: {reason}")]
    Integrity { seq: u64, reason: String },
    #[cfg(any(feature = "log", feature = "knowledge"))]
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "std")]
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(feature = "sqlite")]
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}
