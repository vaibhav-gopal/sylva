//! A hash-chained log: one writer's ops in sequence, each sealing the hash
//! of the one before, so any edit to the past (or a missing op) is detected
//! when the chain is replayed.
//!
//! The hash covers `{seq, id, batch, parent, ts, actor, op}` as JSON, fields in
//! that order. With maps kept as ordered maps, that encoding is canonical.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Error, Result};

/// One sealed op.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Sealed<A, O> {
    /// 1-based position in the log.
    pub seq: u64,
    pub id: Uuid,
    /// Ops committed together share a batch id.
    pub batch: Uuid,
    /// Hash of the previous op; `None` only for seq 1.
    pub parent: Option<String>,
    /// Unix time in milliseconds.
    pub ts: i64,
    pub actor: A,
    pub op: O,
    pub hash: String,
}

#[derive(Serialize)]
struct Hashed<'a, A, O> {
    seq: u64,
    id: &'a Uuid,
    batch: &'a Uuid,
    parent: &'a Option<String>,
    ts: i64,
    actor: &'a A,
    op: &'a O,
}

/// The hash an op with these fields carries (hex SHA-256).
pub fn hash<A: Serialize, O: Serialize>(seq: u64, id: &Uuid, batch: &Uuid, parent: &Option<String>, ts: i64, actor: &A, op: &O) -> Result<String> {
    let bytes = serde_json::to_vec(&Hashed { seq, id, batch, parent, ts, actor, op })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

impl<A: Serialize, O: Serialize> Sealed<A, O> {
    pub fn seal(seq: u64, batch: Uuid, parent: Option<String>, ts: i64, actor: A, op: O) -> Result<Self> {
        let id = Uuid::now_v7();
        let hash = hash(seq, &id, &batch, &parent, ts, &actor, &op)?;
        Ok(Sealed { seq, id, batch, parent, ts, actor, op, hash })
    }

    pub fn compute_hash(&self) -> Result<String> {
        hash(self.seq, &self.id, &self.batch, &self.parent, self.ts, &self.actor, &self.op)
    }
}

/// Seals a batch of ops after `seq` and `head`, as one batch at `ts`.
pub fn seal_batch<A: Serialize + Clone, O: Serialize>(seq: u64, head: Option<String>, ts: i64, actor: &A, ops: Vec<O>) -> Result<Vec<Sealed<A, O>>> {
    let batch = Uuid::now_v7();
    let mut parent = head;
    let mut out = Vec::with_capacity(ops.len());
    for (i, op) in ops.into_iter().enumerate() {
        let o = Sealed::seal(seq + 1 + i as u64, batch, parent, ts, actor.clone(), op)?;
        parent = Some(o.hash.clone());
        out.push(o);
    }
    Ok(out)
}

/// Where a chain stands: the last op's position and hash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tip {
    pub seq: u64,
    pub head: Option<String>,
}

/// Checks that `o` follows `tip` (sequence, parent link, its own hash) and advances the tip.
pub fn check<A: Serialize, O: Serialize>(tip: &mut Tip, o: &Sealed<A, O>) -> Result<()> {
    let bad = |reason: &str| Error::Integrity { seq: o.seq, reason: reason.into() };
    if o.seq != tip.seq + 1 {
        return Err(bad("sequence gap"));
    }
    if o.parent != tip.head {
        return Err(bad("parent hash mismatch"));
    }
    if o.compute_hash()? != o.hash {
        return Err(bad("hash mismatch (operation was modified)"));
    }
    tip.seq = o.seq;
    tip.head = Some(o.hash.clone());
    Ok(())
}

/// Verifies the chain and applies each op, up to and including `until`
/// (`u64::MAX` for all of it). An op that doesn't apply is an integrity error.
pub fn replay<A: Serialize, O: Serialize, E: std::fmt::Display>(ops: &[Sealed<A, O>], until: u64, mut apply: impl FnMut(&O) -> std::result::Result<(), E>) -> Result<Tip> {
    let mut tip = Tip::default();
    for o in ops {
        check(&mut tip, o)?;
        if o.seq > until {
            break;
        }
        apply(&o.op).map_err(|e| Error::Integrity { seq: o.seq, reason: format!("replay failed: {e}") })?;
    }
    Ok(tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_edits_gaps_and_relinks() {
        let mut ops = seal_batch(0, None, 1, &"user", vec![1i64, 2, 3]).unwrap();
        let mut total = 0;
        let tip = replay(&ops, u64::MAX, |n: &i64| -> std::result::Result<(), String> {
            total += n;
            Ok(())
        })
        .unwrap();
        assert_eq!((tip.seq, total), (3, 6));
        assert_eq!(tip.head.as_ref(), Some(&ops[2].hash));
        let more = seal_batch(tip.seq, tip.head.clone(), 2, &"user", vec![4i64]).unwrap();
        assert_eq!(more[0].parent, tip.head);

        let ok = |_: &i64| -> std::result::Result<(), String> { Ok(()) };
        ops[1].op = 20;
        assert!(matches!(replay(&ops, u64::MAX, ok), Err(Error::Integrity { seq: 2, .. })), "edited");
        ops[1].op = 2;
        ops.remove(1);
        assert!(matches!(replay(&ops, u64::MAX, ok), Err(Error::Integrity { seq: 3, .. })), "gap");
        let failing = |_: &i64| -> std::result::Result<(), String> { Err("no".into()) };
        assert!(matches!(replay(&ops[..1], u64::MAX, failing), Err(Error::Integrity { seq: 1, .. })));
        assert!(replay(&ops[..1], 0, failing).is_ok(), "ops after `until` aren't applied");
    }

    #[test]
    fn the_hash_covers_fields_in_a_fixed_order() {
        // Pinned: a change to the encoding would make every existing log fail to verify.
        let id = Uuid::nil();
        let h = hash(1, &id, &id, &None, 0, &"user", &serde_json::json!({ "a": 1 })).unwrap();
        let expected = hex::encode(Sha256::digest(
            br#"{"seq":1,"id":"00000000-0000-0000-0000-000000000000","batch":"00000000-0000-0000-0000-000000000000","parent":null,"ts":0,"actor":"user","op":{"a":1}}"#,
        ));
        assert_eq!(h, expected);
    }
}
