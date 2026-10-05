//! Graphs: the primitive for networks rather than hierarchies.
//!
//! A graph holds typed nodes and edges whose props are JSON values. Every
//! prop, node and edge records where it came from: **stated** (you, or an
//! agent) or **observed** (a scanner). Observations never overwrite what was
//! stated, so re-scanning is always safe. A node can stand for an entity
//! owned elsewhere (`ref`), so graphs connect facts without copying them.
//!
//! Schemas are optional: node types with their props, and edge types with the
//! node types they may connect.

pub mod change;
pub mod query;
pub mod render;

use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use super::attrs::{check_value, validate_attr, AttrDef};
use super::refs::EntityRef;
use super::text;
use crate::error::invalid;
use crate::graph::{Dir, Graph as Structure};
use crate::{Error, Result};

pub type GraphId = Uuid;
pub type NodeId = Uuid;
pub type EdgeId = Uuid;

/// Where a fact came from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "FactSource"))]
#[serde(tag = "kind", content = "by", rename_all = "snake_case")]
pub enum Source {
    /// Said by you or an agent.
    Stated,
    /// Seen by a scanner (its name, e.g. `scan:repos`).
    Observed(String),
}

impl Source {
    pub fn is_stated(&self) -> bool {
        matches!(self, Source::Stated)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Fact {
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub value: Value,
    pub source: Source,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "GraphNode"))]
pub struct Node {
    pub id: NodeId,
    #[serde(rename = "type")]
    pub ty: String,
    pub props: BTreeMap<String, Fact>,
    /// The entity this node stands for, when it's owned elsewhere.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "string | null"))]
    pub reference: Option<EntityRef>,
    pub source: Source,
}

impl Node {
    /// `label`, else `name`, else the type and a short id.
    pub fn label(&self) -> String {
        ["label", "name"]
            .iter()
            .find_map(|k| {
                self.props
                    .get(*k)
                    .and_then(|f| f.value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("{} {}", self.ty, &self.id.to_string()[..8]))
    }

    pub fn prop(&self, key: &str) -> Option<&Value> {
        self.props.get(key).map(|f| &f.value)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "GraphEdge"))]
pub struct Edge {
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    #[serde(rename = "type")]
    pub ty: String,
    pub props: BTreeMap<String, Fact>,
    pub source: Source,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NodeType {
    #[serde(default)]
    pub props: BTreeMap<String, AttrDef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct EdgeType {
    /// Node types an edge may start from (empty: any).
    #[serde(default)]
    pub from: Vec<String>,
    /// Node types it may end at (empty: any).
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub props: BTreeMap<String, AttrDef>,
    /// What the edge is called read backwards (`hosts` → `hosted_by`): an edge
    /// is stored once, and queries may name either direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "GraphSchema"))]
pub struct Schema {
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeType>,
    #[serde(default)]
    pub edges: BTreeMap<String, EdgeType>,
    /// Allow node and edge types (and props) the schema doesn't list.
    #[serde(default)]
    pub open: bool,
}

impl Schema {
    /// The edge type a name means and whether it reads backwards: `hosts` is
    /// (`hosts`, false), its inverse `hosted_by` is (`hosts`, true). Names the
    /// schema doesn't list read forwards as themselves.
    pub fn edge_name<'a>(&'a self, name: &'a str) -> (&'a str, bool) {
        if self.edges.contains_key(name) {
            return (name, false);
        }
        match self.edges.iter().find(|(_, t)| t.inverse.as_deref() == Some(name)) {
            Some((ty, _)) => (ty.as_str(), true),
            None => (name, false),
        }
    }
}

/// A graph as written out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GraphJson {
    pub id: GraphId,
    pub name: String,
    /// What it's about, for read permissions: `machines`, `network`, `stacks`,
    /// `projects`, `schemas`, `personal` …
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Schema>,
    pub nodes: BTreeMap<NodeId, Node>,
    pub edges: BTreeMap<EdgeId, Edge>,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "GraphJson"))]
pub struct Graph {
    pub id: GraphId,
    pub name: String,
    /// What it's about, for read permissions: `machines`, `network`, `stacks`,
    /// `projects`, `schemas`, `personal` …
    pub scope: String,
    pub schema: Option<Schema>,
    inner: Structure<Node, Edge>,
}

impl Graph {
    pub fn new(id: GraphId, name: &str, scope: &str) -> Graph {
        Graph {
            id,
            name: name.to_string(),
            scope: scope.to_string(),
            schema: None,
            inner: Structure::new(),
        }
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.inner.node(id)
    }

    pub fn edge(&self, id: &EdgeId) -> Option<&Edge> {
        self.inner.edge(id).map(|e| e.data)
    }

    /// Every node, in id order.
    pub fn nodes(&self) -> impl DoubleEndedIterator<Item = &Node> + ExactSizeIterator + '_ {
        self.inner.nodes().map(|(_, n)| n)
    }

    /// Every edge, in id order.
    pub fn edges(&self) -> impl DoubleEndedIterator<Item = &Edge> + ExactSizeIterator + '_ {
        self.inner.edges().map(|e| e.data)
    }

    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    /// Edges leaving a node, in id order.
    pub fn out_edges(&self, id: &NodeId) -> impl Iterator<Item = &Edge> + '_ {
        self.inner.out_edges(id).map(|e| e.data)
    }

    /// Edges arriving at a node, in id order.
    pub fn in_edges(&self, id: &NodeId) -> impl Iterator<Item = &Edge> + '_ {
        self.inner.in_edges(id).map(|e| e.data)
    }

    /// The structure itself, for walks and other algorithms.
    pub fn structure(&self) -> &Structure<Node, Edge> {
        &self.inner
    }

    pub fn to_json(&self) -> GraphJson {
        GraphJson {
            id: self.id,
            name: self.name.clone(),
            scope: self.scope.clone(),
            schema: self.schema.clone(),
            nodes: self.nodes().map(|n| (n.id, n.clone())).collect(),
            edges: self.edges().map(|e| (e.id, e.clone())).collect(),
        }
    }

    pub fn from_json(j: GraphJson) -> Result<Graph> {
        let mut g = Graph {
            id: j.id,
            name: j.name,
            scope: j.scope,
            schema: j.schema,
            inner: Structure::new(),
        };
        for (id, n) in j.nodes {
            g.inner.add_node(id, n)?;
        }
        for (id, e) in j.edges {
            g.inner.add_edge(id, e.from, e.to, e)?;
        }
        Ok(g)
    }
}

/// Serializes straight from the structure (no intermediate copy).
impl Serialize for Graph {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        struct Nodes<'a>(&'a Graph);
        impl Serialize for Nodes<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                let mut m = s.serialize_map(Some(self.0.node_count()))?;
                for n in self.0.nodes() {
                    m.serialize_entry(&n.id, n)?;
                }
                m.end()
            }
        }
        struct Edges<'a>(&'a Graph);
        impl Serialize for Edges<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                let mut m = s.serialize_map(Some(self.0.edge_count()))?;
                for e in self.0.edges() {
                    m.serialize_entry(&e.id, e)?;
                }
                m.end()
            }
        }
        #[derive(Serialize)]
        struct Out<'a> {
            id: &'a GraphId,
            name: &'a str,
            scope: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            schema: Option<&'a Schema>,
            nodes: Nodes<'a>,
            edges: Edges<'a>,
        }
        Out {
            id: &self.id,
            name: &self.name,
            scope: &self.scope,
            schema: self.schema.as_ref(),
            nodes: Nodes(self),
            edges: Edges(self),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Graph {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Graph::from_json(GraphJson::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type")]
pub enum GraphOp {
    #[serde(rename = "graph.create")]
    Create {
        id: GraphId,
        name: String,
        scope: String,
    },
    #[serde(rename = "graph.update")]
    Update {
        id: GraphId,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        scope: Option<String>,
    },
    /// Sets (or clears) the schema; everything already in the graph must satisfy it.
    #[serde(rename = "graph.schema")]
    SetSchema { id: GraphId, schema: Option<Schema> },
    #[serde(rename = "graph.delete")]
    Delete { id: GraphId },
    #[serde(rename = "graph.node.add")]
    NodeAdd {
        graph: GraphId,
        id: NodeId,
        #[serde(rename = "node_type")]
        ty: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
        props: BTreeMap<String, Value>,
        #[serde(rename = "ref", default)]
        #[cfg_attr(feature = "ts", ts(type = "string | null"))]
        reference: Option<EntityRef>,
        source: Source,
    },
    /// Sets one prop (`None` removes it).
    #[serde(rename = "graph.node.set")]
    NodeSet {
        graph: GraphId,
        id: NodeId,
        key: String,
        #[cfg_attr(feature = "ts", ts(type = "unknown"))]
        value: Option<Value>,
        source: Source,
    },
    /// Removes a node; its edges must be removed first.
    #[serde(rename = "graph.node.remove")]
    NodeRemove {
        graph: GraphId,
        id: NodeId,
        source: Source,
    },
    #[serde(rename = "graph.edge.add")]
    EdgeAdd {
        graph: GraphId,
        id: EdgeId,
        from: NodeId,
        to: NodeId,
        #[serde(rename = "edge_type")]
        ty: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "Record<string, unknown>"))]
        props: BTreeMap<String, Value>,
        source: Source,
    },
    #[serde(rename = "graph.edge.set")]
    EdgeSet {
        graph: GraphId,
        id: EdgeId,
        key: String,
        #[cfg_attr(feature = "ts", ts(type = "unknown"))]
        value: Option<Value>,
        source: Source,
    },
    #[serde(rename = "graph.edge.remove")]
    EdgeRemove {
        graph: GraphId,
        id: EdgeId,
        source: Source,
    },
}

impl GraphOp {
    /// The entity a new node points at, so the owner of the graphs can check it exists.
    pub fn reference(&self) -> Option<&EntityRef> {
        match self {
            GraphOp::NodeAdd { reference, .. } => reference.as_ref(),
            _ => None,
        }
    }

    /// The graph this op changes.
    pub fn graph_id(&self) -> GraphId {
        match self {
            GraphOp::Create { id, .. }
            | GraphOp::Update { id, .. }
            | GraphOp::SetSchema { id, .. }
            | GraphOp::Delete { id } => *id,
            GraphOp::NodeAdd { graph, .. }
            | GraphOp::NodeSet { graph, .. }
            | GraphOp::NodeRemove { graph, .. }
            | GraphOp::EdgeAdd { graph, .. }
            | GraphOp::EdgeSet { graph, .. }
            | GraphOp::EdgeRemove { graph, .. } => *graph,
        }
    }

    /// What the op does, in words, against the graphs as they were just
    /// before it (so a removed node still has its label): `added tool git`,
    /// `git.version = "2.55"`, `vghydra -has-> git`.
    pub fn describe(&self, before: &Graphs) -> String {
        let g = before.graphs.get(&self.graph_id());
        let short = |id: &Uuid| id.to_string()[..13].to_string();
        let node = |id: &NodeId| {
            g.and_then(|g| g.node(id))
                .map(Node::label)
                .unwrap_or_else(|| short(id))
        };
        let edge = |id: &EdgeId| {
            g.and_then(|g| g.edge(id))
                .map(|e| format!("{} -{}-> {}", node(&e.from), e.ty, node(&e.to)))
                .unwrap_or_else(|| short(id))
        };
        match self {
            GraphOp::Create { name, scope, .. } => format!("created graph {name} ({scope})"),
            GraphOp::Update { name, scope, .. } => {
                let mut parts = Vec::new();
                if let Some(n) = name {
                    parts.push(format!("renamed to {n}"));
                }
                if let Some(s) = scope {
                    parts.push(format!("scope {s}"));
                }
                format!("graph {}", parts.join(", "))
            }
            GraphOp::SetSchema { schema, .. } => if schema.is_some() {
                "set the schema"
            } else {
                "cleared the schema"
            }
            .into(),
            GraphOp::Delete { .. } => "deleted the graph".into(),
            GraphOp::NodeAdd { ty, props, .. } => {
                match ["label", "name"]
                    .iter()
                    .find_map(|k| props.get(*k).and_then(Value::as_str))
                {
                    Some(label) => format!("added {ty} {label}"),
                    None => format!("added a {ty}"),
                }
            }
            GraphOp::NodeSet { id, key, value, .. } => match value {
                Some(v) => format!("{}.{key} = {v}", node(id)),
                None => format!("removed {}.{key}", node(id)),
            },
            GraphOp::NodeRemove { id, .. } => format!("removed {}", node(id)),
            GraphOp::EdgeAdd { from, to, ty, .. } => {
                format!("{} -{ty}-> {}", node(from), node(to))
            }
            GraphOp::EdgeSet { id, key, value, .. } => match value {
                Some(v) => format!("{} {key} = {v}", edge(id)),
                None => format!("removed {key} from {}", edge(id)),
            },
            GraphOp::EdgeRemove { id, .. } => format!("removed {}", edge(id)),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Graphs {
    pub graphs: BTreeMap<GraphId, Graph>,
}

impl Graphs {
    /// Applies one op. References (`ref`) are checked by the caller, which
    /// knows what entities exist.
    pub fn apply(&mut self, op: &GraphOp) -> Result<()> {
        match op {
            GraphOp::Create { id, name, scope } => {
                if self.graphs.contains_key(id) {
                    return invalid(format!("graph {id} already exists"));
                }
                let name = text(name, "graph name")?;
                if self.named(&name, None) {
                    return invalid(format!("a graph is already named '{name}'"));
                }
                self.graphs
                    .insert(*id, Graph::new(*id, &name, &scope_name(scope)?));
            }
            GraphOp::Update { id, name, scope } => {
                let name = name.as_deref().map(|n| text(n, "graph name")).transpose()?;
                if let Some(n) = &name {
                    if self.named(n, Some(*id)) {
                        return invalid(format!("a graph is already named '{n}'"));
                    }
                }
                let scope = scope.as_deref().map(scope_name).transpose()?;
                let g = self.graph_mut(id)?;
                let mut changed = false;
                if let Some(n) = name.filter(|n| *n != g.name) {
                    g.name = n;
                    changed = true;
                }
                if let Some(s) = scope.filter(|s| *s != g.scope) {
                    g.scope = s;
                    changed = true;
                }
                if !changed {
                    return invalid("graph.update does not change anything");
                }
            }
            GraphOp::SetSchema { id, schema } => {
                let g = self.graph_mut(id)?;
                if g.schema == *schema {
                    return invalid("graph.schema does not change anything");
                }
                if let Some(s) = schema {
                    for def in s
                        .nodes
                        .values()
                        .flat_map(|t| t.props.values())
                        .chain(s.edges.values().flat_map(|t| t.props.values()))
                    {
                        validate_attr(def)?;
                    }
                    for n in g.nodes() {
                        check_node(s, n).map_err(|e| {
                            Error::Invalid(format!(
                                "node '{}' doesn't fit the new schema: {e}",
                                n.label()
                            ))
                        })?;
                    }
                    for e in g.edges() {
                        check_edge(s, g, e).map_err(|err| {
                            Error::Invalid(format!(
                                "an edge '{}' doesn't fit the new schema: {err}",
                                e.ty
                            ))
                        })?;
                    }
                }
                g.schema = schema.clone();
            }
            GraphOp::Delete { id } => {
                if self.graphs.remove(id).is_none() {
                    return Err(Error::NotFound(format!("graph {id}")));
                }
            }
            GraphOp::NodeAdd {
                graph,
                id,
                ty,
                props,
                reference,
                source,
            } => {
                let g = self.graph_mut(graph)?;
                if g.inner.contains_node(id) {
                    return invalid(format!("node {id} already exists"));
                }
                let node = Node {
                    id: *id,
                    ty: type_name(ty)?,
                    props: props
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                Fact {
                                    value: v.clone(),
                                    source: source.clone(),
                                },
                            )
                        })
                        .collect(),
                    reference: reference.clone(),
                    source: source.clone(),
                };
                if let Some(s) = &g.schema {
                    check_node(s, &node)?;
                }
                g.inner.add_node(*id, node)?;
            }
            GraphOp::NodeSet {
                graph,
                id,
                key,
                value,
                source,
            } => {
                let g = self.graph_mut(graph)?;
                let mut node = g
                    .node(id)
                    .cloned()
                    .ok_or_else(|| Error::NotFound(format!("node {id}")))?;
                set_prop(&mut node.props, key, value, source)?;
                if let Some(s) = &g.schema {
                    check_node(s, &node)?;
                }
                *g.inner.node_mut(id).expect("checked") = node;
            }
            GraphOp::NodeRemove { graph, id, source } => {
                let g = self.graph_mut(graph)?;
                let node = g
                    .node(id)
                    .ok_or_else(|| Error::NotFound(format!("node {id}")))?;
                if node.source.is_stated() && !source.is_stated() {
                    return invalid(format!(
                        "'{}' was stated; a scanner can't remove it",
                        node.label()
                    ));
                }
                if g.inner.degree(id, Dir::Both) > 0 {
                    return invalid(format!(
                        "'{}' still has edges; remove them first",
                        node.label()
                    ));
                }
                g.inner.remove_node(id);
            }
            GraphOp::EdgeAdd {
                graph,
                id,
                from,
                to,
                ty,
                props,
                source,
            } => {
                let g = self.graph_mut(graph)?;
                if g.inner.contains_edge(id) {
                    return invalid(format!("edge {id} already exists"));
                }
                for end in [from, to] {
                    if !g.inner.contains_node(end) {
                        return Err(Error::NotFound(format!("node {end}")));
                    }
                }
                let ty = type_name(ty)?;
                if g.out_edges(from).any(|e| e.to == *to && e.ty == ty) {
                    return invalid(format!("this '{ty}' edge already exists"));
                }
                let edge = Edge {
                    id: *id,
                    from: *from,
                    to: *to,
                    ty,
                    props: props
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                Fact {
                                    value: v.clone(),
                                    source: source.clone(),
                                },
                            )
                        })
                        .collect(),
                    source: source.clone(),
                };
                if let Some(s) = &g.schema {
                    check_edge(s, g, &edge)?;
                }
                g.inner.add_edge(*id, *from, *to, edge)?;
            }
            GraphOp::EdgeSet {
                graph,
                id,
                key,
                value,
                source,
            } => {
                let g = self.graph_mut(graph)?;
                let mut edge = g
                    .edge(id)
                    .cloned()
                    .ok_or_else(|| Error::NotFound(format!("edge {id}")))?;
                set_prop(&mut edge.props, key, value, source)?;
                if let Some(s) = &g.schema {
                    check_edge(s, g, &edge)?;
                }
                *g.inner.edge_mut(id).expect("checked") = edge;
            }
            GraphOp::EdgeRemove { graph, id, source } => {
                let g = self.graph_mut(graph)?;
                let edge = g
                    .edge(id)
                    .ok_or_else(|| Error::NotFound(format!("edge {id}")))?;
                if edge.source.is_stated() && !source.is_stated() {
                    return invalid("that edge was stated; a scanner can't remove it");
                }
                g.inner.remove_edge(id);
            }
        }
        Ok(())
    }

    /// A graph by name (case-insensitive) or id prefix.
    pub fn find(&self, q: &str) -> Option<GraphId> {
        let q = q.trim();
        self.graphs
            .values()
            .find(|g| g.name.eq_ignore_ascii_case(q))
            .or_else(|| {
                self.graphs
                    .values()
                    .find(|g| !q.is_empty() && g.id.to_string().starts_with(&q.to_lowercase()))
            })
            .map(|g| g.id)
    }

    /// Whether a graph other than `except` has this name (case-insensitive; never an id prefix).
    fn named(&self, name: &str, except: Option<GraphId>) -> bool {
        self.graphs
            .values()
            .any(|g| Some(g.id) != except && g.name.eq_ignore_ascii_case(name))
    }

    fn graph_mut(&mut self, id: &GraphId) -> Result<&mut Graph> {
        self.graphs
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("graph {id}")))
    }
}

/// One prop change, keeping what was stated safe from observations.
fn set_prop(
    props: &mut BTreeMap<String, Fact>,
    key: &str,
    value: &Option<Value>,
    source: &Source,
) -> Result<()> {
    let key = text(key, "prop name")?;
    if let Some(old) = props.get(&key) {
        if old.source.is_stated() && !source.is_stated() {
            return invalid(format!(
                "'{key}' was stated; observations don't overwrite it"
            ));
        }
    }
    match value {
        Some(v) => {
            let fact = Fact {
                value: v.clone(),
                source: source.clone(),
            };
            if props.get(&key) == Some(&fact) {
                return invalid(format!("'{key}' already has that value"));
            }
            props.insert(key, fact);
        }
        None => {
            if props.remove(&key).is_none() {
                return Err(Error::NotFound(format!("prop '{key}'")));
            }
        }
    }
    Ok(())
}

fn check_props(
    s: &Schema,
    defs: &BTreeMap<String, AttrDef>,
    props: &BTreeMap<String, Fact>,
    what: &str,
) -> Result<()> {
    for (k, def) in defs {
        match props.get(k) {
            Some(f) => check_value(k, def, &f.value)?,
            None if def.required => return invalid(format!("{what} needs '{k}'")),
            None => {}
        }
    }
    if !s.open {
        if let Some(k) = props.keys().find(|k| !defs.contains_key(*k)) {
            return invalid(format!("the schema has no prop '{k}' for {what}"));
        }
    }
    Ok(())
}

fn check_node(s: &Schema, n: &Node) -> Result<()> {
    match s.nodes.get(&n.ty) {
        Some(t) => check_props(s, &t.props, &n.props, &format!("a {}", n.ty)),
        None if s.open => Ok(()),
        None => invalid(format!("the schema has no node type '{}'", n.ty)),
    }
}

fn check_edge(s: &Schema, g: &Graph, e: &Edge) -> Result<()> {
    let Some(t) = s.edges.get(&e.ty) else {
        return if s.open {
            Ok(())
        } else {
            invalid(format!("the schema has no edge type '{}'", e.ty))
        };
    };
    let ty_of = |id: &NodeId| g.node(id).map(|n| n.ty.as_str()).unwrap_or("?");
    if !t.from.is_empty() && !t.from.iter().any(|x| x == ty_of(&e.from)) {
        return invalid(format!(
            "'{}' edges start from {}, not {}",
            e.ty,
            t.from.join(" or "),
            ty_of(&e.from)
        ));
    }
    if !t.to.is_empty() && !t.to.iter().any(|x| x == ty_of(&e.to)) {
        return invalid(format!(
            "'{}' edges end at {}, not {}",
            e.ty,
            t.to.join(" or "),
            ty_of(&e.to)
        ));
    }
    check_props(s, &t.props, &e.props, &format!("a '{}' edge", e.ty))
}

/// Types and scopes are short identifiers: `service`, `depends_on`, `machines`.
fn type_name(s: &str) -> Result<String> {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.len() <= 48
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c));
    if !ok {
        return invalid(format!(
            "'{s}' isn't a type name (letters, digits, '_', '-', '.')"
        ));
    }
    Ok(s.to_string())
}

fn scope_name(s: &str) -> Result<String> {
    type_name(s).map(|s| s.to_lowercase())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::knowledge::AttrType;

    fn props(v: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_graph_name_is_never_taken_for_an_id_prefix() {
        let mut gs = Graphs::default();
        let first = Uuid::now_v7();
        gs.apply(&GraphOp::Create {
            id: first,
            name: "Machines".into(),
            scope: "machines".into(),
        })
        .unwrap();
        let prefix = first.to_string()[..4].to_string();
        gs.apply(&GraphOp::Create {
            id: Uuid::now_v7(),
            name: prefix.clone(),
            scope: "machines".into(),
        })
        .unwrap();
        assert!(gs
            .apply(&GraphOp::Create {
                id: Uuid::now_v7(),
                name: prefix.to_uppercase(),
                scope: "machines".into(),
            })
            .is_err());
    }

    #[test]
    fn stated_facts_survive_observations() {
        let mut gs = Graphs::default();
        let (g, hub) = (Uuid::now_v7(), Uuid::now_v7());
        let scan = Source::Observed("scan:machine".into());
        gs.apply(&GraphOp::Create {
            id: g,
            name: "Machines".into(),
            scope: "machines".into(),
        })
        .unwrap();
        gs.apply(&GraphOp::NodeAdd {
            graph: g,
            id: hub,
            ty: "machine".into(),
            props: props(json!({ "name": "vg-pc", "os": "windows" })),
            reference: None,
            source: scan.clone(),
        })
        .unwrap();
        gs.apply(&GraphOp::NodeSet {
            graph: g,
            id: hub,
            key: "role".into(),
            value: Some(json!("hub")),
            source: Source::Stated,
        })
        .unwrap();
        let err = gs.apply(&GraphOp::NodeSet {
            graph: g,
            id: hub,
            key: "role".into(),
            value: Some(json!("workstation")),
            source: scan.clone(),
        });
        assert!(err.unwrap_err().to_string().contains("stated"));
        gs.apply(&GraphOp::NodeSet {
            graph: g,
            id: hub,
            key: "os".into(),
            value: Some(json!("windows 11")),
            source: scan.clone(),
        })
        .unwrap();
        assert!(
            gs.apply(&GraphOp::NodeSet {
                graph: g,
                id: hub,
                key: "os".into(),
                value: Some(json!("windows 11")),
                source: scan.clone()
            })
            .is_err(),
            "no-op"
        );
        let laptop = Uuid::now_v7();
        gs.apply(&GraphOp::NodeAdd {
            graph: g,
            id: laptop,
            ty: "machine".into(),
            props: props(json!({ "name": "mac" })),
            reference: None,
            source: Source::Stated,
        })
        .unwrap();
        assert!(gs
            .apply(&GraphOp::NodeRemove {
                graph: g,
                id: laptop,
                source: scan.clone()
            })
            .is_err());
        let e = Uuid::now_v7();
        gs.apply(&GraphOp::EdgeAdd {
            graph: g,
            id: e,
            from: laptop,
            to: hub,
            ty: "syncs_with".into(),
            props: BTreeMap::new(),
            source: Source::Stated,
        })
        .unwrap();
        assert!(gs
            .apply(&GraphOp::EdgeAdd {
                graph: g,
                id: Uuid::now_v7(),
                from: laptop,
                to: hub,
                ty: "syncs_with".into(),
                props: BTreeMap::new(),
                source: Source::Stated
            })
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(gs
            .apply(&GraphOp::NodeRemove {
                graph: g,
                id: hub,
                source: Source::Stated
            })
            .unwrap_err()
            .to_string()
            .contains("edges"));
        gs.apply(&GraphOp::EdgeRemove {
            graph: g,
            id: e,
            source: Source::Stated,
        })
        .unwrap();
        gs.apply(&GraphOp::NodeRemove {
            graph: g,
            id: hub,
            source: scan,
        })
        .unwrap();
        assert_eq!(gs.graphs[&g].node_count(), 1);
        assert_eq!(gs.find("machines"), Some(g));
    }

    #[test]
    fn schemas_constrain_types_props_and_edge_ends() {
        let mut gs = Graphs::default();
        let g = Uuid::now_v7();
        gs.apply(&GraphOp::Create {
            id: g,
            name: "Stack".into(),
            scope: "stacks".into(),
        })
        .unwrap();
        let text_req = AttrDef {
            ty: AttrType::Text,
            options: vec![],
            unit: None,
            min: None,
            max: None,
            required: true,
        };
        let port = AttrDef {
            ty: AttrType::Number,
            options: vec![],
            unit: None,
            min: None,
            max: None,
            required: false,
        };
        let schema = Schema {
            nodes: [
                (
                    "service".into(),
                    NodeType {
                        props: [("name".into(), text_req), ("port".into(), port)].into(),
                    },
                ),
                ("db".into(), NodeType::default()),
            ]
            .into(),
            edges: [(
                "calls".into(),
                EdgeType {
                    from: vec!["service".into()],
                    to: vec!["service".into(), "db".into()],
                    props: BTreeMap::new(),
                    inverse: Some("called_by".into()),
                },
            )]
            .into(),
            open: false,
        };
        gs.apply(&GraphOp::SetSchema {
            id: g,
            schema: Some(schema),
        })
        .unwrap();
        let s = gs.graphs[&g].schema.as_ref().unwrap();
        assert_eq!(s.edge_name("calls"), ("calls", false));
        assert_eq!(s.edge_name("called_by"), ("calls", true), "an edge read backwards");
        assert_eq!(s.edge_name("other"), ("other", false));
        let add = |gs: &mut Graphs, ty: &str, p: Value| {
            let id = Uuid::now_v7();
            gs.apply(&GraphOp::NodeAdd {
                graph: g,
                id,
                ty: ty.into(),
                props: props(p),
                reference: None,
                source: Source::Stated,
            })
            .map(|_| id)
        };
        let api = add(&mut gs, "service", json!({ "name": "api", "port": 7878 })).unwrap();
        let db = add(&mut gs, "db", json!({})).unwrap();
        assert!(add(&mut gs, "service", json!({ "port": 1 }))
            .unwrap_err()
            .to_string()
            .contains("needs 'name'"));
        assert!(
            add(&mut gs, "service", json!({ "name": "x", "port": "high" }))
                .unwrap_err()
                .to_string()
                .contains("a number")
        );
        assert!(
            add(&mut gs, "service", json!({ "name": "x", "colour": "red" }))
                .unwrap_err()
                .to_string()
                .contains("no prop")
        );
        assert!(add(&mut gs, "queue", json!({}))
            .unwrap_err()
            .to_string()
            .contains("no node type"));
        let edge = |gs: &mut Graphs, from, to| {
            gs.apply(&GraphOp::EdgeAdd {
                graph: g,
                id: Uuid::now_v7(),
                from,
                to,
                ty: "calls".into(),
                props: BTreeMap::new(),
                source: Source::Stated,
            })
        };
        edge(&mut gs, api, db).unwrap();
        assert!(edge(&mut gs, db, api)
            .unwrap_err()
            .to_string()
            .contains("start from service"));
        assert!(gs
            .apply(&GraphOp::SetSchema {
                id: g,
                schema: Some(Schema::default())
            })
            .is_err());
    }

    #[test]
    fn describes_ops_against_the_graphs_before_them() {
        let mut gs = Graphs::default();
        let (g, pc, git, has) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let scan = Source::Observed("scan:machine".into());
        let ops = [
            GraphOp::Create {
                id: g,
                name: "Machines".into(),
                scope: "machines".into(),
            },
            GraphOp::NodeAdd {
                graph: g,
                id: pc,
                ty: "machine".into(),
                props: props(json!({ "name": "vg-pc" })),
                reference: None,
                source: scan.clone(),
            },
            GraphOp::NodeAdd {
                graph: g,
                id: git,
                ty: "tool".into(),
                props: props(json!({ "name": "git" })),
                reference: None,
                source: scan.clone(),
            },
            GraphOp::EdgeAdd {
                graph: g,
                id: has,
                from: pc,
                to: git,
                ty: "has".into(),
                props: BTreeMap::new(),
                source: scan.clone(),
            },
            GraphOp::NodeSet {
                graph: g,
                id: git,
                key: "version".into(),
                value: Some(json!("2.55")),
                source: scan.clone(),
            },
            GraphOp::EdgeRemove {
                graph: g,
                id: has,
                source: scan.clone(),
            },
            GraphOp::NodeRemove {
                graph: g,
                id: git,
                source: scan,
            },
        ];
        let mut said = Vec::new();
        for op in &ops {
            assert_eq!(op.graph_id(), g);
            said.push(op.describe(&gs));
            gs.apply(op).unwrap();
        }
        assert_eq!(
            said,
            [
                "created graph Machines (machines)",
                "added machine vg-pc",
                "added tool git",
                "vg-pc -has-> git",
                "git.version = \"2.55\"",
                "removed vg-pc -has-> git",
                "removed git",
            ]
        );
    }

    #[test]
    fn writes_out_and_reads_back_the_same_json() {
        let (g, _) = query::tests::stack();
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json, serde_json::to_value(g.to_json()).unwrap());
        assert!(json.get("schema").is_none());
        let back: Graph = serde_json::from_value(json).unwrap();
        assert_eq!(back, g);
    }
}
