//! The one error type every layer returns: a change that doesn't apply, something missing,
//! a log that doesn't verify, or what serialization, IO and SQLite report.
//!
//! tend: sylva/Foundation

#[derive(Debug, thiserror::Error)]
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
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(feature = "sqlite")]
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}
