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
- [x] 2026-11-20 — the lake's parquet files are written with zstd level 1: the 1 M-row export went from 120.2 to 36.3 B/row (the plain arm without change columns is 33.0), and every writer in `snapshot::parquet_io` shares the one codec — ADR-0071
- [x] 2026-11-20 — the baseline is a pinned lake version: `okm_lake::baseline::compact_against` records `(table, version, covers)` and deletes only what the lake's manifests hold, `okm_lake::restore::restore_at` puts lake@V back and hands over the number the tail starts above, and a refused compaction leaves the log untouched so the scanned baseline can still be written — ADR-0071
- [x] 2026-11-20 — the pointer is verified, not trusted: `okm-lake::baseline::verify` re-lists the lake, reads the version the pointer names rather than the live state, and refuses a foreign table, a version the lake does not have, a window without a manifest, or coverage below S — ADR-0071
- [x] 2026-11-21 — the tail crosses as EXPORTED PARQUET, not as sealed segment blobs: `okm_lake::baseline::compact_against_exporting` runs the drainer's own writer synchronously over whatever the consumer has not taken, then verifies the version the export produced and points at it, so a cutover stops depending on the drainer's schedule; the plain `compact_against` keeps the fallback meaning (cover what the lake holds, leave the rest as the tail). Measured bounds for the conversion: the window is capped by the policy that triggers compaction (256 MiB pinned, 1 GiB scanned), the export is 36.3 B/row at zstd(1), parquet writing 9.84 M records/s — ADR-0071 §2
- [x] 2026-11-21 — capture defaults to on, and the levels are explicit: `Document::CAPTURED` defaults to true and `#[ok_uncaptured]` opts a table out; the choice rides the store's meta row (`capture = "0"` for opt-outs only, absence means captured) and attach COMPARES it rather than overwriting, so a declaration cannot silently contradict a store; `Location::Off` keeps its narrowed meaning — no local durable directory (ephemeral engine, read-only replica, scratch disk), a physical reason, never "logging is expensive". The assembly level the item assumed turned out to be no level at all: ADR-0074 deleted the verb that could be silent (`Collection::captured` / `Collection::new`) and left `open` as the only way in — ADR-0071 §7–§8.
- [x] 2026-11-20 — the store describes its tables, and the log reads that description: `Collection::open` is the only constructor and it attaches (row seeded or checked, other generation refused), the meta row carries `schema` as `CollectionSchema` bytes, the log learns a table by observing that row (`note_table`, one point read per ns per process), the declaration also rides in every segment header, and `ChangelogStore::register`, `Collection::captured` and the `schema-v<N>.json` sidecar are gone — ADR-0074
- [x] 2026-11-20 — routing is one lookup: a declaration's key starts with its ns as two big-endian bytes, so a commit finds its table in a `u16`-keyed map instead of a `starts_with` pass over every registered header, and the headers a key cannot name in two bytes (only a test document has one) fall to a list that is empty in a real store — ADR-0074
- [x] 2026-11-20 — the plan lives in the table's own static, and the derive's per-ns marker IS that static: `__OKM_NS_{ns}_ENTRY: OnceLock<TablePlan>` (renamed from the sentence-long `_IS_UNIQUE`) holds the compiled plan, `Document::PLAN` is the `&'static` pointer to it, attach fills it with `get_or_init(compile(declaration))` once per table per process, and it reaches the log as a reference inside the record the log already keeps per known table (`Known { meta, plan }`) rather than in a second map beside it — a static in a generic scope is one instance for every instantiation, which is why the storage sits where the declaration does and only a pointer crosses into generic code — ADR-0074
- [x] 2026-11-20 — a batch stops owning its ops, and the boundary becomes the blob: `Batch` keeps `[key][value]…` in one buffer beside a `Vec<OpSpan>`, so a write appends once and `stage()` copies a window instead of walking pairs; the same shape replaces `okm-dynamic`'s `PlannedPut.ops`, which is why this is done with the dynamic merge rather than before it — 113 `.ops` sites across okm-core (35), okm-dynamic (44), okm-lake (8) and the bindings (4) consume the old pair list, and every one of them is either being moved into core anyway or reading the same bytes — the entries side already measures 62.0 ns reused against 92 ns owned (`index_entries_into/reused`), and this is where that difference reaches the write — ADR-0074. Landed 2026-11-20: `Batch` is `{ bytes, ops: Vec<OpSpan> }` (c770d3e), `okm-dynamic`'s plans carry it too (1ef1355), and a write measures 1.3571 us against 1.5825 us (-12.4%)

- [x] 2026-11-21 — the mechanism moves under the declaration: `TablePlan` is the only entry producer, one ledger orchestration, okm-dynamic's mechanism modules in core (the projections stay out; the query surface stays two by decision) — ADR-0076, which holds the targets, the measurements and the per-step record. Landed: §1–§3 complete (`put` 1.2385 → 1.2149 µs); `okm-dynamic` dissolved — the column builder is `okm_lake::columns`, its tests live beside the code they test, and the lake, log and bindings name core directly. The engine matrix moved to `okm_core::testing`, so no dependency points upward (ADR-0059 amendment).
- [x] 2026-11-20 — the window follows the recovery term: `CompactPolicy::pinned()` is 256 MiB for a table whose baseline is a lake version, while the 1 GiB default stays for a table whose baseline is a scan, because for that table the window is the app host's one recoverable region — ADR-0071

## The stores

- [x] 2026-11-20 — the ephemeral engine is a packed B+tree over chunked tables and okm's non-persistent mode (the same declarations, durability chosen at assembly); it is an arm of the engine matrix — ADR-0072
- [x] 2026-11-20 — the matrix's cheap zero-fs default is the ephemeral engine (`TestStore::default()`), slatedb stays as the arm that disagrees and is reached by asking for it or by running `matrix()`, and it stays evidence only while the suite asks `VirtualStorage` rather than the fixture — ADR-0073
- [ ] 2026-11-20 — a generation change is a migration, one table at a time: attach answers a store of another generation by sealing the table (the typed handle refuses both directions, the error names the table and both generations) while the log still reads it with the declaration its segments carry, so the export leg survives a bump; the move is snapshot to the lake under the stored declaration → clear the ns and backfill through a collection assembled over the bare engine (reads refused for the duration, the snapshot serves them) → verify, unseal, vacuum; the backfill records no log, because the log records what changed and only the snapshot records what the old generation held; the driver is an okm-lake subcommand, not a crate — ADR-0075

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
