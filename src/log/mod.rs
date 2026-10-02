//! The log layer: state that remembers.
//!
//! Every change is an op; ops travel in [`Batch`]es; state is the replay of
//! the batches into a [`Projection`]. A batch names the newest batch its
//! author had seen (`after`), which gives a causal order that is the same
//! for any set of batches, so histories written on different branches merge
//! by simply taking both. An op that no longer applies after a merge isn't
//! fatal: it becomes a [`Conflict`], kept as data.
//!
//! - [`history::History`] keeps a projection current as batches arrive and
//!   rebuilds any earlier step from snapshots.
//! - [`files`] keeps batches as one file each in a directory (no merge conflicts).
//! - [`chain`] is the other shape of log: a single writer's hash-chained
//!   sequence of ops, where any edit to the past is detected.
//! - [`sign`] (feature `sign`) signs and verifies batches.

pub mod chain;
pub mod files;
pub mod history;
#[cfg(feature = "sign")]
pub mod sign;
#[cfg(feature = "sqlite")]
pub mod sqlite;

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Error, Result};

/// Who wrote a batch, when, and after what. The first line of a batch file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "BatchHeader"))]
pub struct Header {
    pub batch: Uuid,
    /// The newest batch the author had seen: gives the causal order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
    /// `user`, `agent:<name>`, `alfred:<model>` …
    pub author: String,
    /// Unix time in milliseconds.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Ops written together, applied in order.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Batch<O> {
    pub header: Header,
    pub ops: Vec<O>,
}

impl<O: Serialize + DeserializeOwned> Batch<O> {
    /// The header, then one op per line. This is also the canonical form a signature covers.
    pub fn to_jsonl(&self) -> String {
        let mut out = serde_json::to_string(&self.header).expect("a header serializes");
        for op in &self.ops {
            out.push('\n');
            out.push_str(&serde_json::to_string(op).expect("an op serializes"));
        }
        out.push('\n');
        out
    }

    pub fn from_jsonl(text: &str) -> Result<Self> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: Header = serde_json::from_str(
            lines
                .next()
                .ok_or_else(|| Error::Invalid("empty batch file".into()))?,
        )?;
        let ops = lines
            .map(serde_json::from_str)
            .collect::<std::result::Result<Vec<O>, _>>()?;
        Ok(Batch { header, ops })
    }
}

/// Causal order: a batch comes after the batch it names in `after`; among
/// batches ready at the same time, the smaller (earlier) id wins. Batches
/// whose `after` isn't present (not merged yet, or the very first) are roots.
/// The result is the same for any set of batches, in any input order.
pub fn order<O>(batches: &[Batch<O>]) -> Vec<&Batch<O>> {
    let ids: BTreeSet<Uuid> = batches.iter().map(|b| b.header.batch).collect();
    let mut children: BTreeMap<Uuid, Vec<&Batch<O>>> = BTreeMap::new();
    let mut ready: BTreeMap<Uuid, &Batch<O>> = BTreeMap::new();
    for b in batches {
        match b
            .header
            .after
            .filter(|a| ids.contains(a) && *a != b.header.batch)
        {
            Some(parent) => children.entry(parent).or_default().push(b),
            None => {
                ready.insert(b.header.batch, b);
            }
        }
    }
    let mut out = Vec::with_capacity(batches.len());
    while let Some((&id, _)) = ready.iter().next() {
        let b = ready.remove(&id).expect("present");
        out.push(b);
        for child in children.remove(&id).unwrap_or_default() {
            ready.insert(child.header.batch, child);
        }
    }
    // Anything left is in a cycle (corrupt `after` links): append in id order rather than lose it.
    let mut rest: Vec<&Batch<O>> = children.into_values().flatten().collect();
    rest.sort_by_key(|b| b.header.batch);
    out.extend(rest);
    out
}

/// The batch every new batch should name as `after`: the last one in causal order.
pub fn head<O>(batches: &[Batch<O>]) -> Option<Uuid> {
    order(batches).last().map(|b| b.header.batch)
}

/// What an op is applied with.
#[derive(Clone, Copy, Debug)]
pub struct Meta<'a> {
    pub batch: Uuid,
    pub author: &'a str,
    pub at: i64,
}

/// State built from ops. An `apply` that fails should leave the state as it was.
pub trait Projection: Default {
    type Op;
    type Error: std::fmt::Display;
    fn apply(&mut self, op: &Self::Op, meta: &Meta) -> std::result::Result<(), Self::Error>;
}

/// An op that didn't apply when the history was replayed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Conflict<O> {
    pub batch: Uuid,
    pub index: usize,
    pub op: O,
    pub reason: String,
}

/// Applies one batch, collecting the ops that don't apply.
pub fn apply_batch<P: Projection>(
    state: &mut P,
    batch: &Batch<P::Op>,
    conflicts: &mut Vec<Conflict<P::Op>>,
) where
    P::Op: Clone,
{
    let meta = Meta {
        batch: batch.header.batch,
        author: &batch.header.author,
        at: batch.header.at,
    };
    for (index, op) in batch.ops.iter().enumerate() {
        if let Err(e) = state.apply(op, &meta) {
            conflicts.push(Conflict {
                batch: batch.header.batch,
                index,
                op: op.clone(),
                reason: e.to_string(),
            });
        }
    }
}

/// Replays a history. Never fails: ops that no longer apply are returned as conflicts.
pub fn replay<P: Projection>(batches: &[Batch<P::Op>]) -> (P, Vec<Conflict<P::Op>>)
where
    P::Op: Clone,
{
    replay_ordered(order(batches))
}

/// Replays batches already in causal order.
pub fn replay_ordered<'a, P: Projection>(
    batches: impl IntoIterator<Item = &'a Batch<P::Op>>,
) -> (P, Vec<Conflict<P::Op>>)
where
    P::Op: Clone + 'a,
{
    let mut state = P::default();
    let mut conflicts = Vec::new();
    for b in batches {
        apply_batch(&mut state, b, &mut conflicts);
    }
    (state, conflicts)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A counter that refuses to go below zero.
    #[derive(Default, Clone, Debug, PartialEq)]
    pub struct Counter(pub i64);

    impl Projection for Counter {
        type Op = i64;
        type Error = String;

        fn apply(&mut self, op: &i64, _: &Meta) -> std::result::Result<(), String> {
            if self.0 + op < 0 {
                return Err(format!("{} would go below zero", self.0));
            }
            self.0 += op;
            Ok(())
        }
    }

    pub fn batch(n: u128, after: Option<u128>, ops: Vec<i64>) -> Batch<i64> {
        Batch {
            header: Header {
                batch: Uuid::from_u128(n),
                after: after.map(Uuid::from_u128),
                author: "user".into(),
                at: n as i64,
                message: None,
            },
            ops,
        }
    }

    #[test]
    fn orders_causally_and_merges_branches() {
        // 1 → 2 → 4 on one branch, 1 → 3 on another; 3 sorts before 4 once both are ready.
        let bs = vec![
            batch(4, Some(2), vec![1]),
            batch(3, Some(1), vec![-5]),
            batch(1, None, vec![5]),
            batch(2, Some(1), vec![-1]),
        ];
        let ids: Vec<u128> = order(&bs)
            .iter()
            .map(|b| b.header.batch.as_u128())
            .collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(head(&bs), Some(Uuid::from_u128(4)));
        // 5 - 1 = 4, then -5 doesn't apply (a conflict, not an error), then +1.
        let (state, conflicts): (Counter, _) = replay(&bs);
        assert_eq!(state, Counter(5));
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            (conflicts[0].batch, conflicts[0].op),
            (Uuid::from_u128(3), -5)
        );
    }

    #[test]
    fn jsonl_round_trips() {
        let b = batch(7, Some(6), vec![1, -2]);
        let text = b.to_jsonl();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(Batch::<i64>::from_jsonl(&text).unwrap(), b);
        assert!(Batch::<i64>::from_jsonl("").is_err());
    }
}
