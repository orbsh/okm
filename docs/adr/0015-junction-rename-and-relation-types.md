# ADR-0015: Junction rename and the two relation types

## Status

Accepted (2026-09-17). Rename + ns derivation decided; graph Edge and
field-position auto-sync recorded as future work.

## Context

The existing `EdgeEncode` / `Edge<S, E>` type was misnamed. Analysis of
what it actually does:

- It stores a **bidirectionally materialized relation entry** — FWD and
  REV keys, both empty-valued, both always written. There is no
  direction in the graph sense (a directed edge carries asymmetric
  semantics; FWD/REV here are the same fact seen from two sides, a
  denormalization for two-way O(1) prefix scans).
- The entry value is **empty** — it cannot carry relation attributes
  (join time, role, permission), the classic need of a SQL junction
  table.
- The ns is **manually numbered** (`#[ok_ns(4)]`) — the exact burden
  ADR-0005 removed for indexes (per-index manual numbering + hole
  bookkeeping), reintroduced for relations. And manual numbering admits
  a failure mode derivation cannot: two relations colliding on ns.

The model it actually implements is the **SQL junction table** (many-
to-many middle table with both endpoint identities in the key),
materialized twice for bidirectional scanning. In graph terminology it
is not an edge at all.

Meanwhile the embedded-document work (`Ref<D, K>` / `Refs<D, K>`) put
one-to-many relations into field position as typed key references, and
MODELING's one-to-many section already described the physical form
(foreign key + list, children normalized in their own ns). That leaves
the taxonomy with a naming hole: the many-to-many counterpart is
called "Edge", which reads as graph vocabulary the project does not
implement.

## Decision

### 1. Rename: Edge -> Junction

`EdgeEncode` -> `JunctionEncode`, `Edge<S, E>` -> `Junction<S, E>`.
The name says what it is: a junction table (SQL's term for the
many-to-many middle table). `Relation` was rejected — in relational-
algebra vocabulary it means "any table" and collides with
Collection. `forward_key` / `reverse_key` keep their names: the FWD/
REV double materialization is the mechanism's essence, the names are
accurate.

### 2. Junction ns derives from the endpoints

The manual `#[ok_ns(N)]` on a junction declaration is removed. The ns
is **derived at compile time** from the two endpoint key types' own ns
values (deterministic combination, collision-checked at compile time
across the crate's declarations). Two reasons:

- Identity: a junction is fully determined by its endpoint pair —
  `Org + User` IS the relation's identity; a separate human-assigned
  number adds a bookkeeping fact with zero information.
- The ADR-0005 argument applies verbatim: manual numbering is a hole-
  bookkeeping burden, and unlike indexes, junction ns collisions are a
  real hazard (two human-assigned numbers can silently overlap).

Endpoint ns values are compile-time constants (each key type declares
one), so derivation is a const expression — no runtime cost, no
dictionary, no ADR-0002 KV-bootstrap deadlock.

### 3. Taxonomy: two relation types, two carriers

The one-to-many section of MODELING already described the physical
form; the embedded work gave it a field-level carrier. The full
picture:

- **One-to-many**: `Refs<D, K>` in field position — foreign-key set,
  children normalized in their own ns, single-direction (the child's
  key carries the parent identity; reverse lookup is a prefix scan on
  it). Parasitic on the parent document; the child does not point back.
- **Many-to-many**: `Junction` — first-class relation data, both
  endpoint identities in the entry key, double-materialized for
  two-directional O(1) scans. Not a field: the relation exists
  independently of either endpoint's document.

The dividing line is the same one MODELING already draws for edge vs
index: **can the relation be found from both sides without scanning?**
Junction says yes (two entries); field-position Refs says no (one
direction only, reverse = full scan of the child ns — which is fine
when the child's key carries the parent identity, the one-to-many
case).

### 4. The plural-modeling taxonomy

MODELING needs a complete plural-types guide. Two axes: does the
element have identity, and homogeneous vs heterogeneous.

- **One-to-many (elements have identity, one direction)**: `Refs<D, K>`
  — above.
- **Many-to-many (both sides have identity, bidirectional)**:
  `Junction` — above.
- **Vector (homogeneous list, used as a whole)**: elements of one
  type, read and written as a unit, possibly multi-dimensional (a
  header carries shape and type; one dimension is just a list; many
  is a tensor). Canonical use: embedding vectors (order and dimension
  expressed), numeric sequences. Stored in slot 0's dynamic part, in
  the same class as strings (variable-length TLV frames); dynamic
  elements use LV format. `DynamicValue::Array` is its heterogeneous
  counterpart — more general, more dynamic, per-element type tags,
  slightly higher overhead.
- **Array (heterogeneous list)**: `DynamicValue::Array` — mixed
  element types, recursive dynamic-segment frames.
- **Set (deduplicated elements)**: no carrier yet. The candidate
  implementation is an inverted index (element value -> document
  primary keys holding it) — mechanically identical to
  `#[ok_index]`'s multi-value function index (a func returning
  `Vec<V>` fans out). Whether a dedicated type (`#[ok_set]`-like) or
  a documented "express it with a function index" is decided when a
  real consumer appears.

The dividing-line criterion is unchanged: **elements with identity
(own indexes, sharing, independent updates) -> Ref/Junction; elements
as pure values (read/written as a whole, no independent lifecycle) ->
vector/array**.

## Consequences

- `EdgeEncode` / `Edge` rename touches the derive, core, tests, and
  all four documents (en/zh).
- Junction declarations drop `#[ok_ns(N)]` — a breaking change to
  downstream users (aura mq.rs has no junction declarations; k10r has
  none; the migration cost is currently zero, which is why the rename
  happens now, pre-crates.io).
- The manual-ns removal is also the hook for a future
  `#[ok_relation(...)]` field-position declaration: a `Refs` field in
  a document could name its junction, and `put` would diff the field
  against stored junction entries (auto link/unlink). Recorded as
  future work — it trades RMW for auto-sync and needs a consistency
  story before it lands.

## Future work (explicitly out of scope here)

### A. Graph Edge — a genuinely different type

The graph edge — directed, attribute-carrying, traversable — is a
different data structure and gets a different name (the freed-up
`Edge`). Attributes are dynamic (`DynamicValue`-shaped, no schema pre-
definition, no field-name compression — dynamic frames already carry
names inline). The ns-of-an-edge-name problem (ns is a fixed-width
compile-time number; edge names like "has"/"belongs" are strings) is
solved by hashing the name literal at compile time from
`#[ok_edge("has")]`, with compile-time collision detection — the same
mechanism decision 2 uses for junctions. No KV dictionary (ADR-0002
deadlock avoided), no compression. Not designed further here; no
consumer exists.

### C. Vector types — materialization

The concrete `Vector<D>` design (fixed-width elements BE-serialized
directly; dynamic elements as LV frames; multi-dimensional shape
headers) is worked out when real consumers appear — vector retrieval
(okm-vector) and embedding storage. This ADR only locks the taxonomy
position: homogeneous, whole-value read/write, slot 0 dynamic part,
same class as strings.

### B. Field-position relation declaration + auto-sync

`#[ok_relation(JunctionType)]` on a `Refs` field, with put-time diff
against stored junction entries. Trades RMW for declarative sync;
requires the consistency analysis (two endpoints' documents both
writing the same junction) before design.

## References

- MODELING "Many-to-many relations" — the junction-as-middle-table
  description, `ok_head` truncation discipline, the two-directional
  obligation argument.
- ADR-0005 (index ns derivation) — the manual-numbering burden
  argument, applied verbatim here.
- ADR-0012 (embedded documents) — the one-to-many field-level carrier.
