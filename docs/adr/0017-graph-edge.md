# ADR-0017: Graph Edge — the third relation carrier

Date: 2026-09-19
Status: Accepted (2026-09-20); **the identity clause is superseded by ADR-0067** (2026-11-14) — an edge has no number, its identity is the tuple `(src, predicate, dst)`, and the decision below is stated in that form. The declaration is a GRAPH declaration (`GraphEncode`): one ns, the edge's own payload (the document codec), the attribute faces it declares explicitly, and a compile-time node-type enum. `#[ok_nodes]` lists the member TABLES by type (`#[ok_nodes(EntityRow, MemberRow)]`) and the derive writes the member-set enum plus its `NodeSet` impl, each ns read off the table's own `#[ok_ns]`. The predicate vocabulary sits at 0x04/0x05, the graph's faces at 0x06–0x09; `Graph<S, E>` is a runtime handle that declares nothing, `Graph::new(store)` binds an engine instance, and a delete reads the edge row (0x06) and deletes its declared-face entries precisely. okm-dynamic's handle takes the ns + schema pair (`CollectionSchema::ns_prefix`, `members`).
Related: ADR-0015 (relation taxonomy), ADR-0016 (4-byte head, segment-numbered slots), ADR-0002 (namespace vocabulary)

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
  vocabulary (0x04/0x05, §3). The node COLLECTIONS are the opposite case. The ns
  vocabulary is compile-time closed (ADR-0002, "the vocabulary is code"),
  so a knowledge graph's nodes are rows of DECLARED collections (a facts
  collection, an entities collection, a users collection) and never
  collections created on the fly; a node "type" inside one collection is a
  payload field. Same for node kinds in the fully dynamic form.
- **Parallel facts and attributes** — two "has" facts between the same
  nodes are two assertions, and edges carry dynamic attributes. Neither
  carrier supports this: a Junction's identity IS the endpoint pair and it has
  no per-edge payload, so a second fact about the same pair has nowhere to
  live.
- **Independent existence** — edges outlive or precede their endpoint
  documents' presence in query scope; the edge set is a first-class
  collection, not a derived view.

`Refs` cannot express this (fixed child type, field position, one
direction, no attributes). `Junction` cannot either (fixed endpoint
types, no edge identity, no attributes). The graph edge is the **third
relation carrier**.

## Decision

### 1. The edge SET is a first-class collection, one ns per graph

A graph's edges live in their own Edge collection — one ns for the
edge set of one graph, allocated through the normal ns vocabulary
(ADR-0002, "the vocabulary is code"), symmetric with any document
collection. The collection is first-class; an edge inside it is a row whose
identity is nothing but its tuple (ADR-0067), so there is no registry of
edges to keep, no number to allocate and nothing to preserve across a
restore. No new namespace mechanism: not a per-predicate ns
(predicates are runtime data in the dynamic form), not a global edge pool (a graph
isolates at the ns layer), not residency (edges exist independently of
endpoint documents).

The same ns hosts every entry type of the edge set, dispatched by the
fixed-face slots of ADR-0030.

**The member node set is a declaration, not a registry.** A graph declares
which collections may appear as its nodes
— a compile-time fact, in code, next to the ns vocabulary it belongs to
(a `Node` enum; §4). No slot holds it: the set adds no runtime
information (every candidate collection is already a compile-time ns),
and a metadata entry would only add "who writes it, when, and who wins a
conflict". Runtime creation of node collections is rejected with it: a
knowledge graph's shapes are declared collections whose rows carry a type
field — one ns holding many runtime types — which is strictly cheaper
than an ns per emergent type, and the only reading that keeps the ns
vocabulary closed (PGQ's own rule: tables are declared, not conjured).

### 2. Endpoint references: ns in the ref, width from the declaration

Endpoints are **not compile-time typed**. A reference is
`[ns 2B BE][pkey]` — any member collection's document, no length in front.
The ns acts as the type marker: decoding a reference routes by ns to the
owning collection, so node type is unrestricted.

The pkey width is **not wire data**: it comes from the graph's declaration.
`#[ok_nodes(..)]` is a closed, compile-time known set of member tables and
`KeyEncode` emits one fixed byte count per declaration, so the graph knows every
member's key width up front. A reference therefore pays nothing for its width,
`[src][pred][dst]` parses by width alone, ordering is value order, and a hop
costs zero extra bytes (ADR-0067). ns knowledge still lives in the ref itself:
no cross-collection width registry exists.

The declaration's member set is thus BOTH the width source and the CHECK. It is
applied at write time (an endpoint whose ns is not a member is
`Error::ForeignEndpoint`; a pkey of the wrong width is `Error::Corrupt`) and at
decode (ns → variant; out-of-range = error, never silent). The corollary is a
constraint: a table that wants to appear in a graph must have a fixed-width key.
Variable-width fields may still be indexed inside a collection; they are
refused as member identities. Where a variable key genuinely must participate,
the honest options are to pad it to a declared width or to accept a hash
identity with its collision check — not to smuggle a length into every key.

### 3. Slot allocation (fixed faces and counting families, within the Edge ns)

```text
0x00  primary                (an edge writes no row here — 0x06 IS the edge)
0x01  dynamic segment        [src][pred][dst]   (the edge's open attributes)
0x02/0x03  field-name vocabulary  (the payload's own names ↔ ids)
0x04/0x05  predicate vocabulary   [predicate name ↔ predicate_id]
0x06  out / the edge itself  [src NodeRef][pred][dst NodeRef]  → the edge payload
0x07  in-edges               [dst NodeRef][pred][src NodeRef]  → empty
0x08  predicate+out          [pred][src NodeRef][dst NodeRef]  → empty
0x09  predicate+in           [pred][dst NodeRef][src NodeRef]  → empty
```
(Every face carries BOTH endpoints AND the predicate (ADR-0041, ADR-0067):
`(src, predicate, dst)` is addressable on 0x06, `(predicate, src, dst)` on 0x08.
A ref in one of these keys is `[ns 2B][pkey]` with no length — the width is the
declaration's (§2). The graph's faces are fixed-face
numbers — one byte each, ADR-0030; the declared attribute faces below are
ordinary declared indexes, counting family 0x3nnn. Rows are grouped by ROLE — the two vocabularies first,
then the faces that read their ids: a vocabulary leads the faces it serves.
The NUMBERS stay the mechanism.)

**The predicate has its OWN vocabulary.**
`[ns][0x04][predicate_id] → name` and `[ns][0x05][predicate name] →
predicate_id` — the same bidirectional shape as 0x02/0x03 (id→name leads,
the name carries the bytes), a different vocabulary. Listing a graph's
predicates is then ONE prefix scan of 0x04: an exact list, never
reverse-derived by walking the traversal faces. A shared vocabulary with the
payload's field names would leave
the names indistinguishable and make "which predicates does this graph
have" answerable only by inferring them from the traversal faces — and a
listing must not be an inference. Splitting one vocabulary by id band has the same flaw: a band is a
convention that breaks silently on the first out-of-band allocation,
while a second face pair is structural. Cost: one fixed-face pair
(0x04/0x05, ADR-0030) and two vocabularies
in the edge collection. The field-name vocabulary (0x02/0x03) keeps its
document-side meaning for the payload's own names.

**The pair is the graph's own PREDICATE VOCABULARY.** The
numbers are not graph-specific: the pair is a collection's own token vocabulary
BEYOND field names — a graph's PREDICATES here, and a NODE COLLECTION's kind
tokens where it declares one. A document collection with kinds keeps field names in 0x02/0x03
and its kinds here, so two allocators never share one id space; the kind field
itself stays an ordinary payload field with a declared index. Same pair, same
rule, whichever collection needs it.

**The edge's type is its PREDICATE, not its "kind".**
"Kind" carries no more information than "type" and, in prose, cannot be told
apart from a NODE's kind (a payload field). The edge's type is its
PREDICATE: `predicate_id` on the wire, the predicate vocabulary at
0x04/0x05, the predicate faces at 0x08/0x09. An edge has no number of its own (ADR-0067), so the head is exactly
`[src][pred][dst]` — the predicate is a key dimension because it is the edge's
type, and the identity is the whole tuple.

**Slot layout versus ordinary documents** — stated
explicitly, though not identical-by-construction:

- **Node collection = the standard document layout, unchanged.** The
  node kind face is NOT a new kind of slot: it is an ordinary access
  method in counting family 0x1 (a declared `#[ok_index]` in the
  fixed-ontology form, a caller-allocated `AccessMethod` slot in the
  dynamic form). Node kinds allocate through the collection's own 0x04/0x05
  pair (the same rule as a graph's predicates), kept apart from the
  field-name vocabulary (0x02/0x03) its props use. Zero layout difference
  from any document collection; "one field is the kind" is a modeling
  convention, not a layout fact.
- **Edge collection = the entry types above, a different collection
  shape written in the SAME slot vocabulary.** 0x01/0x02/0x03 keep their
  document-side meanings (dynamic segment, vocabulary) and 0x00 is unused (the
  tuple identifies an edge, so there is no primary row);
  0x06–0x09 are the graph's own faces and 0x04/0x05 its own predicate
  vocabulary, all inside the fixed-face range — no new KIND of slot is
  introduced. The value side
  is not a second encoding either: the edge's payload IS an obj
  (ADR-0012). What makes this collection an Edge rather than a
  document collection is the graph's faces and the endpoints whose width the
  declaration fixes, not a payload format of its own.

- **The edge row (0x06)**: `[ns][0x06][src][pred][dst] -> payload`. The tuple
  is the key and the payload is the value, where `pred` is the PREDICATE (the
  edge's type) as 2 bytes from the graph's own predicate vocabulary (0x04/0x05),
  read back by name through it. The value is an ordinary obj payload
  (ADR-0012): `[hot segment][cold TLV]`, the encoding every document row uses.
  This is the row that traversal already had to read — with no edge number,
  the index entry and the row are one entry.
- **Declared attribute fields — declared EXPLICITLY**: a static field beyond src/dst/predicate is an ordinary
  declared field with an explicit access-method declaration
  (`#[ok_index(name { fields(…) })]`, the same vocabulary a document
  declares): hot/cold placement, a generated encoder AND decoder, and
  that face (`[field value][src][pred][dst]`, counting family 0x3nnn in
  declaration order). No face is generated per field automatically — a
  field nobody scans is paid for by every record, and the node side
  already settled that (a face with no scanner lands with the read path
  that consumes it). One declaration, one codec, both directions, the
  widths living in the declaration instead of in every consumer. The
  edge's identity for those entries is the tuple itself, so an index entry
  tails with `[src][pred][dst]` — the same bytes as the edge's own key — and the
  same `index_entries` path maintains them.
  Filtering runs as an index scan over that face, whose tail is the tuple,
  instead of fetching every edge and decoding; field values are
  the edge's own data (no NodeRef) and never touch endpoint widths. No
  composite faces (`predicate+field`, `field+endpoint`) are built by
  default —
  filter by the most selective face and converge tuples in memory, the
  same stance as document indexes; a composite face is added only when a
  proven access pattern demands it. Each declared index grows a write and a
  delete by one entry — write amplification the declarer opts into.
- **Dynamic attributes (fixed face 0x01)**: the edge's open attributes are the
  obj's dynamic segment — ONE entry per edge, `[ns][0x01][src][pred][dst]`, nTLV
  frames whose field names are vocabulary numbers (ADR-0012) — i.e.
  the same entry, codec and `replace_into`/`attrs` surface any
  document row has. The graph assembly owns that entry (callers stop
  hand-rolling frames). An open attribute a query later needs is
  reached through a path index over its declared path (ADR-0029 — one
  entry per resolved value, the value types with a storage-form
  encoding), never by widening the key. The fully dynamic form needs
  no change: its declaration is empty and its free attributes ride the
  same obj shape.
- **Node-side kind filtering**: filtering by node
  kind / node attributes is a basic graph-query need, answered
  per form. In the fixed-ontology form a node IS an ordinary document
  collection: node "type" = the collection itself (the ns is the type
  marker), and attribute filtering rides the collection's own
  `#[ok_index]` declarations — nothing new. In the fully dynamic form
  node kinds are dynamic-segment data, so two existing mechanisms
  carry the filter, no new segment: (1) **node predicate vocabulary** — a node
  collection that carries kind tokens declares its own pair (0x04/0x05, the
  same rule as the graph's predicates), so its kinds never share an id space
  with the props' field names (0x02/0x03) — and it is naturally separate from
  any graph's vocabulary (vocabularies are per-collection). (2) **node kind face** — one declared access method
  over the node collection's ns, `[kind_id][node pkey]`, maintained by
  the node's normal put path. Composite filtering
  (`(*)-[predicate]->(:node kind)`) scans the 0x08/0x09 face and the node
  kind face and intersects the id sets in memory — two cheap faces,
  one convergence (no triple-composite face is built for it).
- **No separate predicate index**: `[predicate_id]` is a bare prefix of 0x08,
  so scanning it returns every edge of one predicate — the
  `MATCH ()-[r:has]->()` entry point — with the tuple in the trailing position.
  `predicate_id` comes from the PREDICATE vocabulary (0x04/0x05): write-time
  name → id, so every key segment stays fixed-width and sortable. The
  vocabulary is also the LISTING surface: scanning 0x04 enumerates a graph's
  predicates exactly, with no walk over the traversal faces to infer them.
- **Traversal (0x06/0x07)**: all out-edges / in-edges of one node. The
  traversed endpoint's NodeRef leads so the scan prefix matches, the predicate
  sits in the middle and the far endpoint follows — so a hop knows where it goes
  AND what the edge is without reading a row, and `(src, dst)` is addressable
  on 0x06.
- **Predicate-filtered traversal (0x08/0x09)**: the `(*)-[predicate]->(*)`
  face — the predicate leads because it is the filter dimension here, and
  it cannot be a trailing segment on this face.
- **Write protocol**: one write = 0x06 (payload included) + 0x07 + 0x08 +
  0x09 + one counting-family 0x3nnn entry per DECLARED index, in one batch;
  the obj's dynamic entry (fixed face 0x01) rides the same batch when the
  caller wrote fields. Back to back, that is one entry per face and no primary
  row. **A delete deletes precisely**: it reads the 0x06 row, re-derives the
  declared-face entries from the payload it holds, and deletes those exact
  entries — no prefix sweep over the attribute faces. The cost is a read before
  the delete, paid only where a decoder exists: the dynamic form needs its
  schema (its own declaration as data) to read the values back, and degrades to
  the prefix sweep where there is none. Symmetric with a relation table's
  double-write obligation, scaled to the face count.

### 4. One declaration, and a handle that declares nothing

The declaration IS the graph:

```text
#[derive(GraphEncode)]                    the graph's edge collection
#[ok_ns(37)]                              the graph's ns — the ONLY ns source
#[ok_nodes(NodeRow, OtherNodeRow)]        the member set: the node TABLES, by type
#[ok_index(since { fields(since) })]      the edge's attribute faces, explicit (§3)
pub struct Facts { … }                    the graph's name; the edge payload = its attributes
```

One derive, one declaration: the macro emits the whole `GraphEncode` impl
(the payload codec, the declared faces, the ns), the member-set enum, and its
`NodeSet` impl, with `type Nodes` bound to that enum. A hand-written
`impl GraphEncode for Facts { type Nodes = … }` alongside the derive would
be a second impl of the same trait — Rust has no associated-type defaults to
leave the slot open.

plus the member set as a TYPE — an enum whose variants each name one node
collection, so "which nodes" is a compile-time fact in code rather than an
annotation on a payload. The attribute lists TABLES (a table is named in code
by its document type) and the derive writes the enum, so a member's ns is
never typed twice:

```text
#[ok_nodes(EntityRow, MemberRow)]

// generated:
pub enum FactsNode { EntityRow, MemberRow }   // one variant = one COLLECTION (one ns)
impl NodeSet for FactsNode {
    const NS_VALUES: &'static [u16] = &[36, 42];   // each table's own #[ok_ns]
}
```

The member unit is a COLLECTION (its ns), not a document type — the ref
carries exactly that number, so membership is a check at the ns level
(§2). The document type is only how the table is NAMED at the declaration
site; the set the graph declares is a set of collections, and the generated
variants carry nothing (no accessor, no key type — the ns is the whole
content). The naming splits accordingly: the set role belongs to `NodeSet`
(`NS_VALUES` is every member's ns) while the enum itself is SINGULAR,
`<Graph>Node` — a type name names one value, a value names one collection, and
a plural name would claim something its value domain never satisfies.

One graph = one edge declaration (shared by every edge it holds) + one
generated `Node` enum (graph name + `Node`) + one ns. What separates edges
from each other is the predicate (runtime data, vocabulary-allocated); what
separates nodes from each other is the enum variant (compile-time). Both are
derived from the declaration — no hand-written tables, no assembly-time
declaration.

**Assembly is a runtime handle, nothing more.** `Graph<S, E: GraphEncode>`
binds an engine INSTANCE (the same declaration mounts on a mock, on
Fjall, or on a remote store — ADR-0025's one keyspace) and the
per-instance state that cannot live in a type: the vocabulary cache (the
single-writer discipline over predicate ids). There is no allocation position,
because there is no identity to allocate. It carries no ns parameter (the ns comes from `E::NS_PREFIX`),
no node list and no schema: the assembly point has zero declaration content.

- **Fixed ontology**: the declaration above is written in Rust; the node
  collections are ordinary document collections whose rows are ordinary
  documents.
- **Fully dynamic** (knowledge graphs): the SAME declaration carried as
  data — a `CollectionSchema` value (exported from the static side via
  `CollectionSchema::of`, ADR-0025) plus an okm-dynamic handle. An empty
  declaration means identity only, with the payload entirely in the
  dynamic segment (declaring `props: Variant` would put it in slot 0
  — the static regime, not what a dynamic graph wants). Predicates and
  free attributes are vocabulary data either way; endpoints are
  refs whose width the declaration fixes (§2); traversal faces are built through the dynamic
  layer's `AccessMethod` construction. The handle takes the ns and the
  schema the declaration yielded as a PAIR, never a bare ns invented at the
  call site; where the ns is still a separate argument (today's
  `DynamicCollection::new(store, ns, schema, …)`), the export side provides
  the pair and assembly REJECTS a disagreement instead of overriding it.
  `CollectionSchema` carries `ns_prefix` (the
  declaration's own `[ns 2B]`, so the pair is checkable) and `members` (the
  member set, exported by `CollectionSchema::of_graph::<E>()` from
  `E::Nodes::NS_VALUES`) — the one half of a graph declaration a payload
  layout cannot express, and the half a dynamic writer enforces against the
  refs' ns exactly as the static form does through `E::Nodes`.

Both forms share the same wire layout and the same slot semantics, and
they produce identical bytes for the same declaration: mode is a property
of the WRITER, not the data (ADR-0025).

### 5. Position in the taxonomy (ADR-0015 §3)

Three relation carriers, divided by two axes — endpoint typing and
edge independence:

```text
                    endpoint type fixed          endpoint type open
one direction       Refs<D, K>                   —
both directions     Junction                     Graph Edge
```

Graph Edge differs from Junction structurally in exactly two ways,
both forced by the open-endpoint requirement: endpoints are references
(`[ns 2B][pkey]`) whose width the declaration fixes instead of
compile-time typed keys, and the identity is the tuple
`(src, predicate, dst)` instead of the pair, so parallel facts are told apart by
their predicate and multiplicity inside one predicate lives in the payload
(ADR-0067). Everything else — the double-write obligation, the entry-face
design, the ns discipline — carries over. Semantically both are relations; the graph's connection
differs in carrying ATTRIBUTES (its document half) and a DIRECTION, and
in existing independently of the endpoints it names.

## Consequences

- The declaration is a GRAPH declaration (`GraphEncode`): one ns, the
  edge's own payload (the document codec — no second payload encoding),
  the attribute faces it declares explicitly, and a `Node` enum. The
  graph's own faces and the write protocol live in the `Graph<S, E>` handle,
  which declares nothing — it is an engine instance plus per-instance state.
- The member node set is compile-time (a `Node` enum): no slot, no
  registry, no runtime `register()`. Adding a node collection costs a code
  change — the price of the closed ns vocabulary (ADR-0002), and the
  reason a knowledge graph keeps runtime TYPES inside declared collections
  instead of creating collections.
- The graph's shape follows PGQ: vertex tables are declared (the `Node`
  enum), one table may serve as
  both a vertex and an edge, and per-pair edge types are not required —
  one declaration serves every pair.
- `unlink` reads the edge body and deletes its declared-field entries
  exactly; the dynamic form needs its schema for that, and otherwise falls
  back to the prefix sweep.
- Endpoint refs are `[ns 2B][pkey]` with the width taken from the declared
  member table, so there is no cross-collection width registry and no length in
  a key; the endpoint TYPE restriction is the declared `Node` enum, applied as a
  check at write time (§2).
- No change to ADR-0016's slot allocation: 0x06–0x09 are the graph's
  fixed faces inside the Edge collection's ns (ADR-0030).
- No consumer exists yet.
