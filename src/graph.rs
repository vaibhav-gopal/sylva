//! A directed graph with stable keys.
//!
//! Nodes and edges live in dense slots, found by key through an ordered
//! index, so iteration is always in key order (the same on every machine and
//! every replay). Each node keeps its outgoing and incoming edges sorted by
//! edge key, so neighbours, degrees and "is there already an edge from a to
//! b" cost the node's degree, not the size of the graph.

use std::collections::{BTreeMap, VecDeque};

use crate::error::invalid;
use crate::{Error, Key, Result};

/// Which way to follow edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(rename_all = "snake_case"))]
pub enum Dir {
    /// Along edges (from → to).
    #[default]
    Out,
    /// Against edges (who points here).
    In,
    Both,
}

/// A node's edges in one direction: (edge key, edge slot), sorted by key.
type EdgeList<K> = [(K, u32)];

#[derive(Clone, Debug)]
struct NodeSlot<N, K> {
    key: K,
    data: N,
    /// Outgoing edges, sorted by edge key.
    out: Vec<(K, u32)>,
    /// Incoming edges, sorted by edge key.
    inc: Vec<(K, u32)>,
}

#[derive(Clone, Debug)]
struct EdgeSlot<E, K> {
    key: K,
    from: u32,
    to: u32,
    data: E,
}

/// An edge as seen from the graph: its key, its ends and its data.
#[derive(Debug)]
pub struct EdgeRef<'g, E, K> {
    pub key: K,
    pub from: K,
    pub to: K,
    pub data: &'g E,
}

impl<E, K: Copy> Clone for EdgeRef<'_, E, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E, K: Copy> Copy for EdgeRef<'_, E, K> {}

/// A node reached by a walk: how far it is, and the edge that first reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Reached<K> {
    pub node: K,
    pub depth: usize,
    pub via: K,
}

#[derive(Clone, Debug)]
pub struct Graph<N, E, K: Key = uuid::Uuid> {
    nodes: Vec<Option<NodeSlot<N, K>>>,
    free_nodes: Vec<u32>,
    node_index: BTreeMap<K, u32>,
    edges: Vec<Option<EdgeSlot<E, K>>>,
    free_edges: Vec<u32>,
    edge_index: BTreeMap<K, u32>,
}

impl<N, E, K: Key> Default for Graph<N, E, K> {
    fn default() -> Self {
        Graph { nodes: Vec::new(), free_nodes: Vec::new(), node_index: BTreeMap::new(), edges: Vec::new(), free_edges: Vec::new(), edge_index: BTreeMap::new() }
    }
}

fn insert_sorted<K: Ord + Copy>(list: &mut Vec<(K, u32)>, item: (K, u32)) {
    let at = list.partition_point(|(k, _)| *k < item.0);
    list.insert(at, item);
}

fn remove_sorted<K: Ord + Copy>(list: &mut Vec<(K, u32)>, key: &K) {
    if let Ok(at) = list.binary_search_by(|(k, _)| k.cmp(key)) {
        list.remove(at);
    }
}

/// Merges two key-sorted edge lists; an edge in both (a self-loop) comes once, from `a`.
struct Merge<'a, K> {
    a: &'a [(K, u32)],
    b: &'a [(K, u32)],
}

impl<K: Ord + Copy> Iterator for Merge<'_, K> {
    /// (came from `a`, edge slot)
    type Item = (bool, u32);

    fn next(&mut self) -> Option<Self::Item> {
        match (self.a.first(), self.b.first()) {
            (Some(x), Some(y)) if x.0 == y.0 => {
                self.a = &self.a[1..];
                self.b = &self.b[1..];
                Some((true, x.1))
            }
            (Some(x), Some(y)) if x.0 < y.0 => {
                self.a = &self.a[1..];
                Some((true, x.1))
            }
            (_, Some(y)) => {
                self.b = &self.b[1..];
                Some((false, y.1))
            }
            (Some(x), None) => {
                self.a = &self.a[1..];
                Some((true, x.1))
            }
            (None, None) => None,
        }
    }
}

impl<N, E, K: Key> Graph<N, E, K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node_count(&self) -> usize {
        self.node_index.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edge_index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.node_index.is_empty()
    }

    pub fn contains_node(&self, key: &K) -> bool {
        self.node_index.contains_key(key)
    }

    pub fn contains_edge(&self, key: &K) -> bool {
        self.edge_index.contains_key(key)
    }

    fn ns(&self, i: u32) -> &NodeSlot<N, K> {
        self.nodes[i as usize].as_ref().expect("indexed node slots are live")
    }

    fn ns_mut(&mut self, i: u32) -> &mut NodeSlot<N, K> {
        self.nodes[i as usize].as_mut().expect("indexed node slots are live")
    }

    fn es(&self, i: u32) -> &EdgeSlot<E, K> {
        self.edges[i as usize].as_ref().expect("indexed edge slots are live")
    }

    fn edge_ref(&self, i: u32) -> EdgeRef<'_, E, K> {
        let e = self.es(i);
        EdgeRef { key: e.key, from: self.ns(e.from).key, to: self.ns(e.to).key, data: &e.data }
    }

    pub fn node(&self, key: &K) -> Option<&N> {
        self.node_index.get(key).map(|&i| &self.ns(i).data)
    }

    pub fn node_mut(&mut self, key: &K) -> Option<&mut N> {
        let i = *self.node_index.get(key)?;
        Some(&mut self.ns_mut(i).data)
    }

    pub fn edge(&self, key: &K) -> Option<EdgeRef<'_, E, K>> {
        self.edge_index.get(key).map(|&i| self.edge_ref(i))
    }

    pub fn edge_mut(&mut self, key: &K) -> Option<&mut E> {
        let i = *self.edge_index.get(key)?;
        self.edges[i as usize].as_mut().map(|e| &mut e.data)
    }

    pub fn add_node(&mut self, key: K, data: N) -> Result<()> {
        if self.node_index.contains_key(&key) {
            return invalid(format!("node {key} already exists"));
        }
        let slot = NodeSlot { key, data, out: Vec::new(), inc: Vec::new() };
        let i = match self.free_nodes.pop() {
            Some(i) => {
                self.nodes[i as usize] = Some(slot);
                i
            }
            None => {
                self.nodes.push(Some(slot));
                (self.nodes.len() - 1) as u32
            }
        };
        self.node_index.insert(key, i);
        Ok(())
    }

    /// Removes a node and every edge touching it; returns the node's data.
    pub fn remove_node(&mut self, key: &K) -> Option<N> {
        let i = self.node_index.remove(key)?;
        let slot = self.nodes[i as usize].take().expect("indexed node slots are live");
        for (ek, ei) in slot.out.iter().chain(slot.inc.iter()) {
            let Some(e) = self.edges[*ei as usize].take() else { continue }; // a self-loop, already removed
            self.edge_index.remove(ek);
            self.free_edges.push(*ei);
            if e.from != i {
                remove_sorted(&mut self.ns_mut(e.from).out, ek);
            }
            if e.to != i {
                remove_sorted(&mut self.ns_mut(e.to).inc, ek);
            }
        }
        self.free_nodes.push(i);
        Some(slot.data)
    }

    pub fn add_edge(&mut self, key: K, from: K, to: K, data: E) -> Result<()> {
        if self.edge_index.contains_key(&key) {
            return invalid(format!("edge {key} already exists"));
        }
        let f = *self.node_index.get(&from).ok_or_else(|| Error::NotFound(format!("node {from}")))?;
        let t = *self.node_index.get(&to).ok_or_else(|| Error::NotFound(format!("node {to}")))?;
        let slot = EdgeSlot { key, from: f, to: t, data };
        let ei = match self.free_edges.pop() {
            Some(i) => {
                self.edges[i as usize] = Some(slot);
                i
            }
            None => {
                self.edges.push(Some(slot));
                (self.edges.len() - 1) as u32
            }
        };
        self.edge_index.insert(key, ei);
        insert_sorted(&mut self.ns_mut(f).out, (key, ei));
        insert_sorted(&mut self.ns_mut(t).inc, (key, ei));
        Ok(())
    }

    pub fn remove_edge(&mut self, key: &K) -> Option<E> {
        let ei = self.edge_index.remove(key)?;
        let e = self.edges[ei as usize].take().expect("indexed edge slots are live");
        remove_sorted(&mut self.ns_mut(e.from).out, key);
        remove_sorted(&mut self.ns_mut(e.to).inc, key);
        self.free_edges.push(ei);
        Some(e.data)
    }

    /// Every node, in key order.
    pub fn nodes(&self) -> impl DoubleEndedIterator<Item = (K, &N)> + ExactSizeIterator + '_ {
        self.node_index.iter().map(|(k, &i)| (*k, &self.ns(i).data))
    }

    pub fn node_keys(&self) -> impl DoubleEndedIterator<Item = K> + ExactSizeIterator + '_ {
        self.node_index.keys().copied()
    }

    /// Every edge, in key order.
    pub fn edges(&self) -> impl DoubleEndedIterator<Item = EdgeRef<'_, E, K>> + ExactSizeIterator + '_ {
        self.edge_index.values().map(|&i| self.edge_ref(i))
    }

    fn lists(&self, key: &K) -> (&EdgeList<K>, &EdgeList<K>) {
        match self.node_index.get(key) {
            Some(&i) => (&self.ns(i).out, &self.ns(i).inc),
            None => (&[], &[]),
        }
    }

    /// Edges leaving `key`, in edge key order.
    pub fn out_edges(&self, key: &K) -> impl DoubleEndedIterator<Item = EdgeRef<'_, E, K>> + ExactSizeIterator + '_ {
        self.lists(key).0.iter().map(|(_, i)| self.edge_ref(*i))
    }

    /// Edges arriving at `key`, in edge key order.
    pub fn in_edges(&self, key: &K) -> impl DoubleEndedIterator<Item = EdgeRef<'_, E, K>> + ExactSizeIterator + '_ {
        self.lists(key).1.iter().map(|(_, i)| self.edge_ref(*i))
    }

    pub fn degree(&self, key: &K, dir: Dir) -> usize {
        let (out, inc) = self.lists(key);
        match dir {
            Dir::Out => out.len(),
            Dir::In => inc.len(),
            Dir::Both => Merge { a: out, b: inc }.count(),
        }
    }

    /// One step from `key` in `dir`: each edge (in edge key order) with the node at its other end.
    pub fn steps(&self, key: &K, dir: Dir) -> impl Iterator<Item = (EdgeRef<'_, E, K>, K)> + '_ {
        let (out, inc) = self.lists(key);
        let (a, b): (&EdgeList<K>, &EdgeList<K>) = match dir {
            Dir::Out => (out, &[]),
            Dir::In => (&[], inc),
            Dir::Both => (out, inc),
        };
        Merge { a, b }.map(move |(forward, ei)| {
            let e = self.edge_ref(ei);
            let next = if forward { e.to } else { e.from };
            (e, next)
        })
    }

    fn step_slots(&self, node: u32, dir: Dir) -> Merge<'_, K> {
        let s = self.ns(node);
        match dir {
            Dir::Out => Merge { a: &s.out, b: &[] },
            Dir::In => Merge { a: &[], b: &s.inc },
            Dir::Both => Merge { a: &s.out, b: &s.inc },
        }
    }

    /// Nodes within `depth` steps of `start`, breadth first, each once,
    /// nearest first; `follow` decides which edges count.
    pub fn walk(&self, start: &K, dir: Dir, depth: usize, mut follow: impl FnMut(&E) -> bool) -> Vec<Reached<K>> {
        let Some(&s) = self.node_index.get(start) else { return Vec::new() };
        let mut seen = vec![false; self.nodes.len()];
        seen[s as usize] = true;
        let mut queue = VecDeque::from([(s, 0usize)]);
        let mut out = Vec::new();
        while let Some((node, d)) = queue.pop_front() {
            if d == depth {
                continue;
            }
            for (forward, ei) in self.step_slots(node, dir) {
                let e = self.es(ei);
                if !follow(&e.data) {
                    continue;
                }
                let next = if forward { e.to } else { e.from };
                if !seen[next as usize] {
                    seen[next as usize] = true;
                    out.push(Reached { node: self.ns(next).key, depth: d + 1, via: e.key });
                    queue.push_back((next, d + 1));
                }
            }
        }
        out
    }

    /// The shortest path from `from` to `to` (as the edges taken), breadth first.
    pub fn shortest_path(&self, from: &K, to: &K, dir: Dir, max_depth: usize, mut follow: impl FnMut(&E) -> bool) -> Option<Vec<K>> {
        let (&f, &t) = (self.node_index.get(from)?, self.node_index.get(to)?);
        if f == t {
            return Some(Vec::new());
        }
        // For each reached slot: the slot it came from and the edge slot taken.
        let mut back: Vec<Option<(u32, u32)>> = vec![None; self.nodes.len()];
        let mut seen = vec![false; self.nodes.len()];
        seen[f as usize] = true;
        let mut queue = VecDeque::from([(f, 0usize)]);
        while let Some((node, d)) = queue.pop_front() {
            if d == max_depth {
                continue;
            }
            for (forward, ei) in self.step_slots(node, dir) {
                let e = self.es(ei);
                if !follow(&e.data) {
                    continue;
                }
                let next = if forward { e.to } else { e.from };
                if seen[next as usize] {
                    continue;
                }
                seen[next as usize] = true;
                back[next as usize] = Some((node, ei));
                if next == t {
                    let mut edges = vec![self.es(ei).key];
                    let mut cur = node;
                    while let Some((prev, e)) = back[cur as usize] {
                        edges.push(self.es(e).key);
                        cur = prev;
                    }
                    edges.reverse();
                    return Some(edges);
                }
                queue.push_back((next, d + 1));
            }
        }
        None
    }

    /// The nodes in an order where every edge points forward (ties broken by
    /// key), or the nodes that are on or behind a cycle.
    pub fn topological_order(&self) -> std::result::Result<Vec<K>, Vec<K>> {
        let mut indegree: Vec<usize> = self.nodes.iter().map(|n| n.as_ref().map_or(0, |n| n.inc.len())).collect();
        let mut ready: std::collections::BTreeSet<(K, u32)> =
            self.node_index.iter().filter(|(_, &i)| indegree[i as usize] == 0).map(|(k, &i)| (*k, i)).collect();
        let mut out = Vec::with_capacity(self.node_count());
        while let Some((k, i)) = ready.pop_first() {
            out.push(k);
            for (_, ei) in &self.ns(i).out {
                let to = self.es(*ei).to;
                indegree[to as usize] -= 1;
                if indegree[to as usize] == 0 {
                    ready.insert((self.ns(to).key, to));
                }
            }
        }
        if out.len() == self.node_count() {
            Ok(out)
        } else {
            let done: std::collections::BTreeSet<K> = out.into_iter().collect();
            Err(self.node_keys().filter(|k| !done.contains(k)).collect())
        }
    }
}

impl<N: PartialEq, E: PartialEq, K: Key> PartialEq for Graph<N, E, K> {
    fn eq(&self, other: &Self) -> bool {
        self.node_count() == other.node_count()
            && self.edge_count() == other.edge_count()
            && self.nodes().zip(other.nodes()).all(|(a, b)| a == b)
            && self.edges().zip(other.edges()).all(|(a, b)| a.key == b.key && a.from == b.from && a.to == b.to && a.data == b.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// web -calls-> api -calls-> db, worker -calls-> db, api -runs_on-> hub
    fn stack() -> Graph<&'static str, &'static str, u32> {
        let mut g = Graph::new();
        for (k, n) in [(1, "web"), (2, "api"), (3, "worker"), (4, "db"), (5, "hub")] {
            g.add_node(k, n).unwrap();
        }
        for (k, a, b, t) in [(10, 1, 2, "calls"), (11, 2, 4, "calls"), (12, 3, 4, "calls"), (13, 2, 5, "runs_on")] {
            g.add_edge(k, a, b, t).unwrap();
        }
        g
    }

    #[test]
    fn adds_finds_and_walks() {
        let g = stack();
        assert_eq!((g.node_count(), g.edge_count()), (5, 4));
        let mut h = g.clone();
        assert!(h.add_node(1, "dup").is_err());
        assert!(matches!(h.add_edge(99, 1, 42, "x"), Err(Error::NotFound(_))));
        assert_eq!(g.out_edges(&2).map(|e| e.to).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(g.in_edges(&4).map(|e| e.from).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(g.walk(&1, Dir::Out, 1, |_| true).iter().map(|r| r.node).collect::<Vec<_>>(), [2]);
        assert_eq!(g.walk(&1, Dir::Out, 9, |_| true).len(), 3);
        assert_eq!(g.walk(&1, Dir::Out, 9, |t| *t == "calls").len(), 2);
        let hit: Vec<u32> = g.walk(&4, Dir::In, usize::MAX, |_| true).iter().map(|r| r.node).collect();
        assert_eq!(hit, [2, 3, 1]);
        assert_eq!(g.shortest_path(&1, &4, Dir::Out, 10, |_| true), Some(vec![10, 11]));
        assert_eq!(g.shortest_path(&4, &1, Dir::Out, 10, |_| true), None);
        assert_eq!(g.shortest_path(&4, &1, Dir::Both, 10, |_| true).map(|p| p.len()), Some(2));
        assert_eq!(g.degree(&2, Dir::Both), 3);
        assert_eq!(g.topological_order().unwrap()[..2], [1, 2]);
    }

    #[test]
    fn removing_keeps_indexes_consistent() {
        let mut g = stack();
        assert_eq!(g.remove_edge(&11), Some("calls"));
        assert_eq!(g.in_edges(&4).map(|e| e.key).collect::<Vec<_>>(), [12]);
        assert_eq!(g.remove_node(&2), Some("api"));
        assert_eq!(g.edge_count(), 1, "api's edges went with it");
        assert_eq!(g.out_edges(&1).count(), 0);
        // Freed slots are reused without disturbing key order.
        g.add_node(0, "first").unwrap();
        g.add_edge(20, 0, 4, "calls").unwrap();
        assert_eq!(g.nodes().map(|(k, _)| k).collect::<Vec<_>>(), [0, 1, 3, 4, 5]);
        assert_eq!(g.in_edges(&4).map(|e| e.key).collect::<Vec<_>>(), [12, 20]);
        // A self-loop is one edge, in both lists, removed once.
        g.add_edge(30, 5, 5, "self").unwrap();
        assert_eq!(g.steps(&5, Dir::Both).count(), 1);
        g.remove_node(&5);
        assert!(!g.contains_edge(&30));
    }

    #[test]
    fn cycles_are_reported() {
        let mut g = stack();
        g.add_edge(14, 4, 1, "calls").unwrap();
        let on_cycle = g.topological_order().unwrap_err();
        assert!(on_cycle.contains(&1) && on_cycle.contains(&4) && !on_cycle.contains(&3));
    }

    #[test]
    fn equality_ignores_storage_layout() {
        let a = stack();
        let mut b = stack();
        b.add_node(6, "tmp").unwrap();
        b.remove_node(&6);
        assert_eq!(a, b);
        *b.edge_mut(&13).unwrap() = "hosted_on";
        assert_ne!(a, b);
    }
}
