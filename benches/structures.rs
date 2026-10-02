//! The structures on their own: building, walking and looking up, at a small
//! and a large size. Fixtures come from a fixed seed, so every run measures
//! the same data. Run: `cargo bench --all-features --bench structures`.

use divan::{black_box, Bencher};
use sylva::graph::{Dir, Graph};
use sylva::tree::Tree;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// splitmix64: small, fast and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const SIZES: [usize; 2] = [1_000, 100_000];

/// `n` nodes and `3n` edges, skewed towards low-numbered hubs.
fn graph(n: usize) -> Graph<u32, u8, u32> {
    let mut rng = Rng(1);
    let mut g = Graph::new();
    for i in 0..n {
        g.add_node(i as u32, i as u32).unwrap();
    }
    let mut e = 0u32;
    while (e as usize) < 3 * n {
        let (a, b) = (rng.below(n), rng.below(n).min(rng.below(n)));
        if a != b {
            g.add_edge(e, a as u32, b as u32, (e % 3) as u8).unwrap();
            e += 1;
        }
    }
    g
}

/// `n` nodes, breadth first, 4–12 children each.
fn tree(n: usize) -> Tree<u32, u32> {
    let mut rng = Rng(2);
    let mut t = Tree::new();
    t.insert(0, None, None, 0).unwrap();
    let (mut next, mut parent) = (1u32, 0u32);
    while (next as usize) < n {
        for _ in 0..4 + rng.below(9) {
            if next as usize == n {
                break;
            }
            t.insert(next, Some(parent), None, next).unwrap();
            next += 1;
        }
        parent += 1;
    }
    t
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn graph_build(n: usize) -> Graph<u32, u8, u32> {
    graph(black_box(n))
}

#[divan::bench(args = SIZES)]
fn graph_walk_depth_3(bencher: Bencher, n: usize) {
    let g = graph(n);
    bencher.bench_local(|| g.walk(black_box(&1), Dir::Out, 3, |_| true).len());
}

#[divan::bench(args = SIZES)]
fn graph_who_points_at_the_hub(bencher: Bencher, n: usize) {
    let g = graph(n);
    bencher.bench_local(|| g.walk(black_box(&0), Dir::In, 1, |_| true).len());
}

#[divan::bench(args = SIZES)]
fn graph_shortest_path(bencher: Bencher, n: usize) {
    let g = graph(n);
    let far = (n - 1) as u32;
    bencher.bench_local(|| g.shortest_path(black_box(&far), &0, Dir::Both, 64, |_| true));
}

#[divan::bench(args = SIZES)]
fn graph_topological_order(bencher: Bencher, n: usize) {
    let g = graph(n);
    bencher.bench_local(|| g.topological_order().map_or_else(|e| e.len(), |o| o.len()));
}

#[divan::bench(args = SIZES, sample_count = 10)]
fn tree_build(n: usize) -> Tree<u32, u32> {
    tree(black_box(n))
}

#[divan::bench(args = SIZES)]
fn tree_depth_of_every_node(bencher: Bencher, n: usize) {
    let t = tree(n);
    let keys: Vec<u32> = t.keys().collect();
    bencher.bench_local(|| keys.iter().map(|k| t.lineage(k).count()).sum::<usize>());
}

#[divan::bench(args = SIZES)]
fn tree_preorder(bencher: Bencher, n: usize) {
    let t = tree(n);
    bencher.bench_local(|| t.preorder(|_, _| true).count());
}

#[divan::bench(args = SIZES)]
fn tree_moves(bencher: Bencher, n: usize) {
    let t = tree(n);
    let leaves: Vec<u32> = t
        .keys()
        .filter(|k| t.child_count(k) == 0)
        .take(100)
        .collect();
    bencher
        .with_inputs(|| t.clone())
        .bench_local_values(|mut t| {
            for (i, leaf) in leaves.iter().enumerate() {
                let _ = t.move_to(leaf, Some((i % 10) as u32), None);
            }
            t
        });
}
