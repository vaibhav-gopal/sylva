//! Graph and Tree written out and read back with serde (feature `serde`): the written form is their
//! content in a fixed order, never their storage, and reading checks what it rebuilds.
#![cfg(feature = "serde")]

use serde_json::json;
use sylva::graph::Graph;
use sylva::tree::Tree;

fn graph() -> Graph<String, &'static str, u32> {
    let mut g = Graph::new();
    for (k, name) in [(2, "api"), (1, "web"), (3, "db")] {
        g.add_node(k, name.to_string()).unwrap();
    }
    g.add_edge(11, 2, 3, "reads").unwrap();
    g.add_edge(10, 1, 2, "calls").unwrap();
    g
}

#[test]
fn a_graph_writes_out_in_key_order_and_reads_back_equal() {
    let g = graph();
    let written = serde_json::to_value(&g).unwrap();
    assert_eq!(
        written,
        json!({
            "nodes": [
                {"key": 1, "data": "web"},
                {"key": 2, "data": "api"},
                {"key": 3, "data": "db"}
            ],
            "edges": [
                {"key": 10, "from": 1, "to": 2, "data": "calls"},
                {"key": 11, "from": 2, "to": 3, "data": "reads"}
            ]
        })
    );
    let read: Graph<String, String, u32> = serde_json::from_value(written).unwrap();
    assert_eq!(read.node(&2).map(String::as_str), Some("api"));
    assert_eq!(read.out_edges(&1).map(|e| e.to).collect::<Vec<_>>(), [2]);
    assert_eq!(read.in_edges(&3).map(|e| e.key).collect::<Vec<_>>(), [11]);
}

#[test]
fn equal_graphs_write_the_same_whatever_their_slots() {
    // The same content, reached through removals that leave the slots laid out differently.
    let mut churned = graph();
    churned.add_node(9, "gone".to_string()).unwrap();
    churned.add_edge(12, 9, 1, "x").unwrap();
    churned.remove_node(&9);
    let g = graph();
    assert_eq!(churned, g);
    assert_eq!(
        serde_json::to_string(&churned).unwrap(),
        serde_json::to_string(&g).unwrap()
    );
}

#[test]
fn reading_a_broken_graph_fails() {
    let missing_end = json!({"nodes": [{"key": 1, "data": "a"}], "edges": [{"key": 5, "from": 1, "to": 2, "data": "x"}]});
    assert!(serde_json::from_value::<Graph<String, String, u32>>(missing_end).is_err());
    let twice = json!({"nodes": [{"key": 1, "data": "a"}, {"key": 1, "data": "b"}], "edges": []});
    assert!(serde_json::from_value::<Graph<String, String, u32>>(twice).is_err());
}

fn tree() -> Tree<&'static str, u32> {
    // Inserted out of order, and "Shirts" placed before "Jeans": the order to keep is the tree's.
    let mut t = Tree::new();
    t.insert(2, None, None, "Clothing").unwrap();
    t.insert(7, Some(2), None, "Jeans").unwrap();
    t.insert(3, Some(2), Some(0), "Shirts").unwrap();
    t.insert(9, Some(7), None, "Loose").unwrap();
    t.insert(1, None, None, "Shoes").unwrap();
    t
}

#[test]
fn a_tree_writes_out_in_preorder_and_reads_back_equal() {
    let t = tree();
    let written = serde_json::to_value(&t).unwrap();
    assert_eq!(
        written,
        json!({"nodes": [
            {"key": 2, "parent": null, "data": "Clothing"},
            {"key": 3, "parent": 2, "data": "Shirts"},
            {"key": 7, "parent": 2, "data": "Jeans"},
            {"key": 9, "parent": 7, "data": "Loose"},
            {"key": 1, "parent": null, "data": "Shoes"}
        ]})
    );
    let read: Tree<String, u32> = serde_json::from_value(written).unwrap();
    assert_eq!(read.roots().collect::<Vec<_>>(), [2, 1]);
    assert_eq!(
        read.children(&2).collect::<Vec<_>>(),
        [3, 7],
        "siblings keep their order"
    );
    assert_eq!(read.parent(&9), Some(7));
}

#[test]
fn equal_trees_write_the_same_whatever_their_slots() {
    let mut churned = tree();
    churned.insert(5, Some(9), None, "gone").unwrap();
    churned.remove(&5);
    assert_eq!(churned, tree());
    assert_eq!(
        serde_json::to_string(&churned).unwrap(),
        serde_json::to_string(&tree()).unwrap()
    );
}

#[test]
fn reading_a_broken_tree_fails() {
    let orphan = json!({"nodes": [{"key": 1, "parent": 4, "data": "a"}]});
    assert!(serde_json::from_value::<Tree<String, u32>>(orphan).is_err());
    let twice = json!({"nodes": [{"key": 1, "parent": null, "data": "a"}, {"key": 1, "parent": null, "data": "b"}]});
    assert!(serde_json::from_value::<Tree<String, u32>>(twice).is_err());
}
