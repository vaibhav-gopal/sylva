//! Asking a graph things: nodes by type, props and text; neighbours to a
//! depth; the shortest path between two nodes; and impact (everything that
//! reaches a node, i.e. would be affected if it changed).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EdgeId, Graph, Node, NodeId};
pub use crate::graph::Dir;

/// A node reached by a walk: how far, and the edge that first reached it.
pub type Reached = crate::graph::Reached<NodeId>;

/// Which nodes: `{"type": "service", "where": {"port": 7878, "lang": {"in": ["rust", "go"]}}, "text": "auth"}`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Match {
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    #[serde(rename = "where", default)]
    pub conditions: BTreeMap<String, Cond>,
    /// Case-insensitive, over the label and string props.
    #[serde(default)]
    pub text: Option<String>,
}

/// A prop condition: a plain value means equality.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Cond {
    Ops(Ops),
    Eq(Value),
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ops {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not: Option<Value>,
}

impl Cond {
    fn holds(&self, v: Option<&Value>) -> bool {
        match self {
            Cond::Eq(want) => v == Some(want),
            Cond::Ops(o) => {
                o.exists.is_none_or(|e| e == v.is_some())
                    && o.contains
                        .as_ref()
                        .is_none_or(|c| v.is_some_and(|v| contains(v, c)))
                    && o.one_of
                        .as_ref()
                        .is_none_or(|set| v.is_some_and(|v| set.contains(v)))
                    && o.gt
                        .is_none_or(|n| v.and_then(Value::as_f64).is_some_and(|x| x > n))
                    && o.lt
                        .is_none_or(|n| v.and_then(Value::as_f64).is_some_and(|x| x < n))
                    && o.not.as_ref().is_none_or(|n| v != Some(n))
            }
        }
    }
}

/// Substring (case-insensitive) for strings; membership for arrays.
fn contains(v: &Value, needle: &str) -> bool {
    match v {
        Value::String(s) => s.to_lowercase().contains(&needle.to_lowercase()),
        Value::Array(items) => items
            .iter()
            .any(|i| i.as_str().is_some_and(|s| s.eq_ignore_ascii_case(needle))),
        _ => false,
    }
}

pub fn find<'g>(g: &'g Graph, m: &Match) -> Vec<&'g Node> {
    let text = m.text.as_ref().map(|t| t.to_lowercase());
    g.nodes()
        .filter(|n| m.ty.as_ref().is_none_or(|t| n.ty == *t))
        .filter(|n| m.conditions.iter().all(|(k, c)| c.holds(n.prop(k))))
        .filter(|n| {
            text.as_ref().is_none_or(|t| {
                n.label().to_lowercase().contains(t)
                    || n.props.values().any(|f| {
                        f.value
                            .as_str()
                            .is_some_and(|s| s.to_lowercase().contains(t))
                    })
            })
        })
        .collect()
}

/// A node by id prefix, exact label (case-insensitive), or `type:label`.
pub fn resolve(g: &Graph, q: &str) -> Option<NodeId> {
    let q = q.trim();
    let (ty, label) = match q.split_once(':') {
        Some((t, l)) if g.nodes().any(|n| n.ty == t) => (Some(t), l),
        _ => (None, q),
    };
    g.nodes()
        .find(|n| ty.is_none_or(|t| n.ty == t) && n.label().eq_ignore_ascii_case(label))
        .or_else(|| {
            g.nodes()
                .find(|n| q.len() >= 4 && n.id.to_string().starts_with(&q.to_lowercase()))
        })
        .map(|n| n.id)
}

fn of_type<'a>(types: &'a Option<Vec<String>>) -> impl FnMut(&super::Edge) -> bool + 'a {
    move |e| types.as_ref().is_none_or(|t| t.contains(&e.ty))
}

/// Nodes within `depth` steps of `start` (breadth-first, each once, nearest first).
pub fn neighbors(
    g: &Graph,
    start: NodeId,
    dir: Dir,
    edge_types: &Option<Vec<String>>,
    depth: usize,
) -> Vec<Reached> {
    g.structure().walk(&start, dir, depth, of_type(edge_types))
}

/// Everything that reaches `node` (against edges, any depth): what a change to it affects.
pub fn impact(g: &Graph, node: NodeId, edge_types: &Option<Vec<String>>) -> Vec<Reached> {
    neighbors(g, node, Dir::In, edge_types, usize::MAX)
}

/// The shortest path from `from` to `to`, as the edges taken.
pub fn path(
    g: &Graph,
    from: NodeId,
    to: NodeId,
    dir: Dir,
    edge_types: &Option<Vec<String>>,
    max_depth: usize,
) -> Option<Vec<EdgeId>> {
    g.structure()
        .shortest_path(&from, &to, dir, max_depth, of_type(edge_types))
}

#[cfg(test)]
pub mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::knowledge::graphs::{GraphOp, Graphs, Source};

    /// web -calls-> api -calls-> db, worker -calls-> db, api -runs_on-> hub
    pub fn stack() -> (Graph, BTreeMap<&'static str, NodeId>) {
        let mut gs = Graphs::default();
        let g = Uuid::now_v7();
        gs.apply(&GraphOp::Create {
            id: g,
            name: "Stack".into(),
            scope: "stacks".into(),
        })
        .unwrap();
        let mut ids = BTreeMap::new();
        for (name, ty, lang) in [
            ("web", "service", "svelte"),
            ("api", "service", "rust"),
            ("worker", "service", "rust"),
            ("db", "store", "sql"),
            ("hub", "machine", ""),
        ] {
            let id = Uuid::now_v7();
            let props = serde_json::from_value(json!({ "name": name, "lang": lang })).unwrap();
            gs.apply(&GraphOp::NodeAdd {
                graph: g,
                id,
                ty: ty.into(),
                props,
                reference: None,
                source: Source::Stated,
            })
            .unwrap();
            ids.insert(name, id);
        }
        for (a, ty, b) in [
            ("web", "calls", "api"),
            ("api", "calls", "db"),
            ("worker", "calls", "db"),
            ("api", "runs_on", "hub"),
        ] {
            let e = GraphOp::EdgeAdd {
                graph: g,
                id: Uuid::now_v7(),
                from: ids[a],
                to: ids[b],
                ty: ty.into(),
                props: Default::default(),
                source: Source::Stated,
            };
            gs.apply(&e).unwrap();
        }
        (gs.graphs.remove(&g).unwrap(), ids)
    }

    #[test]
    fn find_neighbors_paths_and_impact() {
        let (g, ids) = stack();
        let rust: Match =
            serde_json::from_value(json!({ "type": "service", "where": { "lang": "rust" } }))
                .unwrap();
        let mut names: Vec<String> = find(&g, &rust).iter().map(|n| n.label()).collect();
        names.sort();
        assert_eq!(names, ["api", "worker"]);
        let some: Match =
            serde_json::from_value(json!({ "where": { "lang": { "in": ["svelte", "sql"] } } }))
                .unwrap();
        assert_eq!(find(&g, &some).len(), 2);
        assert_eq!(
            find(
                &g,
                &Match {
                    text: Some("WORK".into()),
                    ..Match::default()
                }
            )
            .len(),
            1
        );
        assert_eq!(resolve(&g, "service:API"), Some(ids["api"]));

        let out = neighbors(&g, ids["web"], Dir::Out, &None, 1);
        assert_eq!(out.iter().map(|r| r.node).collect::<Vec<_>>(), [ids["api"]]);
        assert_eq!(
            neighbors(&g, ids["web"], Dir::Out, &None, 9).len(),
            3,
            "api, db, hub"
        );
        let calls = Some(vec!["calls".to_string()]);
        assert_eq!(
            neighbors(&g, ids["web"], Dir::Out, &calls, 9).len(),
            2,
            "only along calls"
        );

        let hit: BTreeSet<NodeId> = impact(&g, ids["db"], &None)
            .iter()
            .map(|r| r.node)
            .collect();
        assert_eq!(hit, BTreeSet::from([ids["api"], ids["worker"], ids["web"]]));

        let p = path(&g, ids["web"], ids["db"], Dir::Out, &None, 10).unwrap();
        assert_eq!(p.len(), 2);
        assert!(
            path(&g, ids["db"], ids["web"], Dir::Out, &None, 10).is_none(),
            "edges have a direction"
        );
        assert_eq!(
            path(&g, ids["db"], ids["web"], Dir::Both, &None, 10)
                .unwrap()
                .len(),
            2
        );
    }
}
