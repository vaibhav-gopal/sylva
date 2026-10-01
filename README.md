# Sylva

Graphs and trees that remember.

Sylva is a small Rust library with three layers:

| Layer | Feature | What it gives you |
|---|---|---|
| **Structures** | (always) | `Graph<N, E, K>` and `Tree<T, K>`: stable keys, dense storage, indexes, and iteration in key order |
| **Log** | `log` | state as the replay of a log of ops: batches, causal order, conflicts as data, time travel, batch files, a hash chain |
| **Knowledge** | `knowledge` | a typed property graph and a catalog tree built on the two, with schemas, stated vs observed facts, references, queries and diagrams |

Extra features: `sign` (Ed25519 signatures over batches), `sqlite` (the hash chain in SQLite), `ts`
(TypeScript types for the knowledge model, via ts-rs).

It was extracted from [Alfred](../alfred), which uses all of it. The design is in
[docs/DESIGN.md](docs/DESIGN.md).

## Structures

```rust
use sylva::graph::{Dir, Graph};
use sylva::tree::Tree;

let mut g: Graph<&str, &str, u32> = Graph::new();
g.add_node(1, "web")?;
g.add_node(2, "api")?;
g.add_edge(10, 1, 2, "calls")?;
let reached = g.walk(&1, Dir::Out, 3, |edge| *edge == "calls"); // breadth first, nearest first

let mut t: Tree<&str, u32> = Tree::new();
t.insert(1, None, None, "Clothing")?;
t.insert(2, Some(1), None, "Jeans")?;
let names: Vec<&str> = t.lineage(&2).map(|(_, n)| *n).collect(); // ["Jeans", "Clothing"]
```

- Neighbours, degrees and "is there already an edge from a to b" cost the node's degree, not the graph.
- Paths, ancestors and subtree walks are iterators: no allocation per step.
- Removing a node removes its edges (graphs) or its subtree (trees); freed slots are reused.
- Equality compares content, never storage layout.

## Log

```rust
use sylva::log::{history::History, Meta, Projection};

#[derive(Default, Clone)]
struct Balance(i64);

impl Projection for Balance {
    type Op = i64;
    type Error = String;
    fn apply(&mut self, op: &i64, _: &Meta) -> Result<(), String> {
        if self.0 + op < 0 {
            return Err("would go below zero".into());
        }
        self.0 += op;
        Ok(())
    }
}

let mut h: History<Balance> = History::new(64); // a snapshot every 64 batches
h.push(batch)?;          // applies just this batch when it follows the head
let now = h.state();
let then = h.at(3).0;    // the state after the first 3 batches
```

- **Batches** carry `{batch, after, author, at, message}`. `after` names the newest batch the author had seen,
  so batches written on different branches order the same way everywhere and merge by taking both.
- **Conflicts are data**: an op that no longer applies after a merge is recorded, not fatal.
- **`files::BatchDir`** keeps one `<id>.jsonl` per batch, so two branches never edit the same file.
- **`chain`** is the other kind of log: one writer's ops in sequence, each sealing the previous hash, so any edit
  to the past is detected. `sqlite::ChainLog` stores it, with a codec for encrypting payloads.
- **`sign`** signs a batch's canonical form with a domain string you choose, so a signature made for one purpose
  can't be replayed as another.

## Knowledge

`knowledge::graphs` and `knowledge::trees` are ready-made projections: every change is an op
(`graph.node.add`, `node.split`, …) with validation and no-op rejection. Facts record where they came from
(stated by a person or agent, or observed by a scanner), and observations never overwrite what was stated.
Catalog trees retire nodes with a reason instead of deleting them, and declare attributes their descendants
inherit. There are queries (`find`, `neighbors`, `path`, `impact`), a name-based change language for tools,
and Mermaid, DOT and D2 rendering.

## Develop

```sh
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo bench --all-features        # benches/: structures and log
```

Status: in use by Alfred, not published. The op JSON of the knowledge model is a stored-data contract (it is
in Alfred's log and in every repo's `.tend/log`), so it only changes with a migration.
