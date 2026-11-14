# Graph Query Recipes: the faces at work

Query recipes for the graph edge layer (ADR-0017; the identity is ADR-0067): how the faces compose into the shapes familiar from Cypher, what `NodeRef` and a handle each are, where filtering happens, and one worked design (community-based recall). Wire layout lives in [key layout](internals/key-layout.md) and [MODELING.md](MODELING.md); the mechanism records are [ADR-0017](adr/0017-graph-edge.md) and [ADR-0067](adr/0067-an-edge-has-no-identity-of-its-own.md).

## Core stance

One face = one access method; a graph query is composed by scanning the MOST SELECTIVE face first and converging identities in memory — no composite faces, no query engine. An edge has no number: its identity is the tuple `(src, predicate, dst)`, so every primitive answers `EdgeKey`s and a walk answers handles. Bodies are fetched per tuple, only for the survivors. Declaration is execution, same as everywhere in OKM.

## NodeRef: the endpoint a caller speaks

`NodeRef` is the endpoint address the API speaks — `[ns 2B][pkey]`, no length:

```text
┌─────────┬──────────────────┐
│ ns  2B  │  pkey (bare)     │
└─────────┴──────────────────┘
   type        identity
  marker
```

- **`ns` is the type marker.** A graph connects nodes of MANY collections; which collection a node lives in is not a declaration, it is two bytes inside the ref. The ns bytes are the same `[ns 2B BE]` header as `Document::NS_PREFIX` — a node ref is assembled from the source document's own header plus its key encoding: `NodeRef::new(&<Org as Document>::NS_PREFIX, &key.encode())`.
- **The width comes from the graph declaration.** `#[ok_nodes(..)]` is a closed set of member tables and `KeyEncode` fixes each one's width, so a ref parses by width alone: different node collections may declare different pkey widths (4-byte org ids, 16-byte agent ids) and they coexist. This is what makes endpoints OPEN — any document collection participates with zero ceremony, as long as its key is fixed width.
- **Same node, same bytes.** The encoding is canonical, so a tuple matches exactly and mixed-width nodes never alias.

What a caller speaks is not what a row stores: inside the graph's key space an endpoint is an eight-byte handle (next section), and the `(ns, pkey)` pair survives in the registry. `NodeRef` is the boundary type — in on the write, out on the read.

`NodeRef` pairs with `EdgeBody` — the body read of one edge:

```rust
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub predicate_id: u16,     // predicate NAME via g.predicate_vocab_name(predicate_id)
    pub payload: Vec<u8>, // the edge's own payload — an obj payload,
                          // [version][hot_len][hot][cold TLV] (ADR-0017 §3);
                          // the dynamic form leaves it the bare header and
                          // keeps its open attributes in face 0x01
}
```

## Handles: what a walk actually yields

A traversal entry states the whole edge. `walk` / `out` / `inn` yield `EdgeStep`, whose fields all come off the entry's key — no row is read to make a hop, and no registry read either:

```rust
EdgeStep {
    far_handle: Handle,            // the endpoint on the far side of THIS hop
    head: HopHead {
        src_handle: Handle,        // the edge's own direction
        dst_handle: Handle,
        predicate: u16,
    },
}
```

So a walk hands you handles, and you resolve the ones you ask for:

- **One handle:** `g.node(handle)` → one point read of the registry. `Handle::NONE` never parses as a handle, and a handle the registry does not hold is an error, not a guessed ref.
- **A set:** `g.nodes(&sorted)` → ONE range scan spanning first to last (a sparse set pays for its span, not for the face). It takes a SORTED slice: handles are issued monotonically, so a set gathered in issue order is already sorted, but `EdgeKey` order is a different order — sort explicitly rather than assume.
- **A whole tuple:** `g.edge_key(&step)` when you want `(src, predicate, dst)` rather than the piece the walk gave you.

Three rules follow, and they are contract, not advice:

- **A handle is local.** It is the graph's own numbering inside one namespace: never exported in a schema, never in a payload, never in a file another store reads. It means nothing outside the graph that issued it.
- **A round trip is asserted by the tuple.** Writing the same `(src, predicate, dst)` again is the same row; "the same edge" is a question about the tuple, never about a handle. A handle is an implementation detail you may hold for the duration of one query.
- **Numbering may differ after a restore.** A rebuild rewrites the reverse face and nothing reissues handles, but a restore into a fresh store, a re-index, or a compaction (when it exists) may number nodes differently. A handle saved somewhere is not an address; save the tuple and let the graph resolve it again.

Cost: a walk of degree *d* is one scan and zero registry reads. Resolving *n* of its handles is one range scan (`nodes`) or *n* point reads (`node`). The expensive shape is resolving per hop when the answer does not need refs at all.

## The faces, as query shapes

Setup: an `employs` graph over `Org(ns=9)` and `User(ns=10)`, two edges written (org 9 -> user 1 since 2020, org 9 -> user 2 since 2021).

```text
Cypher shape                        okm landing (face)
──────────────────────────────────────────────────────────
(:org {id:9})-[]->()                out_edges          (0x06)
()<-[]-(:user {id:1})               in_edges           (0x07)
(:org {id:9})-[]->(:user {id:1})    out_edges_to       (0x06, then filter)
()-[r:employs]->()                  edges_of_predicate (0x08 bare prefix)
(:org {id:9})-[r:employs]->()       typed_out          (0x08)
()-[r:employs]->(:user {id:1})      typed_in           (0x09)
(:org {id:9})-[r:employs]->(:user {id:1})
                                    edge_at            (0x08, one exact key:
                                                        the assertion identity)
()-[r:employs {since: 2020}]->()    by_attr_face       (declared-index family 0x4nnn)
```

```rust
// Untyped adjacency — every edge touching the node, any predicate.
g.out_edges(&org9);   // -> [(org9, employs, user1), (org9, employs, user2)]
g.in_edges(&user1);   // -> [(org9, employs, user1)]

// Predicate-filtered traversal — the predicate leads the scan prefix (0x08/0x09).
g.typed_out("employs", &org9);       // -> the same two edges, off the typed face
g.typed_in("employs", &user1);

// Pair lookups. The predicate sits BETWEEN the endpoints, so (src, dst) is not a
// contiguous prefix: this filters the source's run, which is the out-degree. The
// typed form is the assertion identity — one exact key.
g.out_edges_to(&org9, &user1);             // -> [(org9, employs, user1)]
g.edge_at("employs", &org9, &user1);       // -> Some((org9, employs, user1))

// Whole-graph predicate scan — the `MATCH ()-[r:employs]->()` entry point.
// There is no predicate face: this is 0x08's bare `[predicate]` prefix.
g.edges_of_predicate("employs");          // -> both

// Attribute face — a declared `#[ok_index]` of the edge. EQUALITY on the whole
// declared field set: the probe must carry every declared field, in declaration
// order. A short probe leaves the remaining fields in front of the tuple and
// matches NOTHING, silently — it is not a prefix scan.
let slot = <__OkmIndex_Employment_by_since as KvIndex>::SLOT;
g.by_attr_face(slot, &2020u16.to_be_bytes());  // -> [(org9, employs, user1)]
g.by_attr_face(slot, &1999u16.to_be_bytes());  // -> [] (no face, no error)

// Body read — endpoints + predicate name + payload, for the surviving tuples.
let body = g.get_edge(&key)?.unwrap();
let predicate = g.predicate_vocab_name(body.predicate_id).unwrap(); // "employs"
```

All primitives answer tuples; a scan that matches nothing returns an empty Vec — unknown predicates and absent attribute values are empty results, never errors.

## Composition: intersect in memory, no composite faces

The faces are deliberately orthogonal. A query they don't cover individually (predicate AND endpoint type AND attribute) is a CONJUNCTION of face scans, intersected on tuples in memory (`EdgeKey` is `Hash` + `Ord` for exactly this):

```rust
// Employ edges of org 9, hired since 2020, where the employee is a User (ns 10)
// — three faces, smallest result set first:
let since: std::collections::HashSet<EdgeKey> =
    g.by_attr_face(slot, &2020u16.to_be_bytes())?.into_iter().collect();
let candidates: Vec<EdgeKey> = g
    .typed_out("employs", &org9)?
    .into_iter()
    .filter(|key| since.contains(key))
    .filter(|key| key.dst.ns_id() == 10)
    .collect();
```

The cost discipline is the same as everywhere in OKM: pick the most selective face first (an exact attribute match beats a predicate scan beats an untyped adjacency), and let the byte ranges do the physical WHERE — in-memory intersection only pays on what survived. When a conjunction becomes hot, promote it to an access method on the EDGE collection (a `#[ok_index]` over its own declared attribute fields) instead of composing at query time — declaration is registration, and the query path collapses to one prefix scan.

## Multi-hop: a stream of steps, a visit set of nodes

Multi-hop is a breadth-first walk — frontier + visited set — and the visited set is keyed by `NodeRef` (it derives `Hash` + `Ord` for exactly this). Revisiting a node is the KV-side way to spend an unbounded scan budget: cycle protection is not optional structure, it is the cost model. Each hop is one prefix scan per direction per edge type; the cost is linear in edges touched, never in graph size.

```rust
// Friends-of-employment shape: org -> users -> orgs, two hops.
let mut frontier = vec![org9];
let mut visited: std::collections::HashSet<NodeRef> = frontier.iter().cloned().collect();
for _ in 0..2 {
    let mut next = Vec::new();
    for n in &frontier {
        for m in g.out_nodes(n) {          // one registry scan per call
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

`out_nodes` is the convenience that resolves each step. The raw form keeps the walk in handles: `for step in g.walk(n) { … step.far_handle … }` resolves nothing, so a walk whose answer is a count, a predicate test or a membership test never touches the registry at all — and a walk that needs refs resolves once per node visited (`g.node`) or once per call for the whole answer (`g.nodes`). Starting a stream from a handle costs one `g.node(handle)` point read, because `walk` takes the address, not the number.

When a walk becomes a hot path, promote the edge collection's hot predicate into a declared `#[ok_index]` (see the composition section) — the walk's per-hop scan is already the physical WHERE; the only in-memory work left is the visited set.

## Community recall: sample nodes, rank their communities

A real design, written down because the shape is not obvious: the graph stores a community-detection result, and a query recalls "which communities does this page of results belong to". Two graph verbs carry it — `clusters_of` on the NODE form, and `members` — and everything else is policy.

**Write side: a run's result is a set of clusters.** Each community is one cluster whose members are the edges the run grouped, with kind `Kind::COMMUNITY`. The cluster's payload is the RUN's metadata — algorithm name, parameters, and when it was produced — because a cluster row's payload is a `Dict` and nothing else in the model has to change to carry it:

```rust
let mut fragment = Fragment::<Employment>::new(
    community.edges.iter().map(|key| Decl::edge(EdgeHead {
        src: key.src.clone(),
        dst: key.dst.clone(),
        predicate: predicate_name.clone(),     // the run groups ONE relation
    })),
);
let mut meta = Dict::new();
meta.insert("algorithm".into(), Variant::Str("lpa".into()));
meta.insert("resolution".into(), Variant::F64(0.6));
meta.insert("produced".into(), Variant::U64(run_started));
fragment.cluster(Cluster::new(Kind::COMMUNITY, meta));
let report = graph.incorporate(fragment)?;   // report.hyper_ids = this run's ids
```

Because a recompute writes a NEW cluster id rather than overwriting one, several runs coexist — a different algorithm, a different parameter, last week's answer — and they are comparable: `Hyperedges::clusters(Kind::COMMUNITY)` enumerates them, `lookup` hands back the payload, and filtering on `payload["algorithm"]` selects the run. The comparison itself is a join on member edges, which is the same tuple identity the graph already speaks.

**Query side: 100 recalled nodes, then count.** The application recalls a page from a vector face — 100 node refs, best first. Their communities come from their incident edges, one `0x0D` run per edge, so the cost is the page's edges and nothing about the graph's size:

```rust
let hits: Vec<NodeRef> = vector_recall(&query, 100);

let mut tally: HashMap<ClusterRef, usize> = HashMap::new();
let mut seen: HashSet<ClusterRef> = HashSet::new();
for node in &hits {
    // The stream collapses ADJACENT repeats; a node with two edges in one
    // community reports it twice, so count each node once.
    seen.clear();
    seen.extend(graph.clusters_of(node));
    for cluster in &seen {
        *tally.entry(*cluster).or_default() += 1;
    }
}

// The sampled nodes decide; the clusters are the answer.
let mut ranked: Vec<(usize, ClusterRef)> = tally.into_iter().map(|(n, c)| (n, c)).collect();
ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
let top: Vec<ClusterRef> = ranked.iter().take(3).map(|(_, c)| *c).collect();
```

In the story this run lands as 30 of the 100 in community A, 20 in B, 10 in C, and the remaining 40 scattered two and three at a time. The top three are the answer; the scatter is noise that the ranking is exactly what discards.

**Expanding to whole communities.** If the result wants the communities rather than the sampled nodes, each top cluster is one `0x0B` prefix scan, and its member rows come back as tuples through the registry — one registry scan per call, for the whole member set of that call:

```rust
let hyperedges = Hyperedges::of::<Employment>(&store);
let communities: Vec<Vec<EdgeKey>> = top
    .iter()
    .map(|cluster| hyperedges.members(*cluster))
    .collect::<Result<_, _>>()?;
```

The application then formats it — a markdown memory block such as `## Community A — 30 of 100 recalled nodes` followed by the expanded members — because that is presentation, and okm does not do presentation.

**What is policy and what is mechanism.** How many clusters to take, how to break a tie, whether to expand to members at all, whether the page size is 100 or 20: all of it is the application's decision, and none of it needs a new face. okm provides exactly the two verbs that make it cheap — `clusters_of` on the node form (one walk plus one membership run per incident edge, i.e. linear in the page's edges) and `members` (one prefix scan plus one registry scan). The alternative shape, walking every cluster and counting how many recalled nodes fall in each, would read the whole `0x0A` and `0x0B` faces to answer a question about 100 nodes.

**A singleton community is storable, and so is an empty one.** A cluster is a scope, and a scope is information by itself: one edge detected as a group is a real answer, and an empty scope is how "this subject is sensitive, do not ask" or "something used to be here and is gone now" gets said. A membership change never deletes the row — a cluster is created when declared and deleted only when explicitly deleted (`.delete(..)`), so a community that loses every member stays behind as an empty scope with an honest count of zero. The scatter in the story is therefore presentation, not a limit of the store: a detector that finds nothing in a subject can still record that it looked.

**Where a run's own facts live.** A recall run has metadata — the query, the page size, the timestamp, which detector ran — and that is not a community. It is a parent cluster: the run declares a cluster of its own kind (`Kind::EVENT`, or a `SCOPE` for a standing topic), each community is declared `.under(run_id)`, `children(kind, run_id)` enumerates the communities of that run, and the parent's payload carries the run's facts. The parent needs no members of its own — a parent whose communities are children and whose member face is empty is exactly the shape this is built for.

## The two forms, one query surface

`Graph<S, E>` (fixed ontology: attributes are declared struct fields, typed attrs bodies) and `okm_dynamic::Graph<S>` (attributes are runtime nTLV frames, `DynEdge { predicate: String, attrs: Dict }`) expose the SAME read methods over the SAME faces. Differences visible to queries:

- `get_edge` returns the raw body (`EdgeBody.payload` = payload bytes) and `edge` decodes it into the declaration's type, vs a decoded `DynEdge` (attrs resolved through the field-name vocabulary).
- The dynamic form has NO declared attribute faces (attribute names are runtime data, so exact-match attr scans are structurally absent — predicate filtering via 0x08/0x09 is complete, not a gap).
- Node-side "kind" is NOT an edge-layer concern: nodes are ordinary document collections, their kinds live in their own declared indexes and dictionaries. `(:node_type)` filtering = the edge face scan ∩ the node collection's own kind-face scan, intersected in memory.

What NOT to model as a graph — merge-vs-split per trust domain, model-vs-state (instance state stays in collections), and cross-graph composition through NodeRefs — is the modeling guide's call, not this doc's: see [MODELING.md](MODELING.md) "What belongs in a graph".