# 0068 — A cluster can hold nodes

Status: decided 2026-11-17, implemented. Narrows ADR-0066 §2 (which left the participants as a read-side projection and refused a node face) and adds two faces to the band ADR-0066 opened.

## Context

ADR-0066 decided that a cluster's members are edges, and that a cluster's NODES are its member edges' endpoints, projected on the reading side. Two consequences of that decision bite:

- **An entity with no edge cannot be in a group at all.** The model's completeness rule is that a node exists only as an endpoint (`Error::BareNode`), so a permission boundary cannot name an entity that has no assertion yet, and an empty community cannot say which entities it would have held.
- **"Which groups is this entity in" costs one scan per incident edge** — `out-degree + in-degree + 1` prefix scans, as the cluster stream's own cost note states. That is the right price for an edge-keyed face and the wrong shape for a hub.

A set of entities is also not a worse spelling of a set of edges. Community detection's output is a vertex partition; encoding it as "the edges this run grouped" needs a boundary convention for every cross-group edge (left, right, both) and silently loses every isolated vertex. The two are different relations over the same graph, and the store should be able to hold whichever one the producer has.

## The decision

**1. Two member faces, one cluster identity, one kind vocabulary.**

```text
0x0D  edge member       [ns][kind][hyper_id][src_h][pred][dst_h]  → empty (authority)
0x12  node member       [ns][kind][hyper_id][handle]              → empty (authority)
0x0F  edge → cluster    [ns][src_h][pred][dst_h][kind][hyper_id]  → empty (derived from 0x0D)
0x13  node → cluster    [ns][handle][kind][hyper_id]              → empty (derived from 0x12)
```

The face numbers are the layout module's; the module is the only place they are written down.

There is **no edge → node-cluster face**. An edge's endpoints are exactly two, so "which node clusters does this edge touch" is two prefix scans of `0x13` — constant. A node's degree is unbounded, which is why the edge side needs its mirror and the node side does not. The asymmetry is cardinality, not taste.

**2. The member kind is carried by the face — not by `kind`, not by a flag.** `kind` stays the caller's vocabulary, with okm assigning it no meaning. ADR-0066 states that the store never learns its application's nouns; the same rule forbids okm from imposing its own noun (member kind) on the application's vocabulary, and a flag byte in the row head would cost a value-layout stamp while still leaving an empty cluster unable to answer, because there is no row to read it from.

So a cluster may hold edge members, node members, or both, and **"who is in this cluster" is the union of the declared node members and the endpoints of its member edges**. A scope's size is a fact about its rows, and the rows are the answer.

**3. Node membership is DECLARED truth, not a projection of the member edges.** This is the part that narrows ADR-0066 §2. A projection cannot hold an entity with no edge, and it cannot survive an edge deletion without dropping a group the entity still belongs to; a member row can do both, and it is the same "one row per member" that ADR-0066 §1 chose over reifying the group as a node.

**4. One rule changes, and it is stated once:** a node exists as an endpoint of some edge **or** as a member of a node cluster. `Error::BareNode` keeps its meaning on the edge path — there is still no "create a bare node" verb — and `delete_node` therefore cannot use "no incident edge = not in the graph" as its test any more; it sweeps the node's `0x13` run.

**5. Faces and roles match the edge side exactly.** `0x12` is Truth, `0x13` is Derived and rebuildable from `0x12` alone; `rebuild_derived` gains a phase for it beside the one `0x0F` already has. Deletion follows the same rule: deleting a cluster takes both member faces and both mirrors, deleting a **node** takes its `0x12` rows (found through one `0x13` scan), and deleting an **edge** leaves every node cluster alone — an entity does not leave a group because an assertion about it went away.

**6. Counts are re-stamped, never incremented.** `members` stays the edge-member count; `nodes` is the node-member count. Both are computed from the face on every write that touches it, which is how the existing count already works and why the write path needs no read-modify-write.

**7. One declaration, no special case.** `Cluster::nodes(iter)` declares node members; the fragment's edges still join the same cluster, so a mixed scope is one declaration rather than two shapes. The verbs follow the subject: `members` answers edges, `nodes` answers nodes, `member_of` / `node_member_of` are the two point tests, and `clusters_of(node)` is the union of `0x13` and the incident-edge path.

## Naming

The prose says **edge cluster** and **node cluster**; the code needs no new word at all, because the existing axis already generalises: a cluster-first face is called `*_MEMBER_SLOT`, an X-first mirror is called `*_CLUSTER_SLOT`, and the only edit is `EDGE_` → `NODE_`.

`hyper-edge` / `hyper-node` is refused. In the literature a hyperedge is a set of **vertices**, so the pair is inverted here — a node set is the hyperedge, and an edge set is one only through the line graph, which is the mismatch ADR-0066 already flags. And `hypernode` names the reified set-as-node of the incidence (star) expansion, which is precisely the shape ADR-0066 §1 rejected: adopting the word would put the rejected design back in the vocabulary.

## Costs, stated where they are paid

- A node member row is 21 bytes against the edge member row's 31, and its mirror is 21 against 31.
- A declared node group writes exactly the nodes it declares — no per-edge multiplier. Its write pays one scan of the cluster's own `0x12` run to re-stamp `nodes`, the same shape the edge face already pays for `members`.
- Ordinary node and edge writes still touch no cluster face: the plan is built from the fragment's declarations alone, so a fragment that declares no cluster has an empty plan and reads nothing.
- Cascade: deleting a node adds one `0x13` prefix scan (precise, and cheaper than the full-face suffix scan the edge path needs), plus one re-read per affected cluster for the count.
- An edge cluster's participants are still a read-side projection. That is unchanged and still the cheap answer: the group's edges are contiguous, and the dedupe is a sorted insert.

## Rejected

- **A derived node face — the mirror ADR-0066 §2 refused.** It cannot hold an entity with no edges, and on a retract it cannot tell "this endpoint's last member edge left" from "this entity belongs here anyway" without a per-`(cluster, node)` count, i.e. a row many edges write — the cross-row read-modify-write the write contract refuses (ADR-0066's own rejection of "the set as one value", and the bitmap entry in PLAN). Steering around it by re-deriving the whole face on every membership change turns each edge delete into a scan of its cluster.
- **The member kind as a `kind` value.** `kind` is the caller's vocabulary; okm imposing a value would make one store's nouns depend on another's structure, and it would collide with an application that wants its own kind number for a node group.
- **A flag byte in the cluster row's head.** It changes the `0x0C` value layout (ADR-0042 stamp) to answer a question the faces already answer — and cannot answer it for an empty cluster, which the model explicitly allows and treats as information.
- **An edge → node-cluster face.** Fixed arity two: the derivation is two prefix scans, and a face would be a row per `(edge, cluster)` maintained against a constant-cost read.
- **Node members only, dropping edge members.** Loses the set's identity over assertions: which of two edge sets over the same vertices, an edge shared by two groups, and an edge's own role. ADR-0066 §1 priced those and they still hold.

## Consequences

- "Which groups is this entity in" is one scan of `0x13`, with the incident-edge path unioned in — the entity-side recall stops being degree-linear.
- A community run can write a vertex partition directly, with no boundary convention for cross-group edges.
- Two relations share one cluster identity: an edge cluster's participants are derived, a node cluster's are declared, and a mixed scope answers the union. No invariant ties the two faces together, because they are different facts and not two copies of one.
- The chain of node existence is no longer single: an endpoint, or a declared member. Every path that assumed the first must say which one it means.