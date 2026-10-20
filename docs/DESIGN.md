# Sylva: design

Sylva holds data structures that remember: graphs and trees with stable keys, a log layer that makes any
state the replay of its history, and a knowledge model built from both. This document is the reference
for how it works and why. The README is the short version.

## 1. Goals and non-goals

**Goals**
- General-purpose graph and tree structures, generic over node and edge data, with the indexes real
  queries need (adjacency both ways, key lookup) and nothing more.
- A log layer that any application state can sit on: changes are ops, state is a replay, history merges.
- Determinism everywhere: the same ops give the same state, byte for byte, on every machine. Hashes,
  signatures and diffs depend on it.
- Pure Rust (SQLite is an optional, bundled feature), so it builds for desktop, server and iOS; without
  the default `std` feature the structures need only `alloc`, so they build for no_std and wasm too.
- At least the performance of the code it replaced (Alfred's, measured in
  `alfred/docs/benchmarks/baseline.md`), checked by benchmarks rather than assumed.

**Non-goals**
- A graph database or a query language. Queries are functions over the structures.
- A general collection library. Interval trees, ledgers and the like belong to whoever needs them.
- Competing with `petgraph` on algorithms. Sylva has the walks its users need: breadth-first walks,
  shortest paths, topological order.

## 2. Layout

```
src/
  lib.rs            the Key trait, re-exports
  error.rs          Error (Invalid, NotFound, Integrity, Json, Io, Db)
  graph.rs          Graph<N, E, K>
  tree.rs           Tree<T, K>
  forest.rs         Forest, Flat, Column: a tree for hot paths
  log/              feature `log`
    mod.rs          Header, Batch, order, head, Projection, Meta, Conflict, replay
    history.rs      History<P>: incremental replay with snapshots
    files.rs        BatchDir: one file per batch
    chain.rs        Sealed<A, O>, hash, check, replay: the hash chain
    sign.rs         feature `sign`: Signer, verify, TrustedKeys, SigDir
    sqlite.rs       feature `sqlite`: ChainLog, Codec
    crypt.rs        feature `crypt`: MasterKey, Cipher, Encrypted (a Codec)
  knowledge/        feature `knowledge`
    refs.rs         EntityRef (`domain/kind/id`, `files:path`, `scheme:path` for the host's own addresses), validate_path
    attrs.rs        AttrDef, AttrType, check_value
    trees.rs        catalog Tree, TreeOp, Trees
    graphs/         property Graph, GraphOp, Graphs; query, change, render
benches/            structures, forest, log (divan)
tests/serde.rs      Graph and Tree written out and read back (feature `serde`)
```

## 3. Structures

### 3.1 Keys and storage

Graph and Tree keep their items in a `Vec` of slots (with a free list) and find them through a
`BTreeMap<K, u32>` from key to slot. So:
- iteration follows the key order, never the order of insertion or the slot layout (determinism);
- lookups by key are `O(log n)`, and everything inside a walk uses slot numbers (`O(1)`);
- removed slots are reused, and `PartialEq` compares content, not layout.

`K` is any `Copy + Ord + Hash + Debug + Display` type, and the caller always names it (there is no
default); Alfred, Tend and the knowledge model use UUIDv7.

With the `serde` feature (no_std too) both write out as their content, never their slots, so equal
structures write the same bytes: a `Graph` as `{"nodes": [{key, data}], "edges": [{key, from, to,
data}]}` in key order, a `Tree` as `{"nodes": [{key, parent, data}]}` in preorder (parents first,
siblings in order). Reading back goes through `add_node`, `add_edge` and `insert`, so a duplicate key,
a missing parent or an edge to a missing node is an error rather than a broken structure. The
knowledge model keeps its own written forms (5.3).

### 3.2 Graph

Each node keeps its outgoing and incoming edges as lists of `(edge key, edge slot)`, sorted by edge key.
That keeps per-node iteration in key order (the same order a scan of all edges by key would give), so
replacing a full scan with the adjacency list changes no result.

| Operation | Cost |
|---|---|
| `node`, `edge`, `add_node`, `add_edge` | `O(log n)` plus insertion into two sorted lists |
| `out_edges`, `in_edges`, `degree`, "edge from a to b?" | `O(degree)` |
| `walk` (breadth first, depth-limited, edge filter) | `O(nodes reached + their edges)`; past 32 nodes reached, also a bit per slot |
| `shortest_path` | breadth first, as above |
| `remove_node` | its edges, plus their entries at the other end |
| `topological_order` | Kahn's algorithm, ties broken by key; `Err` lists the nodes on or behind a cycle |

`Dir::Both` merges the two sorted lists; a self-loop appears once.

### 3.3 Tree

An ordered forest: roots in order, each node with a parent and ordered children (each child's key next
to its slot, so listing children reads no other slot). `entry` and `root_entries` hand out an `Entry`
(key, data, parent, child entries) for walks that read a node and its children together.
`lineage` (the node and its ancestors), `ancestors`, `subtree` and `preorder` are iterators;
`subtree` and `preorder` take a predicate that prunes a node with its subtree.
`move_to` refuses cycles, keeps the tree unchanged on any error, and returns `false` when the node
is already where it was asked to go, so callers can report "doesn't change anything".

### 3.4 Forest

A tree for hot paths, such as a UI's retained tree walked every frame, beside `Tree` (which stays the
keyed tree the knowledge model needs). It trades keys for speed:
- **Ids are `(slot, generation)`.** A slot's generation is even while a node lives there and odd while
  it's free, so removing a node makes every id for it fail safely, and freed slots are reused.
- **Intrusive links** (parent, first and last child, previous and next sibling: one 20-byte record per
  node) make push, insert before or after, move and detach `O(1)` and remove `O(subtree)`, with nothing
  allocated per node or per child. `move_to` has `Tree`'s contract: cycles refused, nothing changed on
  an error, `false` when already in place.
- **`Flat`** is a contiguous preorder index (every slot in preorder, where each subtree ends, and the
  child lists packed together). Full passes read its slot array; skipping walks read its child lists
  with a reused stack. `Flat::update` patches it: the forest logs each parent whose child list
  changed (a run of changes to one parent once) and each freed slot, and the view writes those lists
  again and walks again only the subtrees under them, shifting what follows when one grew or shrank.
  It rebuilds in `O(n)` instead when the roots changed, the log has moved past it, or stale list
  entries outnumber live ones. Neither allocates once its buffers have grown.
- **`Column<T>`** holds per-node data by slot, structure-of-arrays; the forest owns only structure.

Walks follow tree order. Only column sweeps read in slot order, and nothing that has to replay the
same everywhere depends on how slots were reused. `to_tree` exports it as a `Tree` for snapshots and
the log.

## 4. The log layer

### 4.1 Ops, batches and projections

A **projection** is any state with `apply(op, meta) -> Result`. A **batch** is ops written together,
with a header: `batch` (UUIDv7), `after` (the newest batch the author had seen), `author`, `at` (ms)
and an optional `message`. The JSONL form (header line, then one op per line) is canonical: it is
what signatures cover, and line endings don't change it.

### 4.2 Causal order

`order` places each batch after the one it names in `after`. Among batches ready at the same time, the
smaller id comes first. A batch whose `after` isn't present (the first, or one from an unmerged branch)
is a root. The order is the same for any set of batches in any input order, which is what makes
merging two directories of batch files safe. A corrupt `after` cycle is appended in id order rather
than lost.

### 4.3 Conflicts are data

`replay` never fails. An op that doesn't apply (typically after a merge, when two branches changed the
same thing) becomes a `Conflict { batch, index, op, reason }` for a person to resolve.

### 4.4 History

`History` keeps the replayed state current. A batch whose `after` is the head (the usual case) applies
alone. Any other batch re-orders the history and replays from the last snapshot before the first
position that changed. Snapshots are taken every `every` batches, so `at(step)` costs one clone and at
most `every` batches of replay.

### 4.5 The hash chain

For a single writer (an application's own store), `chain` seals ops in sequence: each op's hash covers
`{seq, id, batch, parent, ts, actor, op}` as JSON in that field order, with `parent` the previous op's
hash. `check` and `replay` detect gaps, broken links and edited ops. The encoding is pinned by a test,
because every existing chain depends on it. `sqlite::ChainLog` stores one row per op and runs payloads
through a `Codec`, which gets the op's hash so an encrypting codec can bind each payload to its row.

### 4.5a Encryption at rest

`crypt` (feature `crypt`) is that codec. One random `MasterKey` per store; the data key is derived with
HMAC-SHA256 as `<app>/v1/data`, so applications sharing a master key never share a data key. Each op is
sealed with XChaCha20-Poly1305, its hash as associated data (a payload moved to another row fails to open),
and stored as `e1:` + base64(nonce || ciphertext); rows without the prefix read as plain JSON, so a store
can turn encryption on in place. The format is Alfred's, byte for byte, so its store can move onto it
without converting anything. Where the master key lives is the application's business.

### 4.6 Signatures

`sign::Signer` signs `domain || batch.to_jsonl()`. The domain is the caller's (Tend uses
`alfred-tend-batch-v1\n`, kept as-is so existing signatures stay valid). `verify` gives `Signed(name)`,
`Unsigned` or `Invalid(reason)` against a set of trusted keys. Key custody (where the private key
lives) is the application's job.

## 5. The knowledge model

### 5.1 Property graphs

Nodes and edges have a string type and JSON props. Every prop, node and edge records its source: stated
(by a person or an agent) or observed (by a named scanner). An observation never overwrites a stated
fact or removes a stated node or edge, and a scanner can re-run any number of times; when a scanner
removes a node or edge it observed, the props stated on it go too (the log keeps them). Schemas (optional, possibly `open`) list
node types with typed props, and edge types with the node types they may connect and, optionally, the
edge's name read backwards (`runs_on` ↔ `hosts`; `Schema::edge_name`), since an edge is stored once. A
node may stand for an entity owned elsewhere (`ref`: `domain/kind/id`, `files:path`, or a host's own
`scheme:path` such as a node of another graph), which the owning application checks. On top: `query` (find by type, props
and text; neighbours; paths; impact), `change` (name-based changes, so tools can say
`{"action": "graph_edge", "from": "web", "to": "api", "type": "calls"}`) and `render` (Mermaid, DOT, D2
with grouping and focus).

### 5.2 Catalog trees

Nodes have a name (unique among active siblings, ignoring case), a kind (category, specifier, leaf), an
optional description and attributes. Nothing is deleted: `retire` keeps the node with its reason,
`split` and `merge` record what replaced what, and `restore` brings a node back. Attributes declared on
a category are inherited by its descendants, which makes a tree a catalog schema.

### 5.3 Compatibility contract

- **Op JSON is stored data**: Alfred's op log and every repo's `.tend/log` hold it. Field names, tags
  and enum spellings only change with a migration.
- **Written-out state** (the JSON of a `Tree` or `Graph`) is an API contract. `Tree` and `Graph`
  serialize straight from the structures into the same shape their predecessors had (`TreeJson`,
  `GraphJson`), and read back through them, checking that parents, children and edge ends agree.
- **TypeScript** (feature `ts`) is generated to match: `Tree` and `Graph` export as their JSON shapes.
  Doc comments become JSDoc, so they are part of the output.

## 6. Performance

The bar is Alfred's baseline (`alfred/docs/benchmarks/baseline.md`). The structural wins:
- adding an edge no longer scans all edges (the old graph build was quadratic);
- neighbour steps use adjacency lists instead of scanning every edge;
- paths and lineage are walked without building `Vec<String>`s;
- `History` turns "replay everything on every request" into one batch per write.

Generic code costs nothing at run time (it is compiled per use), so generality shows up only in the API.

The forest's bar is a hand-written arena with a child `Vec` per node (`benches/forest.rs`, 16 children
per node). Medians at 10k / 100k nodes: a full preorder over `Flat` 0.65 / 6.3 µs against 8.9 / 101 µs;
a dirty walk with 1% dirty 1.45 / 16 µs against 1.7 / 21 µs; removing a subtree 10 / 84 µs against
13 / 106 µs; building 69-125 µs / 0.9-1.0 ms against 62-73 µs / 1.0-1.1 ms, level or behind at 10k
depending on the machine's state (below) and slightly ahead at 100k. (The forest's small accessors are
`#[inline]`, so callers in other crates don't pay a call per step; that alone halved its build with
warm memory and cut link walks by a fifth.) Reordering children trades the other way: the forest
relinks two neighbours in about 9 ns whatever the list's length, where the arena shifts the list
(4.4 ns at 16 children, 14 ns at 256, 204 ns at 4096), so the arena is ahead only on short lists,
by the forest's id and cycle checks. `Tree` builds about ten times slower than either.

Bringing `Flat` up to date after a change deep in the tree costs what changed: a child added or
removed 1.5 / 3.2 µs at 10k / 100k nodes, a parent's children reordered about 105 ns; a full rebuild is
about 20 / 340 µs. (A change right under the root still walks nearly everything: about 200 µs at
100k.)

Run each benchmark alone when comparing: the forest allocates its slots up front, so a run right
after another benchmark that freed memory builds up to twice as fast as one that has to fault in
fresh pages, and the machine's state moves it too: building 10k nodes measured 69 µs in one session
and 115-125 µs in another (the same code both times, the arena's 62-73 µs throughout).

## 7. Testing

Every module has unit tests next to it. The knowledge model's tests are the ones its predecessors had, so
behaviour is pinned. Also pinned: the chain's hash encoding, and the written-out JSON of trees and graphs
(streamed and built forms must agree, and must read back to equal values).

## 8. Decisions

| Decision | Why |
|---|---|
| One crate with features, not five | Splits only pay where consumers need different dependencies; features handle that |
| `uuid` only with `log` and `knowledge`; no default key type | A default key that changed with a feature would break a crate when another one in the build turned the feature on |
| A default `std` feature; the structures need only `alloc` | So a UI on wasm or an embedded target gets the structures without the log's dependencies |
| No separate signing crate | Signing is a property of batches; key custody belongs to the application |
| Ordered `BTreeMap` index, not a hash map | Deterministic iteration with no extra sorting, and no hasher dependency |
| Adjacency sorted by edge key | Same order as a scan by key, so results didn't change when the scan went away |
| Conflicts as data, not errors | Merged histories must always replay; people resolve what doesn't apply |
| The caller picks the signature domain | One key can't be tricked into signing for another purpose |
| Encryption keys derived per application | One master key can serve several apps without any two sharing a data key |
| A second tree (`Forest`) beside `Tree` | A tree walked every frame needs generational ids, links and columns; a keyed, ordered tree for catalogs needs the opposite |
| A walk tracks 32 reached slots in place, then a bit per slot | A flag per slot made one step from a node cost the whole graph |
| Catalog-tree split and merge apply to a copy | They are several steps that can each fail, and replay keeps whatever state an op leaves |
| `Error` is `#[non_exhaustive]` | Features add variants, and features unify across a build, so a match elsewhere needs a catch-all |
| Graph and Tree serialize their content, read back checked | Equal structures must write the same bytes, and a written form must not be able to build a broken one |
| The forest's small accessors are `#[inline]` | A call per step across crates halved its build speed; the forest exists to be fast in someone else's loop |
| `Flat` patches from a change log the forest keeps | A change deep in a UI tree shouldn't cost a walk of all of it; the log is bounded, and falling behind it means a rebuild, never a wrong view |

## 9. Next

Nothing planned. Alfred's store moved onto `crypt`, dropping its own copy of the cipher.

Considered and left: persisted snapshots (the longest history is 187 batches, 0.6 MB, and replays
quickly), and a reverse-relation index pattern (graphs already answer "what points at this" in
`O(degree)`, and only Alfred keeps an index of its own).
