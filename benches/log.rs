//! The log layer: replaying a history, keeping one current as batches
//! arrive, rebuilding a past step, and sealing and verifying a hash chain.
//! Run: `cargo bench --all-features --bench log`.

use std::collections::BTreeMap;

use divan::{black_box, Bencher};
use sylva::log::chain::{replay as replay_chain, seal_batch};
use sylva::log::history::History;
use sylva::log::{replay, Batch, Header, Meta, Projection};
use uuid::Uuid;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Balances per account; an op that would go negative doesn't apply.
#[derive(Default, Clone)]
struct Ledger(BTreeMap<u32, i64>);

impl Projection for Ledger {
    type Op = (u32, i64);
    type Error = String;

    fn apply(&mut self, (account, delta): &(u32, i64), _: &Meta) -> Result<(), String> {
        let balance = self.0.entry(*account).or_default();
        if *balance + delta < 0 {
            return Err("would go negative".into());
        }
        *balance += delta;
        Ok(())
    }
}

/// `n` batches of 5 ops each, each after the one before.
fn history(n: usize) -> Vec<Batch<(u32, i64)>> {
    (0..n)
        .map(|b| Batch {
            header: Header {
                batch: Uuid::from_u128(b as u128 + 1),
                after: (b > 0).then(|| Uuid::from_u128(b as u128)),
                author: "user".into(),
                at: b as i64,
                message: None,
            },
            ops: (0..5)
                .map(|i| (((b * 5 + i) % 97) as u32, if i % 4 == 3 { -3 } else { 5 }))
                .collect(),
        })
        .collect()
}

const SIZES: [usize; 2] = [100, 10_000];

/// Replaying everything: what a server without `History` does per request.
#[divan::bench(args = SIZES)]
fn full_replay(bencher: Bencher, n: usize) {
    let h = history(n);
    bencher.bench_local(|| replay::<Ledger>(black_box(&h)).1.len());
}

/// Pushing `n` batches one at a time: what `History` does per write, amortized
/// (divan reports the rate per batch).
#[divan::bench(args = SIZES)]
fn history_push_each(bencher: Bencher, n: usize) {
    let h = history(n);
    bencher
        .counter(divan::counter::ItemsCount::new(n))
        .with_inputs(|| h.clone())
        .bench_local_values(|batches| {
            let mut history = History::<Ledger>::new(64);
            for b in batches {
                history.push(b).unwrap();
            }
            history
        });
}

/// The state halfway through a history (from the nearest snapshot).
#[divan::bench(args = SIZES)]
fn history_at_half(bencher: Bencher, n: usize) {
    let h = History::<Ledger>::from_batches(history(n), 64);
    bencher.bench_local(|| h.at(black_box(n / 2)).0 .0.len());
}

/// Sealing `n` ops into a hash chain, then verifying and replaying it.
#[divan::bench(args = SIZES)]
fn chain_seal_and_verify(bencher: Bencher, n: usize) {
    let ops: Vec<(u32, i64)> = (0..n).map(|i| ((i % 97) as u32, 1)).collect();
    bencher.bench_local(|| {
        let sealed = seal_batch(0, None, 0, &"user", ops.clone()).unwrap();
        let mut sum = 0;
        replay_chain(
            &sealed,
            u64::MAX,
            |(_, d): &(u32, i64)| -> Result<(), String> {
                sum += d;
                Ok(())
            },
        )
        .unwrap();
        sum
    });
}
