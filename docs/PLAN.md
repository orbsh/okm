# OKM — Implementation Plan

Design decisions live in `docs/adr/`. This plan tracks implementation status.

## Phase 1 — Core key/edge layer ✅ shipped

- [x] `KeyEncode` derive: fixed-width fields (`u32` / `u64` / `[u8; N]`), BE,
      compile-time `KEY_LEN` / `FIELD_WIDTHS`, `encode_prefix_named`.
- [x] `EdgeEncode` derive: `#[kv_head(field, …)]` per-endpoint identity width,
      2-byte direction-bit header (niche, ADR-0001), query methods on endpoint
      types.
- [x] `Collection<S, E>` assembly point, no `KvRecord` (ADR-0003).
- [x] Engines: `MockStore` (default), `fjall` (sync, feature), `slatedb`
      (async, feature).
- [x] Hex layout-stability tests (`tests/integration.rs`).

## Phase 2 — Rows / Nodes (ADR-0006, value rules per ADR-0004) ✅ core shipped

- [x] `RowEncode` derive: single item declares identity (`#[kv_ref]` key
      reference), payload fields, and `#[kv_index(...)]` access methods —
      one field list, three destinations (key / value / index).
- [x] Payload encoding: TLV frames `[tag u8][len u32 BE][value BE]`, tag =
      field declaration index (decoupled from names); fixed-width fields
      today, same frame kept for the variable-length regime later.
- [x] `Table<S, K, R>` assembly point: plain generics, no assembly macro;
      holds the shared engine instance; `put(row)` = primary key write +
      index entries in one engine batch; `Row::table(store, ns)` constructor.
- [ ] `save_into(batch, key, value)` cross-collection atomic path (ADR-0003
      mechanism note): encode into an externally owned engine batch without
      touching the collection's internal buffer; for primary + index across
      two assembly points (single-table case already covered by `put`).
- [ ] Optimistic CAS write (`save_with_cas`, MVCC version compare-and-swap
      retry loop) — future extension from the original design doc; no ADR
      record yet.
- [ ] `#[kv_version(n)]` versioned payload with lazy in-memory upgrade.
- [x] Variable-length payload fields (String): the TLV frame's `len u32` IS
      the length prefix (no second one on the wire); `FieldType::Str` in
      the FieldDesc table (width 0 = variable); frame-by-frame walk in the
      Arrow bridge; key side still rejects it at compile time.
- [x] Field wrappers on row fields: `VarInt<T>` (LEB128, u16/u32/u64,
      variable frame), `Quant<P>` (f64 → scaled i64 wire, composes with
      `Reverse` for descending float order), `Enum<T>` (u8 tag via manual
      `impl EnumTag` — explicit tags, not positional), `Offset`
      (`#[kv_offset(base = N)]` → u32 displacement, out-of-range panics) —
      all shipped. `Reverse<T>` ✅ (+ `Reversible` compile-time whitelist:
      the eight fixed-width integer types, floats excluded — no fixed
      bit-flip inverts IEEE-754 order); one annotation applies to every
      destination the field is encoded into. Key/index positions reject
      all wrappers (fixed-width identity rule); descending prefix scan =
      newest-first.
- [x] Column-block regime — **cancelled** (2026-09-09, structural reason):
      `Delta<T>`/`Rle<T>`/shared-dictionary `Offset` are cross-row
      encodings — decoding one row requires the previous row or the whole
      column block. OKM's storage model is row-independent entries
      (`put(row)` = self-sufficient primary + index entries, each
      decodable alone); pushing cross-row statistics into the field
      annotation system breaks that invariant. Their proper home is the
      row-group/columnar tier, which already exists: the Parquet snapshot
      path (Phase 4) provides delta/RLE/dictionary encodings natively —
      a second in-house column-block regime has no increment.
- [ ] Hot/cold promotion procedure (extension field → hot section tail,
      version bump, hex-test guarded).

## Phase 3 — Secondary indexes / access methods (ADR-0005 + 0006) ✅ shipped

- [x] `#[kv_index(name { fields(…), includes(…) })]` on **row structs**.
- [x] Item-local slot counter; slots start at 1 (slot 0 = primary table,
      `PRIMARY_SLOT`), ns = `table_ns` + slot.
- [x] 1-byte slot discriminator: index key `[ns 2B][slot 1B][indexed
      fields][includes fields][primary key ID]`, value empty.
- [x] Per-index generated access-method impl; leftmost-prefix scan API with
      fetch-back (`scan` returns `(Key, Option<Row>)`); whole-table scan.
- [x] `includes()` covering: mechanism free, positioned as materialized view
      for high-fanout queries.
- [x] `Collection` narrowed to edge-only (`EdgeTable`); existing tests moved.
- [x] Variable-length indexed fields follow the secondary-index regime
      (ADR-0005 update 2026-09-09): text-first, primary key at the tail;
      at most one variable-length field per index segment and only in the
      last position (compile-time panic otherwise — fields after it have
      no static width to locate by). Wire = raw UTF-8, no length prefix
      (a prefix would sort by length first and destroy dictionary order);
      exact matching resolves via the trailing primary key + fetch-back.
      Function indexes (`func(path)`) shipped alongside: the sort segment
      is the declared function's result (`IndexFuncResult` — String →
      UTF-8 dictionary order, uints → BE numeric order); the query side
      calls the same function on its probe, so normalization cannot
      drift between encode and scan.
- [ ] Slot holes never reused (policy not yet stress-tested; hex tests lock
      current index layouts).

### Header unification trigger (edge direction bit)

Current state: edge headers are 2B (direction bit niched into the ns field,
ADR-0001) while index headers are 3B (`[ns 2B][slot 1B]`). Merging dir into a
shared `[ns 2B full 16-bit][flags 1B]` header is deliberately NOT done now:
edges are the most numerous keys and the niche bit is free (ns is declared
u16, effectively 32768 = structurally inexhaustible), so unification would
cost every edge key 1 byte to buy unused ns capacity.

Trigger: when the edge side needs a second discriminator of its own (edge
grouping, edge versioning, …), the edge header grows to 3B anyway — at that
point unify all key types into `[ns 2B][flags 1B]` (dir at bit0, ns restored
to full 16 bits). The trigger is "edge needs a new discriminator", never
"ns capacity pressure".

## Phase 4 — Snapshots & packaging

- [ ] Snapshot export/import: rows → Parquet (engine-independent, via
      `KvEngine` scan surface); ns header reversed to descriptive text,
      column names = field names; index entries excluded (derived state,
      rebuilt deterministically on import). Uses: backup, data exchange,
      lakehouse analysis.
- [x] DataFrame bridge (ADR-0007): `Table::to_record_batch()` streaming
      rows → Arrow `RecordBatch` (schema generated by `RowEncode`), eager
      tier; `to_polars()` behind a `polars` feature; lazy pushdown deferred
      (load-gated, ADR-0007 Phase 2); scale-out via the Parquet snapshot
      path above.
- [x] Tooling as library interfaces (not a standalone `okm-cli` — the
      `Table<S, K, R>` binding is compile-time and user-typed, so an
      external binary would have no type context; users wrap these calls
      in their own thin bins):
      `Table::describe()` layout audit table (offsets/widths/TLV frames
      from the `FieldDesc` declaration, no compiler session);
      `parquet_io::export_parquet` / `import_parquet` snapshot round trip
      (feature `parquet`, import restores through `Table::put` so index
      entries rebuild); `Table::json_schema()` — JSON Schema of the
      exported row shape, columns isomorphic to the Parquet file
      (`x-okm-column-order` carries the authoritative column order);
      macro expansion dump to disk via
      `OKM_DERIVE_DUMP=<dir>` (formatted impl source per derive).
- [ ] Publish to crates.io (`okm-core`, `okm-derive`).
- [x] Push to github.com/orbsh/okm (repo referenced by wiki cross-links).

## Phase 5 — Event layer (ADR-0008): reduce rename, subscribe channels, okm-stream

- [x] Rename `okm` → `okm-core`: workspace member, directory, crate name,
      all `okm_core::` references **including ADRs** (user decision: update
      everything so greps stay truthful). Lands first; later phases build
      on the new name.
- [x] Rename aggregate → reduce (`#[kv_reduce]`, `ReduceLogic`,
      `ReduceCodec`, `reduce_get`, `scan_reduces`): semantics unchanged,
      name aligned to the role (stateful reversible reduction over the
      row-event stream).
- [ ] `#[kv_subscribe]`: per-annotated row type sends uniform-format events
      in the write path (sync `try_send`, no handler at the annotation
      site — consumers own the logic, combinators are the adapter); ≥1
      declaration emits a per-row-type `OnceLock` global mpsc + consumer
      accessor. Core stays synchronous; delivery is best-effort, policy
      declared by the subscriber.
- [ ] Event payload carries a monotonic write-batch epoch: gives consumer
      combinators an exact same-batch boundary (glitch-free folding within
      a table), not a debounce heuristic. Cross-table fan-in stays
      eventually-consistent — structurally no atomic "both updated" instant
      exists across independent puts (ADR-0009 §2). Decision lands with
      the payload format in this phase; okm-stream consumes it, does not
      manufacture it.
- [ ] Channel payload form: hand-written event Enum + alignment annotation
      `#[kv_subscribe(EnumName::Variant)]` (send site IS the variant →
      type mismatch is a compile error; match exhaustiveness is free).
      Raw per-type channels remain the fallback when no Enum is declared.
      Evaluated and rejected: (a) derive-writes-file, enum-macro-reads —
      proc-macro reruns only on file change, so the discovered-variants
      file goes stale under incremental compilation (missing/ghost
      variants), plus racy appends and no "run after all derives" ordering
      guarantee; (b) macro-parses-source-file — deterministic input, no
      cache issue, but locks subscription declarations to a single file
      (multi-module/crate breaks it) and pays a full re-parse to save one
      line per type. OPEN: one manual variant line per subscribed type is
      still unsatisfying — options to revisit: build-script based
      collection (rerun-if-changed on the source tree, codegen into a
      checked-in or OUT_DIR module; reliable ordering, costs a build
      step), or accepting the annotation as the single source of truth
      and generating the Enum FROM the annotation paths themselves via
      build script (same collection problem, but source-of-truth moves
      to the annotations — one declaration per type, no separate Enum
      to maintain). DECIDED (user): implement the event Enum via build.rs
      when landing stream functionality — no interim workaround. Precedent
      from fluxora `gen_dispatch!`: proc-macro file dependence CAN be made
      reliable with the `include_bytes!` HACK (forces dep-tracking on the
      parsed file) — that patch suffices when the input is static source
      code, but NOT when the input is a compile byproduct (okm-core scheme (a)):
      build.rs is the structural fix, its `rerun-if-changed` contract being
      the reliable equivalent.
- [ ] `okm-stream` crate: consumes the emitted receivers; Rx-style
      combinators (map/filter/merge/scan) + push-mode multi-table
      fan-in. Zero storage responsibility; pull-mode fan-in stays in
      `okm-query`.
- [ ] Doc pass: INTEGRATION/MODELING twins updated for the event layer
      (inline exactly-once vs channel no-guarantee boundary; reduce
      never a channel consumer; trigger asymmetry).
- [ ] Query recipes doc (okm-query): prefix scan + `group_by` composed
      into the SQL GROUP BY recipe (multi-level rollup by group-segment
      prefix; reduce's compile-time GROUP vs read-time `group_by` vs
      index-sort grouping — same encoders, different landing). The
      where-boundary record: prefix = physical where (free, selectivity
      belongs in key layout), `.filter()` = in-memory where (stdlib,
      no wrapper needed).

## Phase 6 — Commanded RMW: `Table::upsert_with`

User-complemented RMW next to the declarative one (reduce). Same underlying
shape — read old value, compute, write new — but imperative: arbitrary logic
in a runtime closure, one shared implementation for all tables (runtime
behavior, nothing per-type → belongs on `Table`, NOT okm-query (read-side
combinator layer, must not hold the write path) and NOT derive (static
per-type schema artifacts; upsert has none).

- [ ] `Table::upsert_with(key, f: impl FnOnce(Option<R>) -> R) -> Result<R>`:
      `get` → `f(old)` → `put(key, new)` via the normal write path, so index
      maintenance, reduce hooks and (after the event layer) channel emission
      all fire without special-casing. Returns the written row.
- [ ] Correctness boundary documented: single-writer only. OKM is an
      in-process library with a serial write order, so get→f→put cannot
      interleave — no CAS needed. The optimistic-CAS item in Phase 2 stays
      separate (multi-writer future, different mechanism). Same constraint
      that backs reduce's exactly-once.
- [ ] Integration test: upsert on missing key (old = None → insert path),
      on existing key (RMW path), index + reduce entries correctly updated
      through the put path.
- [ ] Doc: MODELING section pairing the two RMW forms — reduce
      (declarative, compile-time fold/unfold, framework-driven on the write
      path) vs `upsert_with` (commanded, runtime closure, caller-driven);
      boundary note that both rest on the single-writer constraint.
