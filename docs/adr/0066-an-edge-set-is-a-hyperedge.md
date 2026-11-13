# ADR-0066: An edge set is a hyperedge

**Status**: Draft (2026-11-12). Merges `docs/ideas/hyperedge-membership.md` and the promoted parts of `docs/ideas/hypergraph.md`; supersedes the latter's member-face layout, which it generalises.

## Context

The graph carries binary edges: a fact is `(src, dst, predicate, payload)`. Some knowledge is not binary — a community, an event's evidence, a subgraph, a permission boundary: **a named set of assertions**. Two shapes are possible. Give the set a node of its own and hang the members off it (star / reified node), or keep the set as rows that name its members.

The star shape loses information the model needs: two sets over the same vertices but different edges cannot be told apart, an edge shared by two sets is invisible, an edge's own role has no place, and the fake node becomes a high-degree hub that community detection reads as a core. The base graph is also already complete — a grouping is an organisational layer over it, not a change to it.

## Decision

**1. A grouping of edges is a set of rows, not a node.** Members live in one row kind, one row per member:

```text
0x0A  the group itself        [ns][0x0A][kind][group_id]              → dynamic payload
0x0B  edge member (authority) [ns][0x0B][kind][group_id][edge_id]     → empty
0x0C  node member (derived)   [ns][0x0C][kind][group_id][node_id][edge_id] → empty, rebuildable
```

`kind` is a one-byte closed vocabulary (community / event / subgraph / scope); `group_id` is allocated at run time, not by declaration order. The face IS the row kind — no in-row discriminator — and `kind` stays a key field, so one face serves every vocabulary. Member rows carry no value: the edge already carries it. Members come out ordered: `0x0B` by edge id, `0x0C` by node id — the second is what makes "the members of this group" a stream with duplicates only adjacent, so deduplication costs one comparison instead of a set.

**2. Members are edges; node membership is derived.** An edge row already carries both endpoints, so a group's nodes are its member edges' endpoints, deduplicated. `0x0C` exists only to make that projection *ordered and streamable*. It is rebuilt from `0x0B` and the edge rows at any time — an index, not a second truth.

**3. Completeness rests on a write-time rule, not on补行.** A node exists only as an endpoint of some edge (no "create a bare node" path), and a group exists only with two or more members. Under those two rules the derivation is total and no "member without an edge" shape is needed.

**4. The group is a row of its own, one key above its members.** `[ns][0x0A][kind][group_id]` holds the payload — member count, weight, last write time — and every member row extends that exact prefix. This is the second row of its kind, not the first: the vocabulary cursor already lives at a bare face prefix, `[ns][0x02]` for the cursor and `[ns][0x02][id]` for the entries, and `refactor(vocab): the cursor is a face-level singleton, not a face of its own` (69197cd) is what moved it there. The reading rule is stated too — a reader tells data from singleton by key length alone (`docs/internals/key-layout.zh-CN.md:75-87`). Two consequences, both wanted: a prefix scan over `(kind, group_id)` yields the group row first and its members after it in one pass, and every member reader bounds the key length, the same way the vocabulary reader already does. The payload is the dynamic segment's shape (`Dict`) because `kind` is a run-time vocabulary, so no declared Rust type exists for it.


**5. Groupings are versioned, and deletion is a write-path contract.** A recompute writes a new group version; the old one stays, so diffs and rollbacks are ordinary set operations. Deleting an edge deletes its membership rows in the same batch; a miss is a freshness problem, not a correctness one, because `0x0C` is rebuildable.

**6. The base graph stays clean.** Algorithms (shortest path, PageRank, components, community detection) run on the edge table alone and never see a grouping. Ignoring cross-group edges is then not a compromise: it is the model's use of the fact that graphs have modular structure. Contraction and expansion — group the edges, compute inside, compute between groups — are a `group by` and a `join`, with no graph restructuring. The residual (edges outside the chosen groups) must stay queryable, and the error is asymmetric: degree-like metrics tolerate truncation, path-like ones do not.

**7. One write entry point, not a new declaration.** `Decl` is always two variants; many node kinds live in the enum `#[derive(GraphEncode)]` already generates for `#[ok_nodes(…)]` (one variant per collection, ns from each table's own `#[ok_ns]`):

```rust
Vec::<Decl<G>>::new()
    .decls([Decl::Node(G::Nodes::User, key), Decl::Edge(head), …])
    .hyper_edge(HyperEdge { kind, id, payload })        // id None = create, Some = merge
    .reduce_node::<User>(|u: &mut User| u.seen += 1)    // per collection: node rows are heterogeneous
    .reduce_edge(|e: &mut E| e.weight += 1)             // one payload type per graph
    .reduce_hyper_edge(|h: &mut Dict| …)                // groups are dynamic
```

Endpoints are created on demand, membership is idempotent (same `(kind, group, member)` upserts), and all three reduces run in the same batch — which is where ADR-0028's counters land. Other operations ride the same row kind: `remove`, `merge`, `split`, `reassign`, `lookup`, `members`, `nodes`, `diff`.

**8. Three fixed faces, taken from the freed core band.** `0x0A`, `0x0B`, `0x0C` — contiguous, and the first three numbers the vector snapshot's move left free. Three faces rather than one face plus an in-row byte, because the model's rule is that the face states what a row is; `0x0D` stays free and `0x0E` is Extension. Counting families are not used for groups: their counters are declaration order with a 4095 ceiling, while groups are runtime-unbounded.

## Consequences

- A fact can belong to many groups, with the graph topology untouched.
- "What is in this group" and "what changed between these two versions" are scans and set operations, not traversals.
- Storage grows by roughly three rows per grouped edge (two derived node rows plus the membership row); the number to watch is rows per edge.
- Anything that recomputes groups must write a version rather than replace; readers see a consistent snapshot of one version.

## Rejected

- **A node per group (star / reified).** Loses set identity, shared membership and per-edge roles, and pollutes algorithms with fake hubs.
- **A counting family per group.** Bounded at 4095 and semantically "declaration order" — wrong for run-time groups.
- **Storing the set as one value.** A set-valued entry is a cross-row read-modify-write, which the write contract refuses.
- **A separate hypergraph declaration.** The graph already declares members; the write API above is the whole addition.
