//! Entity references: every entity is addressed as `<domain>/<kind>/<id>`
//! (e.g. `recount/entry/0193…`), files as `files:<path>`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::invalid;
use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityRef {
    Entity {
        domain: String,
        kind: String,
        id: Uuid,
    },
    File {
        path: String,
    },
}

impl fmt::Display for EntityRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntityRef::Entity { domain, kind, id } => write!(f, "{domain}/{kind}/{id}"),
            EntityRef::File { path } => write!(f, "files:{path}"),
        }
    }
}

impl FromStr for EntityRef {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if let Some(path) = s.strip_prefix("files:") {
            validate_path(path)?;
            return Ok(EntityRef::File {
                path: path.to_string(),
            });
        }
        let parts: Vec<&str> = s.split('/').collect();
        let ident = |p: &str| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
        };
        match parts.as_slice() {
            [domain, kind, id] if ident(domain) && ident(kind) => Ok(EntityRef::Entity {
                domain: domain.to_string(),
                kind: kind.to_string(),
                id: id
                    .parse()
                    .map_err(|_| Error::Invalid(format!("bad id in reference {s}")))?,
            }),
            _ => invalid(format!(
                "not an entity reference (domain/kind/id or files:path): {s}"
            )),
        }
    }
}

impl Serialize for EntityRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EntityRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A safe relative path (`/`-separated, no hidden, relative or empty
/// segments, no characters Windows can't store).
pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > 1024 {
        return invalid("document path must be 1-1024 bytes");
    }
    if path.contains('\\') {
        return invalid(format!("use '/' as the separator: {path}"));
    }
    for part in path.split('/') {
        if part.is_empty() {
            return invalid(format!("empty path segment in {path}"));
        }
        if part.starts_with('.') {
            return invalid(format!("hidden or relative segment '{part}' in {path}"));
        }
        if part.ends_with(' ') || part.ends_with('.') {
            return invalid(format!("segment '{part}' may not end with a space or dot"));
        }
        if part
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return invalid(format!("illegal character in {path}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_refs() {
        let id = Uuid::now_v7();
        let r: EntityRef = format!("recount/entry/{id}").parse().unwrap();
        assert_eq!(r.to_string(), format!("recount/entry/{id}"));
        let f: EntityRef = "files:finance/Receipts/2026/a.pdf".parse().unwrap();
        assert_eq!(
            f,
            EntityRef::File {
                path: "finance/Receipts/2026/a.pdf".into()
            }
        );
        for bad in [
            "recount/entry",
            "Recount/entry/x",
            "files:../x",
            "a/b/not-a-uuid",
        ] {
            assert!(bad.parse::<EntityRef>().is_err(), "{bad}");
        }
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            r#""files:finance/Receipts/2026/a.pdf""#
        );
    }

    #[test]
    fn rejects_unsafe_paths() {
        for bad in [
            "",
            "../x.md",
            "a//b.md",
            "/abs.md",
            ".obsidian/app.json",
            "a\\b.md",
            "C:/x.md",
            "a/./b.md",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_path("keep/Notes/a b.md").is_ok());
    }
}
