# ADR-0066: An edge set is a hyperedge

**Status**: Draft (2026-11-12). Merges `docs/ideas/cluster-membership.md` and the promoted parts of `docs/ideas/hypergraph.md`; supersedes the latter's member-face layout, which it generalises.

This ADR calls it a cluster; the literature calls it a hyperedge, and the difference is that we do not use the arity theory. A community is a cluster whose members were discovered rather than declared.

## Context

The graph carries binary edges: a fact is `(src, dst, predicate, payload)`. Some knowledge is not binary — a community, an event's evidence, a subgraph, a permission boundary: **a named set of assertions**. Two shapes are possible. Give the set a node of its own and hang the members off it (star / reified node), or keep the set as rows that name its members.

The star shape loses information the model needs: two sets over the same vertices but different edges cannot be told apart, an edge shared by two sets is invisible, an edge's own role has no place, and the fake node becomes a high-degree hub that community detection reads as a core. The base graph is also already complete — a grouping is an organisational layer over it, not a change to it.

## Decision

**1. A grouping of edges is a set of rows, not a node.** Members live in one row kind, one row per member:

```text
0x0A  the cluster itself       [ns][0x0A][kind][hyper_id]           → [parent_id u64 BE][dynamic payload]
0x0B  edge member (authority)  [ns][0x0B][kind][hyper_id][src][pred][dst]  → empty
0x0C  child index (derived)    [ns][0x0C][kind][parent][hyper_id]   → empty, rebuildable
```

`kind` is a one-byte closed vocabulary (community / event / subgraph / scope); `hyper_id` is allocated at run time, not by declaration order. The face IS the row kind — no in-row discriminator — and `kind` stays a key field, so one face serves every vocabulary. Member rows carry no value: the edge already carries it. A member IS its tuple `(src, predicate, dst)` — an edge has no number to carry (ADR-0067) — and the tuple's widths come from the graph's `#[ok_nodes]` declaration, so a member row parses by width like any traversal entry. Members come out ordered by the tuple, and that is the only order the store promises.

**2. Members are edges; the participants are their endpoints.** An edge row already carries both endpoints, so a cluster's nodes are its member edges' endpoints, deduplicated. That projection lives OUTSIDE the store: there is no derived node face. A caller who needs the participants takes the member edges and sorts or dedupes on its own side — inserting into a sorted list is log n, and no HashMap is needed. A face mirroring the endpoints would cost a fourth row per grouped edge, and node kinds are heterogeneous, so the mirror would cost more to maintain than it saves.

**3. Completeness rests on a write-time rule, not on补行.** A node exists only as an endpoint of some edge (no "create a bare node" path). A group needs no counterpart: it is a SCOPE, and a scope is information by itself — "this subject is sensitive, do not ask" and "something used to be here and is gone now" are statements only an empty group can make, and a one-member group is a scope that currently holds one thing. So a group row is created when it is declared and deleted only when it is explicitly deleted (`.delete(..)`); a membership change never takes the row with it, and its `members` count is an honest count that may be zero. Under the bare-node rule no "member without an edge" shape is needed, and the endpoints a reader projects out of the member edges are always real.

**4. The group is a row of its own, one key shorter than its members.** `[ns][0x0A][kind][hyper_id]` holds the payload — member count, weight, last write time — while its members sit on `0x0B`, a face of their own. Same shape as the vocabulary cursor at `[ns][0x02]` (69197cd), with one difference that matters: the cursor's entries share its face, so a scan must bound the key length to avoid reading the cursor as an entry, whereas here the face byte differs and no length test is needed. Listing a kind's clusters is `[ns][0x0A][kind]`; listing a cluster's members is `[ns][0x0B][kind][hyper_id]` — two scans, each unambiguous. The value is a fixed head plus a dynamic tail: `[parent_id u64 BE][payload]`. `parent_id` is fixed width and first so a reader slices it without decoding anything, and `0` means "no parent" — a reserved value, not a nullable flag, so the head is always eight bytes. Groups therefore nest through the cluster row itself, with no second face. One cost is stated rather than hidden: reading a cluster's parent is free, listing a cluster's children is a scan of `[ns][0x0A][kind]` unless the derived face below exists — which it does. The payload is the dynamic segment's shape (`Dict`) because `kind` is a run-time vocabulary, so no declared Rust type exists for it.


**5. Groupings are versioned, and deletion is a write-path contract.** A recompute writes a new group version; the old one stays, so diffs and rollbacks are ordinary set operations. Deleting an edge deletes its membership rows in the same batch and leaves the group row standing with a lowered count — the group is a scope, not an alias for its members; a miss is a freshness problem, not a correctness one, because the child index rebuilds from the cluster rows.

**6. The base graph stays clean.** Algorithms (shortest path, PageRank, components, community detection) run on the edge table alone and never see a grouping. Ignoring cross-group edges is then not a compromise: it is the model's use of the fact that graphs have modular structure. Contraction and expansion — group the edges, compute inside, compute between clusters — are a `group by` and a `join`, with no graph restructuring. The residual (edges outside the chosen clusters) must stay queryable, and the error is asymmetric: degree-like metrics tolerate truncation, path-like ones do not.

**7. One write entry point, not a new declaration.** `Decl` is always two variants; many node kinds live in the enum `#[derive(GraphEncode)]` already generates for `#[ok_nodes(…)]` (one variant per collection, ns from each table's own `#[ok_ns]`):

```rust
let mut fragment = Fragment::<G>::new([Decl::Node(G::Nodes::User, key), Decl::Edge(head), …]);
fragment.cluster(Cluster { kind, id, payload })         // id None = create, Some = merge
 .on_node_conflict::<User>(|old, _declared| User { seen: old.seen + 1 })   // per collection: node rows are heterogeneous
 .on_edge_conflict(|_stored, declared| E { weight: declared.weight + 1 })  // one payload type per graph
 .on_cluster_conflict(|_stored, declared| declared.clone());                // clusters are dynamic
graph.incorporate(fragment);                            // endpoints, edges, member rows, three hooks

// Only the declarations are required: a fragment with nothing attached writes just the nodes and edges.
graph.incorporate(Fragment::new([Decl::Node(G::Nodes::User, key)]));
```

Endpoints are created on demand, membership is idempotent (same `(kind, group, member)` upserts), and all three conflict hooks run in the same batch — which is where ADR-0028's counters land. Each hook is handed the stored payload beside the declared one and answers the row to write, so "keep", "overwrite" and "take the newest stamp" are one expression each. Other operations ride the same row kind: `remove`, `incorporate`, `split`, `reassign`, `lookup`, `members`, `diff`.

**8. Four fixed faces, taken from the freed core band.** `0x0A` the cluster, `0x0B` edge members, `0x0C` the derived child index `[ns][0x0C][kind][parent][child]`, which turns "list a cluster's children" into a clean prefix scan; it is derived from `parent_id` in the cluster rows and rebuildable by scanning them. `0x0D` the derived edge → cluster index `[ns][0x0D][src][pred][dst][kind][hyper_id]`, the same membership read the other way: `0x0B` answers "who is in this cluster", `0x0D` answers "which clusters is this edge in", and it is derived from the member rows alone. Faces rather than one face plus in-row bytes, because the model's rule is that the face states what a row is. A new row kind takes the next number on its own side of the one-byte space (ADR-0030's two-ended allocation), not a sub-kind byte. Counting families are not used for clusters: their counters are declaration order with a 4095 ceiling, while clusters are runtime-unbounded.

Why the reverse index is worth a face, and why the tuple leads it: an edge's clusters are otherwise found by a SUFFIX scan of `0x0B` (the retract path does exactly that), which reads every member row of the kind — fine once per retract, wrong once per traversal hop. `0x0D` makes the same question one prefix scan, and leading with the tuple puts its entries in the order a traversal stream already has (both lead with `src`), so "the member edges of C that these nodes point at" is one `intersect_seek` between two ordered streams instead of a hash set built on the caller's side. It is Derived, not truth: written in the same batch as the `0x0B` rows on every add, remove and retract, classified Derived in the change log, and rebuilt from `0x0B` alone by `rebuild_derived`.

## Consequences

- A fact can belong to many clusters, with the graph topology untouched.
- "What is in this group" and "what changed between these two versions" are scans and set operations, not traversals.
- Storage grows by one row per grouped edge (the membership row); the child index is one row per nesting edge, not per member. The number to watch is rows per edge.
- Anything that recomputes clusters must write a version rather than replace; readers see a consistent snapshot of one version.

## Rejected

- **A node per group (star / reified).** Loses set identity, shared membership and per-edge roles, and pollutes algorithms with fake hubs.
- **A counting family per group.** Bounded at 4095 and semantically "declaration order" — wrong for run-time clusters.
- **Storing the set as one value.** A set-valued entry is a cross-row read-modify-write, which the write contract refuses.
- **A separate hypergraph declaration.** The graph already declares members; the write API above is the whole addition.
