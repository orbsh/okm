# ADR-0017: Graph Edge — the third relation carrier

Date: 2026-09-19
Status: Draft
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

- **Primary (0x0)**: the edge body — `[src NodeRef][dst NodeRef]
  [kind_id][attributes]`. Attributes are a dynamic segment (slot-1
  frames within the value): graph attributes are naturally dynamic.
  Declared attribute fields are a possible later enhancement.
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
  `#[ok_edge]`-shaped declarations; the ns registry is compile-time
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
