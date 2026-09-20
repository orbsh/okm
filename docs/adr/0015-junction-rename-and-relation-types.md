# ADR-0015: Junction rename and the two relation types

## Status

Accepted (2026-09-17). Rename and two-ns residency stand; the 2-byte slot
**segment table was superseded by ADR-0016** (4-bit segment + 12-bit counter,
junction = segment 0x3) — see there for the current allocation
segments decided; the ns-derivation scheme was rejected; graph Edge landed
as ADR-0017; the field-position auto-sync (`#[ok_relation]`) was REJECTED
after analysis on 2026-09-20 — the imperative link/unlink pairing stands
as the junction's interface (see Future work §B).

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

### 2. Two-ns residency: each endpoint's ns hosts one direction

The manual `#[ok_ns(N)]` on a junction declaration is removed. A
junction's two physical entries live **in the two endpoint documents'
own ns** — one entry per ns, no third ns:

```
ns_org  [ns_org ][slot 0x3nnn?][org·identity][user·identity]   // from A (ADR-0016:
ns_user [ns_user][slot 0x3nnn?][user·identity][org·identity]   // one entry per endpoint ns,
                                                               // local identity leads, dir bit in n)
```

A junction is **unidirectional per entry**: each entry answers exactly
one query direction ("all users of this org" from ns_org; "all orgs of
this user" from ns_user). The two entries are the same relation fact
written twice — the double materialization that makes both directions
O(1). No derived ns, no hash, no third namespace: the relation lives
with its endpoints, one slot in each.

Why not a derived independent ns (the earlier draft): a compile-time
hash of the two endpoint ns values was rejected — not for collision
risk but for **indeterminacy**. A hashed ns is an opaque number whose
relation to the endpoints is invisible, and the project's identity
discipline (keys declared, namespaces declared, nothing manufactured)
extends to relation storage: the entries live where the endpoints
live.

**Isolation from ordinary indexes**: the slot is widened and segmented
(ADR-0016): **2 bytes (u16, big-endian)** — a 4-bit segment number plus a
12-bit in-segment counter. Ordinary indexes and junctions differ in the
segment alone (`0x1` vs `0x3`), a structural distinction rather than a
convention. The full segment table, the document-self layout, and the
consequences (head growth, counter independence, hex-lock rewrite) live
in ADR-0016 — the allocation proposed here (byte-wide segments,
junction at `0x80`/`0x81`) was superseded during review: direction
moved out of the slot entirely (one entry per endpoint ns, above), so
the junction needs a single segment, and segments being a 16-entry
enumeration freed the remaining bits for the counter.

### 3. Taxonomy: two relation types, two carriers

The one-to-many section of MODELING already described the physical
form; the embedded work gave it a field-level carrier. The full
picture:

- **One-to-many**: `Refs<D, K>` in field position — foreign-key set,
  children normalized in their own ns, single-direction (the child's
  key carries the parent identity; reverse lookup is a prefix scan on
  it). Parasitic on the parent document; the child does not point back.
- **Many-to-many**: `Junction` — first-class relation fact, both
  endpoint identities in the entry key, double-materialized for
  two-directional O(1) scans. Not a field: the relation fact exists
  independently of either endpoint's document; physically it is
  **two-ns residency** — each endpoint ns hosts one direction (see
  decision 2).

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
  type, read and written as a unit. Storage: a variable-length cold
  TLV frame like strings — payload = [count u32 BE] + elements;
  homogeneous fixed-width elements are bare V (zero per-element
  overhead), dynamic-width elements carry per-element LV. Length is
  data, not schema; `#[ok_len(N)]` is an encode-time application
  contract (exported to the dynamic reader as `expect_len`; decode
  never checks — bypassing the decoder is the reader's own problem).
  Canonical use: embedding vectors (okm-vector), numeric sequences.
  On get_document it lifts to DynVal::Array — the dynamic layer has
  no Vector type. Multi-dim shape is application-layer. Elements are
  pure values — NOT a relation carrier.
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
  story before it lands. (Update 2026-09-20: rejected — see Future
  work §B.)
- The junction is unidirectional per entry: the "two slots per
  junction" of the old design (14/15 in one ns) becomes "one slot in
  each endpoint's ns" — the relation fact is written once per
  endpoint ns, each hosting the direction that ns answers.

## Future work (explicitly out of scope here)

### A. Graph Edge — a genuinely different type

The graph edge — directed, attribute-carrying, traversable — is a
different data structure and gets a different name (the freed-up
`Edge`). Attributes are dynamic (`DynamicValue`-shaped, no schema pre-
definition, no field-name compression — dynamic frames already carry
names inline). The ns-of-an-edge-name problem (ns is a compile-time
declared number; edge names like "has"/"belongs" are strings): the
earlier draft's compile-time hash from `#[ok_edge("has")]` was
rejected along with decision 2's derivation (indeterminacy). The graph
Edge's namespace mechanism (allocation within an independent ns
segment, edge name in key or payload) is left to design — unlike the
junction's two-ns residency, a graph edge typically exists
independently of any endpoint document, so residency may not apply.
Not designed further here; no consumer exists.

### C. Vector types — materialization

Scheduled (PLAN P3.5, following the 4-byte head): declared form
`Vector<T, N>` — element type and dimension in the type, fixed-width
elements BE-serialized directly; dynamic elements as LV frames;
multi-dimensional shape headers. okm-vector's `encode_f32s` migrates
onto it. This ADR locks the taxonomy position: homogeneous, whole-
value read/write, slot 0 dynamic part, same class as strings.

### B. Field-position relation declaration + auto-sync — REJECTED (2026-09-20)

Update: the `#[ok_relation(JunctionType)]` idea was analyzed and
**rejected without implementation**. The deciding argument chain:

- The mechanism would be a direct generalization of Refs' put-time diff
  (stale-release), so the mechanics are cheap. The cost is semantic:
  **the truth source of a junction has no natural home row.** Refs works
  because the parent row is the single owner (one-directional by
  structure); a junction's two endpoints are peers — declaring the field
  on one side makes the other side's `delete` unable to clean up without
  a reverse RMW chain, and a stale field key resurrects a deleted edge
  on the next put.
- Two write entry points over the same junction (field diff + explicit
  `link`/`unlink`) fight each other: a handler that unlinks directly and
  a later field-preserving put that diffs the stale key back.
- The diff buys nothing unless the reverse scan is required — and
  requiring it is exactly the "is this edge needed at all" question
  MODELING already poses. A relation with no reverse-lookup need should
  not be a junction in the first place.
- The imperative pairing (`link` writes both entries, `unlink` deletes
  both) keeps the code IS the event property: the call site is the
  causal record, no old state is ever reconstructed, and the write
  domain never crosses into the peer's ns implicitly.

The imperative pairing stands as the junction's interface. If a real
consumer surfaces with set-shaped mutation over one endpoint, the
lighter escape is an explicit `Junction::sync_from(row)` command — same
diff, but an explicit verb, not put-path magic. Do not re-propose the
field-position form without a named consumer.

## References

- MODELING "Many-to-many relations" — the junction-as-middle-table
  description, `ok_head` truncation discipline, the two-directional
  obligation argument.
- ADR-0005 (index ns derivation) — the manual-numbering burden
  argument, applied verbatim here.
- ADR-0012 (embedded documents) — the one-to-many field-level carrier.
