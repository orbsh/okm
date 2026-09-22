# Graph Query Recipes: the Six Faces at Work

Query recipes for the graph edge layer (ADR-0017): how the six entry
faces compose into the shapes familiar from Cypher, what `NodeRef` is
and why it is self-describing, and where filtering happens. Wire layout
and the write path live in the [modeling guide](MODELING.md); the
mechanism record is [ADR-0017](adr/0017-graph-edge.md).

## Core stance

One face = one access method; a graph query is composed by scanning the
MOST SELECTIVE face first and converging ids in memory — no composite
faces, no query engine. Every primitive returns `Vec<u64>` (edge ids);
bodies are fetched per id only for the survivors. Declaration is
execution, same as everywhere in OKM.

## NodeRef: the self-describing node reference

`NodeRef` is the endpoint address of a graph edge — `[ns 2B][len
varint][pkey]`:

```text
┌─────────┬───────────┬──────────────────┐
│ ns  2B  │ len varint│  pkey (bare)     │
└─────────┴───────────┴──────────────────┘
   type        width        identity
  marker     rides along
```

- **`ns` is the type marker.** A graph connects nodes of MANY
  collections; which collection a node lives in is not a declaration,
  it is two bytes inside the ref. The ns bytes are the same
  `[ns 2B BE]` header as `Document::NS_PREFIX` — a node ref is
  assembled from the source document's own header plus its key
  encoding: `NodeRef::new(&<Org as Document>::NS_PREFIX, &key.encode())`.
- **`len` rides in the ref.** Different node collections may declare
  different pkey widths (4-byte org ids, 16-byte agent ids) and they
  coexist. Decoding cuts the pkey by the varint — no registry, no
  compile-time endpoint binding. This is what makes endpoints OPEN:
  any document collection participates with zero ceremony.
- **Same node, same bytes.** The encoding is canonical, so the
  traversal faces prefix-match exactly and mixed-width nodes never
  alias.

`NodeRef` pairs with `EdgeBody` — the body read of one edge:

```rust
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind_id: u16,     // kind NAME via g.kind_name(kind_id)
    pub attrs: Vec<u8>,   // the edge's own payload (fixed-ontology form:
                          // the EdgeEncode payload; dynamic form: nTLV frames)
}
```

Multi-hop traversal is just following `EdgeBody.dst` as the next hop's
`NodeRef`.

## The six faces, as query shapes

Setup: an `employs` graph over `Org(ns=9)` and `User(ns=10)`, two edges
written (org 9 -> user 1 since 2020, org 9 -> user 2 since 2021).

```text
Cypher shape                        okm landing (face)
──────────────────────────────────────────────────────────
(:org {id:9})-[]->()                out_edges          (0x5)
()<-[]-(:user {id:1})               in_edges           (0x6)
()-[r:employs]->()                  edges_of_kind      (0x4)
(:org {id:9})-[r:employs]->()       typed_out          (0x7)
()-[r:employs]->(:user {id:1})      typed_in           (0x8)
()-[r:employs {since: 2020}]->()    by_attr_face       (0x1)
```

```rust
// Untyped adjacency — every edge touching the node, any kind.
g.out_edges(&org9);                  // -> [1, 2]
g.in_edges(&user1);                  // -> [1]

// Kind-qualified traversal — kind leads the scan prefix (0x7/0x8).
g.typed_out("employs", &org9);       // -> [1, 2]
g.typed_in("employs", &user1);       // -> [1]

// Whole-graph kind scan — the `MATCH ()-[r:employs]->()` entry point.
g.edges_of_kind("employs");          // -> [1, 2]

// Attribute face — declared fields of the edge fact, exact-match probe.
let slot = __OkmEdgeIndex_Employment_since_year::SLOT;
g.by_attr_face(slot, &2020u16.to_be_bytes());  // -> [1]
g.by_attr_face(slot, &1999u16.to_be_bytes());  // -> [] (no face, no error)

// Body read — endpoints + kind name + attrs, for the surviving ids.
let body = g.get_edge(1).unwrap();
let kind = g.kind_name(body.kind_id).unwrap(); // -> "employs"
```

All primitives return edge ids; a scan that matches nothing returns an
empty Vec — unknown kinds and absent attribute values are empty
results, never errors.

## Composition: intersect in memory, no composite faces

The faces are deliberately orthogonal. A predicate they don't cover
individually (kind AND endpoint type AND attribute) is a CONJUNCTION of
face scans, intersected on edge ids in memory:

```rust
// Employ edges of org 9, hired since 2020, where the employee is a
// User (ns 10) — three faces, smallest result set first:
let candidates: Vec<u64> = g.by_attr_face(slot, &2020u16.to_be_bytes())
    .into_iter()
    .filter(|id| g.typed_out("employs", &org9).contains(id))
    .filter(|id| g.get_edge(*id).unwrap().dst.ns_id() == 10)
    .collect();
```

The cost discipline is the same as everywhere in OKM: pick the most
selective face first (an exact attribute match beats a kind scan beats
an untyped adjacency), and let the byte ranges do the physical WHERE —
in-memory intersection only pays on what survived. When a conjunction
becomes hot, promote it to an access method on the EDGE collection
(a `#[ok_index]` over its own declared attribute fields) instead of
composing at query time — declaration is registration, and the query
path collapses to one prefix scan.

## Multi-hop: the id/node split

The primitives return **edge ids** — the identity currency (parallel
edges, `get_edge`, `unlink` all hang off it). The traversal currency is
**`NodeRef`**. The bridge is the neighbor family, a convenience
composition over the face scan + fetch-back (zero new wire):

```rust
// One hop, nodes only:
g.out_nodes(&user1);                    // -> [org(9), org(10)]
g.in_nodes(&org9);                      // -> [user(1)]
g.typed_out_nodes("employs", &user1);   // kind-qualified neighbors
g.typed_in_nodes("employs", &org9);

// Parallel edges PRESERVE multiplicity (two facts between one pair
// yield the same neighbor twice); a set is the caller's dedupe:
let mut uniq = g.out_nodes(&user1);
uniq.sort();
uniq.dedup();
```

Multi-hop itself is a breadth-first walk — frontier + visited set — and
the visited set is keyed by `NodeRef` (it derives `Hash` + `Ord` for
exactly this). Revisiting a node is the KV-side way to spend an
unbounded scan budget: cycle protection is not optional structure, it
is the cost model. Each hop is one prefix scan per direction per edge
type; the cost is linear in edges touched, never in graph size.

```rust
// Friends-of-employment shape: org -> users -> orgs, two hops.
let mut frontier = vec![org9];
let mut visited: std::collections::HashSet<NodeRef> = frontier.iter().cloned().collect();
for _ in 0..2 {
    let mut next = Vec::new();
    for n in &frontier {
        for m in g.out_nodes(n) {
            if visited.insert(m.clone()) {
                next.push(m);
            }
        }
    }
    if next.is_empty() { break; }
    frontier = next;
}
// `visited` = every node within two hops, deduped.
```

When a walk becomes a hot path, promote the edge collection's hot
predicate into a declared `#[ok_index]` (see the composition section)
— the walk's per-hop scan is already the physical WHERE; the only
in-memory work left is the visited set.

## The two forms, one query surface

`Graph<S, E>` (fixed ontology: attributes are declared struct fields,
typed attrs bodies) and `DynamicGraph<S>` (attributes are runtime
nTLV frames, `DynEdge { kind: String, attrs: BTreeMap<..> }`) expose
the SAME read methods over the SAME faces. Differences visible to
queries:

- `get_edge` returns the typed payload (`EdgeBody.attrs` = edge-type
  bytes) vs a decoded `DynEdge` (attrs resolved through the
  dictionary).
- The dynamic form has NO 0x1 attribute face (attribute names are
  runtime data, so exact-match attr scans are structurally absent —
  kind filtering via 0x4/0x7/0x8 is complete, not a gap).
- Node-side "kind" is NOT an edge-layer concern: nodes are ordinary
  document collections, their kinds live in their own declared indexes
  and dictionaries. `(:node_type)` filtering = the edge face scan ∩
  the node collection's own kind-face scan, intersected in memory.
