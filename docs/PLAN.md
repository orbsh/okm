# OKM — Implementation Plan

Design decisions live in `docs/adr/`; this file is the index — what shipped, when, and which record decided it. The reasoning, the alternatives and the costs are in those files, never here.

## The key space

- [x] Phase 1 — Core key/edge layer ✅ shipped — ADR-0001, ADR-0003, ADR-0049
- [x] Phase 2 — Rows / Nodes ✅ core shipped — ADR-0003, ADR-0004, ADR-0006, ADR-0035, ADR-0042
- [x] Phase 3 — Secondary indexes / access methods ✅ shipped — ADR-0005, ADR-0019
  - [x] Header unification trigger (edge direction bit) — ADR-0001

## Rows, documents, and the graph

- [x] Phase 4 — Snapshots & packaging — ADR-0007
- [x] Phase 5 — Event layer: reduce, subscribe, okm-stream — ADR-0008, ADR-0009
  - [x] Preset reduce combinators — ADR-0023, ADR-0024
  - [x] Reduce hooks receive the decoded key — ADR-0023, ADR-0024
- [x] Phase 6 — Commanded RMW: `Table::upsert_with`
- [x] Phase 7 — VirtualStorage: engine as boundary, remote backend, NestStorage derive — ADR-0002, ADR-0005, ADR-0010, ADR-0011, ADR-0020, ADR-0022, ADR-0029, ADR-0035, ADR-0038
- [x] Phase 8 — Object model: one encoding, field-name vocabulary, kv_ → ok_ — ADR-0006, ADR-0011, ADR-0012
- [x] Phase 9 — wrappers: `Option<T>`
- [x] Phase 10 — edge keys via slots: retire the direction-bit niche (high priority) — ADR-0001
- [x] Slot shapes: fixed face / counting family — ADR-0030
- [x] Graph edge: one declaration, a handle, a predicate vocabulary — ADR-0010, ADR-0017
- [x] Embedded documents (2026-09-17, shipped)
- [x] Terminology (2026-09-17, decided)
- [x] Parquet Variant export (shipped, 2026-09-17) — ADR-0012, ADR-0022
- [x] Scalar list fields (closed, 2026-09-21)
- [x] Junction rename + ns derivation — ADR-0015, ADR-0016, ADR-0017
- [x] Plural modeling types — ADR-0015
- [x] Range scan + lazy iteration — ADR-0020, ADR-0021
- [x] Wire encoding refinements (execution order, 2026-09-17) — ADR-0004, ADR-0012, ADR-0015
- [x] Phase 7 — Two-mode record — ADR-0025, ADR-0026
- [x] In-browser VirtualStorage family — ADR-0018, ADR-0028
- [x] Semantic batch writes — ADR-0026, ADR-0031, ADR-0047
- [x] Categorical values as data — ADR-0007, ADR-0030, ADR-0032, ADR-0037
- [x] ADR-0034 — table names and the export shape (2026-10-21) — ADR-0034, ADR-0035
- [x] Whole-document API: merge / replace / purge / view — ADR-0005, ADR-0010, ADR-0018, ADR-0030, ADR-0035, ADR-0037, ADR-0038
  - [x] A value too big for the row is its own table — ADR-0003, ADR-0031, ADR-0036
  - [x] Vocabulary allocation: one rule, two implementations, only one checked — ADR-0012, ADR-0030, ADR-0032, ADR-0037
- [x] Panic surface audit and the unwrap gate (2026-10-24, gated) — ADR-0010, ADR-0028, ADR-0037, ADR-0039, ADR-0040

## The log, the segment, and the lake

- [x] 2026-10-27 — the graph faces carry both endpoints — ADR-0020, ADR-0041, ADR-0048
- [x] 2026-10-27 — layout versions are stamps — ADR-0042
- [x] 2026-10-28 — the table's own record — ADR-0042, ADR-0043
- [x] 2026-10-28 — a table is a key type plus a row type — ADR-0036, ADR-0044
- [x] 2026-10-28 — the change log: capture at the commit boundary, drain out of process — ADR-0032, ADR-0043, ADR-0045, ADR-0058, ADR-0065
- [x] 2026-10-28 — the append-only log — ADR-0046
- [x] Uuid4 / Uuid7 — an id is 16 bytes, and identity belongs in the key
- [x] Junction as a table — implementation checklist — ADR-0045, ADR-0050
- [x] Phase 4.14 — The junction retires ✅ shipped — ADR-0015, ADR-0045, ADR-0050, ADR-0051, ADR-0054
- [x] 2026-11-10 — the segment is the migration path — ADR-0007, ADR-0026, ADR-0034, ADR-0042, ADR-0056, ADR-0058
- [x] 2026-11-10 — the dynamic segment gets a block table at its head — ADR-0012, ADR-0030, ADR-0045, ADR-0056, ADR-0057
- [x] 2026-11-10 — the payload loses its header — ADR-0042
- [x] 2026-11-10 — the async face becomes a crate — ADR-0010, ADR-0020, ADR-0028, ADR-0059
- [x] 2026-11-10 — the cutover announces itself — ADR-0056
- [x] 2026-11-10 — the changelog directory is permanent; a generation is a SEGMENT (owed) — ADR-0042, ADR-0043, ADR-0048, ADR-0056
- [x] 2026-11-10 — the sync browser engine leaves core — ADR-0028
- [x] 2026-11-11 — the entry header is one byte, and the codec moves out — ADR-0012, ADR-0043, ADR-0056, ADR-0057, ADR-0060
- [x] 2026-11-11 — the lake is a key space — ADR-0061, ADR-0065
- [x] 2026-11-12 — a point read answers parts, not one assembled map — ADR-0035
- [x] 2026-11-12 — one write verb, and no assembly function — ADR-0035
- [x] 2026-11-12 — the document map is opaque and insertion-ordered — ADR-0012, ADR-0035

## Packaging and gates

- [x] 2026-10-31 — packaging and CI: four workspaces, one gate
- [x] 2026-11-18 — formatting is its own gate, and it stops at the workspace it names

## Ideas — recorded, not designed

- [x] Recorded (2026-11-10) — a SIMD intersection proposal, evaluated — ADR-0020, ADR-0021, ADR-0059

## Open

- [ ] Optimistic CAS write (`save_with_cas`) — deferred 2026-10-31, no owner
- [ ] Interning exposed to the bindings — deferred 2026-10-31 — ADR-0032
- [ ] `okm-series` has no production caller: nothing outside its tests writes to it — deferred 2026-10-31
- [ ] A schema-aware index layer (block index or custom ART) — recorded and measured, not designed; the ceiling on a 200 000-row fjall corpus is 1.20× — `docs/ideas/schema-aware-index.md`
- [ ] A bitmap index for set membership (Roaring) — recorded, not designed; reopens on a measured reuse ratio — `docs/ideas/bitmap-index.md`
- [ ] A native n-ary relation carrier — recorded, not designed; reopens on a query distribution where N≥3 relations are the normal case — `docs/ideas/hypergraph.md`
- [ ] SurrealKV as an `okm-async` backend — measured, not started: +2.86 MB, 150 transitive crates, and `commit()` panics without a Tokio runtime — ADR-0064
