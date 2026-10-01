//! Name-based graph changes → ops (for tools and CLIs): `{"action": "graph_edge", "graph": "Stack", "from":
//! "web", "to": "api", "type": "calls"}`. Nodes are named by label, `type:label`
//! or id prefix; each change applies to a preview, so later changes can name
//! graphs and nodes added earlier in the same request.

use std::collections::BTreeMap;

use serde_json::Value;
use uuid::Uuid;

use super::{query, EdgeId, GraphId, GraphOp, Graphs, NodeId, Schema, Source};
use crate::knowledge::refs::EntityRef;

/// One line per action, for tool descriptions and `--help`.
pub const ACTIONS: &str = "graph {name, scope?} (default scope: stacks); graph_update {graph, name?, scope?}; \
graph_schema {graph, schema|null}; graph_delete {graph}; \
graph_node {graph, type, label, props?, ref?} (ref: what it stands for); graph_set {graph, node, key, value (null removes)}; \
graph_node_remove {graph, node}; graph_edge {graph, from, to, type, props?}; graph_edge_set {graph, from, to, type, key, value}; \
graph_edge_remove {graph, from, to, type}";

/// What a `graph_node` stands for, from the change (`ref`, or host-specific keys).
pub type RefResolver<'a> = dyn Fn(&Value, &str) -> Result<Option<EntityRef>, String> + 'a;

/// The default: a `ref` entity reference (`recount/entry/…`, `files:…`).
pub fn plain_ref(c: &Value, at: &str) -> Result<Option<EntityRef>, String> {
    match c.get("ref").and_then(Value::as_str) {
        Some(r) => r.parse().map(Some).map_err(|e| format!("{at}: {e}")),
        None => Ok(None),
    }
}

/// Resolves a whole request of graph changes, recorded with `source`.
pub fn resolve(graphs: &Graphs, changes: &[Value], source: &Source, refs: &RefResolver) -> Result<Vec<GraphOp>, String> {
    let mut preview = graphs.clone();
    let mut ops = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        let at = format!("change {i}");
        let op = one(&preview, c, &at, source, refs)?.ok_or_else(|| format!("{at}: not a graph change"))?;
        preview.apply(&op).map_err(|e| format!("{at}: {e}"))?;
        ops.push(op);
    }
    if ops.is_empty() {
        return Err("no changes".into());
    }
    Ok(ops)
}

/// One change, or `None` when its action isn't a graph action.
pub fn one(p: &Graphs, c: &Value, at: &str, source: &Source, refs: &RefResolver) -> Result<Option<GraphOp>, String> {
    let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
    let need = |k: &str| s(k).ok_or_else(|| format!("{at}: '{k}' is required"));
    let Some(action) = s("action") else { return Ok(None) };
    let props = |k: &str| -> Result<BTreeMap<String, Value>, String> {
        match c.get(k) {
            None | Some(Value::Null) => Ok(BTreeMap::new()),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("{at}: '{k}' must be an object: {e}")),
        }
    };
    let value = || c.get("value").filter(|v| !v.is_null()).cloned();
    Ok(Some(match action.as_str() {
        "graph" => GraphOp::Create { id: Uuid::now_v7(), name: need("name")?, scope: s("scope").unwrap_or_else(|| "stacks".into()) },
        "graph_update" => GraphOp::Update { id: graph(p, c, at)?, name: s("name"), scope: s("scope") },
        "graph_schema" => {
            let schema = match c.get("schema") {
                None | Some(Value::Null) => None,
                Some(v) => Some(serde_json::from_value::<Schema>(v.clone()).map_err(|e| format!("{at}: bad schema: {e}"))?),
            };
            GraphOp::SetSchema { id: graph(p, c, at)?, schema }
        }
        "graph_delete" => GraphOp::Delete { id: graph(p, c, at)? },
        "graph_node" => {
            let g = graph(p, c, at)?;
            let mut props = props("props")?;
            props.insert("label".into(), Value::String(need("label")?));
            GraphOp::NodeAdd { graph: g, id: Uuid::now_v7(), ty: need("type")?, props, reference: refs(c, at)?, source: source.clone() }
        }
        "graph_set" => {
            let g = graph(p, c, at)?;
            GraphOp::NodeSet { graph: g, id: node(p, g, &need("node")?, at)?, key: need("key")?, value: value(), source: source.clone() }
        }
        "graph_node_remove" => {
            let g = graph(p, c, at)?;
            GraphOp::NodeRemove { graph: g, id: node(p, g, &need("node")?, at)?, source: source.clone() }
        }
        "graph_edge" => {
            let g = graph(p, c, at)?;
            let (from, to) = (node(p, g, &need("from")?, at)?, node(p, g, &need("to")?, at)?);
            GraphOp::EdgeAdd { graph: g, id: Uuid::now_v7(), from, to, ty: need("type")?, props: props("props")?, source: source.clone() }
        }
        "graph_edge_set" => {
            let g = graph(p, c, at)?;
            GraphOp::EdgeSet { graph: g, id: edge(p, g, c, at)?, key: need("key")?, value: value(), source: source.clone() }
        }
        "graph_edge_remove" => {
            let g = graph(p, c, at)?;
            GraphOp::EdgeRemove { graph: g, id: edge(p, g, c, at)?, source: source.clone() }
        }
        _ => return Ok(None),
    }))
}

/// A graph by name or id prefix (`graph`).
fn graph(p: &Graphs, c: &Value, at: &str) -> Result<GraphId, String> {
    let q = c.get("graph").and_then(Value::as_str).ok_or_else(|| format!("{at}: 'graph' is required"))?;
    p.find(q).ok_or_else(|| format!("{at}: no graph matches '{q}'"))
}

/// A node by label, `type:label` or id prefix.
fn node(p: &Graphs, g: GraphId, q: &str, at: &str) -> Result<NodeId, String> {
    query::resolve(&p.graphs[&g], q).ok_or_else(|| format!("{at}: the graph has no node '{q}'"))
}

/// The edge `from -type-> to`.
fn edge(p: &Graphs, g: GraphId, c: &Value, at: &str) -> Result<EdgeId, String> {
    let need = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("{at}: '{k}' is required"));
    let (from, to, ty) = (node(p, g, &need("from")?, at)?, node(p, g, &need("to")?, at)?, need("type")?);
    p.graphs[&g]
        .out_edges(&from)
        .find(|e| e.to == to && e.ty == ty)
        .map(|e| e.id)
        .ok_or_else(|| format!("{at}: no '{ty}' edge between those nodes"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn names_resolve_across_one_request() {
        let gs = Graphs::default();
        let changes = [
            json!({ "action": "graph", "name": "Network", "scope": "network" }),
            json!({ "action": "graph_node", "graph": "Network", "type": "host", "label": "vg-pc" }),
            json!({ "action": "graph_node", "graph": "Network", "type": "host", "label": "nixos" }),
            json!({ "action": "graph_edge", "graph": "Network", "from": "vg-pc", "to": "host:nixos", "type": "hands_off_to" }),
            json!({ "action": "graph_edge_set", "graph": "Network", "from": "vg-pc", "to": "nixos", "type": "hands_off_to", "key": "when", "value": "later" }),
        ];
        let ops = resolve(&gs, &changes, &Source::Stated, &plain_ref).unwrap();
        assert_eq!(ops.len(), 5);
        let mut gs = gs;
        for op in &ops {
            gs.apply(op).unwrap();
        }
        let g = &gs.graphs[&gs.find("network").unwrap()];
        assert_eq!(g.edges().next().unwrap().props["when"].value, json!("later"));
        assert!(resolve(&gs, &[json!({ "action": "graph_node", "graph": "Network", "type": "host", "label": "x", "ref": "nope" })], &Source::Stated, &plain_ref).is_err());
        assert!(resolve(&gs, &[json!({ "action": "rename" })], &Source::Stated, &plain_ref).unwrap_err().contains("not a graph change"));
    }
}
