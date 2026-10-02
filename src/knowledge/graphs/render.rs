//! Diagrams from graphs: a graph plus a view spec (which node
//! and edge types, a focus node and depth, grouping by a prop, direction)
//! rendered to Mermaid, Graphviz DOT or D2. Ids are generated (`n1`, `n2` …)
//! and labels escaped, so any graph renders safely.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::query::{neighbors, Dir};
use super::{Edge, Graph, Node, NodeId};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Mermaid,
    Dot,
    D2,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    LR,
    TB,
    RL,
    BT,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct View {
    /// Only these node types (default: all).
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Only these edge types (default: all).
    #[serde(default)]
    pub edge_types: Option<Vec<String>>,
    /// Only what's within `depth` steps of this node (either direction).
    #[serde(default)]
    pub focus: Option<NodeId>,
    #[serde(default)]
    pub depth: Option<usize>,
    /// Draw nodes with the same value of this prop (or `type`) in one box.
    #[serde(default)]
    pub group_by: Option<String>,
    #[serde(default)]
    pub direction: Direction,
    /// Label edges with their type (default true).
    #[serde(default = "yes")]
    pub edge_labels: bool,
}

fn yes() -> bool {
    true
}

/// The nodes and edges a view selects.
pub fn select<'g>(g: &'g Graph, view: &View) -> (Vec<&'g Node>, Vec<&'g Edge>) {
    let in_focus: Option<BTreeSet<NodeId>> = view.focus.map(|f| {
        let mut set: BTreeSet<NodeId> =
            neighbors(g, f, Dir::Both, &view.edge_types, view.depth.unwrap_or(1))
                .into_iter()
                .map(|r| r.node)
                .collect();
        set.insert(f);
        set
    });
    let nodes: Vec<&Node> = g
        .nodes()
        .filter(|n| view.types.as_ref().is_none_or(|t| t.contains(&n.ty)))
        .filter(|n| in_focus.as_ref().is_none_or(|s| s.contains(&n.id)))
        .collect();
    let shown: BTreeSet<NodeId> = nodes.iter().map(|n| n.id).collect();
    let edges = g
        .edges()
        .filter(|e| view.edge_types.as_ref().is_none_or(|t| t.contains(&e.ty)))
        .filter(|e| shown.contains(&e.from) && shown.contains(&e.to))
        .collect();
    (nodes, edges)
}

pub fn render(g: &Graph, view: &View, format: Format) -> String {
    let (nodes, edges) = select(g, view);
    let ids: BTreeMap<NodeId, String> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, format!("n{}", i + 1)))
        .collect();
    let group = |n: &Node| -> Option<String> {
        let key = view.group_by.as_deref()?;
        if key == "type" {
            return Some(n.ty.clone());
        }
        n.prop(key).map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
    };
    // Groups in a stable order; ungrouped nodes last.
    let mut groups: BTreeMap<Option<String>, Vec<&Node>> = BTreeMap::new();
    for n in &nodes {
        groups.entry(group(n)).or_default().push(n);
    }
    fn edge_label<'e>(view: &View, e: &'e Edge) -> Option<&'e str> {
        view.edge_labels.then_some(e.ty.as_str())
    }
    let mut out = String::new();
    match format {
        Format::Mermaid => {
            out.push_str(&format!("flowchart {:?}\n", view.direction));
            for (gi, (name, members)) in groups.iter().enumerate() {
                let pad = if name.is_some() { "    " } else { "  " };
                if let Some(name) = name {
                    out.push_str(&format!("  subgraph g{gi}[\"{}\"]\n", mermaid_text(name)));
                }
                for n in members {
                    out.push_str(&format!(
                        "{pad}{}[\"{}\"]\n",
                        ids[&n.id],
                        mermaid_text(&n.label())
                    ));
                }
                if name.is_some() {
                    out.push_str("  end\n");
                }
            }
            for e in &edges {
                match edge_label(view, e) {
                    Some(l) => out.push_str(&format!(
                        "  {} -->|{}| {}\n",
                        ids[&e.from],
                        mermaid_text(l),
                        ids[&e.to]
                    )),
                    None => out.push_str(&format!("  {} --> {}\n", ids[&e.from], ids[&e.to])),
                }
            }
        }
        Format::Dot => {
            let rank = match view.direction {
                Direction::LR => "LR",
                Direction::TB => "TB",
                Direction::RL => "RL",
                Direction::BT => "BT",
            };
            out.push_str(&format!(
                "digraph \"{}\" {{\n  rankdir={rank};\n  node [shape=box];\n",
                quoted(&g.name)
            ));
            for (gi, (name, members)) in groups.iter().enumerate() {
                let pad = if name.is_some() { "    " } else { "  " };
                if let Some(name) = name {
                    out.push_str(&format!(
                        "  subgraph cluster_{gi} {{\n    label=\"{}\";\n",
                        quoted(name)
                    ));
                }
                for n in members {
                    out.push_str(&format!(
                        "{pad}{} [label=\"{}\"];\n",
                        ids[&n.id],
                        quoted(&n.label())
                    ));
                }
                if name.is_some() {
                    out.push_str("  }\n");
                }
            }
            for e in &edges {
                let label = edge_label(view, e)
                    .map(|l| format!(" [label=\"{}\"]", quoted(l)))
                    .unwrap_or_default();
                out.push_str(&format!("  {} -> {}{label};\n", ids[&e.from], ids[&e.to]));
            }
            out.push_str("}\n");
        }
        Format::D2 => {
            let dir = match view.direction {
                Direction::LR => "right",
                Direction::TB => "down",
                Direction::RL => "left",
                Direction::BT => "up",
            };
            out.push_str(&format!("direction: {dir}\n"));
            // In D2 a node inside a container is addressed as `container.node`.
            let mut path: BTreeMap<NodeId, String> = BTreeMap::new();
            for (gi, (name, members)) in groups.iter().enumerate() {
                if let Some(name) = name {
                    out.push_str(&format!("g{gi}: \"{}\" {{\n", quoted(name)));
                }
                for n in members {
                    let id = &ids[&n.id];
                    match name {
                        Some(_) => {
                            out.push_str(&format!("  {id}: \"{}\"\n", quoted(&n.label())));
                            path.insert(n.id, format!("g{gi}.{id}"));
                        }
                        None => {
                            out.push_str(&format!("{id}: \"{}\"\n", quoted(&n.label())));
                            path.insert(n.id, id.clone());
                        }
                    }
                }
                if name.is_some() {
                    out.push_str("}\n");
                }
            }
            for e in &edges {
                let label = edge_label(view, e)
                    .map(|l| format!(": \"{}\"", quoted(l)))
                    .unwrap_or_default();
                out.push_str(&format!("{} -> {}{label}\n", path[&e.from], path[&e.to]));
            }
        }
    }
    out
}

/// Mermaid labels are quoted; quotes and brackets become entity codes.
fn mermaid_text(s: &str) -> String {
    s.replace('"', "#quot;")
        .replace('<', "#lt;")
        .replace('>', "#gt;")
        .replace('\n', " ")
}

fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::graphs::query::tests::stack;

    #[test]
    fn renders_mermaid_dot_and_d2() {
        let (g, ids) = stack();
        let m = render(
            &g,
            &View {
                edge_labels: true,
                ..View::default()
            },
            Format::Mermaid,
        );
        assert!(m.starts_with("flowchart LR\n"));
        assert!(m.contains("[\"api\"]") && m.contains("-->|calls|"), "{m}");

        let grouped = View {
            group_by: Some("type".into()),
            direction: Direction::TB,
            edge_labels: true,
            ..View::default()
        };
        let m = render(&g, &grouped, Format::Mermaid);
        assert!(
            m.contains("subgraph") && m.contains("[\"service\"]") && m.starts_with("flowchart TB"),
            "{m}"
        );

        let only_calls = View {
            edge_types: Some(vec!["calls".into()]),
            types: Some(vec!["service".into(), "store".into()]),
            edge_labels: false,
            ..View::default()
        };
        let dot = render(&g, &only_calls, Format::Dot);
        assert!(
            dot.starts_with("digraph \"Stack\"")
                && !dot.contains("hub")
                && dot.matches("->").count() == 3,
            "{dot}"
        );

        let focus = View {
            focus: Some(ids["db"]),
            depth: Some(1),
            edge_labels: true,
            ..View::default()
        };
        let d2 = render(&g, &focus, Format::D2);
        assert!(
            d2.contains("\"api\"") && d2.contains("\"worker\"") && !d2.contains("\"web\""),
            "one step from db: {d2}"
        );

        let d2 = render(
            &g,
            &View {
                group_by: Some("lang".into()),
                edge_labels: true,
                ..View::default()
            },
            Format::D2,
        );
        assert!(
            d2.contains("g") && d2.contains(".n"),
            "edges address nodes inside containers: {d2}"
        );
        assert_eq!(mermaid_text("a \"b\" <c>"), "a #quot;b#quot; #lt;c#gt;");
    }
}
