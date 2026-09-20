# ADR-0017: Graph Edge — the third relation carrier

Date: 2026-09-19
Status: Accepted (2026-09-20; both forms implemented — EdgeEncode derive + Graph<S, E> fixed-ontology, okm-dynamic Graph<S> fully dynamic). Updates 2026-09-20: §2 endpoint refs re-decided to scheme b (self-describing [ns 2B][len varint][pkey]) — scheme c (registry) abandoned; §3 declared attribute fields, node-side kind filtering, slot-layout clarification.
Related: ADR-0015 (relation taxonomy), ADR-0016 (4-byte head, segment-numbered slots), ADR-0002 (namespace dictionary)

## Context

The relation taxonomy (ADR-0015 §3) has two carriers: `Refs<D, K>` for
one-to-many (field position, single direction, children of a fixed
parent type) and `Junction` for many-to-many (standalone entries,
double-materialized, both endpoints compile-time typed). Both bind
their endpoints to **declared document types at compile time** — the
derive resolves `<D as Document>::Key` and `NS_PREFIX`.

Graph applications break this binding. A knowledge graph or an LLM-
written property graph has:

- **Heterogeneous endpoints** — one graph connects nodes of many types
  (Person, Org, Document, …); declaring one Edge type per endpoint-type
  pair is a combinatorial explosion, and PGQ semantics allow any node
  table to participate.
- **Runtime-emerging kinds** — edge kinds ("has", "owns") are data, not
  compile-time vocabulary. Same for node kinds in the fully dynamic form.
- **Edge identity and attributes** — parallel edges exist (two "has"
  facts between the same nodes are two facts), and edges carry dynamic
  attributes. Neither carrier supports this: a Junction's identity IS
  the endpoint pair, and neither has per-edge payload.
- **Independent existence** — edges outlive or precede their endpoint
  documents' presence in query scope; the edge set is a first-class
  collection, not a derived view.

`Refs` cannot express this (fixed child type, field position, one
direction, no attributes). `Junction` cannot either (fixed endpoint
types, no edge identity, no attributes). The graph edge is the **third
relation carrier**.

## Decision

### 1. Edge = a first-class collection, one ns per graph

A graph's edges live in their own Edge collection — one ns for the
edge set of one graph, allocated through the normal ns dictionary
(ADR-0002, "the dictionary is code"), symmetric with any document
collection. No new namespace mechanism: not per-edge-kind ns (kinds
are runtime data in the dynamic form), not a global edge pool (a graph
isolates at the ns layer), not residency (edges exist independently of
endpoint documents).

The same ns hosts every entry kind of the edge set, dispatched by the
segment-numbered slots of ADR-0016.

### 2. Self-describing endpoint references

Endpoints are **not compile-time typed**. A reference is
`[ns 2B BE][pkey]` — any collection's document, any key shape. The
ns acts as the type marker: decoding a reference routes by ns to the
owning collection, so node type is unrestricted.

The pkey boundary is **not stored in the key**. The width of a
collection's pkey is a schema fact (`KeyEncode::KEY_LEN`), and the
Edge implementation resolves it through a **ns → KEY_LEN registry**.
Three candidate schemes were weighed:

- **a. Fixed-width convention** — require all node collections in one
  graph to share a single pkey width (e.g. all u64 ids). Zero key
  overhead, but a hard constraint on modeling: one heterogeneous node
  with a 20-byte composite key breaks it, and cross-graph reuse of the
  registry gets awkward. Rejected — too rigid for real graphs.
- **b. Self-describing segment (TLV)** — `[ns 2B][pkey_len varint]
  [pkey]`. Fully general, works for any pkey shape, decoding needs no
  registry. Cost: every reference grows by 1+ bytes (the len prefix),
  and every decode pays a varint step. Rejected — the Edge collection
  holds thousands of references, and the per-reference overhead
  multiplies across the traversal faces (0x5-0x8), which are the
  highest-volume entries in the store.
- **c. Registry lookup (CHOSEN)** — pkey width comes from the schema
  (`KeyEncode::KEY_LEN`); the Edge implementation holds a **ns →
  KEY_LEN registry** (compile-time generated for the declared form, a
  runtime table for the dynamic form) and slices the pkey by lookup.
  Zero key overhead, extends the Ref discipline — "the reference is
  key bytes, ns knowledge lives at the declaration point" — from one
  bound endpoint type to a registry of endpoint types. The registry is
  a new cross-collection schema fact: compile-time form is generated,
  dynamic form is user-maintained.

- **Update 2026-09-20 — scheme b (self-describing ref) CHOSEN, scheme c
  abandoned.** The original draft's rejection of b mis-counted: the
  shared varint codec encodes pkey widths (4–16 bytes) in ONE byte, so
  the per-ref cost is ~1 byte (~5% on traversal-face keys), not "1+
  bytes multiplying across the face". Against that stands everything
  the registry costs: a new cross-collection schema fact, a
  `nodes(...)` declaration whose hand-written width literals drift
  from the node keys, a runtime `register()` ceremony in the dynamic
  form, and a decode failure mode (unregistered ns) that exists only
  to defend the registry itself. Refs become
  `[ns 2B][len varint][pkey]` — width is wire data, traversal faces
  still prefix-match exactly (same node = same bytes), and both forms
  need zero endpoint declaration. The Ref discipline is thereby
  RETIRED for graph edges: ns knowledge no longer lives at a
  declaration point; it lives in the ref itself.

### 3. Slot allocation (ADR-0016 segments, within the Edge ns)

```text
0x0  primary      [edge_id]                                  → value = edge body
0x2/0x3  kind dictionary  (name ↔ kind_id, reused mechanism)
0x4  kind index   [kind_id]                                  → edge_id set
0x5  out-edges    [src NodeRef][edge_id]
0x6  in-edges     [dst NodeRef][edge_id]
0x7  kind+out     [kind_id][src NodeRef][edge_id]
0x8  kind+in      [kind_id][dst NodeRef][edge_id]
```

**Slot layout versus ordinary documents (Update 2026-09-20)** — stated
explicitly, though not identical-by-construction:

- **Node collection = the standard document layout, unchanged.** The
  node kind face is NOT a new segment: it is an ordinary access method
  in the 0x1 segment (a declared `#[ok_index]` in the fixed-ontology
  form, a caller-allocated `AccessMethod` slot in the dynamic form).
  Node kinds allocate through the collection's own 0x2/0x3 dictionary
  — the same dictionary every collection has. Zero layout difference
  from any document collection; "one field is the kind" is a modeling
  convention, not a layout fact.
- **Edge collection = the eight entry kinds above, a different
  collection shape written in the SAME segment language.** 0x0/0x1/
  0x2/0x3 keep their document-side segment semantics (primary,
  declared indexes, dictionary); 0x4–0x8 are ordinary in-ns segments
  of the Edge collection per ADR-0016 — no new segment type is
  introduced. The Edge is not "a document with special segments"; it
  is another kind of collection composed from the shared vocabulary.

- **Primary (0x0)**: the edge body — `[src NodeRef][dst NodeRef]
  [kind_id][attributes]`. Attributes are a dynamic segment (slot-1
  frames within the value): graph attributes are naturally dynamic.
- **Declared attribute fields (Update 2026-09-20)**: in the fixed-
  ontology form the edge declaration may carry static fields beyond
  src/dst/kind. Each declared field is an ordinary declared index —
  segment 0x1 (`0x1001+n`, declaration order), one face
  `[field value][edge_id]` per field. The derive generates the access
  method directly; filtering runs as an index scan over that face and
  decodes back to the primary, instead of fetching every edge and
  decoding. Field values are the edge's own attributes — no NodeRef,
  fixed width by construction — so they never touch the ns → KEY_LEN
  registry. This is disjoint from the kind dictionary: kinds are a
  runtime-growing vocabulary, declared fields are compile-time
  declarations encoded straight into the index key. No composite faces
  (`kind+field`, `field+endpoint`) are built by default — filter by
  the most selective face and converge edge_ids in memory, the same
  stance as document indexes; a composite face is added only when a
  proven access pattern demands it. Dynamic form is unaffected (empty
  declarations, attributes entirely in the dynamic segment). Each
  declared field grows link/unlink by one entry — write amplification
  the declarer opts into.
- **Node-side kind filtering (Update 2026-09-20)**: filtering by node
  kind / node attributes is a basic graph-query need, answered
  per form. In the fixed-ontology form a node IS an ordinary document
  collection: node "type" = the collection itself (the ns is the type
  marker), and attribute filtering rides the collection's own
  `#[ok_index]` declarations — nothing new. In the fully dynamic form
  node kinds are dynamic-segment data, so two existing mechanisms
  carry the filter, no new segment: (1) **node kind dictionary** —
  every collection already owns its own 0x2/0x3 dictionary; node kinds
  allocate through it, and the node collection's dictionary is
  naturally separate from the edge collection's (dictionaries are
  per-collection). (2) **node kind face** — one declared access method
  over the node collection's ns, `[kind_id][node pkey]`, maintained by
  the node's normal put path. Composite filtering
  (`(*)-[edge_kind]->(:node_kind)`) scans the 0x7/0x8 face and the node
  kind face and intersects the id sets in memory — two cheap faces,
  one convergence (no triple-composite face is built for it).
- **kind index (0x4)**: all edges of one kind — the `MATCH ()-[r:has]->()`
  entry point. `kind_id` comes from the kind dictionary (0x2/0x3):
  write-time name → id, so every key segment stays fixed-width and
  sortable.
- **Traversal (0x5/0x6)**: all out-edges / in-edges of one node. The
  full NodeRef leads so the scan prefix matches; `edge_id` trails —
  parallel edges (two facts between the same nodes) each get their own
  entry, and the trailing id decodes back to the primary.
- **Typed traversal (0x7/0x8)**: the `(*)-[kind]->(*)` face — kind
  leads because it is the filter dimension here; kind cannot be a
  trailing segment on this face.
- **Write protocol**: `link` = primary + 0x4 + 0x5 + 0x6 + 0x7 + 0x8 in
  one batch (six entries); `unlink` deletes the same six. Symmetric
  with the Junction's double-write obligation, scaled to the face count.

### 4. Two forms, one mechanism

- **Fixed ontology**: edge kinds and node collections declared at
  compile time. The Edge derive generates the six-face entries from
  `#[ok_edge]`-shaped declarations (declared attribute fields add one
  0x1 face each, see §3); the ns registry is compile-time
  constants.
- **Fully dynamic** (knowledge graphs): a `KgNode` collection and a
  `KgEdge` collection, **empty declarations** — identity only, payload
  entirely in the dynamic segment (declaring `props: DynamicValue`
  would put it in slot 0, which is the static regime — not what a
  dynamic graph wants). Kinds and node kinds are dynamic-segment data;
  the endpoint registry is a runtime table; traversal faces are built
  through the dynamic layer's `AccessMethod` construction.

Both forms share the same wire layout and the same slot semantics;
they differ only in where the declaration lives (derive constants vs
runtime schema).

### 5. Position in the taxonomy (ADR-0015 §3, amended)

Three relation carriers, divided by two axes — endpoint typing and
edge independence:

```text
                    endpoint type fixed          endpoint type open
one direction       Refs<D, K>                   —
both directions     Junction                     Graph Edge
```

Graph Edge differs from Junction structurally in exactly two ways,
both forced by the open-endpoint requirement: endpoints are
self-describing references (ns + registry-delimited pkey) instead of
compile-time typed keys, and the trailing `edge_id` (instead of the
peer identity) because parallel edges exist. Everything else — the
double-write obligation, the entry-face design, the ns discipline —
carries over.

## Consequences

- New derive (`EdgeEncode` variant or a distinct `GraphEdgeEncode`) +
  a small runtime registry type. The six-face write protocol lives in
  a `Graph<S, E>` assembly point symmetric with `Junction<S, E>`.
- The ns → KEY_LEN registry is a new cross-collection schema fact;
  compile-time form is generated, dynamic form is user-maintained.
- No change to ADR-0016's slot segments: 0x4–0x8 are ordinary
  in-ns segments of the Edge collection.
- No consumer exists yet; the design is recorded before
  `#[ok_relation]` work begins, since both share the write-path diff
  machinery.
