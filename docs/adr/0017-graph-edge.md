# ADR-0017: Graph Edge — the third relation carrier

Date: 2026-09-19
Status: Accepted (2026-09-20; both forms implemented — EdgeEncode derive + Graph<S, E> fixed-ontology, okm-dynamic Graph<S> fully dynamic). Updates 2026-09-20: §2 endpoint refs re-decided to scheme b (self-describing [ns 2B][len varint][pkey]) — scheme c (registry) abandoned; §3 declared attribute fields, node-side kind filtering, slot-layout clarification. Updates 2026-10-17: §3's edge payload is an obj (ADR-0012) — one document declaration generates both codec directions and the declared faces, so the derive's encoder-only `attrs()` / `attr_faces()` / `ATTR_SLOTS` half-mechanism retires; §3's write protocol, §4 and Consequences follow, and the registry wording §2 abandoned is swept out of §4, §5 and Consequences. Updates 2026-10-17 (second): the declaration is a GRAPH declaration (`GraphEncode` — one ns, the edge's own payload, explicitly declared attribute faces, plus a compile-time node-type enum); `EdgeEncode` retires with its encoder-only half; `Graph<S, E>` is a runtime HANDLE that declares nothing (no ns parameter, no node list, no schema); the member node set lives in code as a `Nodes` enum, never in a slot; `unlink` deletes precisely by decoding the edge body; Context's PGQ sentence corrected. Code status: LANDED 2026-10-19 — `GraphEncode` derive (member set via `#[ok_nodes(Enum)]`), predicate vocabulary at 0x04/0x05, faces 0x06–0x0A, `Graph::new(store)`, exact unlink; okm-dynamic's handle takes the ns + schema pair (`CollectionSchema::ns_prefix`).
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
  pair is a combinatorial explosion. PGQ draws the line the other way
  round, and this ADR follows it: a property graph DECLARES its vertex
  tables and each edge table's SOURCE/DESTINATION references — an edge
  table may reference several vertex tables, and one table may appear as
  both a vertex and an edge under different aliases — while the node TYPE
  per edge is one declaration, not one per pair. What PGQ forbids, and
  what is forbidden here, is undeclared participation.
- **Runtime-emerging predicates** — the edge's predicate ("has", "owns"),
  i.e. its type, is data: it allocates through the graph's own predicate
  dictionary (0x04/0x05, §3). The node COLLECTIONS are the opposite case. The ns
  vocabulary is compile-time closed (ADR-0002, "the dictionary is code"),
  so a knowledge graph's nodes are rows of DECLARED collections (a facts
  collection, an entities collection, a users collection) and never
  collections created on the fly; a node "type" inside one collection is a
  payload field. Same for node kinds in the fully dynamic form.
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
collection. No new namespace mechanism: not a per-predicate ns
(predicates are runtime data in the dynamic form), not a global edge pool (a graph
isolates at the ns layer), not residency (edges exist independently of
endpoint documents).

The same ns hosts every entry type of the edge set, dispatched by the
segment-numbered slots of ADR-0016.

**The member node set is a declaration, not a registry (Update
2026-10-17).** A graph declares which collections may appear as its nodes
— a compile-time fact, in code, next to the ns dictionary it belongs to
(a `Nodes` enum; §4). No slot holds it: the set adds no runtime
information (every candidate collection is already a compile-time ns),
and a metadata entry would only add "who writes it, when, and who wins a
conflict". Runtime creation of node collections is rejected with it: a
knowledge graph's shapes are declared collections whose rows carry a type
field — one ns holding many runtime types — which is strictly cheaper
than an ns per emergent type, and the only reading that keeps the ns
dictionary closed (PGQ's own rule: tables are declared, not conjured).

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
  multiplies across the traversal faces (0x07-0x0A), which are the
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

- **Update 2026-10-17 — the ref stays self-describing, the member set is a
  CHECK.** Refs keep `[ns 2B][len varint][pkey]` on the wire: a script or
  LLM writer has no `Nodes` enum, and a ref must be writable without one.
  The declaration's member set is applied at decode (ns → variant;
  out-of-range = error, never silent) and at write time (the endpoint's
  collection must be declared). Widths still come from the ref itself —
  the retired registry was a WIDTH lookup, the member set is a TYPE
  restriction: the first is wire data, the second is a declaration.

### 3. Slot allocation (fixed faces and counting families, within the Edge ns)

```text
0x00  primary            [edge_id]                       → value = edge body
0x01  dynamic segment    (the edge's open attributes)   → one entry per edge
0x02/0x03  field-name dictionary   (the payload's own names ↔ ids)
0x04/0x05  kind vocabulary         [predicate name ↔ predicate_id]  (this graph's)
0x06  predicate index    [predicate_id]                  → edge_id set
0x07  out-edges          [src NodeRef][edge_id]
0x08  in-edges           [dst NodeRef][edge_id]
0x09  predicate+out      [predicate_id][src NodeRef][edge_id]
0x0A  predicate+in       [predicate_id][dst NodeRef][edge_id]
```
(The graph's faces are fixed-face numbers — one byte each, ADR-0030; the
declared attribute faces below are counting family 0x1. Rows are grouped
by ROLE — the two vocabularies first, then the faces that read their ids:
a dictionary leads the faces it serves. The NUMBERS stay the mechanism.)

**The predicate has its OWN dictionary (Update 2026-10-17).**
`[ns][0x04][predicate_id] → name` and `[ns][0x05][predicate name] →
predicate_id` — the same bidirectional shape as 0x02/0x03 (id→name leads,
the name carries the bytes), a different vocabulary. Listing a graph's predicates is then ONE prefix scan of 0x04:
an exact list, never reverse-derived by walking the index faces. Sharing
one vocabulary with the payload's field names (the previous shape) left
the names indistinguishable and made "which predicates does this graph
have" answerable only by scanning 0x06 — a listing must not be an
inference. Splitting one vocabulary by id band was rejected for the same
class of reason the project separates physically elsewhere: a band is a
convention that breaks silently on the first out-of-band allocation,
while a second face pair is structural. Cost: one fixed-face pair of the
sixteen (0x04/0x05, ADR-0030; 0x0B–0x0F stay free) and two vocabularies
in the edge collection. The field-name dictionary (0x02/0x03) keeps its
document-side meaning for the payload's own names.

**The pair is okm's KIND VOCABULARY (Update 2026-10-17).** 0x04/0x05 is not
graph-specific: it is a collection's own token vocabulary BEYOND field names —
a graph's PREDICATES here, and a NODE COLLECTION's kind tokens where it
declares one. A document collection with kinds keeps field names in 0x02/0x03
and its kinds here, so two allocators never share one id space; the kind field
itself stays an ordinary payload field with a declared index. Same pair, same
rule, whichever collection needs it.

**"kind" is retired as a name for the edge's type (Update 2026-10-17).**
It carries no more information than "type" and, in prose, cannot be told
apart from a NODE's kind (a payload field). The edge's type is its
PREDICATE: `predicate_id` on the wire, the predicate dictionary at
0x04/0x05, "predicate index" / "predicate-filtered traversal" for the
faces. `edge_id` was considered and rejected — that name is the edge's
own identity (the primary key), and reusing it for the third head segment
would make `[src][dst][edge_id][payload]` read as if it named the edge.

**Slot layout versus ordinary documents (Update 2026-09-20)** — stated
explicitly, though not identical-by-construction:

- **Node collection = the standard document layout, unchanged.** The
  node kind face is NOT a new kind of slot: it is an ordinary access
  method in counting family 0x1 (a declared `#[ok_index]` in the
  fixed-ontology form, a caller-allocated `AccessMethod` slot in the
  dynamic form). Node kinds allocate through the collection's own kind
  vocabulary (0x04/0x05) — the pair in §3, kept apart from the
  field-name dictionary (0x02/0x03) its props use. Zero layout difference
  from any document collection; "one field is the kind" is a modeling
  convention, not a layout fact.
- **Edge collection = the entry types above, a different collection
  shape written in the SAME slot vocabulary.** 0x00/0x01/0x02/0x03 keep
  their document-side meanings (primary, dynamic segment, dictionary);
  0x06–0x0A are the graph's own faces and 0x04/0x05 its own predicate
  dictionary, all inside the fixed-face range — no new KIND of slot is
  introduced. The value side
  is not a second encoding either: the edge's payload IS an obj
  (ADR-0012). What makes this collection an Edge rather than a
  document collection is the graph's faces and the self-describing
  endpoints, not a payload format of its own.

- **Primary (0x00)**: the edge body — `[src NodeRef][dst NodeRef]
  [predicate_id][obj payload]`, where `predicate_id` is the PREDICATE
  (the edge's type) carried as a key dimension: 2 bytes from the graph's
  own predicate dictionary (0x04/0x05), read back by name through it. The
  trailing region is an ordinary obj
  payload (ADR-0012): `[version u8][hot_len u16 BE][hot segment][cold
  TLV]`, the encoding every document row uses. `src` / `dst` /
  `predicate_id` are the graph's own head, and the payload starts at a
  computable offset because refs are self-describing and `predicate_id`
  is fixed width.
- **Declared attribute fields — declared EXPLICITLY (Update
  2026-10-17)**: a static field beyond src/dst/predicate is an ordinary
  declared field with an explicit access-method declaration
  (`#[ok_index(name { fields(…) })]`, the same vocabulary a document
  declares): hot/cold placement, a generated encoder AND decoder, and
  that face (`[field value][edge_id]`, counting family 0x1, `0x1001+n` in
  declaration order). No face is generated per field automatically — a
  field nobody scans is paid for by every record, and the node side
  already settled that (a face with no scanner lands with the read path
  that consumes it). This is the shape the bespoke encoder-only
  half-mechanism (`KvGraph::attrs`, `attr_faces`, `ATTR_SLOTS`, and with
  them the `EdgeEncode` payload derive) produced by hand, and it retires
  with this decision: one declaration, one codec, both directions, the
  widths living in the declaration instead of in every consumer. The
  edge's identity for those entries is its own key (`[edge_id u64]`, a
  `KeyEncode` newtype), so index entries tail with the edge id exactly as
  the attr faces did, and the same `index_entries` path maintains them.
  Filtering runs as an index scan over that face, decoded back to the
  primary, instead of fetching every edge and decoding; field values are
  the edge's own data (no NodeRef) and never touch endpoint widths. No
  composite faces (`predicate+field`, `field+endpoint`) are built by
  default —
  filter by the most selective face and converge edge_ids in memory, the
  same stance as document indexes; a composite face is added only when a
  proven access pattern demands it. Each declared index grows link/unlink
  by one entry — write amplification the declarer opts into.
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
  carry the filter, no new segment: (1) **node kind vocabulary** — a node
  collection that carries kind tokens declares its own pair (0x04/0x05, the
  same rule as the graph's predicates), so its kinds never share an id space
  with the props' field names (0x02/0x03) — and it is naturally separate from
  any graph's vocabulary (vocabularies are per-collection). (2) **node kind face** — one declared access method
  over the node collection's ns, `[kind_id][node pkey]`, maintained by
  the node's normal put path. Composite filtering
  (`(*)-[predicate]->(:node kind)`) scans the 0x09/0x0A face and the node
  kind face and intersects the id sets in memory — two cheap faces,
  one convergence (no triple-composite face is built for it).
- **Predicate index (0x06)**: all edges of one predicate — the
  `MATCH ()-[r:has]->()` entry point. `predicate_id` comes from the
  PREDICATE dictionary (0x04/0x05): write-time name → id, so every key
  segment stays fixed-width and sortable. The dictionary is also the
  LISTING surface: scanning 0x04 enumerates a graph's predicates exactly,
  with no walk over this face to infer them.
- **Traversal (0x07/0x08)**: all out-edges / in-edges of one node. The
  full NodeRef leads so the scan prefix matches; `edge_id` trails —
  parallel edges (two facts between the same nodes) each get their own
  entry, and the trailing id decodes back to the primary.
- **Predicate-filtered traversal (0x09/0x0A)**: the `(*)-[predicate]->(*)`
  face — the predicate leads because it is the filter dimension here, and
  it cannot be a trailing segment on this face.
- **Write protocol**: `link` = primary + one counting-family 0x1 entry per
  DECLARED index + 0x06 + 0x07 + 0x08 + 0x09 + 0x0A in one batch; the
  obj's dynamic entry (fixed face 0x01) rides the same batch when the
  caller wrote fields. **`unlink` deletes precisely (Update 2026-10-17)**:
  it decodes the edge body, reads the declared field values out of it, and
  deletes those exact entries — no prefix sweep over the attribute faces.
  The cost is a read before the delete, paid only where a decoder exists:
  the dynamic form needs its schema (its own declaration as data) to read
  the values back, and degrades to the prefix sweep where there is none.
  Symmetric with the Junction's double-write obligation, scaled to the
  face count.

### 4. One declaration, and a handle that declares nothing

The declaration IS the graph (Update 2026-10-17):

```text
#[derive(GraphEncode)]                    the graph's edge collection
#[ok_ns(37)]                              the graph's ns — the ONLY ns source
#[ok_nodes(FactNodes)]                    the member set (a compile-time enum)
#[ok_index(since { fields(since) })]      the edge's attribute faces, explicit (§3)
pub struct FactEdge { … }                 the edge's payload = its attributes
```

One derive, one declaration: the macro emits the whole `GraphEncode` impl
(the payload codec, the declared faces, the ns) with `type Nodes` bound to the
enum the attribute names. A hand-written `impl GraphEncode for FactEdge { type
Nodes = … }` alongside the derive would be a second impl of the same trait —
Rust has no associated-type defaults to leave the slot open.

plus the member set as a TYPE — an enum over the node collections, so
"which nodes" is a compile-time fact in code rather than an annotation on
a payload:

```text
pub enum FactNodes { Entities, Users, Facts }   // the variants are COLLECTIONS
impl NodeSet for FactNodes {
    const NS_VALUES: &'static [u16] = &[36, 35, 37];   // one ns per variant
}
```

The member unit is a COLLECTION (its ns), not a document type — the ref
carries exactly that number, so membership is a check at the ns level
(§2). A static declaration may let a variant name the key type of the
collection it stands for, for typed endpoint decoding; the set the graph
declares is a set of collections.

One graph = one edge declaration (shared by every edge it holds) + one
`Nodes` enum + one ns. What separates edges from each other is the
predicate (runtime data, dictionary-allocated); what separates nodes from
each other is the enum variant (compile-time). Both are derived from the
declaration — no hand-written tables, no assembly-time declaration.

**Assembly is a runtime handle, nothing more.** `Graph<S, E: GraphEncode>`
binds an engine INSTANCE (the same declaration mounts on a mock, on
Fjall, or on a remote store — ADR-0025's one keyspace) and the
per-instance state that cannot live in a type: the dictionary cache (the
single-writer discipline over predicate ids) and the edge_id allocation
position. It carries no ns parameter (the ns comes from `E::NS_PREFIX`),
no node list and no schema; after this decision the assembly point has
zero declaration content.

- **Fixed ontology**: the declaration above is written in Rust; the node
  collections are ordinary document collections whose rows are ordinary
  documents.
- **Fully dynamic** (knowledge graphs): the SAME declaration carried as
  data — a `CollectionSchema` value (exported from the static side via
  `CollectionSchema::of`, ADR-0025) plus an okm-dynamic handle. An empty
  declaration means identity only, with the payload entirely in the
  dynamic segment (declaring `props: DynamicValue` would put it in slot 0
  — the static regime, not what a dynamic graph wants). Predicates and
  free attributes are dictionary data either way; endpoints are
  self-describing refs (§2); traversal faces are built through the dynamic
  layer's `AccessMethod` construction. The handle takes the ns and the
  schema the declaration yielded as a PAIR, never a bare ns invented at the
  call site; where the ns is still a separate argument (today's
  `DynamicCollection::new(store, ns, schema, …)` — `CollectionSchema`
  itself carries no ns), the export side provides the pair and assembly
  REJECTS a disagreement instead of overriding it.

Both forms share the same wire layout and the same slot semantics, and
they produce identical bytes for the same declaration: mode is a property
of the WRITER, not the data (ADR-0025).

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
carries over. Semantically both are relations; the graph's connection
differs in carrying ATTRIBUTES (its document half) and a DIRECTION, and
in existing independently of the endpoints it names.

## Consequences

- The declaration is a GRAPH declaration (`GraphEncode`): one ns, the
  edge's own payload (the document codec — no second payload encoding),
  the attribute faces it declares explicitly, and a `Nodes` enum.
  `EdgeEncode` and the encoder-only attribute half (`attrs()` /
  `attr_faces()` / `ATTR_SLOTS`) retire with it. The graph's own faces and
  the six-entry write protocol live in the `Graph<S, E>` handle, which
  declares nothing — it is an engine instance plus per-instance state.
- The member node set is compile-time (a `Nodes` enum): no slot, no
  registry, no runtime `register()`. Adding a node collection costs a code
  change — the price of the closed ns dictionary (ADR-0002), and the
  reason a knowledge graph keeps runtime TYPES inside declared collections
  instead of creating collections.
- The graph's shape follows PGQ rather than the draft's reading of it:
  vertex tables are declared (the `Nodes` enum), one table may serve as
  both a vertex and an edge, and per-pair edge types are not required —
  one declaration serves every pair.
- `unlink` reads the edge body and deletes its declared-field entries
  exactly; the dynamic form needs its schema for that, and otherwise falls
  back to the prefix sweep.
- Endpoint refs are self-describing (`[ns 2B][len varint][pkey]`), so
  there is no cross-collection width registry; the endpoint TYPE
  restriction is the declared `Nodes` enum, applied as a check (§2).
- No change to ADR-0016's slot allocation: 0x06–0x0A are the graph's
  fixed faces inside the Edge collection's ns (ADR-0030).
- No consumer exists yet; the design is recorded before
  `#[ok_relation]` work begins, since both share the write-path diff
  machinery. (Update 2026-09-20: the `#[ok_relation]` field-position
  form was subsequently REJECTED — ADR-0015 Future work §B; the
  junction's imperative link/unlink pairing stands, and this ADR's
  put-path `link_into` batch surface is unaffected.)
