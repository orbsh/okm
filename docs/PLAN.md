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
- [x] 2026-11-21 — a dynamic reader opens from the store: `DynamicCollection::open(store, ns)` reads the declaration out of the table's meta row, refuses a row whose schema names another ns (the pair rule), and compiles `TablePlan` once from those bytes; a handle from `open` carries no access methods or reduce groups, because the store never records them — `new` stays the form for a caller that declares them — ADR-0074 §5
- [x] 2026-11-20 — routing is one lookup: a declaration's key starts with its ns as two big-endian bytes, so a commit finds its table in a `u16`-keyed map instead of a `starts_with` pass over every registered header, and the headers a key cannot name in two bytes (only a test document has one) fall to a list that is empty in a real store — ADR-0074
- [x] 2026-11-20 — the plan lives in the table's own static, and the derive's per-ns marker IS that static: `__OKM_NS_{ns}_ENTRY: OnceLock<TablePlan>` (renamed from the sentence-long `_IS_UNIQUE`) holds the compiled plan, `Document::PLAN` is the `&'static` pointer to it, attach fills it with `get_or_init(compile(declaration))` once per table per process, and it reaches the log as a reference inside the record the log already keeps per known table (`Known { meta, plan }`) rather than in a second map beside it — a static in a generic scope is one instance for every instantiation, which is why the storage sits where the declaration does and only a pointer crosses into generic code — ADR-0074
- [x] 2026-11-20 — a batch stops owning its ops, and the boundary becomes the blob: `Batch` keeps `[key][value]…` in one buffer beside a `Vec<OpSpan>`, so a write appends once and `stage()` copies a window instead of walking pairs; the same shape replaces `okm-dynamic`'s `PlannedPut.ops`, which is why this is done with the dynamic merge rather than before it — 113 `.ops` sites across okm-core (35), okm-dynamic (44), okm-lake (8) and the bindings (4) consume the old pair list, and every one of them is either being moved into core anyway or reading the same bytes — the entries side already measures 62.0 ns reused against 92 ns owned (`index_entries_into/reused`), and this is where that difference reaches the write — ADR-0074. Landed 2026-11-20: `Batch` is `{ bytes, ops: Vec<OpSpan> }` (c770d3e), `okm-dynamic`'s plans carry it too (1ef1355), and a write measures 1.3571 us against 1.5825 us (-12.4%)

- [x] 2026-11-21 — the mechanism moves under the declaration: the compiled entry layout is the only entry producer (the plan carries routing, the layout rides the collection — it can be refused and it is per handle), one ledger orchestration, okm-dynamic's mechanism modules in core (the projections stay out; the query surface stays two by decision) — ADR-0076, which holds the targets, the measurements and the per-step record. Landed: §1–§3 complete (`put` 1.2385 → 1.2149 µs); `okm-dynamic` dissolved — the column builder is `okm_lake::columns`, its tests live beside the code they test, and the lake, log and bindings name core directly. The engine matrix moved to `okm_core::testing`, so no dependency points upward (ADR-0059 amendment).
- [x] 2026-11-21 — the dynamic write path executes the same plan as the typed one: `DynamicCollection` compiles its `Plain` faces into an `EntryLayout` (compiled when the declaration arrives, recompiled by `declare_index`, which is also where a face the schema cannot answer is refused now instead of on the first write), `plan_put` walks the payload bytes it just encoded instead of looking every indexed name up in the document map, the callable faces (`Func` / `Partial`) keep their runtime producer and append into the same buffer — one producer per face — and the overwrite sweep asks for KEYS (`index_keys`), so a covered face stops encoding includes nobody reads. Over the bench's two faces the dynamic producer measures 187.7 ns against the walk's 27.1 ns (a rerun read 179.2 / 27.3), and a warm plan allocates 8 times against 17 for the pair-list producer body alone. `rebuild_faces` runs the same walk fed by the row slot's stored value (it IS the payload), through `produce` so the cold tags are checked, and it fetches the decoded document only when a callable or path face needs one — one producer for both paths, and the reason a categorical (`Vocab`) face is rebuildable at all — ADR-0076 §1
- [x] 2026-11-20 — the window follows the recovery term: `CompactPolicy::pinned()` is 256 MiB for a table whose baseline is a lake version, while the 1 GiB default stays for a table whose baseline is a scan, because for that table the window is the app host's one recoverable region — ADR-0071

## The stores

- [x] 2026-11-20 — the ephemeral engine is a packed B+tree over chunked tables and okm's non-persistent mode (the same declarations, durability chosen at assembly); it is an arm of the engine matrix — ADR-0072
- [x] 2026-11-20 — the matrix's cheap zero-fs default is the ephemeral engine (`TestStore::default()`), slatedb stays as the arm that disagrees and is reached by asking for it or by running `matrix()`, and it stays evidence only while the suite asks `VirtualStorage` rather than the fixture — ADR-0073
- [x] 2026-11-21 — a generation change moves one table without stopping it: the generation is one bit of the address (`LAYOUT_VERSION`'s parity in the high bit of the ns, `ns < 0x8000`, so offsets and slots do not move), each generation keeps its own record row at `[ns|parity][0x1F]` and attach takes the larger version as current — no pointer row, no seal, the flip is one put; the old generation serves every read while the migrator copies its range under the new declaration and replays the old log's tail into it, then a microsecond quiesce, then the vacuum. Work it needs: the derive's `ns < 0x8000` assertion and the parity in the const header, `TablePlan::compile` deriving the bit from `schema.layout_version`, attach probing both parities, and the subcommand (copy → replay → flip → vacuum) — ADR-0075. Landed as `okm-log migrate` (verdict, copy, tail, flip) and `okm-log vacuum` (the old range, its record and its log directory); the quiesce is the caller's — `--root` given means the tail is replayed, absent means the caller quiesced the table itself

## Packaging and gates

- [x] 2026-10-31 — packaging and CI: four workspaces, one gate
- [x] 2026-11-18 — formatting is its own gate, and it stops at the workspace it names

## Ideas — recorded, not designed

- [x] Recorded (2026-11-10) — a SIMD intersection proposal, evaluated — ADR-0020, ADR-0021, ADR-0059

## Open

- [ ] Optimistic CAS write (`save_with_cas`) — deferred 2026-10-31, no owner
- [ ] Interning exposed to the bindings — deferred 2026-10-31 — ADR-0032
- [ ] `okm-series` has no caller inside the workspace: the engine never writes to it. Its one consumer is `bindings/okm-series-python`, which CI accepts through a wheel round trip (`series-wheel`), so the crate is exercised and shipped — just not used by anything the store runs — deferred 2026-10-31
- [x] A schema-aware index layer (block index or custom ART) — closed 2026-11-23: the ceiling on a 200 000-row fjall corpus is 1.20× end to end with the search itself at 0.3 % of a query, and every option owns the engine — ADR-0080
- [ ] A bitmap index for set membership (Roaring) — recorded, not designed; reopens on a measured reuse ratio — `docs/ideas/bitmap-index.md`
- [ ] A native n-ary relation carrier — recorded, not designed; reopens on a query distribution where N≥3 relations are the normal case — `docs/ideas/hypergraph.md`
- [x] SurrealKV as an `okm-async` backend — closed 2026-11-23: the capabilities that distinguish it are the ones nothing here asks for (engine MVCC snapshots belong to the changelog, in-engine encryption belongs to the volume), while it costs +2.86 MB, 150 transitive crates and a Tokio-only commit — ADR-0064
- [x] A migrated table is verified before the flip: `compare_generations` reads both ranges through their own declarations and refuses a row the new one lacks, a field whose value differs, or a field the new declaration dropped — it runs in `migrate` before the record is written and in `vacuum` before the first key is deleted, so both irreversible steps sit behind a comparison that already passed — ADR-0075
