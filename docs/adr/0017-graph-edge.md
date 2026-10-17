# ADR-0017: Graph Edge — the third relation carrier

Date: 2026-09-19
Status: Accepted (2026-09-20; both forms implemented — EdgeEncode derive + Graph<S, E> fixed-ontology, okm-dynamic Graph<S> fully dynamic). Updates 2026-09-20: §2 endpoint refs re-decided to scheme b (self-describing [ns 2B][len varint][pkey]) — scheme c (registry) abandoned; §3 declared attribute fields, node-side kind filtering, slot-layout clarification. Updates 2026-10-17: §3's edge payload is an obj (ADR-0012) — one document declaration generates both codec directions and the declared faces, so the derive's encoder-only `attrs()` / `attr_faces()` / `ATTR_SLOTS` half-mechanism retires; §3's write protocol, §4 and Consequences follow, and the registry wording §2 abandoned is swept out of §4, §5 and Consequences.
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

### 3. Slot allocation (fixed faces and counting families, within the Edge ns)

```text
0x00  primary       [edge_id]                                → value = edge body
0x02/0x03  kind dictionary  (name ↔ kind_id, reused mechanism)
0x04  kind index    [kind_id]                                → edge_id set
0x05  out-edges     [src NodeRef][edge_id]
0x06  in-edges      [dst NodeRef][edge_id]
0x07  kind+out      [kind_id][src NodeRef][edge_id]
0x08  kind+in       [kind_id][dst NodeRef][edge_id]
```
(The graph's faces are fixed-face numbers — one byte each, ADR-0030; the
declared attribute faces below are counting family 0x1.)

**Slot layout versus ordinary documents (Update 2026-09-20)** — stated
explicitly, though not identical-by-construction:

- **Node collection = the standard document layout, unchanged.** The
  node kind face is NOT a new kind of slot: it is an ordinary access
  method in counting family 0x1 (a declared `#[ok_index]` in the
  fixed-ontology form, a caller-allocated `AccessMethod` slot in the
  dynamic form). Node kinds allocate through the collection's own
  0x02/0x03 dictionary — the same dictionary every collection has.
  Zero layout difference from any document collection; "one field is
  the kind" is a modeling convention, not a layout fact.
- **Edge collection = the eight entry kinds above, a different
  collection shape written in the SAME slot vocabulary.** 0x00/0x01/
  0x02/0x03 keep their document-side meanings (primary, dynamic
  segment, dictionary); 0x04–0x08 are the graph's own faces inside the
  fixed-face range — no new kind of slot is introduced. The value side
  is not a second encoding either: the edge's payload IS an obj
  (ADR-0012). What makes this collection an Edge rather than a
  document collection is the graph's faces and the self-describing
  endpoints, not a payload format of its own.

- **Primary (0x00)**: the edge body — `[src NodeRef][dst NodeRef]
  [kind_id][obj payload]`. The trailing region is an ordinary obj
  payload (ADR-0012): `[version u8][hot_len u16 BE][hot segment][cold
  TLV]`, the encoding every document row uses. `src` / `dst` /
  `kind_id` are the graph's own head, and the payload starts at a
  computable offset because refs are self-describing and `kind_id` is
  fixed width.
- **Declared attribute fields**: in the fixed-ontology form the edge
  declaration IS a document declaration — `#[ok_edge(ns = N)]` on a
  `DocumentEncode` type — so a static field beyond src/dst/kind is an
  ordinary declared field: hot/cold placement, a generated encoder AND
  decoder, and a generated 0x1 face (`[field value][edge_id]`, counting
  family 0x1, `0x1001+n` in declaration order). That is the shape the bespoke
  encoder-only half-mechanism (`KvGraph::attrs`, `attr_faces`,
  `ATTR_SLOTS`, and with them the `EdgeEncode` payload derive)
  produced by hand, and it retires with this decision: one
  declaration, one codec, both directions, the widths living in the
  declaration instead of in every consumer. The edge's identity for
  those entries is its own key (`[edge_id u64]`, a `KeyEncode`
  newtype), so document index entries tail with the edge id exactly as
  the attr faces did, and the same `index_entries` path maintains
  them. Filtering still runs as an index scan over that face, decoded
  back to the primary, instead of fetching every edge and decoding;
  field values are the edge's own data (no NodeRef) and never touch
  endpoint widths. No composite faces (`kind+field`, `field+endpoint`)
  are built by default — filter by the most selective face and
  converge edge_ids in memory, the same stance as document indexes; a
  composite face is added only when a proven access pattern demands
  it. Each declared field grows link/unlink by one entry — write
  amplification the declarer opts into.
- **Dynamic attributes (fixed face 0x01)**: the edge's open attributes are the
  obj's dynamic segment — ONE entry per edge, `[ns][0x01][edge_id]`, nTLV
  frames whose field names are dictionary numbers (ADR-0012) — i.e.
  the same entry, codec and `put_fields`/`get_fields` surface any
  document row has. The graph assembly owns that entry (callers stop
  hand-rolling frames). An open attribute a query later needs is
  reached through a path index over its declared path (ADR-0029 — one
  entry per resolved value, the value types with a storage-form
  encoding), never by widening the key. The fully dynamic form needs
  no change: its declaration is empty and its free attributes ride the
  same obj shape.
- **Node-side kind filtering (Update 2026-09-20)**: filtering by node
  kind / node attributes is a basic graph-query need, answered
  per form. In the fixed-ontology form a node IS an ordinary document
  collection: node "type" = the collection itself (the ns is the type
  marker), and attribute filtering rides the collection's own
  `#[ok_index]` declarations — nothing new. In the fully dynamic form
  node kinds are dynamic-segment data, so two existing mechanisms
  carry the filter, no new segment: (1) **node kind dictionary** —
  every collection already owns its own 0x02/0x03 dictionary; node kinds
  allocate through it, and the node collection's dictionary is
  naturally separate from the edge collection's (dictionaries are
  per-collection). (2) **node kind face** — one declared access method
  over the node collection's ns, `[kind_id][node pkey]`, maintained by
  the node's normal put path. Composite filtering
  (`(*)-[edge_kind]->(:node_kind)`) scans the 0x07/0x08 face and the node
  kind face and intersects the id sets in memory — two cheap faces,
  one convergence (no triple-composite face is built for it).
- **Kind index (0x04)**: all edges of one kind — the `MATCH ()-[r:has]->()`
  entry point. `kind_id` comes from the kind dictionary (0x02/0x03):
  write-time name → id, so every key segment stays fixed-width and
  sortable.
- **Traversal (0x05/0x06)**: all out-edges / in-edges of one node. The
  full NodeRef leads so the scan prefix matches; `edge_id` trails —
  parallel edges (two facts between the same nodes) each get their own
  entry, and the trailing id decodes back to the primary.
- **Typed traversal (0x07/0x08)**: the `(*)-[kind]->(*)` face — kind
  leads because it is the filter dimension here; kind cannot be a
  trailing segment on this face.
- **Write protocol**: `link` = primary + one counting-family 0x1 entry per
  declared field + 0x04 + 0x05 + 0x06 + 0x07 + 0x08 in one batch; the
  obj's dynamic entry (fixed face 0x01) rides the same batch when the
  caller wrote fields.
  `unlink` deletes the same entries. Symmetric with the Junction's
  double-write obligation, scaled to the face count.

### 4. Two forms, one mechanism

- **Fixed ontology**: edge kinds and node collections declared at
  compile time. The edge declaration is a document declaration
  (`DocumentEncode` plus `#[ok_edge]` for the graph's ns), and the
  assembly point generates the six-face entries from it — declared
  fields are the obj's own declared indexes, one 0x1 face each (see
  §3).
- **Fully dynamic** (knowledge graphs): a `KgNode` collection and a
  `KgEdge` collection, **empty declarations** — identity only, payload
  entirely in the dynamic segment (declaring `props: DynamicValue`
  would put it in slot 0, which is the static regime — not what a
  dynamic graph wants). Kinds and node kinds are dynamic-segment data;
  endpoints are self-describing refs (§2), so no endpoint declaration
  or registry exists here either; traversal faces are built through the
  dynamic layer's `AccessMethod` construction.

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
self-describing references (`[ns 2B][len varint][pkey]`) instead of
compile-time typed keys, and the trailing `edge_id` (instead of the
peer identity) because parallel edges exist. Everything else — the
double-write obligation, the entry-face design, the ns discipline —
carries over.

## Consequences

- The edge declaration reuses the document codec (`DocumentEncode` +
  `#[ok_edge(ns = N)]`): there is no second payload encoding and no
  encoder-only attribute half to keep in step. The six-face write
  protocol lives in a `Graph<S, E>` assembly point symmetric with
  `Junction<S, E>`.
- Endpoint refs are self-describing (`[ns 2B][len varint][pkey]`), so
  there is no cross-collection width registry and no endpoint
  declaration (§2).
- No change to ADR-0016's slot allocation: 0x04–0x08 are the graph's
  fixed faces inside the Edge collection's ns (ADR-0030).
- No consumer exists yet; the design is recorded before
  `#[ok_relation]` work begins, since both share the write-path diff
  machinery. (Update 2026-09-20: the `#[ok_relation]` field-position
  form was subsequently REJECTED — ADR-0015 Future work §B; the
  junction's imperative link/unlink pairing stands, and this ADR's
  put-path `link_into` batch surface is unaffected.)
