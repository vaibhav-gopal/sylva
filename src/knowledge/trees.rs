//! Trees and catalogs: one primitive for every hierarchy (design trees,
//! clothing categories, item categories, areas, accounts).
//!
//! A tree is built only from unit ops, so replaying the log shows exactly
//! how it grew. Nothing is deleted: retired nodes keep their reason and what
//! replaced them. Category nodes can declare attributes that descendants
//! inherit, which makes a tree a *catalog* schema.

use std::collections::{BTreeMap, BTreeSet};

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use super::attrs::{validate_attr, AttrDef};
use super::text;
use crate::error::invalid;
use crate::tree::{Keys, Tree as Forest};
use crate::{Error, Result};

pub type TreeId = Uuid;
pub type NodeId = Uuid;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// An inner grouping ("Lower-wear", "Auth").
    Category,
    /// Qualifies its parent ("Loose" under "Jeans", "OAuth" under "Login").
    Specifier,
    /// A terminal item: a feature in Tend. Leaves have no children.
    Leaf,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Retired {
    pub reason: String,
    /// Nodes that took over (from a split or merge).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaced_by: Vec<NodeId>,
}

/// A node's own facts; its place in the tree (parent, children) is the tree's.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub description: Option<String>,
    /// Attributes declared here, inherited by descendants.
    pub attrs: BTreeMap<String, AttrDef>,
    pub retired: Option<Retired>,
}

// A node as written out, with its parent and children. (A plain comment: ts-rs
// copies doc comments into the TypeScript, and `Node`'s has none.)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "Node"))]
pub struct NodeJson {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    /// Ordered children (active and retired).
    pub children: Vec<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Attributes declared here, inherited by descendants.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, AttrDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<Retired>,
}

/// A tree as written out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct TreeJson {
    pub id: TreeId,
    pub name: String,
    /// What the tree is for, e.g. `tend`, `catalog`, `areas`, `accounts`.
    pub purpose: String,
    pub roots: Vec<NodeId>,
    pub nodes: BTreeMap<NodeId, NodeJson>,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "TreeJson"))]
pub struct Tree {
    pub id: TreeId,
    pub name: String,
    /// What the tree is for, e.g. `tend`, `catalog`, `areas`, `accounts`.
    pub purpose: String,
    nodes: Forest<Node>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NewNode {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type")]
pub enum TreeOp {
    #[serde(rename = "tree.create")]
    Create {
        id: TreeId,
        name: String,
        purpose: String,
    },
    #[serde(rename = "tree.rename")]
    RenameTree { id: TreeId, name: String },
    /// Adds under `parent` (a root when `None`), at `index` or the end.
    #[serde(rename = "node.add")]
    Add {
        tree: TreeId,
        parent: Option<NodeId>,
        node: NewNode,
        index: Option<usize>,
    },
    #[serde(rename = "node.rename")]
    Rename {
        tree: TreeId,
        id: NodeId,
        name: String,
    },
    #[serde(rename = "node.describe")]
    Describe {
        tree: TreeId,
        id: NodeId,
        text: Option<String>,
    },
    #[serde(rename = "node.kind")]
    SetKind {
        tree: TreeId,
        id: NodeId,
        kind: NodeKind,
    },
    #[serde(rename = "node.move")]
    Move {
        tree: TreeId,
        id: NodeId,
        parent: Option<NodeId>,
        index: Option<usize>,
    },
    /// Replaces a node with new siblings; the original is retired. Its
    /// children stay with it unless moved in the same batch.
    #[serde(rename = "node.split")]
    Split {
        tree: TreeId,
        id: NodeId,
        parts: Vec<NewNode>,
        reason: String,
    },
    /// Replaces several siblings with one new node, which receives all their
    /// active children; the originals are retired.
    #[serde(rename = "node.merge")]
    Merge {
        tree: TreeId,
        ids: Vec<NodeId>,
        into: NewNode,
        reason: String,
    },
    /// Retires a node (and keeps it). Active children must be moved or retired first.
    #[serde(rename = "node.retire")]
    Retire {
        tree: TreeId,
        id: NodeId,
        reason: String,
    },
    #[serde(rename = "node.restore")]
    Restore { tree: TreeId, id: NodeId },
    #[serde(rename = "node.attr.set")]
    SetAttr {
        tree: TreeId,
        id: NodeId,
        name: String,
        def: AttrDef,
    },
    #[serde(rename = "node.attr.remove")]
    RemoveAttr {
        tree: TreeId,
        id: NodeId,
        name: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Trees {
    pub trees: BTreeMap<TreeId, Tree>,
}

impl Trees {
    pub fn apply(&mut self, op: &TreeOp) -> Result<()> {
        if let TreeOp::Create { id, name, purpose } = op {
            if self.trees.contains_key(id) {
                return invalid(format!("tree {id} already exists"));
            }
            let tree = Tree::new(
                *id,
                &text(name, "tree name")?,
                &text(purpose, "tree purpose")?.to_lowercase(),
            );
            self.trees.insert(*id, tree);
            return Ok(());
        }
        let tree_id = match op {
            TreeOp::Create { .. } => unreachable!(),
            TreeOp::RenameTree { id, .. } => id,
            TreeOp::Add { tree, .. }
            | TreeOp::Rename { tree, .. }
            | TreeOp::Describe { tree, .. }
            | TreeOp::SetKind { tree, .. }
            | TreeOp::Move { tree, .. }
            | TreeOp::Split { tree, .. }
            | TreeOp::Merge { tree, .. }
            | TreeOp::Retire { tree, .. }
            | TreeOp::Restore { tree, .. }
            | TreeOp::SetAttr { tree, .. }
            | TreeOp::RemoveAttr { tree, .. } => tree,
        };
        let tree = self
            .trees
            .get_mut(tree_id)
            .ok_or_else(|| Error::NotFound(format!("tree {tree_id}")))?;
        tree.apply(op)
    }
}

impl Tree {
    /// An empty tree (for owners that manage a single tree directly, like Tend).
    pub fn new(id: TreeId, name: &str, purpose: &str) -> Self {
        Tree {
            id,
            name: name.to_string(),
            purpose: purpose.to_string(),
            nodes: Forest::new(),
        }
    }

    // ---- reading ----

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn contains(&self, id: &NodeId) -> bool {
        self.nodes.contains(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every node (active and retired), in id order.
    pub fn nodes(&self) -> impl DoubleEndedIterator<Item = &Node> + ExactSizeIterator + '_ {
        self.nodes.iter().map(|(_, n)| n)
    }

    pub fn ids(&self) -> impl DoubleEndedIterator<Item = NodeId> + ExactSizeIterator + '_ {
        self.nodes.keys()
    }

    pub fn parent(&self, id: &NodeId) -> Option<NodeId> {
        self.nodes.parent(id)
    }

    /// Ordered children (active and retired).
    pub fn children(&self, id: &NodeId) -> Keys<'_, Node, NodeId> {
        self.nodes.children(id)
    }

    pub fn roots(&self) -> Keys<'_, Node, NodeId> {
        self.nodes.roots()
    }

    /// The structure itself, for walks (lineage, subtrees, preorder).
    pub fn forest(&self) -> &Forest<Node> {
        &self.nodes
    }

    /// Exists and isn't retired.
    pub fn is_active(&self, id: &NodeId) -> bool {
        self.nodes.get(id).is_some_and(|n| n.retired.is_none())
    }

    pub fn active_children(&self, id: &NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(|c| self.is_active(c))
    }

    /// Path of names from the root, e.g. `["Clothing", "Lower-wear", "Jeans"]`.
    pub fn path(&self, id: &NodeId) -> Result<Vec<String>> {
        if !self.contains(id) {
            return Err(Error::NotFound(format!("node {id}")));
        }
        let mut out: Vec<String> = self
            .nodes
            .lineage(id)
            .map(|(_, n)| n.name.clone())
            .collect();
        out.reverse();
        Ok(out)
    }

    /// Every attribute that applies at `id`: its own plus its ancestors'.
    pub fn inherited_attrs(&self, id: &NodeId) -> Result<BTreeMap<String, AttrDef>> {
        if !self.contains(id) {
            return Err(Error::NotFound(format!("node {id}")));
        }
        let mut out = BTreeMap::new();
        for (_, node) in self.nodes.lineage(id) {
            for (k, v) in &node.attrs {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        Ok(out)
    }

    // ---- changing ----

    pub fn apply(&mut self, op: &TreeOp) -> Result<()> {
        match op {
            TreeOp::Create { .. } => unreachable!("handled by Trees"),
            TreeOp::RenameTree { name, .. } => {
                let name = text(name, "tree name")?;
                if name == self.name {
                    return invalid("tree.rename does not change anything");
                }
                self.name = name;
            }
            TreeOp::Add {
                parent,
                node,
                index,
                ..
            } => self.add(*parent, node, *index)?,
            TreeOp::Rename { id, name, .. } => {
                let name = text(name, "node name")?;
                if self.require(id)?.name == name {
                    return invalid("node.rename does not change anything");
                }
                self.check_unique(self.parent(id), &name, Some(*id))?;
                self.node_mut(id)?.name = name;
            }
            TreeOp::Describe { id, text: t, .. } => {
                let t = t
                    .as_ref()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let node = self.node_mut(id)?;
                if node.description == t {
                    return invalid("node.describe does not change anything");
                }
                node.description = t;
            }
            TreeOp::SetKind { id, kind, .. } => {
                let node = self.require(id)?;
                if node.kind == *kind {
                    return invalid("node.kind does not change anything");
                }
                if *kind == NodeKind::Leaf && self.nodes.child_count(id) > 0 {
                    return invalid("a node with children can't become a leaf");
                }
                self.node_mut(id)?.kind = *kind;
            }
            TreeOp::Move {
                id, parent, index, ..
            } => self.move_node(*id, *parent, *index)?,
            TreeOp::Split {
                id, parts, reason, ..
            } => {
                let reason = text(reason, "reason")?;
                if parts.len() < 2 {
                    return invalid("a split needs at least two parts");
                }
                self.active(id)?;
                let parent = self.parent(id);
                let at = self.position(id)? + 1;
                for (i, part) in parts.iter().enumerate() {
                    // A part may reuse the original's name; the original is retired below.
                    self.add_unchecked_name(parent, part, Some(at + i), &[*id])?;
                }
                self.retire(*id, reason, parts.iter().map(|p| p.id).collect(), true)?;
            }
            TreeOp::Merge {
                ids, into, reason, ..
            } => {
                let reason = text(reason, "reason")?;
                if ids.len() < 2 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
                    return invalid("a merge needs at least two distinct nodes");
                }
                self.active(&ids[0])?;
                let parent = self.parent(&ids[0]);
                for id in ids {
                    self.active(id)?;
                    if self.parent(id) != parent {
                        return invalid("merged nodes must be siblings");
                    }
                }
                let at = self.position(&ids[0])?;
                // The originals keep their names until retired, so check uniqueness against the rest.
                self.add_unchecked_name(parent, into, Some(at), ids)?;
                for id in ids {
                    let kids: Vec<NodeId> = self.active_children(id).collect();
                    for kid in kids {
                        self.move_node(kid, Some(into.id), None)?;
                    }
                    self.retire(*id, reason.clone(), vec![into.id], false)?;
                }
            }
            TreeOp::Retire { id, reason, .. } => {
                let reason = text(reason, "reason")?;
                self.retire(*id, reason, Vec::new(), false)?;
            }
            TreeOp::Restore { id, .. } => {
                let node = self.require(id)?;
                if node.retired.is_none() {
                    return invalid("node is not retired");
                }
                let (parent, name) = (self.parent(id), node.name.clone());
                if let Some(p) = parent {
                    self.active(&p)
                        .map_err(|_| Error::Invalid("restore the parent first".into()))?;
                }
                self.check_unique(parent, &name, Some(*id))?;
                self.node_mut(id)?.retired = None;
            }
            TreeOp::SetAttr { id, name, def, .. } => {
                let name = text(name, "attribute name")?.to_lowercase();
                validate_attr(def)?;
                if self.inherited_attrs(id)?.contains_key(&name)
                    && !self.require(id)?.attrs.contains_key(&name)
                {
                    return invalid(format!(
                        "attribute '{name}' is already defined by an ancestor"
                    ));
                }
                let node = self.node_mut(id)?;
                if node.attrs.get(&name) == Some(def) {
                    return invalid("node.attr.set does not change anything");
                }
                node.attrs.insert(name, def.clone());
            }
            TreeOp::RemoveAttr { id, name, .. } => {
                let name = name.trim().to_lowercase();
                if self.node_mut(id)?.attrs.remove(&name).is_none() {
                    return Err(Error::NotFound(format!("attribute {name}")));
                }
            }
        }
        Ok(())
    }

    fn add(&mut self, parent: Option<NodeId>, new: &NewNode, index: Option<usize>) -> Result<()> {
        let name = text(&new.name, "node name")?;
        self.check_unique(parent, &name, None)?;
        self.add_unchecked_name(
            parent,
            &NewNode {
                name,
                ..new.clone()
            },
            index,
            &[],
        )
    }

    /// Adds a node, checking names only against siblings not in `ignore`.
    fn add_unchecked_name(
        &mut self,
        parent: Option<NodeId>,
        new: &NewNode,
        index: Option<usize>,
        ignore: &[NodeId],
    ) -> Result<()> {
        if self.nodes.contains(&new.id) {
            return invalid(format!("node {} already exists", new.id));
        }
        let name = text(&new.name, "node name")?;
        let clash = self
            .nodes
            .siblings(parent.as_ref())
            .any(|s| !ignore.contains(&s) && self.same_name_active(&s, &name));
        if clash {
            return invalid(format!("a sibling is already named '{name}'"));
        }
        if let Some(p) = parent {
            if self.active(&p)?.kind == NodeKind::Leaf {
                return invalid("leaves can't have children");
            }
        }
        let node = Node {
            id: new.id,
            name,
            kind: new.kind,
            description: None,
            attrs: BTreeMap::new(),
            retired: None,
        };
        self.nodes.insert(new.id, parent, index, node)
    }

    fn move_node(
        &mut self,
        id: NodeId,
        parent: Option<NodeId>,
        index: Option<usize>,
    ) -> Result<()> {
        let name = self.active(&id)?.name.clone();
        let old_parent = self.parent(&id);
        if let Some(p) = parent {
            if p == id || self.nodes.is_ancestor(&id, &p) {
                return invalid("can't move a node under itself");
            }
            if self.active(&p)?.kind == NodeKind::Leaf {
                return invalid("leaves can't have children");
            }
        }
        if old_parent != parent {
            self.check_unique(parent, &name, Some(id))?;
        }
        if !self.nodes.move_to(&id, parent, index)? {
            return invalid("node.move does not change anything");
        }
        Ok(())
    }

    fn retire(
        &mut self,
        id: NodeId,
        reason: String,
        replaced_by: Vec<NodeId>,
        keep_children: bool,
    ) -> Result<()> {
        self.active(&id)?;
        if !keep_children && self.active_children(&id).next().is_some() {
            return invalid("move or retire this node's active children first");
        }
        self.node_mut(&id)?.retired = Some(Retired {
            reason,
            replaced_by,
        });
        Ok(())
    }

    fn same_name_active(&self, id: &NodeId, name: &str) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|n| n.retired.is_none() && n.name.eq_ignore_ascii_case(name))
    }

    fn check_unique(
        &self,
        parent: Option<NodeId>,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<()> {
        let clash = self
            .nodes
            .siblings(parent.as_ref())
            .any(|s| Some(s) != except && self.same_name_active(&s, name));
        if clash {
            return invalid(format!("a sibling is already named '{name}'"));
        }
        Ok(())
    }

    fn position(&self, id: &NodeId) -> Result<usize> {
        self.nodes
            .position(id)
            .ok_or_else(|| Error::NotFound(format!("node {id}")))
    }

    fn require(&self, id: &NodeId) -> Result<&Node> {
        self.nodes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("node {id}")))
    }

    fn node_mut(&mut self, id: &NodeId) -> Result<&mut Node> {
        self.nodes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("node {id}")))
    }

    fn active(&self, id: &NodeId) -> Result<&Node> {
        let node = self.require(id)?;
        if node.retired.is_some() {
            return invalid(format!("node '{}' is retired", node.name));
        }
        Ok(node)
    }

    // ---- writing out ----

    /// The written-out form (what [`Serialize`] produces).
    pub fn to_json(&self) -> TreeJson {
        TreeJson {
            id: self.id,
            name: self.name.clone(),
            purpose: self.purpose.clone(),
            roots: self.roots().collect(),
            nodes: self.nodes().map(|n| (n.id, self.node_json(n))).collect(),
        }
    }

    fn node_json(&self, n: &Node) -> NodeJson {
        NodeJson {
            id: n.id,
            name: n.name.clone(),
            kind: n.kind,
            parent: self.parent(&n.id),
            children: self.children(&n.id).collect(),
            description: n.description.clone(),
            attrs: n.attrs.clone(),
            retired: n.retired.clone(),
        }
    }

    /// Rebuilds a tree from its written-out form, checking that parents and children agree.
    pub fn from_json(j: TreeJson) -> Result<Tree> {
        let mut tree = Tree::new(j.id, &j.name, &j.purpose);
        let mut queue: std::collections::VecDeque<(NodeId, Option<NodeId>)> =
            j.roots.iter().map(|r| (*r, None)).collect();
        let mut nodes = j.nodes;
        while let Some((id, parent)) = queue.pop_front() {
            let n = nodes.remove(&id).ok_or_else(|| {
                Error::Invalid(format!("node {id} is listed but missing (or listed twice)"))
            })?;
            if n.parent != parent {
                return invalid(format!(
                    "node {id} says its parent is {:?}, but it's listed under {:?}",
                    n.parent, parent
                ));
            }
            queue.extend(n.children.iter().map(|c| (*c, Some(id))));
            let node = Node {
                id,
                name: n.name,
                kind: n.kind,
                description: n.description,
                attrs: n.attrs,
                retired: n.retired,
            };
            tree.nodes.insert(id, parent, None, node)?;
        }
        if let Some(stray) = nodes.keys().next() {
            return invalid(format!("node {stray} isn't reachable from the roots"));
        }
        Ok(tree)
    }
}

/// Serializes straight from the structure (no intermediate copy of the tree).
impl Serialize for Tree {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        struct Ids<'a>(Keys<'a, Node, NodeId>);
        impl Serialize for Ids<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.collect_seq(self.0.clone())
            }
        }
        #[derive(Serialize)]
        struct NodeOut<'a> {
            id: &'a NodeId,
            name: &'a str,
            kind: NodeKind,
            parent: Option<NodeId>,
            children: Ids<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<&'a String>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            attrs: &'a BTreeMap<String, AttrDef>,
            #[serde(skip_serializing_if = "Option::is_none")]
            retired: Option<&'a Retired>,
        }
        struct NodesOut<'a>(&'a Tree);
        impl Serialize for NodesOut<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                let t = self.0;
                let mut map = s.serialize_map(Some(t.len()))?;
                for n in t.nodes() {
                    let out = NodeOut {
                        id: &n.id,
                        name: &n.name,
                        kind: n.kind,
                        parent: t.parent(&n.id),
                        children: Ids(t.children(&n.id)),
                        description: n.description.as_ref(),
                        attrs: &n.attrs,
                        retired: n.retired.as_ref(),
                    };
                    map.serialize_entry(&n.id, &out)?;
                }
                map.end()
            }
        }
        #[derive(Serialize)]
        struct TreeOut<'a> {
            id: &'a TreeId,
            name: &'a str,
            purpose: &'a str,
            roots: Ids<'a>,
            nodes: NodesOut<'a>,
        }
        TreeOut {
            id: &self.id,
            name: &self.name,
            purpose: &self.purpose,
            roots: Ids(self.roots()),
            nodes: NodesOut(self),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Tree {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Tree::from_json(TreeJson::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::AttrType;

    struct T {
        trees: Trees,
        tree: TreeId,
    }

    impl T {
        fn new() -> Self {
            let mut trees = Trees::default();
            let tree = Uuid::now_v7();
            trees
                .apply(&TreeOp::Create {
                    id: tree,
                    name: "Wardrobe".into(),
                    purpose: "catalog".into(),
                })
                .unwrap();
            T { trees, tree }
        }
        fn add(&mut self, parent: Option<NodeId>, name: &str, kind: NodeKind) -> NodeId {
            let id = Uuid::now_v7();
            self.op(TreeOp::Add {
                tree: self.tree,
                parent,
                node: NewNode {
                    id,
                    name: name.into(),
                    kind,
                },
                index: None,
            })
            .unwrap();
            id
        }
        fn op(&mut self, op: TreeOp) -> Result<()> {
            self.trees.apply(&op)
        }
        fn t(&self) -> &Tree {
            &self.trees.trees[&self.tree]
        }
        fn names(&self, parent: Option<NodeId>) -> Vec<(String, bool)> {
            let t = self.t();
            t.forest()
                .siblings(parent.as_ref())
                .map(|c| {
                    (
                        t.node(&c).unwrap().name.clone(),
                        t.node(&c).unwrap().retired.is_some(),
                    )
                })
                .collect()
        }
    }

    #[test]
    fn builds_and_moves() {
        let mut t = T::new();
        let clothing = t.add(None, "Clothing", NodeKind::Category);
        let lower = t.add(Some(clothing), "Lower-wear", NodeKind::Category);
        let jeans = t.add(Some(lower), "Jeans", NodeKind::Category);
        let loose = t.add(Some(jeans), "Loose", NodeKind::Specifier);
        assert_eq!(
            t.t().path(&loose).unwrap(),
            ["Clothing", "Lower-wear", "Jeans", "Loose"]
        );

        assert!(
            t.op(TreeOp::Move {
                tree: t.tree,
                id: lower,
                parent: Some(loose),
                index: None
            })
            .is_err(),
            "no cycles"
        );
        let dup = Uuid::now_v7();
        let clash = TreeOp::Add {
            tree: t.tree,
            parent: Some(jeans),
            node: NewNode {
                id: dup,
                name: "loose".into(),
                kind: NodeKind::Specifier,
            },
            index: None,
        };
        assert!(
            t.op(clash).is_err(),
            "sibling names are unique, ignoring case"
        );

        let leaf = t.add(Some(clothing), "Scarf", NodeKind::Leaf);
        assert!(
            t.op(TreeOp::Move {
                tree: t.tree,
                id: jeans,
                parent: Some(leaf),
                index: None
            })
            .is_err(),
            "leaves have no children"
        );
        t.op(TreeOp::Move {
            tree: t.tree,
            id: jeans,
            parent: Some(clothing),
            index: Some(0),
        })
        .unwrap();
        assert_eq!(
            t.names(Some(clothing))
                .iter()
                .map(|x| x.0.as_str())
                .collect::<Vec<_>>(),
            ["Jeans", "Lower-wear", "Scarf"]
        );
        let missing = TreeOp::Add {
            tree: t.tree,
            parent: Some(Uuid::now_v7()),
            node: NewNode {
                id: Uuid::now_v7(),
                name: "x".into(),
                kind: NodeKind::Leaf,
            },
            index: None,
        };
        assert!(
            matches!(t.op(missing), Err(Error::NotFound(_))),
            "a missing parent is an error, not a panic"
        );
        let before = t.t().clone();
        assert!(t
            .op(TreeOp::Move {
                tree: t.tree,
                id: jeans,
                parent: Some(lower),
                index: Some(9)
            })
            .is_err());
        assert_eq!(t.t(), &before, "a failed move changes nothing");
    }

    #[test]
    fn split_merge_retire_keep_history() {
        let mut t = T::new();
        let auth = t.add(None, "Auth", NodeKind::Category);
        let login = t.add(Some(auth), "Login", NodeKind::Leaf);
        let (pw, pk) = (Uuid::now_v7(), Uuid::now_v7());
        t.op(TreeOp::Split {
            tree: t.tree,
            id: login,
            parts: vec![
                NewNode {
                    id: pw,
                    name: "Password login".into(),
                    kind: NodeKind::Leaf,
                },
                NewNode {
                    id: pk,
                    name: "Passkey login".into(),
                    kind: NodeKind::Leaf,
                },
            ],
            reason: "passkeys need their own flow".into(),
        })
        .unwrap();
        assert_eq!(
            t.names(Some(auth)),
            [
                ("Login".into(), true),
                ("Password login".into(), false),
                ("Passkey login".into(), false)
            ]
        );
        assert_eq!(
            t.t()
                .node(&login)
                .unwrap()
                .retired
                .as_ref()
                .unwrap()
                .replaced_by,
            vec![pw, pk]
        );

        let merged = Uuid::now_v7();
        t.op(TreeOp::Merge {
            tree: t.tree,
            ids: vec![pw, pk],
            into: NewNode {
                id: merged,
                name: "Login".into(),
                kind: NodeKind::Category,
            },
            reason: "one sign-in screen after all".into(),
        })
        .unwrap();
        assert!(
            t.t().node(&pw).unwrap().retired.is_some()
                && t.t().node(&merged).unwrap().retired.is_none()
        );

        assert!(
            t.op(TreeOp::Retire {
                tree: t.tree,
                id: auth,
                reason: "x".into()
            })
            .is_err(),
            "active children first"
        );
        t.op(TreeOp::Retire {
            tree: t.tree,
            id: merged,
            reason: "moved to SSO".into(),
        })
        .unwrap();
        t.op(TreeOp::Retire {
            tree: t.tree,
            id: auth,
            reason: "moved to SSO".into(),
        })
        .unwrap();
        assert_eq!(t.t().len(), 5, "nothing is ever deleted");
        t.op(TreeOp::Restore {
            tree: t.tree,
            id: auth,
        })
        .unwrap();
        assert!(
            t.op(TreeOp::Retire {
                tree: t.tree,
                id: auth,
                reason: " ".into()
            })
            .is_err(),
            "a reason is required"
        );
    }

    #[test]
    fn attributes_are_inherited() {
        let mut t = T::new();
        let clothing = t.add(None, "Clothing", NodeKind::Category);
        let jeans = t.add(Some(clothing), "Jeans", NodeKind::Category);
        let formality = AttrDef {
            ty: AttrType::Scale,
            options: vec![],
            unit: None,
            min: Some(1),
            max: Some(5),
            required: true,
        };
        t.op(TreeOp::SetAttr {
            tree: t.tree,
            id: clothing,
            name: "Formality".into(),
            def: formality.clone(),
        })
        .unwrap();
        let fit = AttrDef {
            ty: AttrType::Enum,
            options: vec!["slim".into(), "loose".into()],
            unit: None,
            min: None,
            max: None,
            required: false,
        };
        t.op(TreeOp::SetAttr {
            tree: t.tree,
            id: jeans,
            name: "fit".into(),
            def: fit,
        })
        .unwrap();
        let attrs = t.t().inherited_attrs(&jeans).unwrap();
        assert_eq!(attrs.keys().collect::<Vec<_>>(), ["fit", "formality"]);
        assert!(
            t.op(TreeOp::SetAttr {
                tree: t.tree,
                id: jeans,
                name: "formality".into(),
                def: formality
            })
            .is_err(),
            "no shadowing"
        );
        let bad = AttrDef {
            ty: AttrType::Enum,
            options: vec![],
            unit: None,
            min: None,
            max: None,
            required: false,
        };
        assert!(t
            .op(TreeOp::SetAttr {
                tree: t.tree,
                id: jeans,
                name: "wash".into(),
                def: bad
            })
            .is_err());
    }

    #[test]
    fn writes_out_and_reads_back_the_same_json() {
        let mut t = T::new();
        let clothing = t.add(None, "Clothing", NodeKind::Category);
        let jeans = t.add(Some(clothing), "Jeans", NodeKind::Category);
        t.add(Some(clothing), "Scarf", NodeKind::Leaf);
        t.op(TreeOp::Describe {
            tree: t.tree,
            id: jeans,
            text: Some("denim".into()),
        })
        .unwrap();
        let tree = t.t().clone();
        let json = serde_json::to_value(&tree).unwrap();
        assert_eq!(
            json,
            serde_json::to_value(tree.to_json()).unwrap(),
            "streamed and built forms agree"
        );
        assert_eq!(
            json["nodes"][jeans.to_string()]["parent"],
            serde_json::json!(clothing)
        );
        assert!(
            json["nodes"][clothing.to_string()]
                .get("description")
                .is_none(),
            "empty fields are left out"
        );
        let back: Tree = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, tree);
        let mut broken = json;
        broken["nodes"][jeans.to_string()]["parent"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Tree>(broken).is_err());
    }
}
