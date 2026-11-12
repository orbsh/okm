# Idea: a native hypergraph (n-ary relation) carrier

**Status**: idea, recorded 2026-10-24. Not a decision — no ADR, no PLAN item,
nothing here is committed. The proposal in §1–§6 is an outside document
(Chinese, 2026-10-24) reproduced in substance; §7 checks its premises against
the code as it stands today, and §8 lists what a real design would have to
settle first.

**Re-checked 2026-11-12.** The carrier this proposal was written against has retired. `Junction` — and the `collection.rs` / `junction.rs` paths §1 cited — no longer exist (ADR-0051, 2026-11-04), and a many-to-many relation is now a collection whose key is the pair, with its reverse direction declared as a lead face over the key's own bytes (ADR-0054). §1's two-carrier premise and §8.7's comparison are superseded by §9, which states the comparison that decides anything now: a hyperedge against a **relation table**.

Companion: `~/world/krystallizer/docs/ideas/hypergraph-population.md` — the
ingestion side (how hyperedges get proposed and written without an LLM in the
write path). It moved to the service that owns the write side: okm offers no
extraction, no learner and no thresholds, so that half is memory-layer policy, and
only its constraints on this carrier are okm's business. The two are decidable
separately except on arity — a bounded member list here forces the pipeline to
merge before writing.

## 1. The claim

okm has one dedicated relation carrier, and it is binary:

- `Graph<S, E>` (ADR-0017) — open `NodeRef` endpoints, own identity (`edge_id`), attributes, parallel edges, runtime predicates (`okm-core/src/model/graph.rs`), faces `0x06`–`0x09`.

Many-to-many is not a carrier at all: it is a collection whose key is the pair (ADR-0051), with the reverse direction declared as a lead face over the key's own bytes (ADR-0054). The shared entry layout underneath is the same one `Graph` borrows — namespace, one-byte fixed face, index segment, primary-key tail. `Junction`, the carrier this section originally listed, retired in ADR-0051; the model kept the shape and dropped the machinery, because a collection already had every capability the junction needed to build by hand.

A graph edge binds one source and one destination. The proposal's premise: high-order
relations (agent memory, multi-party audit, risk) routinely bind 3+ participants
at once — `{user A, org B, asset C}` all participating in `[audit event X]` —
and modelling that as a reified node plus k binary edges costs k times the
round trips and loses "these k facts are one relation".

The proposed carrier: `#[derive(HyperGraphEncode)]` + a `HyperGraph<S, E>`
handle. A hyperedge is **reified physically**: one payload row plus N membership
faces. A member face makes "every edge containing member M" a leftmost-prefix
scan; intersecting two member faces answers "edges containing both M and N" —
which is the query the proposal treats as the whole point.

## 2. Layout as proposed

```text
primary      [ns 2B][slot 0x00][edge_id BE]            → hyperedge payload
member face  [ns 2B][face slot][member NodeRef][edge_id BE] → empty / covering includes
```

Rules the proposal states:

1. Every participant is one `NodeRef` — the same self-describing form a graph
   edge already uses, so pkey width stays wire data.
2. The face key's tail is a fixed-width big-endian edge id, so two member scans
   merge by pointer comparison with no deserialization.
3. The hypergraph claims its own segment/slot range so its keys cannot collide
   with document collections or junctions.

## 3. Declaration as proposed

```rust
#[derive(HyperGraphEncode, Clone, Default)]
#[ok_ns(1000)]
#[ok_entities(User, Org, Asset)]            // admitted member tables
#[ok_index(by_time { fields(audit_year) })] // faces on the edge's own payload
pub struct AuditIncident {
    pub audit_year: u16,
    pub risk_level: u8,
    pub predicate: String,
}
```

The macro is said to also emit a head type (`edge_id` + `members: Vec<NodeRef>`)
and an `HyperEdge` trait impl.

## 4. API as proposed

- `HyperGraph::link(edge_id, members, &payload)` — one internally built `Batch`
  so the payload row and all N member faces appear atomically.
- `iter_edge_ids_by_member(&NodeRef)` — prefix scan of one member face.
- `scan_edge_ids_by_member(&NodeRef)` per member, composed by
  `okm_query::intersect(stream_a, stream_b)` into the common edge set.
- `_into` twins (`link_into`, `unlink_into`) for the cross-collection batch
  path; `unlink_into` reads the payload row first, then deletes exactly the
  payload and every face it implies.

## 5. Cost claims

- Zero parsing: face-key offsets are fixed by the derive at compile time;
  decoding is pointer slicing.
- Memory-friendly: 2-byte ns instead of string prefixes; the layout is claimed
  to suit LSM bloom filters.

## 6. What the proposal does not fix

Arity bound, member multiplicity, per-member role/position, edge-id allocation,
predicate-qualified member faces, and whether a hypergraph is a new declaration
kind or an arity of the graph declaration. §8 treats these as the real design
work.

## 7. Checked against the code (2026-10-24)

Each row is a premise the proposal asserts and what the repository actually says.

| proposal says | code says |
|---|---|
| hypergraph takes face `0x0A` | `0x0A` is `okm-vector`'s per-shard graph (`okm-vector/src/index.rs:33`); the graph's own faces stop at `0x09` (`EDGE_PREDICATE_IN_SLOT`, `okm-core/src/model/graph.rs:81`). Fixed faces in use: `0x00`–`0x0A`, `0x0B` a retired snapshot slot, `0x0F` the table meta (ADR-0043) — so the free band is `0x0C`–`0x0E`, three faces |
| face slot is 2 bytes | a fixed face is **1 byte**; only counting families (`0x1nnn`/`0x2nnn`) are 2 bytes — ADR-0030 lines 45-46 and 85-97, implemented by `write_slot` (`okm-core/src/model/index.rs:81-91`) |
| `NodeRef` = `[ns 2B][len u16][pkey]` | `[ns 2B][len **varint**][pkey]`, `len < 64` in one byte (`okm-core/src/model/graph.rs:104-111`) |
| edge id is 4B/8B | `u64` (8-byte BE) everywhere: `link`, `edge_ids`, faces |
| `#[ok_entities(User, Org, Asset)]` | the existing declaration vocabulary is `#[ok_nodes(EntityRow, MemberRow)]`, taking document **tables** by type; the derive reads each table's own `#[ok_ns]` (`okm-core/src/model/graph.rs:196-216`, ADR-0017 status line, update 2026-10-21) |
| `link(edge_id, members, payload)` | `Graph::link(&EdgeHead{src, dst, predicate}, &E, edge_id: u64)` (`okm-core/src/model/graph.rs:224-232, 373`); an N-member head is a new shape, and the body is fixed `[src ref][dst ref][predicate_vocab_id][payload]` (`graph.rs:236`, `decode_body` at `graph.rs:259`) — the payload offset is computable only because there are exactly two refs, so an open member list needs a count or length prefix |
| payload indexes are a hypergraph feature | the payload is an ordinary obj (ADR-0012): one document declaration already yields both codec directions and its `#[ok_index]` faces, counting family `0x1` (`graph.rs:39-46`) |
| `okm_query::intersect` | ships as a lazy adapter: `a.intersect(b)` / `a.merge(b)` over key-ordered streams (`okm-query/src/lib.rs:66,88`), set-semantic over keys with one tuple per distinct key — a pair-expanding join is the caller's fetch loop, not an operator. Today's graph face scans still materialize into `Vec` (`Graph::edge_ids`, `graph.rs:404`), so what is new API is a streaming k-way intersection over *faces*, though `VirtualStorage::scan_range` is available underneath |
| `unlink_into` is read-before-write | confirmed: it reads the primary body, decodes it, deletes the exact faces plus the payload-declared index entries (`graph.rs:553-590`). For N members this works only if the body carries the member list |
| link/unlink are one atomic commit | confirmed for the batch path: one `commit_batch` for documents and edges together (ADR-0031, `link_into` doc at `graph.rs:382-395`). One asymmetry carries over: vocabulary allocation commits on its own, so a failed outer commit can leave an orphan predicate/vocab entry — harmless and documented |

## 8. Open questions (the design work, not the recording)

1. **Third declaration or an arity of the graph?** `GraphEncode`'s faces
   `0x06–0x0A` are src/dst-shaped: out, in, predicate+out, predicate+in. A
   set-shaped graph collapses out/in into one member face and one
   predicate+member face, and the direction distinction disappears from the edge
   layer. Whether that is a second carrier or a widening of ADR-0017 decides
   everything downstream (declaration, derive, unlink).
2. **Arity bound.** Bounded N (say ≤ 16) lets the body keep computable offsets
   and honors the zero-parsing claim; unbounded N needs a count/varint prefix in
   the body, so the payload offset becomes a cursor.
3. **Multiplicity.** A member-face key cannot distinguish two occurrences of the
   same member, so a repeated participant collapses into one face entry. If
   "the same asset twice with different roles" must be representable, member
   identity needs position or role in the key.
4. **Per-member role.** Hypergraph literature's usual next requirement
   (`approver`, `receiver`). A role byte or role id in the face key widens every
   face and fixes the scan order to role-first or member-first; keeping it in
   the payload keeps keys narrow but makes role queries a full fetch.
5. **Face numbers.** `0x0C–0x0F` are free; reusing `0x07` as "member" would
   reinterpret live graph keys, so a hypergraph must live in its own ns
   (ADR-0017 §1 already isolates one ns per graph), never sharing one with a
   binary edge set.
6. **Membership check.** `NodeSet`/`#[ok_nodes]` is the existing declared-set
   mechanism (a declaration, no slot). Reuse it rather than inventing
   `ok_entities`, or the vocabulary grows two names for one idea.
7. **Does the existing pattern already answer the use case?** A relation table does (ADR-0051 + ADR-0054), and it is a stronger baseline than the binary-edge modelling this section used to compare against: N members are N rows, `put` writes the row and every face in one batch, attributes on the relation are ordinary indexable columns, the reverse direction is a lead face sliced from the key (so it cannot drift from the key), and the rows travel through the changelog and to the lake. Against that baseline a hyperedge trades N rows for 1 row + N faces. §9 states what it wins and what it loses.

## 9. A hyperedge against a relation table (2026-11-12)

The baseline is not a junction and not "a row plus binary edges". It is one collection whose key is the pair, `#[ok_lead]` for the reverse direction, and ordinary columns for whatever the relation knows:

```text
#[derive(KeyEncode)]  struct MembershipKey { channel_id: u64, member_id: Uuid7, joined_ms: u64 }
#[derive(DocumentEncode)] #[ok_ns(110)] #[ok_lead(by_member { fields(member_id) })]
struct Membership { nickname: String, since_ms: u64, role: u8 }
```

| need | relation table (today) | hyperedge (proposed) |
|---|---|---|
| which relations touch M | lead face `by_member` → whole key decoded, row fetched | member face → edge id, then the payload row |
| which members a relation has | prefix scan of the pair key | one body read (N refs inline) |
| relations containing A and B | two lead-face streams intersected (`OrderedSets`) | two member-face streams intersected |
| attributes of the relation | ordinary columns | payload |
| attributes of a member *within* the relation | ordinary columns (`role`, `since_ms`) | **not expressible** without widening the face key with role/position, or storing per-member data in the body and fetching it |
| members in a meaningful order (join order, sequence) | free: an extra key column, re-read by another lead face | **not expressible**: the member face collapses order |
| "these N members are one relation" | convention only — a missing row is invisible | structural: body plus faces written once |
| delete the whole relation | scan the prefix, then `purge` each row | read the body, delete every implied face |
| export / changelog | the row is a row | the payload row is; the faces are derived state |

What it wins is the last two rows of that shape — one object per relation, so the members cannot be torn apart and the relation's own attributes exist once instead of N times — plus half the index entries, since direction is gone and `out`/`in` collapse into one member face.

What it loses is the row above them, and for agent memory that row is usually the point: a relation is rarely symmetric (a member's role, weight, valid-from, or confidence belongs to *that member in that relation*), and a set-shaped carrier has no place for it except widening the face key or paying a fetch. It also loses the key-as-address allowance — any immutable ordering dimension rides in a pair-keyed table's key and comes back through a face, while a hyperedge's member face has no place to put an ordering.

So the decision does not turn on arity as §5 assumed. It turns on **where the attributes live**: relation-level only → a hyperedge is genuinely cheaper and its invariant is real; per-member too → the relation table already models it and the hyperedge would have to rebuild that ability inside the face key. The prototype worth building, if any, is the set-shaped form of the graph (reusing the direction-only faces `0x07`/`0x09` and their predicate twins, ADR-0017 §1 keeping one ns per graph), not a third carrier — and only after a workload shows relations with no per-member attributes are the common case.
