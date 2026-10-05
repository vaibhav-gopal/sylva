//! A forest for trees walked every frame: generational ids, intrusive links, and per-node data
//! in columns.
//!
//! [`Tree`](crate::tree::Tree) suits knowledge trees: keys you choose, an ordered index, a list
//! of children per node, iteration in key order. A UI's retained tree needs something else:
//!
//! - **Generational ids.** An [`Id`] is a slot index and the slot's generation. Removing a node
//!   bumps the generation, so a stale id fails safely instead of reaching whatever reused the
//!   slot. Freed slots are reused from a free list.
//! - **Intrusive links, for changing the tree.** Each node has a parent, first and last child, and
//!   previous and next sibling, in one record per node, so inserting, moving and detaching are
//!   O(1), removing is O(subtree), and nothing allocates per node or per child.
//! - **A flat preorder index, for walking it.** Following links chases pointers: each step waits
//!   for the last load. [`Flat`] lists every slot in preorder with where each subtree ends, so a
//!   full pass reads one contiguous array and a dirty walk jumps over clean subtrees in one step.
//!   It's rebuilt (O(n), no allocation once warm) only when the structure changed.
//! - **Column storage.** The forest owns only structure. Per-node data lives in [`Column`]s
//!   indexed by the dense slot ([`Id::slot`]), structure-of-arrays, so a pass reads only the
//!   columns it needs.
//!
//! Walks follow tree order (the sibling links). Only column sweeps read in slot order, so nothing
//! that should replay the same everywhere depends on how slots were reused.
//!
//! ```
//! use sylva::forest::{Column, Flat, Forest, Visit};
//!
//! let mut f = Forest::new();
//! let root = f.push_root();
//! let child = f.push_child(root)?;
//! let mut width: Column<f32> = Column::new();
//! width.fit(&f);
//! *width.get_mut(child) = 120.0;
//!
//! let mut flat = Flat::new();
//! flat.update(&f); // rebuilds only when the structure changed
//! let mut seen = Vec::new();
//! flat.walk(|slot| {
//!     seen.push(slot);
//!     if slot as usize == root.slot() { Visit::Enter } else { Visit::Skip }
//! });
//! assert_eq!(seen.len(), 2);
//! # Ok::<(), sylva::Error>(())
//! ```
//!
//! tend: sylva/Structures/Forest

use crate::error::invalid;
use crate::{Error, Result};

const NONE: u32 = u32::MAX;

/// A node's id: its slot and the slot's generation when the node was made.
///
/// A slot's generation is even while a node lives in it and odd while it's free, so an id (always
/// even) never matches a free slot. It wraps after 2^31 reuses of one slot: only then could a
/// stale id match again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id {
    index: u32,
    generation: u32,
}

impl Id {
    /// The dense slot this id refers to (the index into [`Column`]s).
    pub fn slot(self) -> usize {
        self.index as usize
    }
}

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

/// What a walk does at a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visit {
    /// Visit its children too.
    Enter,
    /// Skip everything below it.
    Skip,
}

/// A node's links: parent, first and last child, previous and next sibling (`NONE` for none).
#[derive(Clone, Copy, Debug)]
struct Links {
    parent: u32,
    first: u32,
    last: u32,
    prev: u32,
    next: u32,
}

const UNLINKED: Links = Links {
    parent: NONE,
    first: NONE,
    last: NONE,
    prev: NONE,
    next: NONE,
};

/// A forest of nodes with generational ids, intrusive links (one record per node) and
/// traversals that allocate nothing. [`Flat`] gives passes a contiguous preorder view of it.
#[derive(Clone, Debug)]
pub struct Forest {
    links: Vec<Links>,
    /// Per slot: even while a node lives there, odd while it's free.
    generation: Vec<u32>,
    free: Vec<u32>,
    first_root: u32,
    last_root: u32,
    len: usize,
    /// Bumped by every change to the structure, so a [`Flat`] knows when it's stale.
    version: u64,
}

impl Default for Forest {
    fn default() -> Self {
        Forest::new()
    }
}

impl Forest {
    /// An empty forest.
    pub fn new() -> Forest {
        Forest {
            links: Vec::new(),
            generation: Vec::new(),
            free: Vec::new(),
            first_root: NONE,
            last_root: NONE,
            len: 0,
            version: 0,
        }
    }

    /// An empty forest with room for `n` nodes before it reallocates.
    pub fn with_capacity(n: usize) -> Forest {
        let mut f = Forest::new();
        f.links.reserve(n);
        f.generation.reserve(n);
        f
    }

    /// How many nodes are alive.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the forest has no nodes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of slots, alive or free: the length columns must have.
    pub fn slots(&self) -> usize {
        self.links.len()
    }

    /// Changes whenever the structure changes (a node added, moved or removed).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether `id` refers to a live node (not removed, not a reused slot).
    pub fn contains(&self, id: Id) -> bool {
        self.generation.get(id.index as usize) == Some(&id.generation)
    }

    fn check(&self, id: Id) -> Result<u32> {
        if self.contains(id) {
            Ok(id.index)
        } else {
            Err(Error::NotFound(format!("node {id}")))
        }
    }

    fn id(&self, index: u32) -> Option<Id> {
        (index != NONE).then(|| Id {
            index,
            generation: self.generation[index as usize],
        })
    }

    /// The id of the live node in `slot` (for walks over [`Flat`], which yield slots).
    pub fn id_at(&self, slot: usize) -> Option<Id> {
        let generation = *self.generation.get(slot)?;
        (generation % 2 == 0).then_some(Id {
            index: slot as u32,
            generation,
        })
    }

    fn allocate(&mut self) -> u32 {
        self.len += 1;
        self.version += 1;
        if let Some(i) = self.free.pop() {
            let g = &mut self.generation[i as usize];
            *g = g.wrapping_add(1);
            return i;
        }
        let i = self.links.len() as u32;
        assert!(i != NONE, "forest full");
        self.links.push(UNLINKED);
        self.generation.push(0);
        i
    }

    /// Links `i` (unlinked) under `parent` (`NONE`: the root list), before `before` (`NONE`: last).
    fn link(&mut self, i: u32, parent: u32, before: u32) {
        let prev = if before == NONE {
            if parent == NONE {
                self.last_root
            } else {
                self.links[parent as usize].last
            }
        } else {
            self.links[before as usize].prev
        };
        let s = i as usize;
        self.links[s].parent = parent;
        self.links[s].prev = prev;
        self.links[s].next = before;
        match prev {
            NONE if parent == NONE => self.first_root = i,
            NONE => self.links[parent as usize].first = i,
            p => self.links[p as usize].next = i,
        }
        match before {
            NONE if parent == NONE => self.last_root = i,
            NONE => self.links[parent as usize].last = i,
            b => self.links[b as usize].prev = i,
        }
    }

    /// Takes `i` out of its sibling list; its subtree stays attached to it.
    fn unlink(&mut self, i: u32) {
        let s = i as usize;
        let Links {
            parent, prev, next, ..
        } = self.links[s];
        match prev {
            NONE if parent == NONE => self.first_root = next,
            NONE => self.links[parent as usize].first = next,
            p => self.links[p as usize].next = next,
        }
        match next {
            NONE if parent == NONE => self.last_root = prev,
            NONE => self.links[parent as usize].last = prev,
            n => self.links[n as usize].prev = prev,
        }
        self.links[s].parent = NONE;
        self.links[s].prev = NONE;
        self.links[s].next = NONE;
    }

    /// Adds a root, after the existing roots.
    pub fn push_root(&mut self) -> Id {
        let i = self.allocate();
        self.link(i, NONE, NONE);
        self.id(i).expect("just allocated")
    }

    /// Adds a node as `parent`'s last child.
    pub fn push_child(&mut self, parent: Id) -> Result<Id> {
        let p = self.check(parent)?;
        let i = self.allocate();
        self.link(i, p, NONE);
        Ok(self.id(i).expect("just allocated"))
    }

    /// Adds a node just before `sibling`, under the same parent (or among the roots).
    pub fn insert_before(&mut self, sibling: Id) -> Result<Id> {
        let s = self.check(sibling)?;
        let parent = self.links[s as usize].parent;
        let i = self.allocate();
        self.link(i, parent, s);
        Ok(self.id(i).expect("just allocated"))
    }

    /// Adds a node just after `sibling`, under the same parent (or among the roots).
    pub fn insert_after(&mut self, sibling: Id) -> Result<Id> {
        let s = self.check(sibling)?;
        let Links { parent, next, .. } = self.links[s as usize];
        let i = self.allocate();
        self.link(i, parent, next);
        Ok(self.id(i).expect("just allocated"))
    }

    /// Moves `id` (with its subtree) under `parent` (`None`: among the roots), before `before`
    /// (`None`: last). Fails, changing nothing, if `parent` is `id` or lies inside its subtree, or
    /// if `before` isn't a child of `parent`; returns `false` when `id` is already there.
    pub fn move_to(&mut self, id: Id, parent: Option<Id>, before: Option<Id>) -> Result<bool> {
        let i = self.check(id)?;
        let p = parent.map_or(Ok(NONE), |p| self.check(p))?;
        let b = before.map_or(Ok(NONE), |b| self.check(b))?;
        if b == i {
            return invalid(format!("can't move {id} before itself"));
        }
        if b != NONE && self.links[b as usize].parent != p {
            return invalid(format!(
                "{} isn't a child of the new parent",
                self.id(b).expect("checked")
            ));
        }
        let mut a = p;
        while a != NONE {
            if a == i {
                return invalid(format!("can't move {id} into its own subtree"));
            }
            a = self.links[a as usize].parent;
        }
        let here = self.links[i as usize];
        if here.parent == p && here.next == b {
            return Ok(false);
        }
        self.unlink(i);
        self.link(i, p, b);
        self.version += 1;
        Ok(true)
    }

    /// Removes `id` and its whole subtree, calling `removed` for each node (children before their
    /// parent) so columns can drop what they hold. Returns how many nodes went.
    pub fn remove(&mut self, id: Id, mut removed: impl FnMut(Id)) -> Result<usize> {
        let root = self.check(id)?;
        self.unlink(root);
        self.version += 1;
        // Postorder over the detached subtree: it now has no parent and no siblings.
        let mut count = 0;
        let mut i = self.deepest_first(root);
        loop {
            let Links { next, parent, .. } = self.links[i as usize];
            let gone = self.id(i).expect("alive");
            self.free_slot(i);
            removed(gone);
            count += 1;
            if i == root {
                break;
            }
            i = if next != NONE {
                self.deepest_first(next)
            } else {
                parent
            };
        }
        Ok(count)
    }

    fn free_slot(&mut self, i: u32) {
        let s = i as usize;
        self.generation[s] = self.generation[s].wrapping_add(1);
        self.links[s] = UNLINKED;
        self.free.push(i);
        self.len -= 1;
    }

    /// The deepest first descendant of `i` (`i` itself if it has no children).
    fn deepest_first(&self, mut i: u32) -> u32 {
        while self.links[i as usize].first != NONE {
            i = self.links[i as usize].first;
        }
        i
    }

    /// `id`'s parent (`None` for a root or a dead id).
    pub fn parent(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].parent))
    }

    /// `id`'s first child.
    pub fn first_child(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].first))
    }

    /// `id`'s last child.
    pub fn last_child(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].last))
    }

    /// The sibling after `id`.
    pub fn next_sibling(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].next))
    }

    /// The sibling before `id`.
    pub fn prev_sibling(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].prev))
    }

    /// The roots, in order.
    pub fn roots(&self) -> Siblings<'_> {
        Siblings {
            forest: self,
            next: self.first_root,
        }
    }

    /// `id`'s children, in order (none for a dead id).
    pub fn children(&self, id: Id) -> Siblings<'_> {
        let next = self
            .check(id)
            .map_or(NONE, |i| self.links[i as usize].first);
        Siblings { forest: self, next }
    }

    /// `id`'s ancestors, nearest first (not `id` itself).
    pub fn ancestors(&self, id: Id) -> Ancestors<'_> {
        let next = self
            .check(id)
            .map_or(NONE, |i| self.links[i as usize].parent);
        Ancestors { forest: self, next }
    }

    /// `root` and its descendants, parents before children, siblings in order, by following
    /// links. Allocates nothing; for whole passes, [`Flat`] is faster.
    pub fn preorder(&self, root: Id) -> Preorder<'_> {
        let r = self.check(root).unwrap_or(NONE);
        Preorder {
            forest: self,
            root: r,
            next: r,
        }
    }

    /// `root` and its descendants, children before parents. Allocates nothing.
    pub fn postorder(&self, root: Id) -> Postorder<'_> {
        let r = self.check(root).unwrap_or(NONE);
        let next = if r == NONE {
            NONE
        } else {
            self.deepest_first(r)
        };
        Postorder {
            forest: self,
            root: r,
            next,
        }
    }

    /// The slot that follows `i` in preorder within `root`'s subtree (`NONE` at its end).
    fn preorder_next(&self, i: u32, root: u32) -> u32 {
        let first = self.links[i as usize].first;
        if first != NONE {
            return first;
        }
        self.after_subtree(i, root)
    }

    /// The slot that follows `i`'s subtree in preorder within `root`'s (`NONE` at its end).
    fn after_subtree(&self, i: u32, root: u32) -> u32 {
        let mut j = i;
        loop {
            if j == root {
                return NONE;
            }
            let next = self.links[j as usize].next;
            if next != NONE {
                return next;
            }
            j = self.links[j as usize].parent;
        }
    }

    /// Walks `root`'s subtree in preorder by following links, letting `visit` skip whole
    /// subtrees. Allocates nothing; [`Flat::walk`] does the same over the flat index, faster.
    pub fn walk(&self, root: Id, mut visit: impl FnMut(Id) -> Visit) {
        let Ok(r) = self.check(root) else { return };
        let mut i = r;
        loop {
            let id = self.id(i).expect("alive");
            let next = match visit(id) {
                Visit::Enter => self.preorder_next(i, r),
                Visit::Skip => self.after_subtree(i, r),
            };
            if next == NONE {
                return;
            }
            i = next;
        }
    }

    /// Marks `id` and its ancestors in `marks`, stopping at the first one already marked: how a
    /// pass records "something changed at or below here", so a dirty walk can skip the rest.
    /// O(depth) for the first mark on a path, O(1) amortized over a pass. `marks` must cover
    /// every slot ([`Column::fit`]).
    pub fn mark_with_ancestors(&self, id: Id, marks: &mut Column<bool>) -> Result<()> {
        let mut i = self.check(id)?;
        while i != NONE && !marks.values[i as usize] {
            marks.values[i as usize] = true;
            i = self.links[i as usize].parent;
        }
        Ok(())
    }

    /// The forest as a [`Tree`](crate::tree::Tree), keyed and filled by the closures: for
    /// snapshots, persistence through the log, and debugging views.
    pub fn to_tree<T, K: crate::Key>(
        &self,
        mut key: impl FnMut(Id) -> K,
        mut data: impl FnMut(Id) -> T,
    ) -> Result<crate::tree::Tree<T, K>> {
        let mut tree = crate::tree::Tree::new();
        for root in self.roots() {
            for id in self.preorder(root) {
                let parent = self.parent(id).map(&mut key);
                tree.insert(key(id), parent, None, data(id))?;
            }
        }
        Ok(tree)
    }
}

/// A contiguous preorder view of a [`Forest`]: every live slot in preorder (roots in order), and
/// for each position where its subtree ends. Full passes read [`Flat::slots`] front to back;
/// [`Flat::walk`] skips clean subtrees in one step.
///
/// [`Flat::update`] rebuilds it when the forest's structure changed since the last update, in
/// O(n) and without allocating once its buffers have grown. Between a change and the next
/// update it describes the forest as it was.
#[derive(Clone, Debug, Default)]
pub struct Flat {
    slots: Vec<u32>,
    ends: Vec<u32>,
    sizes: Vec<u32>,
    /// Every node's children, contiguous and in order (compressed rows): the children of slot
    /// `s` are `children[child_range[s][0]..child_range[s][1]]`, the roots `children[..roots]`.
    children: Vec<u32>,
    child_range: Vec<[u32; 2]>,
    roots: u32,
    /// Reused by [`Flat::walk`].
    stack: Vec<u32>,
    version: Option<u64>,
}

impl Flat {
    /// An empty view: [`Flat::update`] fills it.
    pub fn new() -> Flat {
        Flat::default()
    }

    /// Rebuilds the view if `forest`'s structure changed since the last update. Returns whether
    /// it rebuilt.
    pub fn update(&mut self, forest: &Forest) -> bool {
        if self.version == Some(forest.version()) {
            return false;
        }
        self.version = Some(forest.version());
        self.slots.clear();
        let mut root = forest.first_root;
        while root != NONE {
            let mut i = root;
            while i != NONE {
                self.slots.push(i);
                i = forest.preorder_next(i, root);
            }
            root = forest.links[root as usize].next;
        }
        // Subtree sizes, children before parents (preorder reversed), then where each ends.
        self.sizes.clear();
        self.sizes.resize(forest.slots(), 0);
        for &s in self.slots.iter().rev() {
            self.sizes[s as usize] += 1;
            let p = forest.links[s as usize].parent;
            if p != NONE {
                self.sizes[p as usize] += self.sizes[s as usize];
            }
        }
        self.ends.clear();
        self.ends.extend(
            self.slots
                .iter()
                .enumerate()
                .map(|(k, &s)| k as u32 + self.sizes[s as usize]),
        );
        // Children lists, contiguous: what a skipping walk reads (siblings share cache lines).
        self.children.clear();
        let mut root = forest.first_root;
        while root != NONE {
            self.children.push(root);
            root = forest.links[root as usize].next;
        }
        self.roots = self.children.len() as u32;
        self.child_range.clear();
        self.child_range.resize(forest.slots(), [0, 0]);
        for &s in &self.slots {
            let start = self.children.len() as u32;
            let mut c = forest.links[s as usize].first;
            while c != NONE {
                self.children.push(c);
                c = forest.links[c as usize].next;
            }
            self.child_range[s as usize] = [start, self.children.len() as u32];
        }
        true
    }

    /// Every live slot, in preorder.
    pub fn slots(&self) -> &[u32] {
        &self.slots
    }

    /// For each position in [`Flat::slots`], the position just past its subtree.
    pub fn ends(&self) -> &[u32] {
        &self.ends
    }

    /// Walks every slot in preorder, letting `visit` skip whole subtrees. It reads the contiguous
    /// child lists, so the siblings it checks share cache lines; allocates nothing once warm.
    pub fn walk(&mut self, mut visit: impl FnMut(u32) -> Visit) {
        let mut stack = core::mem::take(&mut self.stack);
        stack.clear();
        stack.extend(self.children[..self.roots as usize].iter().rev());
        while let Some(s) = stack.pop() {
            if visit(s) == Visit::Enter {
                let [start, end] = self.child_range[s as usize];
                stack.extend(self.children[start as usize..end as usize].iter().rev());
            }
        }
        self.stack = stack;
    }

    /// The children of `slot`, in order, from the contiguous lists.
    pub fn children_of(&self, slot: u32) -> &[u32] {
        let [start, end] = self.child_range[slot as usize];
        &self.children[start as usize..end as usize]
    }
}

/// Siblings in order (children of a node, or the roots).
pub struct Siblings<'f> {
    forest: &'f Forest,
    next: u32,
}

impl Iterator for Siblings<'_> {
    type Item = Id;
    fn next(&mut self) -> Option<Id> {
        let i = self.next;
        if i == NONE {
            return None;
        }
        self.next = self.forest.links[i as usize].next;
        self.forest.id(i)
    }
}

/// Ancestors, nearest first.
pub struct Ancestors<'f> {
    forest: &'f Forest,
    next: u32,
}

impl Iterator for Ancestors<'_> {
    type Item = Id;
    fn next(&mut self) -> Option<Id> {
        let i = self.next;
        if i == NONE {
            return None;
        }
        self.next = self.forest.links[i as usize].parent;
        self.forest.id(i)
    }
}

/// A preorder walk of a subtree, by links.
pub struct Preorder<'f> {
    forest: &'f Forest,
    root: u32,
    next: u32,
}

impl Iterator for Preorder<'_> {
    type Item = Id;
    fn next(&mut self) -> Option<Id> {
        let i = self.next;
        if i == NONE {
            return None;
        }
        self.next = self.forest.preorder_next(i, self.root);
        self.forest.id(i)
    }
}

/// A postorder walk of a subtree, by links.
pub struct Postorder<'f> {
    forest: &'f Forest,
    root: u32,
    next: u32,
}

impl Iterator for Postorder<'_> {
    type Item = Id;
    fn next(&mut self) -> Option<Id> {
        let i = self.next;
        if i == NONE {
            return None;
        }
        let f = self.forest;
        self.next = if i == self.root {
            NONE
        } else if f.links[i as usize].next != NONE {
            f.deepest_first(f.links[i as usize].next)
        } else {
            f.links[i as usize].parent
        };
        f.id(i)
    }
}

/// Per-node data, one entry per slot: structure-of-arrays storage beside a [`Forest`].
#[derive(Clone, Debug, Default)]
pub struct Column<T> {
    values: Vec<T>,
}

impl<T: Clone + Default> Column<T> {
    /// An empty column.
    pub fn new() -> Column<T> {
        Column { values: Vec::new() }
    }

    /// Grows the column to cover every slot of `forest` (new entries are `T::default()`).
    pub fn fit(&mut self, forest: &Forest) {
        if self.values.len() < forest.slots() {
            self.values.resize(forest.slots(), T::default());
        }
    }

    /// The value for `id` (the caller keeps ids live; a stale id reads its slot's current value).
    pub fn get(&self, id: Id) -> &T {
        &self.values[id.slot()]
    }

    /// The value for `id`, to change.
    pub fn get_mut(&mut self, id: Id) -> &mut T {
        &mut self.values[id.slot()]
    }

    /// All values, by slot: for passes that sweep a whole column.
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }

    /// All values, by slot, to change.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a(b(d, e), c), r
    fn sample() -> (Forest, [Id; 6]) {
        let mut f = Forest::new();
        let a = f.push_root();
        let b = f.push_child(a).unwrap();
        let c = f.push_child(a).unwrap();
        let d = f.push_child(b).unwrap();
        let e = f.push_child(b).unwrap();
        let r = f.push_root();
        (f, [a, b, c, d, e, r])
    }

    #[test]
    fn traversals_follow_tree_order() {
        let (f, [a, b, c, d, e, r]) = sample();
        assert_eq!(f.preorder(a).collect::<Vec<_>>(), [a, b, d, e, c]);
        assert_eq!(f.postorder(a).collect::<Vec<_>>(), [d, e, b, c, a]);
        assert_eq!(f.preorder(b).collect::<Vec<_>>(), [b, d, e]);
        assert_eq!(f.roots().collect::<Vec<_>>(), [a, r]);
        assert_eq!(f.children(b).collect::<Vec<_>>(), [d, e]);
        assert_eq!(f.ancestors(e).collect::<Vec<_>>(), [b, a]);
        assert_eq!((f.first_child(a), f.last_child(a)), (Some(b), Some(c)));
        assert_eq!((f.prev_sibling(c), f.next_sibling(b)), (Some(b), Some(c)));
        assert_eq!(f.postorder(c).collect::<Vec<_>>(), [c], "a leaf alone");
    }

    #[test]
    fn walk_skips_subtrees() {
        let (f, [a, b, c, ..]) = sample();
        let mut seen = vec![];
        f.walk(a, |id| {
            seen.push(id);
            if id == b {
                Visit::Skip
            } else {
                Visit::Enter
            }
        });
        assert_eq!(seen, [a, b, c]);
    }

    #[test]
    fn flat_matches_links_and_skips() {
        let (mut f, [a, b, c, d, e, r]) = sample();
        let mut flat = Flat::new();
        assert!(flat.update(&f));
        assert!(!flat.update(&f), "unchanged structure: no rebuild");
        let slots: Vec<usize> = flat.slots().iter().map(|&s| s as usize).collect();
        assert_eq!(slots, [a, b, d, e, c, r].map(Id::slot));
        assert_eq!(flat.ends(), [5, 4, 3, 4, 5, 6]);
        let mut seen = vec![];
        flat.walk(|s| {
            seen.push(s as usize);
            if s as usize == b.slot() {
                Visit::Skip
            } else {
                Visit::Enter
            }
        });
        assert_eq!(seen, [a, b, c, r].map(Id::slot));
        let kids: Vec<usize> = flat
            .children_of(a.slot() as u32)
            .iter()
            .map(|&s| s as usize)
            .collect();
        assert_eq!(kids, [b, c].map(Id::slot));
        assert!(f.move_to(c, Some(b), Some(d)).unwrap());
        assert!(flat.update(&f), "a move changes the structure");
        let slots: Vec<usize> = flat.slots().iter().map(|&s| s as usize).collect();
        assert_eq!(slots, [a, b, c, d, e, r].map(Id::slot));
    }

    #[test]
    fn inserts_and_moves() {
        let (mut f, [a, b, c, d, e, r]) = sample();
        let x = f.insert_before(c).unwrap();
        let y = f.insert_after(c).unwrap();
        assert_eq!(f.children(a).collect::<Vec<_>>(), [b, x, c, y]);
        let z = f.insert_after(r).unwrap();
        assert_eq!(f.roots().collect::<Vec<_>>(), [a, r, z]);
        assert!(f.move_to(d, Some(c), None).unwrap());
        assert_eq!(f.children(b).collect::<Vec<_>>(), [e]);
        assert_eq!(f.children(c).collect::<Vec<_>>(), [d]);
        assert!(f.move_to(c, None, Some(r)).unwrap());
        assert_eq!(f.roots().collect::<Vec<_>>(), [a, c, r, z]);

        let version = f.version();
        assert!(!f.move_to(c, None, Some(r)).unwrap(), "already there");
        assert!(!f.move_to(z, None, None).unwrap(), "already last");
        assert_eq!(
            f.version(),
            version,
            "a move that changes nothing isn't a change"
        );

        let before = f.preorder(a).collect::<Vec<_>>();
        assert!(f.move_to(a, Some(b), None).is_err(), "into its own subtree");
        assert!(f.move_to(a, Some(a), None).is_err(), "under itself");
        assert!(
            f.move_to(b, Some(a), Some(d)).is_err(),
            "before a non-child"
        );
        assert!(f.move_to(b, Some(a), Some(b)).is_err(), "before itself");
        assert_eq!(
            f.preorder(a).collect::<Vec<_>>(),
            before,
            "failures change nothing"
        );
    }

    #[test]
    fn remove_frees_the_subtree_and_stales_ids() {
        let (mut f, [a, b, c, d, e, _]) = sample();
        let mut gone = vec![];
        assert_eq!(f.remove(b, |id| gone.push(id)).unwrap(), 3);
        assert_eq!(gone, [d, e, b], "children before their parent");
        assert_eq!(f.children(a).collect::<Vec<_>>(), [c]);
        assert!(!f.contains(d));
        assert_eq!(f.id_at(d.slot()), None, "a free slot has no id");
        // The most recently freed slot (b's) is reused, under a new generation.
        let reused = f.push_child(c).unwrap();
        assert_eq!(reused.slot(), b.slot());
        assert_ne!(reused, b);
        assert!(
            !f.contains(b),
            "a stale id never reaches the slot's new node"
        );
        assert!(f.push_child(b).is_err());
        assert!(f.contains(reused));
        assert_eq!(f.id_at(reused.slot()), Some(reused));
        assert_eq!(f.len(), 4);
        assert_eq!(f.slots(), 6, "slots are kept for reuse");
    }

    #[test]
    fn removing_everything_then_rebuilding() {
        let (mut f, [a, .., r]) = sample();
        f.remove(a, |_| {}).unwrap();
        f.remove(r, |_| {}).unwrap();
        assert!(f.is_empty() && f.roots().next().is_none());
        let n = f.push_root();
        let m = f.push_child(n).unwrap();
        assert_eq!(f.preorder(n).collect::<Vec<_>>(), [n, m]);
        assert_eq!(f.slots(), 6, "no slot was added while free ones were left");
    }

    #[test]
    fn columns_follow_slots() {
        let (f, [a, b, ..]) = sample();
        let mut widths = Column::<f32>::new();
        widths.fit(&f);
        *widths.get_mut(b) = 3.0;
        assert_eq!(*widths.get(b), 3.0);
        assert_eq!(*widths.get(a), 0.0);
        assert_eq!(widths.as_slice().len(), f.slots());
    }

    #[test]
    fn marks_spread_to_ancestors_once() {
        let (mut f, [a, b, c, d, e, r]) = sample();
        let mut dirty = Column::<bool>::new();
        dirty.fit(&f);
        f.mark_with_ancestors(d, &mut dirty).unwrap();
        let marked = |dirty: &Column<bool>| [a, b, c, d, e, r].map(|id| *dirty.get(id));
        assert_eq!(marked(&dirty), [true, true, false, true, false, false]);
        // b is marked already, so e's mark stops there.
        f.mark_with_ancestors(e, &mut dirty).unwrap();
        assert_eq!(marked(&dirty), [true, true, false, true, true, false]);
        f.remove(c, |_| {}).unwrap();
        assert!(f.mark_with_ancestors(c, &mut dirty).is_err(), "a stale id");
    }

    #[test]
    fn exports_to_a_tree() {
        let (f, [a, b, ..]) = sample();
        let tree = f.to_tree(|id| id.slot() as u32, |id| id.slot()).unwrap();
        assert_eq!(tree.children(&(a.slot() as u32)).count(), 2);
        assert_eq!(tree.lineage(&(b.slot() as u32)).count(), 2);
    }
}
