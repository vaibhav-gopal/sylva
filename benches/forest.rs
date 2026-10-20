//! The forest against a plain arena with a child list per node (the obvious hand-written tree)
//! and against `Tree`, on what a UI's passes do: build, walk everything, walk only dirty
//! subtrees, remove a subtree, and (for the forest) rebuild its flat preorder index.
//!
//! The shape: a root with 16 children per node, `n` nodes in all, numbered breadth first.
//! Run: `cargo bench --bench forest`.

use divan::{black_box, Bencher};
use sylva::forest::{Flat, Forest, Id, Visit};
use sylva::tree::Tree;

fn main() {
    divan::main();
}

const SIZES: [usize; 2] = [10_000, 100_000];

/// Parent of node `i` in the shape every structure is built with (0 is the root).
fn parent_of(i: usize) -> usize {
    (i - 1) / 16
}

fn build_forest(n: usize) -> (Forest, Vec<Id>) {
    let mut f = Forest::with_capacity(n);
    let mut ids = Vec::with_capacity(n);
    ids.push(f.push_root());
    for i in 1..n {
        let id = f.push_child(ids[parent_of(i)]).unwrap();
        ids.push(id);
    }
    (f, ids)
}

/// The obvious arena: a Vec of nodes, each with its children in a Vec.
struct Arena {
    children: Vec<Vec<u32>>,
}

fn build_arena(n: usize) -> Arena {
    let mut children = vec![Vec::new(); n];
    for i in 1..n {
        children[parent_of(i)].push(i as u32);
    }
    Arena { children }
}

impl Arena {
    fn preorder(&self, root: u32, stack: &mut Vec<u32>, mut f: impl FnMut(u32) -> Visit) {
        stack.clear();
        stack.push(root);
        while let Some(i) = stack.pop() {
            if f(i) == Visit::Enter {
                stack.extend(self.children[i as usize].iter().rev());
            }
        }
    }
}

fn build_tree(n: usize) -> Tree<u32, u32> {
    let mut t = Tree::new();
    t.insert(0, None, None, 0).unwrap();
    for i in 1..n {
        t.insert(i as u32, Some(parent_of(i) as u32), None, i as u32)
            .unwrap();
    }
    t
}

/// Which slots have something dirty at or below them, with 1% of nodes dirty.
fn dirty_below(f: &Forest, ids: &[Id]) -> Vec<bool> {
    let mut d = vec![false; f.slots()];
    for i in (0..ids.len()).step_by(100) {
        let mut j = ids[i];
        d[j.slot()] = true;
        while let Some(p) = f.parent(j) {
            d[p.slot()] = true;
            j = p;
        }
    }
    d
}

#[divan::bench(args = SIZES)]
fn build_forest_nodes(n: usize) -> Forest {
    build_forest(n).0
}

#[divan::bench(args = SIZES)]
fn build_arena_nodes(n: usize) -> Arena {
    build_arena(n)
}

#[divan::bench(args = SIZES)]
fn build_tree_nodes(n: usize) -> Tree<u32, u32> {
    build_tree(n)
}

#[divan::bench(args = SIZES)]
fn flat_rebuild(bencher: Bencher, n: usize) {
    let (mut f, ids) = build_forest(n);
    let mut flat = Flat::new();
    flat.update(&f);
    let mut first = true;
    bencher.bench_local(|| {
        // A structural change, as after reconciliation, then the rebuild. It alternates, since
        // a move to where the node already is changes nothing and rebuilds nothing.
        let before = if first { Some(ids[1]) } else { None };
        first = !first;
        f.move_to(ids[2], Some(ids[0]), before).unwrap();
        black_box(flat.update(&f))
    });
}

#[divan::bench(args = SIZES)]
fn flat_update_deep_insert(bencher: Bencher, n: usize) {
    let (mut f, ids) = build_forest(n);
    let mut flat = Flat::new();
    flat.update(&f);
    // A node near the bottom gains a child, then loses it: what reconciliation does most.
    let deep = ids[n / 2];
    let mut added: Option<Id> = None;
    bencher.bench_local(|| {
        match added.take() {
            Some(id) => {
                f.remove(id, |_| {}).unwrap();
            }
            None => added = Some(f.push_child(deep).unwrap()),
        }
        black_box(flat.update(&f))
    });
}

#[divan::bench(args = SIZES)]
fn flat_update_deep_reorder(bencher: Bencher, n: usize) {
    let (mut f, ids) = build_forest(n);
    let mut flat = Flat::new();
    flat.update(&f);
    // A parent one level above the leaves reorders its children.
    let parent = f.parent(ids[n - 1]).unwrap();
    bencher.bench_local(|| {
        let first = f.first_child(parent);
        let last = f.last_child(parent).unwrap();
        f.move_to(last, Some(parent), first).unwrap();
        black_box(flat.update(&f))
    });
}

#[divan::bench(args = SIZES)]
fn preorder_flat(bencher: Bencher, n: usize) {
    let (f, _) = build_forest(n);
    let mut flat = Flat::new();
    flat.update(&f);
    bencher.bench_local(|| {
        let mut sum = 0usize;
        for &s in flat.slots() {
            sum += s as usize;
        }
        black_box(sum)
    });
}

#[divan::bench(args = SIZES)]
fn preorder_links(bencher: Bencher, n: usize) {
    let (f, ids) = build_forest(n);
    bencher.bench_local(|| {
        let mut sum = 0usize;
        for id in f.preorder(ids[0]) {
            sum += id.slot();
        }
        black_box(sum)
    });
}

#[divan::bench(args = SIZES)]
fn preorder_arena(bencher: Bencher, n: usize) {
    let a = build_arena(n);
    let mut stack = Vec::with_capacity(n);
    bencher.bench_local(|| {
        let mut sum = 0usize;
        a.preorder(0, &mut stack, |i| {
            sum += i as usize;
            Visit::Enter
        });
        black_box(sum)
    });
}

#[divan::bench(args = SIZES)]
fn preorder_tree(bencher: Bencher, n: usize) {
    let t = build_tree(n);
    bencher.bench_local(|| {
        let mut sum = 0usize;
        for (k, _) in t.subtree(&0, |_, _| true) {
            sum += k as usize;
        }
        black_box(sum)
    });
}

#[divan::bench(args = SIZES)]
fn dirty_walk_flat(bencher: Bencher, n: usize) {
    let (f, ids) = build_forest(n);
    let dirty = dirty_below(&f, &ids);
    let mut flat = Flat::new();
    flat.update(&f);
    bencher.bench_local(|| {
        let mut visited = 0usize;
        flat.walk(|s| {
            visited += 1;
            if dirty[s as usize] {
                Visit::Enter
            } else {
                Visit::Skip
            }
        });
        black_box(visited)
    });
}

#[divan::bench(args = SIZES)]
fn dirty_walk_links(bencher: Bencher, n: usize) {
    let (f, ids) = build_forest(n);
    let dirty = dirty_below(&f, &ids);
    bencher.bench_local(|| {
        let mut visited = 0usize;
        f.walk(ids[0], |id| {
            visited += 1;
            if dirty[id.slot()] {
                Visit::Enter
            } else {
                Visit::Skip
            }
        });
        black_box(visited)
    });
}

#[divan::bench(args = SIZES)]
fn dirty_walk_arena(bencher: Bencher, n: usize) {
    let (f, ids) = build_forest(n);
    let dirty = dirty_below(&f, &ids);
    let a = build_arena(n);
    let mut stack = Vec::with_capacity(n);
    bencher.bench_local(|| {
        let mut visited = 0usize;
        a.preorder(0, &mut stack, |i| {
            visited += 1;
            if dirty[i as usize] {
                Visit::Enter
            } else {
                Visit::Skip
            }
        });
        black_box(visited)
    });
}

#[divan::bench(args = SIZES)]
fn remove_subtree_forest(bencher: Bencher, n: usize) {
    bencher
        .with_inputs(|| build_forest(n))
        .bench_local_values(|(mut f, ids)| {
            black_box(f.remove(ids[1], |_| {}).unwrap());
            f
        });
}

#[divan::bench(args = SIZES)]
fn remove_subtree_arena(bencher: Bencher, n: usize) {
    // The arena's remove: detach from the parent's list, then free the subtree's lists.
    let mut stack = Vec::with_capacity(n);
    bencher
        .with_inputs(|| build_arena(n))
        .bench_local_values(|mut a| {
            a.children[0].retain(|&c| c != 1);
            stack.clear();
            stack.push(1u32);
            let mut count = 0usize;
            while let Some(i) = stack.pop() {
                stack.extend(core::mem::take(&mut a.children[i as usize]));
                count += 1;
            }
            black_box(count);
            a
        });
}

#[divan::bench(args = SIZES)]
fn reorder_children_forest(bencher: Bencher, n: usize) {
    let (mut f, ids) = build_forest(n);
    bencher.bench_local(|| {
        // The root's last child to the front, as a keyed list reorders.
        let first = f.first_child(ids[0]);
        let last = f.last_child(ids[0]).unwrap();
        black_box(f.move_to(last, Some(ids[0]), first).unwrap())
    });
}

/// Child-list lengths for the reorder benchmarks below: the arena's move shifts the whole list, the
/// forest's relinks two neighbours whatever its length.
const WIDTHS: [usize; 3] = [16, 256, 4096];

#[divan::bench(args = WIDTHS)]
fn reorder_wide_forest(bencher: Bencher, width: usize) {
    let mut f = Forest::with_capacity(width + 1);
    let root = f.push_root();
    for _ in 0..width {
        f.push_child(root).unwrap();
    }
    bencher.bench_local(|| {
        let first = f.first_child(root);
        let last = f.last_child(root).unwrap();
        black_box(f.move_to(last, Some(root), first).unwrap())
    });
}

#[divan::bench(args = WIDTHS)]
fn reorder_wide_arena(bencher: Bencher, width: usize) {
    let mut list: Vec<u32> = (1..=width as u32).collect();
    bencher.bench_local(|| {
        let last = list.pop().unwrap();
        list.insert(0, last);
        black_box(list.len())
    });
}

#[divan::bench(args = SIZES)]
fn reorder_children_arena(bencher: Bencher, n: usize) {
    let mut a = build_arena(n);
    bencher.bench_local(|| {
        let list = &mut a.children[0];
        let last = list.pop().unwrap();
        list.insert(0, last);
        black_box(list.len())
    });
}
