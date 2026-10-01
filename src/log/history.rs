//! A projection kept current as batches arrive, with snapshots for the past.
//!
//! Appending a batch that follows the current head (the usual case) applies
//! just that batch. A batch that lands elsewhere in the causal order (a
//! merged branch) replays from the last snapshot before it. Any earlier step
//! is rebuilt from the nearest snapshot at or before it.

use std::collections::BTreeSet;

use uuid::Uuid;

use super::{apply_batch, order, Batch, Conflict, Projection};
use crate::error::invalid;
use crate::Result;

#[derive(Clone, Debug)]
pub struct History<P: Projection> {
    /// In causal order.
    batches: Vec<Batch<P::Op>>,
    ids: BTreeSet<Uuid>,
    state: P,
    conflicts: Vec<Conflict<P::Op>>,
    /// (batches replayed, state, conflicts so far), every `every` batches; the first is the empty state.
    snapshots: Vec<(usize, P, usize)>,
    every: usize,
}

impl<P> History<P>
where
    P: Projection + Clone,
    P::Op: Clone,
{
    /// An empty history taking a snapshot every `every` batches (at least 1).
    pub fn new(every: usize) -> Self {
        History { batches: Vec::new(), ids: BTreeSet::new(), state: P::default(), conflicts: Vec::new(), snapshots: vec![(0, P::default(), 0)], every: every.max(1) }
    }

    pub fn from_batches(batches: Vec<Batch<P::Op>>, every: usize) -> Self {
        let mut h = Self::new(every);
        let ordered: Vec<Batch<P::Op>> = order(&batches).into_iter().cloned().collect();
        h.ids = ordered.iter().map(|b| b.header.batch).collect();
        h.batches = ordered;
        h.rebuild_from(0);
        h
    }

    /// Adds a batch, keeping the causal order. A batch already here is an error.
    pub fn push(&mut self, batch: Batch<P::Op>) -> Result<()> {
        let id = batch.header.batch;
        if self.ids.contains(&id) {
            return invalid(format!("batch {id} is already in the history"));
        }
        let follows_head = match batch.header.after {
            Some(a) => self.head() == Some(a),
            None => self.batches.is_empty(),
        };
        self.ids.insert(id);
        if follows_head {
            apply_batch(&mut self.state, &batch, &mut self.conflicts);
            self.batches.push(batch);
            if self.batches.len().is_multiple_of(self.every) {
                self.snapshots.push((self.batches.len(), self.state.clone(), self.conflicts.len()));
            }
            return Ok(());
        }
        // Lands elsewhere in the order: re-order, and replay from where the order first differs.
        let mut all = std::mem::take(&mut self.batches);
        all.push(batch);
        let ordered: Vec<Batch<P::Op>> = order(&all).into_iter().cloned().collect();
        let first_change = all.iter().zip(&ordered).position(|(a, b)| a.header.batch != b.header.batch).unwrap_or(all.len() - 1);
        self.batches = ordered;
        self.rebuild_from(first_change);
        Ok(())
    }

    /// Recomputes state from the last snapshot at or before `step`.
    fn rebuild_from(&mut self, step: usize) {
        self.snapshots.retain(|(n, _, _)| *n <= step);
        let (n, state, c) = self.snapshots.last().cloned().expect("the empty snapshot is never dropped");
        self.state = state;
        self.conflicts.truncate(c);
        for (i, b) in self.batches.iter().enumerate().skip(n) {
            apply_batch(&mut self.state, b, &mut self.conflicts);
            let done = i + 1;
            if done.is_multiple_of(self.every) {
                self.snapshots.push((done, self.state.clone(), self.conflicts.len()));
            }
        }
    }

    /// The state after every batch.
    pub fn state(&self) -> &P {
        &self.state
    }

    pub fn conflicts(&self) -> &[Conflict<P::Op>] {
        &self.conflicts
    }

    /// The batches, in causal order.
    pub fn batches(&self) -> &[Batch<P::Op>] {
        &self.batches
    }

    pub fn len(&self) -> usize {
        self.batches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn contains(&self, batch: &Uuid) -> bool {
        self.ids.contains(batch)
    }

    /// The batch a new one should follow.
    pub fn head(&self) -> Option<Uuid> {
        self.batches.last().map(|b| b.header.batch)
    }

    /// The state (and conflicts) after the first `step` batches.
    pub fn at(&self, step: usize) -> (P, Vec<Conflict<P::Op>>) {
        let step = step.min(self.batches.len());
        if step == self.batches.len() {
            return (self.state.clone(), self.conflicts.clone());
        }
        let (n, state, c) = self.snapshots.iter().rev().find(|(n, _, _)| *n <= step).cloned().expect("the empty snapshot is first");
        let mut state = state;
        let mut conflicts = self.conflicts[..c].to_vec();
        for b in &self.batches[n..step] {
            apply_batch(&mut state, b, &mut conflicts);
        }
        (state, conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{batch, Counter};
    use super::super::replay;
    use super::*;

    #[test]
    fn appends_incrementally_and_rebuilds_the_past() {
        let mut h: History<Counter> = History::new(2);
        h.push(batch(1, None, vec![5])).unwrap();
        h.push(batch(2, Some(1), vec![-1])).unwrap();
        h.push(batch(3, Some(2), vec![10])).unwrap();
        assert_eq!(h.state(), &Counter(14));
        assert_eq!(h.at(1).0, Counter(5));
        assert_eq!(h.at(2).0, Counter(4));
        assert_eq!(h.at(0).0, Counter(0));
        assert!(h.push(batch(3, Some(2), vec![1])).is_err(), "no duplicates");
    }

    #[test]
    fn a_merged_branch_lands_in_causal_order() {
        let mut h: History<Counter> = History::new(1);
        for b in [batch(1, None, vec![5]), batch(2, Some(1), vec![-1]), batch(4, Some(2), vec![1])] {
            h.push(b).unwrap();
        }
        // 3 was written after 1 on another branch; it sorts before 4.
        h.push(batch(3, Some(1), vec![-5])).unwrap();
        let ids: Vec<u128> = h.batches().iter().map(|b| b.header.batch.as_u128()).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        let all: Vec<_> = h.batches().to_vec();
        let (replayed, conflicts): (Counter, _) = replay(&all);
        assert_eq!(h.state(), &replayed);
        assert_eq!(h.conflicts(), conflicts.as_slice());
        assert_eq!(h.conflicts().len(), 1, "-5 from 4 doesn't apply");
    }
}
