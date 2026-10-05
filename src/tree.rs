//! An ordered forest with stable keys.
//!
//! Nodes live in dense slots, found by key through an ordered index, with
//! parent links and ordered child lists. Ancestors, paths and subtree walks
//! are iterators over slots: no allocation per step, no lookups by key.

use alloc::collections::BTreeMap;
use alloc::{format, vec, vec::Vec};

use crate::error::invalid;
use crate::{Error, Key, Result};

#[derive(Clone, Debug)]
struct Slot<T, K> {
    key: K,
    data: T,
    parent: Option<u32>,
    /// Each child's key and slot, in order (the key is kept so listing children reads no other slot).
    children: Vec<(K, u32)>,
}

#[derive(Clone, Debug)]
pub struct Tree<T, K: Key> {
    slots: Vec<Option<Slot<T, K>>>,
    free: Vec<u32>,
    index: BTreeMap<K, u32>,
    roots: Vec<(K, u32)>,
}

impl<T, K: Key> Default for Tree<T, K> {
    fn default() -> Self {
        Tree {
            slots: Vec::new(),
            free: Vec::new(),
            index: BTreeMap::new(),
            roots: Vec::new(),
        }
    }
}

/// Keys of a list of slots (children or roots), in order.
pub struct Keys<'t, T, K: Key> {
    list: core::slice::Iter<'t, (K, u32)>,
    tree: core::marker::PhantomData<&'t Tree<T, K>>,
}

impl<'t, T, K: Key> Keys<'t, T, K> {
    fn of(list: &'t [(K, u32)]) -> Self {
        Keys {
            list: list.iter(),
            tree: core::marker::PhantomData,
        }
    }
}

impl<T, K: Key> Clone for Keys<'_, T, K> {
    fn clone(&self) -> Self {
        Keys {
            list: self.list.clone(),
            tree: core::marker::PhantomData,
        }
    }
}

impl<T, K: Key> Iterator for Keys<'_, T, K> {
    type Item = K;

    fn next(&mut self) -> Option<K> {
        self.list.next().map(|&(k, _)| k)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.list.size_hint()
    }
}

impl<T, K: Key> DoubleEndedIterator for Keys<'_, T, K> {
    fn next_back(&mut self) -> Option<K> {
        self.list.next_back().map(|&(k, _)| k)
    }
}

impl<T, K: Key> ExactSizeIterator for Keys<'_, T, K> {}

/// A node and its ancestors, nearest first.
pub struct Lineage<'t, T, K: Key> {
    tree: &'t Tree<T, K>,
    next: Option<u32>,
}

impl<'t, T, K: Key> Iterator for Lineage<'t, T, K> {
    type Item = (K, &'t T);

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.next?;
        let s = self.tree.s(i);
        self.next = s.parent;
        Some((s.key, &s.data))
    }
}

/// A node found in the tree: its data, parent and children, read from its slot
/// (walking from an entry to its children needs no lookups by key).
pub struct Entry<'t, T, K: Key> {
    tree: &'t Tree<T, K>,
    slot: u32,
}

impl<T, K: Key> Clone for Entry<'_, T, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, K: Key> Copy for Entry<'_, T, K> {}

impl<'t, T, K: Key> Entry<'t, T, K> {
    pub fn key(&self) -> K {
        self.tree.s(self.slot).key
    }

    pub fn data(&self) -> &'t T {
        &self.tree.s(self.slot).data
    }

    pub fn parent(&self) -> Option<K> {
        self.tree.s(self.slot).parent.map(|p| self.tree.s(p).key)
    }

    /// The children's keys, in order.
    pub fn children(&self) -> Keys<'t, T, K> {
        Keys::of(&self.tree.s(self.slot).children)
    }

    /// The children as entries, in order.
    pub fn child_entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = Entry<'t, T, K>> + ExactSizeIterator + 't {
        let tree = self.tree;
        tree.s(self.slot)
            .children
            .iter()
            .map(move |&(_, slot)| Entry { tree, slot })
    }
}

/// A depth-first, preorder walk; `keep` decides which nodes (with their subtrees) are visited.
pub struct Preorder<'t, T, K: Key, F> {
    tree: &'t Tree<T, K>,
    stack: Vec<u32>,
    keep: F,
}

impl<'t, T, K: Key, F: FnMut(&K, &T) -> bool> Iterator for Preorder<'t, T, K, F> {
    type Item = (K, &'t T);

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.stack.pop()?;
        let s = self.tree.s(i);
        for &(key, c) in s.children.iter().rev() {
            if (self.keep)(&key, &self.tree.s(c).data) {
                self.stack.push(c);
            }
        }
        Some((s.key, &s.data))
    }
}

impl<T, K: Key> Tree<T, K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    fn s(&self, i: u32) -> &Slot<T, K> {
        self.slots[i as usize]
            .as_ref()
            .expect("indexed slots are live")
    }

    fn s_mut(&mut self, i: u32) -> &mut Slot<T, K> {
        self.slots[i as usize]
            .as_mut()
            .expect("indexed slots are live")
    }

    fn ix(&self, key: &K) -> Result<u32> {
        self.index
            .get(key)
            .copied()
            .ok_or_else(|| Error::NotFound(format!("node {key}")))
    }

    fn list(&self, parent: Option<u32>) -> &Vec<(K, u32)> {
        match parent {
            Some(p) => &self.s(p).children,
            None => &self.roots,
        }
    }

    fn list_mut(&mut self, parent: Option<u32>) -> &mut Vec<(K, u32)> {
        match parent {
            Some(p) => &mut self.s_mut(p).children,
            None => &mut self.roots,
        }
    }

    pub fn get(&self, key: &K) -> Option<&T> {
        self.index.get(key).map(|&i| &self.s(i).data)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut T> {
        let i = *self.index.get(key)?;
        Some(&mut self.s_mut(i).data)
    }

    /// The node's parent (`None` for a root, or a key that isn't here).
    pub fn parent(&self, key: &K) -> Option<K> {
        let i = *self.index.get(key)?;
        self.s(i).parent.map(|p| self.s(p).key)
    }

    pub fn roots(&self) -> Keys<'_, T, K> {
        Keys::of(&self.roots)
    }

    /// A node with its place in the tree, for walks that read data, parent and children together.
    pub fn entry(&self, key: &K) -> Option<Entry<'_, T, K>> {
        self.index.get(key).map(|&slot| Entry { tree: self, slot })
    }

    /// The roots as entries, in order.
    pub fn root_entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = Entry<'_, T, K>> + ExactSizeIterator + '_ {
        self.roots
            .iter()
            .map(move |&(_, slot)| Entry { tree: self, slot })
    }

    /// The node's children, in order (none for a key that isn't here).
    pub fn children(&self, key: &K) -> Keys<'_, T, K> {
        match self.index.get(key) {
            Some(&i) => Keys::of(&self.s(i).children),
            None => Keys::of(&[]),
        }
    }

    /// The children of `parent`, or the roots for `None`.
    pub fn siblings(&self, parent: Option<&K>) -> Keys<'_, T, K> {
        match parent {
            Some(p) => self.children(p),
            None => self.roots(),
        }
    }

    pub fn child_count(&self, key: &K) -> usize {
        self.index.get(key).map_or(0, |&i| self.s(i).children.len())
    }

    /// Where the node sits among its siblings.
    pub fn position(&self, key: &K) -> Option<usize> {
        let i = *self.index.get(key)?;
        self.list(self.s(i).parent)
            .iter()
            .position(|&(_, x)| x == i)
    }

    /// Adds a node under `parent` (a root for `None`) at `index`, or last.
    pub fn insert(
        &mut self,
        key: K,
        parent: Option<K>,
        index: Option<usize>,
        data: T,
    ) -> Result<()> {
        if self.index.contains_key(&key) {
            return invalid(format!("node {key} already exists"));
        }
        let p = parent.map(|p| self.ix(&p)).transpose()?;
        let len = self.list(p).len();
        let at = index.unwrap_or(len);
        if at > len {
            return invalid("index out of range");
        }
        let slot = Slot {
            key,
            data,
            parent: p,
            children: Vec::new(),
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i as usize] = Some(slot);
                i
            }
            None => {
                self.slots.push(Some(slot));
                (self.slots.len() - 1) as u32
            }
        };
        self.list_mut(p).insert(at, (key, i));
        self.index.insert(key, i);
        Ok(())
    }

    /// Moves a node (with its subtree) under `parent` at `index`, or last.
    /// Returns `false`, changing nothing, when it's already there.
    pub fn move_to(&mut self, key: &K, parent: Option<K>, index: Option<usize>) -> Result<bool> {
        let i = self.ix(key)?;
        let p = parent.map(|p| self.ix(&p)).transpose()?;
        if let Some(p) = p {
            if p == i || self.is_ancestor_slot(i, p) {
                return invalid("can't move a node under itself");
            }
        }
        let old = self.s(i).parent;
        let from = self
            .list(old)
            .iter()
            .position(|&(_, x)| x == i)
            .expect("a node is in its parent's list");
        self.list_mut(old).remove(from);
        let len = self.list(p).len();
        let at = index.unwrap_or(len);
        if at > len || (old == p && at == from) {
            self.list_mut(old).insert(from, (*key, i));
            return if at > len {
                invalid("index out of range")
            } else {
                Ok(false)
            };
        }
        self.list_mut(p).insert(at, (*key, i));
        self.s_mut(i).parent = p;
        Ok(true)
    }

    /// Removes a node and its subtree; returns them in preorder.
    pub fn remove(&mut self, key: &K) -> Option<Vec<(K, T)>> {
        let i = *self.index.get(key)?;
        let parent = self.s(i).parent;
        self.list_mut(parent).retain(|&(_, x)| x != i);
        let mut out = Vec::new();
        let mut stack = vec![i];
        while let Some(n) = stack.pop() {
            let s = self.slots[n as usize]
                .take()
                .expect("indexed slots are live");
            self.index.remove(&s.key);
            self.free.push(n);
            stack.extend(s.children.iter().rev().map(|&(_, c)| c));
            out.push((s.key, s.data));
        }
        Some(out)
    }

    fn is_ancestor_slot(&self, ancestor: u32, of: u32) -> bool {
        let mut cur = self.s(of).parent;
        while let Some(p) = cur {
            if p == ancestor {
                return true;
            }
            cur = self.s(p).parent;
        }
        false
    }

    /// Whether `ancestor` is above `of` (not counting `of` itself).
    pub fn is_ancestor(&self, ancestor: &K, of: &K) -> bool {
        match (self.index.get(ancestor), self.index.get(of)) {
            (Some(&a), Some(&o)) => self.is_ancestor_slot(a, o),
            _ => false,
        }
    }

    /// The node, then its parent, and so on up to its root.
    pub fn lineage(&self, key: &K) -> Lineage<'_, T, K> {
        Lineage {
            tree: self,
            next: self.index.get(key).copied(),
        }
    }

    /// Its parent, grandparent … (not the node itself).
    pub fn ancestors(&self, key: &K) -> Lineage<'_, T, K> {
        Lineage {
            tree: self,
            next: self.index.get(key).and_then(|&i| self.s(i).parent),
        }
    }

    /// Steps from its root (a root has depth 0).
    pub fn depth(&self, key: &K) -> Option<usize> {
        self.contains(key).then(|| self.ancestors(key).count())
    }

    /// The node's root.
    pub fn root_of(&self, key: &K) -> Option<K> {
        self.lineage(key).last().map(|(k, _)| k)
    }

    /// `key` and its subtree in preorder, skipping (with their subtrees) the nodes `keep` rejects.
    pub fn subtree<F: FnMut(&K, &T) -> bool>(&self, key: &K, keep: F) -> Preorder<'_, T, K, F> {
        Preorder {
            tree: self,
            stack: self.index.get(key).copied().into_iter().collect(),
            keep,
        }
    }

    /// Every tree of the forest in preorder, roots in order.
    pub fn preorder<F: FnMut(&K, &T) -> bool>(&self, mut keep: F) -> Preorder<'_, T, K, F> {
        let stack = self
            .roots
            .iter()
            .rev()
            .filter(|&&(k, r)| keep(&k, &self.s(r).data))
            .map(|&(_, r)| r)
            .collect();
        Preorder {
            tree: self,
            stack,
            keep,
        }
    }

    /// Every node, in key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &T)> + ExactSizeIterator + '_ {
        self.index.iter().map(|(k, &i)| (*k, &self.s(i).data))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = K> + ExactSizeIterator + '_ {
        self.index.keys().copied()
    }
}

impl<T: PartialEq, K: Key> PartialEq for Tree<T, K> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.roots().eq(other.roots())
            && self.iter().zip(other.iter()).all(|((ka, a), (kb, b))| {
                ka == kb
                    && a == b
                    && self.parent(&ka) == other.parent(&kb)
                    && self.children(&ka).eq(other.children(&kb))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 Clothing > 2 Lower-wear > 3 Jeans > 4 Loose; 1 > 5 Scarf
    fn wardrobe() -> Tree<&'static str, u32> {
        let mut t = Tree::new();
        t.insert(1, None, None, "Clothing").unwrap();
        t.insert(2, Some(1), None, "Lower-wear").unwrap();
        t.insert(3, Some(2), None, "Jeans").unwrap();
        t.insert(4, Some(3), None, "Loose").unwrap();
        t.insert(5, Some(1), None, "Scarf").unwrap();
        t
    }

    #[test]
    fn builds_walks_and_paths() {
        let t = wardrobe();
        let path: Vec<&str> = t
            .lineage(&4)
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(path, ["Clothing", "Lower-wear", "Jeans", "Loose"]);
        assert_eq!(t.depth(&4), Some(3));
        assert_eq!(t.root_of(&4), Some(1));
        assert_eq!(t.children(&1).collect::<Vec<_>>(), [2, 5]);
        assert_eq!(
            t.subtree(&1, |_, _| true)
                .map(|(k, _)| k)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        assert_eq!(
            t.subtree(&1, |k, _| *k != 3)
                .map(|(k, _)| k)
                .collect::<Vec<_>>(),
            [1, 2, 5],
            "skips 3 and below"
        );
        assert!(t.is_ancestor(&1, &4) && !t.is_ancestor(&4, &1) && !t.is_ancestor(&4, &4));
        let e = t.entry(&2).unwrap();
        assert_eq!((e.key(), *e.data(), e.parent()), (2, "Lower-wear", Some(1)));
        assert_eq!(
            e.child_entries().map(|c| *c.data()).collect::<Vec<_>>(),
            ["Jeans"]
        );
        assert_eq!(t.root_entries().map(|r| r.key()).collect::<Vec<_>>(), [1]);
        assert!(matches!(
            t.clone().insert(9, Some(42), None, "x"),
            Err(Error::NotFound(_))
        ));
        assert!(
            t.clone().insert(9, Some(1), Some(3), "x").is_err(),
            "index out of range"
        );
    }

    #[test]
    fn moves_without_cycles() {
        let mut t = wardrobe();
        assert!(t.move_to(&2, Some(4), None).is_err(), "no cycles");
        assert!(t.move_to(&2, Some(2), None).is_err());
        assert!(!t.move_to(&5, Some(1), Some(1)).unwrap(), "already there");
        assert!(t.move_to(&3, Some(1), Some(0)).unwrap());
        assert_eq!(t.children(&1).collect::<Vec<_>>(), [3, 2, 5]);
        assert_eq!(t.parent(&3), Some(1));
        assert!(t.move_to(&3, None, Some(7)).is_err());
        assert_eq!(
            t.children(&1).collect::<Vec<_>>(),
            [3, 2, 5],
            "a failed move changes nothing"
        );
        assert!(t.move_to(&3, None, None).unwrap());
        assert_eq!(t.roots().collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn removes_subtrees_and_reuses_slots() {
        let mut t = wardrobe();
        let gone = t.remove(&2).unwrap();
        assert_eq!(gone.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(t.len(), 2);
        t.insert(6, Some(1), Some(0), "Hats").unwrap();
        assert_eq!(t.children(&1).collect::<Vec<_>>(), [6, 5]);
        let mut u = Tree::new();
        u.insert(1, None, None, "Clothing").unwrap();
        u.insert(6, Some(1), None, "Hats").unwrap();
        u.insert(5, Some(1), None, "Scarf").unwrap();
        assert_eq!(t, u, "equal regardless of slot reuse");
    }
}
