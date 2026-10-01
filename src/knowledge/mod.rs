//! A knowledge model on the structures: typed property graphs and catalog
//! trees, built only from ops (so a [`crate::log`] can replay them), with
//! schemas, facts that remember whether they were stated or observed, and
//! references to entities owned elsewhere.

pub mod attrs;
pub mod graphs;
pub mod refs;
pub mod trees;

pub use attrs::{check_value, validate_attr, AttrDef, AttrType};
pub use refs::{validate_path, EntityRef};

use crate::error::invalid;
use crate::Result;

/// Trimmed, non-empty text.
pub(crate) fn text(s: &str, what: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() {
        return invalid(format!("{what} must not be empty"));
    }
    Ok(s.to_string())
}
