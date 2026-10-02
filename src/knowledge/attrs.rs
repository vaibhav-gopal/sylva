//! Typed attributes: what a catalog node declares for its descendants, and
//! what a graph schema declares for node and edge props.

use serde::{Deserialize, Serialize};

use super::refs::EntityRef;
use crate::error::invalid;
use crate::Result;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AttrType {
    Text,
    Number,
    /// One of `options`.
    Enum,
    Color,
    /// An integer scale, e.g. formality 1–5 (`min`/`max`).
    Scale,
    Date,
    Money,
    /// An EntityRef.
    Ref,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AttrDef {
    #[serde(rename = "type")]
    pub ty: AttrType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub max: Option<i64>,
    #[serde(default)]
    pub required: bool,
}

pub fn validate_attr(def: &AttrDef) -> Result<()> {
    match def.ty {
        AttrType::Enum if def.options.is_empty() => invalid("an enum attribute needs options"),
        AttrType::Scale => match (def.min, def.max) {
            (Some(a), Some(b)) if a < b => Ok(()),
            _ => invalid("a scale attribute needs min < max"),
        },
        _ => Ok(()),
    }
}

/// Checks a JSON value against an attribute definition (catalog items, graph props).
/// Money is `{"amount": <minor units>, "currency": "CAD"}`; dates are `YYYY-MM-DD`.
pub fn check_value(name: &str, def: &AttrDef, v: &serde_json::Value) -> Result<()> {
    use serde_json::Value;
    let bad = |want: &str| invalid(format!("'{name}' must be {want}"));
    match def.ty {
        AttrType::Text | AttrType::Color => {
            if v.is_string() {
                Ok(())
            } else {
                bad("text")
            }
        }
        AttrType::Number => {
            if v.is_number() {
                Ok(())
            } else {
                bad("a number")
            }
        }
        AttrType::Enum => match v.as_str() {
            Some(s) if def.options.iter().any(|o| o == s) => Ok(()),
            _ => bad(&format!("one of {}", def.options.join(", "))),
        },
        AttrType::Scale => match v.as_i64() {
            Some(n) if def.min.is_none_or(|m| n >= m) && def.max.is_none_or(|m| n <= m) => Ok(()),
            _ => bad(&format!(
                "an integer from {} to {}",
                def.min.unwrap_or(i64::MIN),
                def.max.unwrap_or(i64::MAX)
            )),
        },
        AttrType::Date => match v.as_str() {
            Some(s)
                if s.len() == 10
                    && s.as_bytes()[4] == b'-'
                    && s.as_bytes()[7] == b'-'
                    && s.chars().filter(char::is_ascii_digit).count() == 8 =>
            {
                Ok(())
            }
            _ => bad("a date (YYYY-MM-DD)"),
        },
        AttrType::Money => match v {
            Value::Object(m)
                if m.get("amount").is_some_and(Value::is_i64)
                    && m.get("currency")
                        .and_then(Value::as_str)
                        .is_some_and(|c| c.len() == 3) =>
            {
                Ok(())
            }
            _ => bad("money ({\"amount\": minor units, \"currency\": \"CAD\"})"),
        },
        AttrType::Ref => match v.as_str().map(str::parse::<EntityRef>) {
            Some(Ok(_)) => Ok(()),
            _ => bad("an entity reference"),
        },
    }
}
