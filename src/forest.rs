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

use alloc::{format, vec::Vec};

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
    #[inline]
    pub fn slot(self) -> usize {
        self.index as usize
    }
}

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

/// Why [`Forest::move_to`] refuses a move.
enum Refused {
    BeforeItself,
    NotAChild(u32),
    IntoSubtree,
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
    /// What changed, for [`Flat::update`] to patch rather than rebuild: each parent whose child
    /// list changed (`NONE` for the roots) and each freed slot, a run of changes to one parent once.
    /// Bounded: the oldest half goes when it fills, and a `Flat` that far behind rebuilds.
    changes: Vec<u32>,
    /// How many entries have gone from the front of `changes`.
    changes_base: u64,
}

/// How many entries [`Forest`]'s change log holds before its oldest half goes.
const CHANGES_CAP: usize = 4096;

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
            changes: Vec::new(),
            changes_base: 0,
        }
    }

    /// Records that `slot`'s child list changed (`NONE`: the roots), or that `slot` was freed.
    #[inline]
    fn touched(&mut self, slot: u32) {
        if self.changes.last() == Some(&slot) {
            return;
        }
        if self.changes.len() >= CHANGES_CAP {
            self.changes.drain(..CHANGES_CAP / 2);
            self.changes_base += (CHANGES_CAP / 2) as u64;
        }
        self.changes.push(slot);
    }

    /// The change log's end, as a count of every entry ever made.
    fn changes_end(&self) -> u64 {
        self.changes_base + self.changes.len() as u64
    }

    /// The entries from `from` (a count, as [`Forest::changes_end`] gives) on, or `None` if some
    /// of them are gone.
    fn changes_from(&self, from: u64) -> Option<&[u32]> {
        let at = from.checked_sub(self.changes_base)?;
        self.changes.get(at as usize..)
    }

    /// An empty forest with room for `n` nodes before it reallocates.
    pub fn with_capacity(n: usize) -> Forest {
        let mut f = Forest::new();
        f.links.reserve(n);
        f.generation.reserve(n);
        f
    }

    /// How many nodes are alive.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the forest has no nodes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The number of slots, alive or free: the length columns must have.
    #[inline]
    pub fn slots(&self) -> usize {
        self.links.len()
    }

    /// Changes whenever the structure changes (a node added, moved or removed).
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether `id` refers to a live node (not removed, not a reused slot).
    #[inline]
    pub fn contains(&self, id: Id) -> bool {
        self.generation.get(id.index as usize) == Some(&id.generation)
    }

    #[inline]
    fn check(&self, id: Id) -> Result<u32> {
        if self.contains(id) {
            Ok(id.index)
        } else {
            Err(Error::NotFound(format!("node {id}")))
        }
    }

    #[inline]
    fn id(&self, index: u32) -> Option<Id> {
        (index != NONE).then(|| Id {
            index,
            generation: self.generation[index as usize],
        })
    }

    /// The id of the live node in `slot` (for walks over [`Flat`], which yield slots).
    #[inline]
    pub fn id_at(&self, slot: usize) -> Option<Id> {
        let generation = *self.generation.get(slot)?;
        (generation % 2 == 0).then_some(Id {
            index: slot as u32,
            generation,
        })
    }

    #[inline]
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
    #[inline]
    fn link(&mut self, i: u32, parent: u32, before: u32) {
        self.touched(parent);
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
    #[inline]
    fn unlink(&mut self, i: u32) {
        let s = i as usize;
        let Links {
            parent, prev, next, ..
        } = self.links[s];
        self.touched(parent);
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
    #[inline]
    pub fn push_root(&mut self) -> Id {
        let i = self.allocate();
        self.link(i, NONE, NONE);
        self.id(i).expect("just allocated")
    }

    /// Adds a node as `parent`'s last child.
    #[inline]
    pub fn push_child(&mut self, parent: Id) -> Result<Id> {
        let p = self.check(parent)?;
        let i = self.allocate();
        self.link(i, p, NONE);
        Ok(self.id(i).expect("just allocated"))
    }

    /// Adds a node just before `sibling`, under the same parent (or among the roots).
    #[inline]
    pub fn insert_before(&mut self, sibling: Id) -> Result<Id> {
        let s = self.check(sibling)?;
        let parent = self.links[s as usize].parent;
        let i = self.allocate();
        self.link(i, parent, s);
        Ok(self.id(i).expect("just allocated"))
    }

    /// Adds a node just after `sibling`, under the same parent (or among the roots).
    #[inline]
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
    #[inline]
    pub fn move_to(&mut self, id: Id, parent: Option<Id>, before: Option<Id>) -> Result<bool> {
        let i = self.check(id)?;
        let p = parent.map_or(Ok(NONE), |p| self.check(p))?;
        let b = before.map_or(Ok(NONE), |b| self.check(b))?;
        if b == i {
            return self.refuse(id, Refused::BeforeItself);
        }
        if b != NONE && self.links[b as usize].parent != p {
            return self.refuse(id, Refused::NotAChild(b));
        }
        let here = self.links[i as usize];
        if here.parent == p {
            // Among its own siblings (a reorder): `p` is its parent, so it can't be inside `id`.
            if here.next == b {
                return Ok(false);
            }
        } else {
            let mut a = p;
            while a != NONE {
                if a == i {
                    return self.refuse(id, Refused::IntoSubtree);
                }
                a = self.links[a as usize].parent;
            }
        }
        self.unlink(i);
        self.link(i, p, b);
        self.version += 1;
        Ok(true)
    }

    /// Why a move is refused. The message is built out of line, so `move_to` stays small.
    #[cold]
    #[inline(never)]
    fn refuse(&self, id: Id, why: Refused) -> Result<bool> {
        match why {
            Refused::BeforeItself => invalid(format!("can't move {id} before itself")),
            Refused::NotAChild(b) => invalid(format!(
                "{} isn't a child of the new parent",
                self.id(b).expect("checked")
            )),
            Refused::IntoSubtree => invalid(format!("can't move {id} into its own subtree")),
        }
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

    #[inline]
    fn free_slot(&mut self, i: u32) {
        let s = i as usize;
        self.touched(i);
        self.generation[s] = self.generation[s].wrapping_add(1);
        self.links[s] = UNLINKED;
        self.free.push(i);
        self.len -= 1;
    }

    /// The deepest first descendant of `i` (`i` itself if it has no children).
    #[inline]
    fn deepest_first(&self, mut i: u32) -> u32 {
        while self.links[i as usize].first != NONE {
            i = self.links[i as usize].first;
        }
        i
    }

    /// `id`'s parent (`None` for a root or a dead id).
    #[inline]
    pub fn parent(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].parent))
    }

    /// `id`'s first child.
    #[inline]
    pub fn first_child(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].first))
    }

    /// `id`'s last child.
    #[inline]
    pub fn last_child(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].last))
    }

    /// The sibling after `id`.
    #[inline]
    pub fn next_sibling(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].next))
    }

    /// The sibling before `id`.
    #[inline]
    pub fn prev_sibling(&self, id: Id) -> Option<Id> {
        self.check(id)
            .ok()
            .and_then(|i| self.id(self.links[i as usize].prev))
    }

    /// The roots, in order.
    #[inline]
    pub fn roots(&self) -> Siblings<'_> {
        Siblings {
            forest: self,
            next: self.first_root,
        }
    }

    /// `id`'s children, in order (none for a dead id).
    #[inline]
    pub fn children(&self, id: Id) -> Siblings<'_> {
        let next = self
            .check(id)
            .map_or(NONE, |i| self.links[i as usize].first);
        Siblings { forest: self, next }
    }

    /// `id`'s ancestors, nearest first (not `id` itself).
    #[inline]
    pub fn ancestors(&self, id: Id) -> Ancestors<'_> {
        let next = self
            .check(id)
            .map_or(NONE, |i| self.links[i as usize].parent);
        Ancestors { forest: self, next }
    }

    /// `root` and its descendants, parents before children, siblings in order, by following
    /// links. Allocates nothing; for whole passes, [`Flat`] is faster.
    #[inline]
    pub fn preorder(&self, root: Id) -> Preorder<'_> {
        let r = self.check(root).unwrap_or(NONE);
        Preorder {
            forest: self,
            root: r,
            next: r,
        }
    }

    /// `root` and its descendants, children before parents. Allocates nothing.
    #[inline]
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
    #[inline]
    fn preorder_next(&self, i: u32, root: u32) -> u32 {
        let first = self.links[i as usize].first;
        if first != NONE {
            return first;
        }
        self.after_subtree(i, root)
    }

    /// The slot that follows `i`'s subtree in preorder within `root`'s (`NONE` at its end).
    #[inline]
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
/// [`Flat::update`] brings it up to date when the forest's structure changed since the last
/// update: it patches what changed (the subtrees under each parent whose children changed) when
/// it can, and otherwise rebuilds in O(n); neither allocates once its buffers have grown. Between
/// a change and the next update it describes the forest as it was. A `Flat` follows one forest.
#[derive(Clone, Debug, Default)]
pub struct Flat {
    slots: Vec<u32>,
    ends: Vec<u32>,
    /// Every node's children, contiguous and in order (compressed rows): the children of slot
    /// `s` are `children[child_range[s][0]..child_range[s][1]]`, the roots
    /// `children[roots[0]..roots[1]]`. A list that changed is written again at the end, leaving
    /// `garbage` stale entries until the next rebuild.
    children: Vec<u32>,
    child_range: Vec<[u32; 2]>,
    roots: [u32; 2],
    garbage: usize,
    /// Reused by [`Flat::walk`].
    stack: Vec<u32>,
    /// Reused by every walk of the child lists: per level, `[position, next child, end]`.
    frames: Vec<[u32; 3]>,
    /// Reused by a patch: the changed slots and a mark per slot for them; the subtrees to walk
    /// again (`[old position, slot, ancestors' positions from, to]`) with their ancestors'
    /// positions; the path up from one of them; and one subtree's new preorder.
    touched: Vec<u32>,
    marked: Vec<bool>,
    regions: Vec<[u32; 4]>,
    above: Vec<u32>,
    path: Vec<u32>,
    region_slots: Vec<u32>,
    region_ends: Vec<u32>,
    version: Option<u64>,
    /// How far into the forest's change log this view has read.
    seen: u64,
    /// How many updates walked only the changed subtrees again (the tests check patches happen).
    #[cfg(test)]
    region_patches: usize,
}

/// Appends the preorder of the trees whose roots are `children[list[0]..list[1]]` to `slots`, and
/// where each subtree ends to `ends`, positions counted from `base`. It walks the contiguous child
/// lists, not the links: a frame per level holds its position and the children left to visit.
fn preorder_into(
    children: &[u32],
    child_range: &[[u32; 2]],
    list: [u32; 2],
    base: u32,
    slots: &mut Vec<u32>,
    ends: &mut Vec<u32>,
    frames: &mut Vec<[u32; 3]>,
) {
    /// Appends `s`; a leaf (most nodes) ends where it starts and needs no frame.
    #[inline]
    fn enter(
        s: u32,
        child_range: &[[u32; 2]],
        offset: u32,
        slots: &mut Vec<u32>,
        ends: &mut Vec<u32>,
        frames: &mut Vec<[u32; 3]>,
    ) {
        let at = slots.len();
        slots.push(s);
        let [first, end] = child_range[s as usize];
        if first == end {
            ends.push(at as u32 + offset + 1);
        } else {
            ends.push(0);
            frames.push([at as u32, first, end]);
        }
    }
    // A position is an index into `slots` plus `offset` (wrapping: `base` may be below it).
    let offset = base.wrapping_sub(slots.len() as u32);
    frames.clear();
    for &root in &children[list[0] as usize..list[1] as usize] {
        enter(root, child_range, offset, slots, ends, frames);
        while let Some(top) = frames.last_mut() {
            if top[1] < top[2] {
                let c = children[top[1] as usize];
                top[1] += 1;
                enter(c, child_range, offset, slots, ends, frames);
            } else {
                let at = top[0] as usize;
                frames.pop();
                ends[at] = (slots.len() as u32).wrapping_add(offset);
            }
        }
    }
}

impl Flat {
    /// An empty view: [`Flat::update`] fills it.
    pub fn new() -> Flat {
        Flat::default()
    }

    /// Brings the view up to date if `forest`'s structure changed since the last update, by
    /// patching what changed when it can and rebuilding otherwise. Returns whether the view changed.
    pub fn update(&mut self, forest: &Forest) -> bool {
        if self.version == Some(forest.version()) {
            return false;
        }
        // The last entry read is read again: a later change to the same parent merges into it.
        let patched = self.version.is_some()
            && match forest.changes_from(self.seen.saturating_sub(1)) {
                Some(changes) => self.patch(forest, changes),
                None => false,
            };
        if !patched {
            self.rebuild(forest);
        }
        self.version = Some(forest.version());
        self.seen = forest.changes_end();
        true
    }

    /// Builds the view from scratch: the child lists in one pass over the slots (each sibling
    /// chain followed once, the roots first), then preorder over them.
    fn rebuild(&mut self, forest: &Forest) {
        self.children.clear();
        let mut root = forest.first_root;
        while root != NONE {
            self.children.push(root);
            root = forest.links[root as usize].next;
        }
        self.roots = [0, self.children.len() as u32];
        self.child_range.clear();
        self.child_range.reserve(forest.slots());
        for links in &forest.links {
            let start = self.children.len() as u32;
            let mut c = links.first;
            while c != NONE {
                self.children.push(c);
                c = forest.links[c as usize].next;
            }
            self.child_range.push([start, self.children.len() as u32]);
        }
        self.garbage = 0;
        self.full_preorder();
    }

    fn full_preorder(&mut self) {
        self.slots.clear();
        self.ends.clear();
        preorder_into(
            &self.children,
            &self.child_range,
            self.roots,
            0,
            &mut self.slots,
            &mut self.ends,
            &mut self.frames,
        );
    }

    /// Applies `changes` (the forest's log since the last update): writes the changed child lists
    /// again, then walks again only the subtrees under them. Returns false, changing nothing that
    /// a rebuild won't replace, when it's better to rebuild (too much stale in the child lists).
    fn patch(&mut self, forest: &Forest, changes: &[u32]) -> bool {
        let n = forest.slots();
        if self.child_range.len() < n {
            self.child_range.resize(n, [0, 0]);
        }
        if self.marked.len() < n {
            self.marked.resize(n, false);
        }
        self.touched.clear();
        let mut roots_changed = false;
        for &s in changes {
            if s == NONE {
                roots_changed = true;
            } else if !self.marked[s as usize] {
                self.marked[s as usize] = true;
                self.touched.push(s);
            }
        }
        // The changed child lists written again at the end; a freed slot's emptied.
        if roots_changed {
            let start = self.children.len() as u32;
            let mut r = forest.first_root;
            while r != NONE {
                self.children.push(r);
                r = forest.links[r as usize].next;
            }
            self.garbage += (self.roots[1] - self.roots[0]) as usize;
            self.roots = [start, self.children.len() as u32];
        }
        for k in 0..self.touched.len() {
            let s = self.touched[k];
            let [a, b] = self.child_range[s as usize];
            self.garbage += (b - a) as usize;
            let start = self.children.len() as u32;
            if forest.id_at(s as usize).is_some() {
                let mut c = forest.links[s as usize].first;
                while c != NONE {
                    self.children.push(c);
                    c = forest.links[c as usize].next;
                }
            }
            self.child_range[s as usize] = [start, self.children.len() as u32];
        }
        let compact = self.garbage > self.children.len() - self.garbage;
        let regions = !compact && !roots_changed && self.find_regions(forest);
        for &s in &self.touched {
            self.marked[s as usize] = false;
        }
        if compact {
            return false;
        }
        if regions {
            self.walk_regions_again();
            #[cfg(test)]
            {
                self.region_patches += 1;
            }
        } else {
            // The roots changed, or the subtrees overlap: preorder again over the patched lists.
            self.full_preorder();
        }
        true
    }

    /// Finds the subtrees to walk again: under each live changed slot none of whose ancestors
    /// changed (the others lie inside one of those), each at its position in the view as it was.
    /// False when one can't be placed, or two overlap.
    fn find_regions(&mut self, forest: &Forest) -> bool {
        self.regions.clear();
        self.above.clear();
        for k in 0..self.touched.len() {
            let s = self.touched[k];
            if forest.id_at(s as usize).is_none() {
                continue;
            }
            self.path.clear();
            let mut a = forest.links[s as usize].parent;
            let mut covered = false;
            while a != NONE {
                if self.marked[a as usize] {
                    covered = true;
                    break;
                }
                self.path.push(a);
                a = forest.links[a as usize].parent;
            }
            if covered {
                continue;
            }
            let from = self.above.len() as u32;
            let Some(at) = self.old_position(s) else {
                return false;
            };
            self.regions.push([at, s, from, self.above.len() as u32]);
        }
        self.regions.sort_unstable();
        self.regions
            .windows(2)
            .all(|w| w[1][0] >= self.ends[w[0][0] as usize])
    }

    /// `s`'s position in the view as it was, found from the top: across the roots before its
    /// root, then down its ancestors (`path`, nearest first), stepping over earlier siblings by
    /// where each one ends. Sound because no list along the way changed. Pushes the ancestors'
    /// positions to `above`.
    fn old_position(&mut self, s: u32) -> Option<u32> {
        let top = self.path.last().copied().unwrap_or(s);
        let mut at = 0u32;
        let roots = &self.children[self.roots[0] as usize..self.roots[1] as usize];
        let mut found = false;
        for &r in roots {
            if r == top {
                found = true;
                break;
            }
            at = *self.ends.get(at as usize)?;
        }
        if !found {
            return None;
        }
        let mut parent = top;
        for i in (0..self.path.len()).rev() {
            self.above.push(at);
            let next = if i == 0 { s } else { self.path[i - 1] };
            let [first, end] = self.child_range[parent as usize];
            let mut pos = at + 1;
            let mut found = false;
            for &c in &self.children[first as usize..end as usize] {
                if c == next {
                    found = true;
                    break;
                }
                pos = *self.ends.get(pos as usize)?;
            }
            if !found {
                return None;
            }
            at = pos;
            parent = next;
        }
        (self.slots.get(at as usize) == Some(&s)).then_some(at)
    }

    /// Walks each region's subtree again from its patched child lists and splices it in, from the
    /// last region to the first so the positions not yet patched stay as they were. A region that
    /// grew or shrank shifts where everything after it ends, and its ancestors' ends.
    fn walk_regions_again(&mut self) {
        for k in (0..self.regions.len()).rev() {
            let [at, s, from, to] = self.regions[k];
            let old_end = self.ends[at as usize];
            self.region_slots.clear();
            self.region_ends.clear();
            preorder_into(
                &self.children,
                &self.child_range,
                self.child_range[s as usize],
                at + 1,
                &mut self.region_slots,
                &mut self.region_ends,
                &mut self.frames,
            );
            let new_end = at + 1 + self.region_slots.len() as u32;
            let inside = at as usize + 1..old_end as usize;
            self.slots
                .splice(inside.clone(), self.region_slots.iter().copied());
            self.ends.splice(inside, self.region_ends.iter().copied());
            if new_end != old_end {
                let delta = new_end.wrapping_sub(old_end);
                for e in &mut self.ends[new_end as usize..] {
                    *e = e.wrapping_add(delta);
                }
                self.ends[at as usize] = new_end;
                for &q in &self.above[from as usize..to as usize] {
                    self.ends[q as usize] = self.ends[q as usize].wrapping_add(delta);
                }
            }
        }
    }

    /// Every live slot, in preorder.
    #[inline]
    pub fn slots(&self) -> &[u32] {
        &self.slots
    }

    /// For each position in [`Flat::slots`], the position just past its subtree.
    #[inline]
    pub fn ends(&self) -> &[u32] {
        &self.ends
    }

    /// Walks every slot in preorder, letting `visit` skip whole subtrees. It reads the contiguous
    /// child lists, so the siblings it checks share cache lines; allocates nothing once warm.
    pub fn walk(&mut self, mut visit: impl FnMut(u32) -> Visit) {
        let mut stack = core::mem::take(&mut self.stack);
        stack.clear();
        stack.extend(
            self.children[self.roots[0] as usize..self.roots[1] as usize]
                .iter()
                .rev(),
        );
        while let Some(s) = stack.pop() {
            if visit(s) == Visit::Enter {
                let [start, end] = self.child_range[s as usize];
                stack.extend(self.children[start as usize..end as usize].iter().rev());
            }
        }
        self.stack = stack;
    }

    /// The children of `slot`, in order, from the contiguous lists.
    #[inline]
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
    #[inline]
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
    #[inline]
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
    #[inline]
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
    #[inline]
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
    use alloc::vec;

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
    fn flat_agrees_with_the_links_after_removals_and_reuse() {
        // Three levels, then churn: remove subtrees, reuse their slots elsewhere, move nodes.
        let mut f = Forest::new();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let r = f.push_root();
            ids.push(r);
            for _ in 0..4 {
                let c = f.push_child(r).unwrap();
                ids.push(c);
                for _ in 0..3 {
                    ids.push(f.push_child(c).unwrap());
                }
            }
        }
        f.remove(ids[1], |_| {}).unwrap();
        // The second root's second child (each root's block is 17 ids: itself, then 4 x (child + 3)).
        f.remove(ids[17 + 5], |_| {}).unwrap();
        let r = f.roots().nth(1).unwrap();
        let fresh: Vec<Id> = (0..6).map(|_| f.push_child(r).unwrap()).collect();
        f.move_to(fresh[0], Some(fresh[1]), None).unwrap();
        f.move_to(f.roots().next().unwrap(), None, None).unwrap();

        let mut flat = Flat::new();
        flat.update(&f);
        let by_links: Vec<u32> = f
            .roots()
            .flat_map(|root| f.preorder(root))
            .map(|id| id.slot() as u32)
            .collect();
        assert_eq!(flat.slots(), by_links, "preorder, roots in order");
        for (k, &s) in flat.slots().iter().enumerate() {
            let id = f.id_at(s as usize).unwrap();
            let size = f.preorder(id).count() as u32;
            assert_eq!(flat.ends()[k], k as u32 + size, "where {id}'s subtree ends");
            let kids: Vec<u32> = f.children(id).map(|c| c.slot() as u32).collect();
            assert_eq!(flat.children_of(s), kids, "{id}'s children, in order");
        }
    }

    /// A small fixed-seed generator, so a failure here reproduces.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// The view `flat` must equal one built from scratch: same preorder, ends, child lists and
    /// skipping walk.
    fn assert_same_as_fresh(f: &Forest, flat: &mut Flat, step: usize) {
        let mut fresh = Flat::new();
        fresh.update(f);
        assert_eq!(flat.slots(), fresh.slots(), "preorder after step {step}");
        assert_eq!(flat.ends(), fresh.ends(), "ends after step {step}");
        for &s in fresh.slots() {
            assert_eq!(
                flat.children_of(s),
                fresh.children_of(s),
                "children of {s} after step {step}"
            );
        }
        let (mut a, mut b) = (vec![], vec![]);
        let skip = |s: u32| {
            if s.is_multiple_of(3) {
                Visit::Skip
            } else {
                Visit::Enter
            }
        };
        flat.walk(|s| {
            a.push(s);
            skip(s)
        });
        fresh.walk(|s| {
            b.push(s);
            skip(s)
        });
        assert_eq!(a, b, "skipping walk after step {step}");
    }

    #[test]
    fn a_patched_flat_matches_a_rebuilt_one() {
        let mut rng = Rng(0x5eed_f1a7);
        let mut f = Forest::new();
        let mut live: Vec<Id> = Vec::new();
        for _ in 0..4 {
            let r = f.push_root();
            live.push(r);
            for _ in 0..5 {
                let c = f.push_child(r).unwrap();
                live.push(c);
                for _ in 0..3 {
                    live.push(f.push_child(c).unwrap());
                }
            }
        }
        let mut flat = Flat::new();
        flat.update(&f);
        for step in 0..4000 {
            // A batch of 1 to 6 changes, mostly deep in the tree, sometimes at the roots.
            for _ in 0..1 + rng.below(6) {
                live.retain(|&id| f.contains(id));
                let pick = live[rng.below(live.len())];
                match rng.below(10) {
                    0..=2 => live.push(f.push_child(pick).unwrap()),
                    3 => live.push(f.insert_before(pick).unwrap()),
                    4 => live.push(f.insert_after(pick).unwrap()),
                    5 | 6 => {
                        let to = live[rng.below(live.len())];
                        let before = f.children(to).nth(rng.below(4));
                        let _ = f.move_to(pick, Some(to), before);
                    }
                    7 => {
                        // Reorder among siblings.
                        let parent = f.parent(pick);
                        let before = match parent {
                            Some(p) => f.first_child(p),
                            None => f.roots().next(),
                        };
                        let _ = f.move_to(pick, parent, before);
                    }
                    8 if live.len() > 30 => {
                        f.remove(pick, |_| {}).unwrap();
                    }
                    9 if rng.below(8) == 0 => live.push(f.push_root()),
                    _ => live.push(f.push_child(pick).unwrap()),
                }
            }
            flat.update(&f);
            assert_same_as_fresh(&f, &mut flat, step);
        }
        assert!(
            flat.region_patches > 1000,
            "patched {} times",
            flat.region_patches
        );
    }

    #[test]
    fn a_flat_too_far_behind_rebuilds() {
        let mut f = Forest::new();
        let root = f.push_root();
        let mut flat = Flat::new();
        flat.update(&f);
        // More changes than the log keeps, to many parents, before the next update.
        let mut parents = vec![root];
        for i in 0..CHANGES_CAP * 2 {
            let p = parents[i % parents.len()];
            parents.push(f.push_child(p).unwrap());
        }
        assert!(flat.update(&f));
        assert_same_as_fresh(&f, &mut flat, 0);
        assert!(!flat.update(&f), "nothing changed since");
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
