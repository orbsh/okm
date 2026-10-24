# Idea: a native hypergraph (n-ary relation) carrier

**Status**: idea, recorded 2026-10-24. Not a decision — no ADR, no PLAN item,
nothing here is committed. The proposal in §1–§6 is an outside document
(Chinese, 2026-10-24) reproduced in substance; §7 checks its premises against
the code as it stands today, and §8 lists what a real design would have to
settle first.

Companion: [hypergraph-population.md](hypergraph-population.md) — the ingestion
side (how hyperedges get proposed and written without an LLM in the write path);
the two are decidable independently.

## 1. The claim

okm has two relation carriers, both binary:

- `Junction<S, E>` (`okm-core/src/model/collection.rs:17`) — compile-time typed
  endpoints, identity IS the endpoint pair, one entry per direction
  (`okm-core/src/model/junction.rs:1-30`).
- `Graph<S, E>` (ADR-0017) — open `NodeRef` endpoints, own identity
  (`edge_id`), attributes, parallel edges, runtime predicates
  (`okm-core/src/model/graph.rs:1-40`).

Each binds one source and one destination. The proposal's premise: high-order
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
| hypergraph takes face `0x0A` | `0x0A` is `EDGE_PREDICATE_IN_SLOT` (`okm-core/src/model/graph.rs:62`); free fixed faces are `0x0C–0x0F` (ADR-0030, line 97) |
| face slot is 2 bytes | a fixed face is **1 byte**; only counting families (`0x1nnn`/`0x2nnn`) are 2 bytes — ADR-0030 lines 45-46 and 85-97, implemented by `write_slot` (`okm-core/src/model/index.rs:81-91`) |
| `NodeRef` = `[ns 2B][len u16][pkey]` | `[ns 2B][len **varint**][pkey]`, `len < 64` in one byte (`okm-core/src/model/graph.rs:104-111`) |
| edge id is 4B/8B | `u64` (8-byte BE) everywhere: `link`, `edge_ids`, faces |
| `#[ok_entities(User, Org, Asset)]` | the existing declaration vocabulary is `#[ok_nodes(EntityRow, MemberRow)]`, taking document **tables** by type; the derive reads each table's own `#[ok_ns]` (`okm-core/src/model/graph.rs:196-216`, ADR-0017 status line, update 2026-10-21) |
| `link(edge_id, members, payload)` | `Graph::link(&EdgeHead{src, dst, predicate}, &E, edge_id: u64)` (`okm-core/src/model/graph.rs:224-232, 373`); an N-member head is a new shape, and the body is fixed `[src ref][dst ref][predicate_vocab_id][payload]` (`graph.rs:236`, `decode_body` at `graph.rs:259`) — the payload offset is computable only because there are exactly two refs, so an open member list needs a count or length prefix |
| payload indexes are a hypergraph feature | the payload is an ordinary obj (ADR-0012): one document declaration already yields both codec directions and its `#[ok_index]` faces, counting family `0x1` (`graph.rs:39-46`) |
| `okm_query::intersect` | does not exist; `okm-query` ships `merge_join(left, right)` over key-ordered `(K, V)` pairs (`okm-query/src/lib.rs:32`), and today's graph face scans materialize into `Vec` (`Graph::edge_ids`, `graph.rs:404`) — a streaming k-way intersection would be new API, though `VirtualStorage::scan_range_iter` is available underneath |
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
7. **Does the existing pattern already answer the use case?** Reifying the
   relation as a document row plus binary edges to participants is expressible
   today, with no new carrier. The hypergraph trades 1 row + N edges for 1 row +
   N faces: cheaper for "which relations touch this member" (no edge fetch),
   identical for "what are this relation's participants" (one body read). The
   decision hinges on which query dominates, not on the encoding.