# OKM — Implementation Plan

Design decisions live in `docs/adr/`. This plan tracks implementation status.

## Phase 1 — Core key/edge layer ✅ shipped

- [x] `KeyEncode` derive: fixed-width fields (`u32` / `u64` / `[u8; N]`), BE,
      compile-time `KEY_LEN` / `FIELD_WIDTHS`, `encode_prefix_named`.
- [x] `EdgeEncode` derive: `#[ok_head(field, …)]` per-endpoint identity width,
      2-byte direction-bit header (niche, ADR-0001), query methods on endpoint
      types.
- [x] `Collection<S, E>` assembly point, no `KvRecord` (ADR-0003).
- [x] Engines: `fjall` (sync, feature), `slatedb`
      (async, feature).
- [x] Hex layout-stability tests (`tests/integration.rs`).

## Phase 2 — Rows / Nodes (ADR-0006, value rules per ADR-0004) ✅ core shipped

- [x] `ObjEncode` derive: single item declares identity (`#[ok_ref]` key
      reference), payload fields, and `#[ok_index(...)]` access methods —
      one field list, three destinations (key / value / index).
- [x] Payload encoding: TLV frames `[tag u8][len u32 BE][value BE]`, tag =
      field declaration index (decoupled from names); fixed-width fields
      today, same frame kept for the variable-length regime later.
- [x] `Collection<S, K, R>` assembly point (formerly `Table`): plain generics,
      no assembly macro; holds the shared engine instance. `put(row)` writes
      the whole set — primary entry, one face per access method, embedded
      children with their faces, the releases the old document dropped, the
      reduce fold/unfold RMWs and any vocabulary growth — as ONE engine commit
      (landed 2026-10-22: the "internal buffer" half of ADR-0003's division of
      labour; `delete` and the whole-document writes (`merge` / `replace` / `purge`,
      ADR-0035) follow the same rule, and the reduce
      RMW reads through `BatchStore` so a fold sees its own batch's writes).
      `Row::table(store, ns)` constructor.
- [x] `save_into(batch, key, value)` cross-collection atomic path (ADR-0003
      mechanism note): encode into an externally owned engine batch without
      touching the collection's internal buffer; for primary + index across
      two assembly points (single-table case already covered by `put`).
      Landed 2026-09-11: `KvBatch` trait + `KvEngine::batch`/`commit_batch`
      (default carrier Batch — op list replayed; fjall overrides
      commit_batch with its native cross-keyspace Batch, one real WAL
      write). `Table::save_into` (primary + all index entries) and
      `EdgeTable::save_into` (forward + reverse) encode into the batch;
      save_into is the encoding surface — reduce folds / subscribe
      emission / overwrite unfold stay put-path only (documented on the
      method).
- [ ] Optimistic CAS write (`save_with_cas`, MVCC version compare-and-swap
      retry loop) — future extension from the original design doc; no ADR
      record yet.
- [x] `#[kv_version(n)]` versioned payload with lazy in-memory upgrade.
      Landed under the name `#[ok_layout(version = N)]` (renamed at
      implementation to avoid colliding with CAS/MVCC "row version";
      PLAN entry name stale until now). Wire: 1-byte layout version in
      the payload header; decode accepts any older version (append-only
      rule — missing tail fields take declared defaults) and rejects
      newer. "Lazy upgrade" = rewrite-on-read deliberately NOT done:
      unread old records stay in the old format (ADR-0004's definition
      — no write-bandwidth burn, the SQL ALTER TABLE contrast).
      Locked by codec_v2_test (older-decodes-with-defaults,
      newer-rejected, LAYOUT_VERSION constant).
- [x] Variable-length payload fields (String): the TLV frame's `len u32` IS
      the length prefix (no second one on the wire); `FieldType::Str` in
      the FieldDesc table (width 0 = variable); frame-by-frame walk in the
      Arrow bridge; key side still rejects it at compile time.
- [x] Field wrappers on row fields: `VarInt<T>` (LEB128, u16/u32/u64,
      variable frame), `Quant<P>` (f64 → scaled i64 wire, composes with
      `Reverse` for descending float order), `Enum<T>` (u8 tag via manual
      `impl EnumTag` — explicit tags, not positional), `Offset`
      (`#[ok_offset(base = N)]` → u32 displacement, out-of-range panics) —
      all shipped. `Reverse<T>` ✅ (+ `Reversible` compile-time whitelist:
      the eight fixed-width integer types, floats excluded — no fixed
      bit-flip inverts IEEE-754 order); one annotation applies to every
      destination the field is encoded into. Key/index positions reject
      all wrappers (fixed-width identity rule); descending prefix scan =
      newest-first.
- [x] Column-block regime — **cancelled** (2026-09-09, structural reason):
      `Delta<T>`/`Rle<T>`/shared-vocabulary `Offset` are cross-row
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

- [x] `#[ok_index(name { fields(…), includes(…) })]` on **row structs**.
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
      (a prefix would sort by length first and destroy vocabulary order);
      exact matching resolves via the trailing primary key + fetch-back.
      Function indexes (`func(path)`) shipped alongside: the sort segment
      is the declared function's result (`IndexFuncResult` — String →
      UTF-8 vocabulary order, uints → BE numeric order); the query side
      calls the same function on its probe, so normalization cannot
      drift between encode and scan.
- [x] Deprecating an index declaration: removal is NOT allowed (declaration
      order is a persistent contract — deleting a middle entry shifts every
      later slot onto stale data, the same silent-corruption class as the
      rejected additive ns derivation). Instead: `#[ok_index(name { … },
      deprecated)]` keeps the slot reserved but generates no write path, no
      marker struct, no scan surface; `Table::prune_deprecated_slots()`
      prefix-scans `[ns][deprecated slot]` and deletes the stale entries
      left from before the deprecation (idempotent, returns the count).
      Row::DEPRECATED_SLOTS is derive-emitted. Tests lock: slot reservation,
      no writes to the deprecated slot, prune, no-op prune without
      deprecated declarations.
- [x] Partial indexes (`where(path)`, ADR-0019): a declared row-level
      predicate on either index form — a rejected row contributes no
      entries. Expanded as a `KvIndex::admits` override (trait default
      `true`), consulted once per document at the head of `entry_pairs`
      (the single generation point shared by put/delete/save_into). Not
      part of the entry address; scans untouched. A `func` returning an
      empty `Vec` remains the per-value drop. Tests lock admission,
      coexistence with `includes` covering, delete symmetry, the empty-Vec
      idiom, and the dangling-entry behaviour of a predicate flip (the
      pre-existing overwrite contract).

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

- [x] Snapshot export/import: rows → Parquet (engine-independent, via
      `KvEngine` scan surface); ns header reversed to descriptive text,
      column names = field names; index entries excluded (derived state,
      rebuilt deterministically on import). Uses: backup, data exchange,
      lakehouse analysis. Landed (tooling.rs parquet_io: export_parquet /
      import_parquet through Table::put — the normal write contract, so
      index entries rebuild on import). Fix that surfaced with it:
      scan_rows_raw / scan_keys now filter slot-0 only — the ns segment
      also holds index entries (slots 1+) since Phase 3, which leaked
      into exports as garbage rows.
- [x] DataFrame bridge (ADR-0007): `Table::to_record_batch()` streaming
      rows → Arrow `RecordBatch` (schema generated by `ObjEncode`), eager
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

## Phase 5 — Event layer (ADR-0008): reduce, subscribe, okm-stream

- [x] Rename `okm` → `okm-core`: workspace member, directory, crate name,
      all `okm_core::` references **including ADRs** (user decision: update
      everything so greps stay truthful). Lands first; later phases build
      on the new name.
- [x] Rename aggregate → reduce (`#[ok_reduce]`, `ReduceLogic`,
      `ReduceCodec`, `reduce_get`, `scan_reduces`): semantics unchanged,
      name aligned to the role (stateful reversible reduction over the
      row-event stream).
- [x] `#[ok_subscribe]`: per-annotated row type sends uniform-format events
      in the write path (sync `try_send`, no handler at the annotation
      site — consumers own the logic, combinators are the adapter); ≥1
      declaration emits a per-row-type `OnceLock` global mpsc + consumer
      accessor. Core stays synchronous; delivery is best-effort, policy
      declared by the subscriber.
- [x] Event payload carries a monotonic write-batch epoch: gives consumer
      combinators an exact same-batch boundary (glitch-free folding within
      a table), not a debounce heuristic. Cross-table fan-in stays
      eventually-consistent — structurally no atomic "both updated" instant
      exists across independent puts (ADR-0009 §2). Decision lands with
      the payload format in this phase; okm-stream consumes it, does not
      manufacture it.
- [x] Subscribe payload form: the event Enum is generated by build.rs from
      `#[ok_subscribe]` (bare) declarations — the variant IS the row type
      name, the enum name is `RowEvent` by default and renameable per-row
      via `#[ok_event_enum(Alias)]` (multiple aliases = multiple enums,
      each with its own `SINK_<ENUM>` cell). Zero hand-written
      mapping: send site and enum are derived from the same declaration,
      so drift is a compile error and match exhaustiveness is free. The
      per-row-type bare subscription (degraded fallback) is explicitly
      UNSUPPORTED (user decision 2026-09-11) — `#[ok_subscribe]` rejects
      arguments, the derive emits no per-row statics, and
      `SinkCell`/`Event` remain core library types for hand-assembled
      streams only. Evaluated and rejected: (a) derive-writes-file,
      enum-macro-reads —
      proc-macro reruns only on file change, so the discovered-variants
      file goes stale under incremental compilation (missing/ghost
      variants), plus racy appends and no "run after all derives" ordering
      guarantee; (b) macro-parses-source-file — deterministic input, no
      cache issue, but locks subscription declarations to a single file
      (multi-module/crate breaks it) and pays a full re-parse to save one
      line per type; (c) hand-written Enum + variant-path annotation —
      one manual line per subscribed type and a second source of truth
      the annotation can drift from. Precedent
      from fluxora `gen_dispatch!`: proc-macro file dependence CAN be made
      reliable with the `include_bytes!` HACK (forces dep-tracking on the
      parsed file) — that patch suffices when the input is static source
      code, but NOT when the input is a compile byproduct (okm-core scheme (a)):
      build.rs is the structural fix, its `rerun-if-changed` contract being
      the reliable equivalent.
- [x] `okm-stream` crate: consumes the emitted receivers; Rx-style
      combinators (filter/map/with_previous/distinct_by) + push-mode
      pipelines registered directly as `EventSink`s. Zero storage
      responsibility; pull-mode fan-in stays in `okm-query`. Landed
      2026-09-11: synchronous by default (the write path's try_send
      drives combinators inline; `tokio` feature defers the executor
      bridge), field-level subscription as `filter_field`, change
      detection as `distinct_by` — consumer-side interest, per the
      `Event.old` rejection below. Multi-table fan-in via the
      generated per-enum sinks (each `SINK_<ENUM>` is its own
      stream head); merge/fan-in combinators over multiple heads land
      with the first consumer that needs them.
- [x] Doc pass: INTEGRATION/MODELING twins updated for the event layer
      (in-write exactly-once vs subscription no-guarantee boundary; reduce
      never consumes a subscription; side-effect asymmetry). MODELING.md /
      MODELING.zh-CN.md landed 2026-09-11 ("Write-path events: reduce and
      subscribe" — epoch semantics, both disciplines, transport
      boundary); INTEGRATION twins cover the side-effect asymmetry.
      (Vocabulary unified 2026-10-22: channel → subscribe, trigger → the
      write-path-edge side effect; the landed texts were re-worded to match.)
- [x] Query recipes doc (okm-query): prefix scan + `group_by` composed
      into the SQL GROUP BY recipe (multi-level rollup by group-segment
      prefix; reduce's compile-time GROUP vs read-time `group_by` vs
      index-sort grouping — same encoders, different landing). The
      where-boundary record: prefix = physical where (free, selectivity
      belongs in key layout), `.filter()` = in-memory where (stdlib,
      no wrapper needed). Landed 2026-09-11 (docs/query-recipes.md,
      query-recipes.zh-CN.md; README links it).

### Preset reduce combinators (ADR-0023, accepted 2026-09-22 — shipped 2026-09-23)

- [x] `okm_core` ships `Count`, `Sum`, `HighWater`, `LowWater` as generic
      `ReduceLogic` impls; `#[ok_reduce(Count { group(..) })]` resolves
      by name — no new attribute/wire/slot rules. Integer-only (u64);
      unfold ambiguity (watermark vs shrinkable extreme) resolved by
      NAME: only the HighWater/LowWater variants ship — a true un-max needs a second
      structure (exactly the ceremony presets retire), so bare `Max`/
      `Min` are deliberately absent (ADR amendment). The whole-table
      single-group mode drops the group block: `#[ok_reduce(Count)]` →
      entry `[ns][slot]`, no group segment. NOT built into the engine:
      row count stays one declaration away, not a write-path tax on
      every table. Semantics in core (`model/presets.rs`), the derive
      emits a local forward marker + field source (coherence: a foreign
      generic cannot receive the local `Reduce` impl). `LowWater`'s acc
      is `LowAcc` (Default = identity `u64::MAX`) — the typed RMW seeds
      accs with `Default::default()`, a bare u64 cannot carry the
      minimum's identity. Tests in `preset_reduce_test.rs` (grouped +
 no-group + key-field aggregation via ADR-0024). Sum's acc type
 follows the field: unsigned → u64, signed (i8–i64) → i64,
 Quant<f64, P> → its exact fixed-point i64 wire (never a bare
 float); this required signed-integer payload encoding in the
 derive (same fixed-width BE wire, no new FieldType kinds).
 Watermark presets stay unsigned-only (KeyEncode carries no sign;
 the watermark contract is an unsigned-domain contract).

### Reduce hooks receive the decoded key (ADR-0024, accepted 2026-09-22 — shipped 2026-09-23)

- [x] `ReduceLogic` hooks become `fold/unfold(acc, key: &Self::Key, item)` —
      the write path already holds the decoded `&Key` at every call site
      (`__okm_apply_reduces(store, key, ..)`); a parameter pass, never a
      decode. GROUP may name key fields (two-source rule per
      `KvIndex::encode_named`; name in both sources resolves to the KEY —
      the compile-error rule demoted to key-wins: key fields are invisible
      at the DocumentEncode expansion point, see the derive's
      `__okm_encode_group_named`).
      okm-dynamic `ReduceLogic` + Python `add_reduce` callables landed in
      the same batch (dynamic key = decoded key-field map, not bytes).
      Breaking: all in-tree fold/unfold impls take the new parameter.
      Tests: `reduce_test.rs::group_and_fold_use_key_fields` (key-field
      group + key-field fold).
      Retires the mirror-field pattern (aura MaxInstanceId → ADR-0023
      `HighWater` over a key field).

## Phase 6 — Commanded RMW: `Table::upsert_with`

User-complemented RMW next to the declarative one (reduce). Same underlying
shape — read old value, compute, write new — but imperative: arbitrary logic
in a runtime closure, one shared implementation for all tables (runtime
behavior, nothing per-type → belongs on `Table`, NOT okm-query (read-side
combinator layer, must not hold the write path) and NOT derive (static
per-type schema artifacts; upsert has none).

Phase 6 is also the **baseline for event-layer granularity decisions**
(user principle: analyze against the final state, not a temporary one —
a premise that a planned phase will invalidate is not an exclusion
argument). With `upsert_with` as the standard write path, the old row is
in hand on every write, so field-level diff is a free byproduct; any
field-granularity subscription/filtering must be priced against THAT
world, not the current read-free put. Phase 6 lands before okm-stream
for exactly this reason — it is the foundation the stream layer's
change-detection combinators rest on.

Event granularity — evaluated and rejected: `Event.old` (attaching the
pre-write row snapshot to every event). The consumer side can derive the
same at zero write-path cost: okm-stream combinators are stateful anyway
(scan/merge/fold carry buffers), so a per-key previous-row cache in a
`with_previous`/`distinct` combinator gives diff/change-detection without
doubling the clone tax on every write. Events stay the minimal fact
(op/epoch/key/row); before/after semantics are a consumer-side derivation,
matching the best-effort subscription contract (precision-bound consumers use
inline reduce). Field-level subscription is therefore an okm-stream
combinator capability, never a declaration-side feature.

Put overwrite fix (landed with Phase 6): `put` on an existing key now
unfolds the stored row from every reduce group before folding the new
row — previously the second write double-counted (count=2 where the
table holds one row). The overwrite detection read is exactly the
prerequisite the event-granularity analysis priced against: with the
old row in hand on every write, field-level diff is a free byproduct
and the Phase 6 baseline premise is now real, not planned.

- [x] `Table::upsert_with(key, f: impl FnOnce(Option<R>) -> R) -> Result<R>`:
      `get` → `f(old)` → `put(key, new)` via the normal write path, so index
      maintenance, reduce hooks and (after the event layer) subscription emission
      all fire without special-casing. Returns the written row.
      Landed 2026-09-11 together with the put overwrite fix (below).
- [x] Correctness boundary documented: single-writer only. OKM is an
      in-process library with a serial write order, so get→f→put cannot
      interleave — no CAS needed. The optimistic-CAS item in Phase 2 stays
      separate (multi-writer future, different mechanism). Same constraint
      that backs reduce's exactly-once. Documented in the upsert_with
      doc-comment (table.rs) and the MODELING RMW section.
- [x] Integration test: upsert on missing key (old = None → insert path),
      on existing key (RMW path), index + reduce entries correctly updated
      through the put path (tests/upsert_test.rs: all four paths asserted
      — insert, RMW, index scan, reduce fold, subscription emission).
- [x] Doc: MODELING section pairing the two RMW forms — reduce
      (declarative, compile-time fold/unfold, framework-driven on the write
      path) vs `upsert_with` (commanded, runtime closure, caller-driven);
      boundary note that both rest on the single-writer constraint. Landed
      2026-09-11 ("Two read-modify-writes" in MODELING.md /
      MODELING.zh-CN.md), which also updated the event-layer section for
      the bare-only ok_subscribe form (build.rs-derived enum).

## Phase 7 — VirtualStorage: engine as boundary, remote backend, NestStorage derive (ADR-0010)

Implementation naming note: the receiver is declared as
`#[derive(NestStorage)]` + `#[ok_ns(N)]` (okm-core/src/nest.rs) — the
same `#[ok_ns]` attribute every other derive reads, so one attribute
keeps one meaning: "declare this side's ns number". The receiver that
nests an existing engine behind that declared prefix and executes
frames on it.

2026-09-14 refinements (all landed): okm-wire frames unified — one
`OpFrame` (tag covers put/delete/get/scan, reads carry an empty value
segment) + one `OpResponse` (`[has_value][value][suffixes]`), replacing
the WriteFrame/ReadFrame/ReadResponse trio. Receiver intake unified —
`NestStorage::apply(bytes) -> Option<OpResponse>` is the single
transport-free surface (every decodable frame answers: a write's answer is
the empty ack, a refusal rides inside it as `fault`, ADR-0038; `None` =
the frame does not decode); the mpsc
pump and a WS adapter call the same `Arc<NestStorage>`; sender side is
plain `VirtualStorage` (`RemoteStore`), unaware of nesting. The unused
`kv_nest` attribute registration was dropped — `#[ok_ns]` is the one
prefix-declaring attribute. Full-keys-in-storage decision recorded as
ADR-0011; the index-entry header rule (header carried once, primary
key bare in the tail) recorded in ADR-0005.

- [x] Trait boundary rename: `KvEngine` → `VirtualStorage` (module
      `engine` → `storage`; async twin `KvEngineAsync` →
      `VirtualStorageAsync`), shipped 2026-09-12. The trait speaks only
      encoded bytes (put/get/del/scan_suffix/batch/commit_batch — no
      method changes, no alias layer). Four backend shapes:
      fjall / slatedb / redb / remote; backend struct names
      unchanged. redb shipped 2026-09-12 (feature 'redb'): single-file
      B-tree, read-deterministic complement to fjall's LSM; batch =
      one write transaction; in the TestStore engine matrix
      (engine_matrix_test runs the same verification across all
      enabled backends).
- [x] Remote backend (sender side): impl VirtualStorage; write = Batch
      op list hand-framed with counted lengths into a frame
      `[op][batch bytes]` — no serde/postcard, the frame is counted
      fields, not a protocol (ADR-0010 §2 frame layout; codec lives in
      the zero-dependency `okm-wire` crate). Fire-and-forget (one frame =
      one receiver WAL commit; channel order = write order). Read =
      request frame + response — correlation is the consumer's choice
      inside the trait impl (an mpsc round trip is the reference example,
      not the contract). Transport is backend-internal (in-process
      channel / UDS / existing WS connection) — fixed at assembly, no
      declared endpoint.
- [x] `NestStorage` derive: empty struct + prefix declaration (`#[ok_ns
      (N)]`) → NO data methods, exactly one execution surface (`serve`
      engine → `Arc<NestStorage>` whose `apply(bytes) -> Option<OpResponse>`
      is the transport-free intake (every decodable frame answers once — a
      write's empty ack included — and a refusal is a `fault` in it); prepend declared prefix → plain
      byte-level engine execution → response frame back). Receiver
      holds no OKM semantics; storing garbage is indistinguishable from
      storing data. Declaration shares `#[ok_ns]` with every other
      derive (no separate attribute).
- [x] ns declaration moves to the Table side: `#[ok_ns(...)]` read by
      ObjEncode/EdgeEncode (declared, never hand-filled at `Table::new` —
      the ns parameter disappears from the constructor); KeyEncode's
      `ok_ns` attribute registration removed. Declared on the ROW struct
      (the row is the table's declaration point: `#[ok_ref]` pins the
      key type, so the row determines `Table<S, K, R>` entirely) and
      emitted as `Row::NS_PREFIX` (`&'static [u8]`, big-endian `[ns 2B]`);
      all hooks (`index_entries`, `__okm_apply_reduces`, `entry_key`,
      `entry_prefix`, `scan_index`) take the prefix slice, not a u16. A
      key type carries no ns: the same key shape may serve several rows /
      tables, each with its own declared ns. Single ns per row type (one
      OKM = one domain model); the engine choice stays
      per-assembly-point (local/remote freely mixable — remote is just
      another KvEngine impl).
- [x] Benchmarks (criterion, `okm-core/benches/core_paths.rs`): baseline
      recorded 2026-09-12 in
      [internals/benchmarks.md](internals/benchmarks.md) — key encode
      5.5ns / decode 3.0ns, payload encode 92ns (TLV+String dominated),
      index scan ~120ns/row (linear in fanout), semantic put 1.3µs vs
      raw batch op 41ns, dynamic codec tax measured (key ~8×, payload
      on par). Engine benches (fjall/slatedb/remote round trip) land
      with their integrations; re-run with
      `cargo bench -p okm-core --bench core_paths -- --baseline initial`.
      No CI regression gates initially.
- [~] Dynamic codec (Python first, then Steel): schema-driven
      encoder/decoder/scan built from structured schema exports —
      in-process use for embedded-language Booths. Capability ceiling
      RESCOPED (2026-09-22, ADR-0022): subscribe stays excluded; func/
      partial indexes and reduce become binding-implementable under the
      deployment-shape contract (see the ADR-0022 checklist item). Core
      shipped 2026-09-12: `okm-core::schema::
      CollectionSchema::of` (structured export, serde behind `schema-serde`) +
      `okm-dynamic` crate (Value tree; encode/decode mirroring the derive's
      byte layout; version gate + unknown-tag skip); cross-language byte
      equality locked by dynamic_cross_test. Shipped 2026-09-17: version-
      default migration — literal `#[ok_default]` exports through
      `Row::DEFAULTS` (const `DefaultValueConst`, &'static str for Str;
      `"x".to_string()` unwrapped) into `FieldSchema::default` (owned,
      serde'd); okm-dynamic decode fills absent hot fields (truncated
      tail, no longer an error) and cold fields from schema defaults,
      zero fallback per kind when no literal. Locked by
      version_default_migration_on_dynamic_read (v2 bytes through a v3
      schema). Shipped 2026-09-17: PyO3 binding (`bindings/okm-python`,
      maturin, pyo3 0.25) — `Schema.from_json` parses the serde'd
      CollectionSchema; `encode_key`/`encode_payload` coerce Python scalars to
      each field's schema kind (Python ints carry no width); `decode_*`
      return dicts. Verified both directions byte-identical with the Rust
      derive (verify.py: Rust→Python read + Python→Rust decode). Also
      fixed: CollectionSchema Serialize omitted `slots` while Deserialize
      required it (round-trip asymmetry). Steel binding shipped 2026-09-17
      (`bindings/okm-steel`, steel-core 0.7): `register(vm)` installs
      `okm-schema-from-json!` / `okm-schema-version!` / `okm-encode-key!` /
      `okm-encode-payload!` / `okm-decode-key!` / `okm-decode-payload!`;
      schema handles are integer ids into a thread-local registry (steel's
      Custom-type escape hatch is sealed); bytes cross as vectors of
      integers (ByteVector field is crate-private); VM round trip locked by
      vm_roundtrip (Rust writes → steel reads → steel re-encodes byte-
      identical).
- [~] Schema reverse-import (Python/Steel-declared key/row/table → Rust
      runtime execution) — REJECTED (2026-09-22). The Rust runtime cannot
      predict dynamically-added Python/Steel schemas, so a reverse import
      would force the Rust side into the same dynamic mode, which is what
      okm-dynamic already provides on the bindings side (no increment).
      Positioning: Python/Steel are BINDINGS — they never interact with
      the Rust parts directly (transparent channels like
      NestStorage/remote excepted).
- [x] Bindings semantic alignment — ADR-0022 (2026-09-22): the dynamic
      codec's "permanent ceiling" is rescoped. Bindings implement
      semantics via host-language callables (binding-time registration):
      func/partial indexes (`Schema.add_func_index`, `admits` callable)
      and reduce (`Schema.add_reduce` with fold/unfold callables +
      declared acc codec). Subscribe stays excluded (write-path
      broadcast, not a per-document derivation). Deployments:
      embedded (Python owns the engine, single writer, in-process
      calling discipline) and remote/Aura (callables run in the booth,
      operation payloads carry semantic RESULTS — derived entry bytes,
      absolute acc values, old document for overwrite unfold; remote
      executes as one framed batch, never hosts callables). Precondition
      for remote reduce: single-writer-per-group (Aura's partitioned
      booth model satisfies it); steady-state puts carry an absolute
      acc at zero extra round trips, only restart recovery reads once;
      document write + acc update are atomic via the framed batch.
      Acceptance: (1) embedded — calling-discipline test (put/delete/
      overwrite vs accumulator); (2) remote — Python-booth operations
      land byte-identically to Rust-side (cross-language byte equality,
      extended from codec bytes to semantic entries). Implementation:
      okm-dynamic `AccessMethod` func/admits variants + `DynamicCollection`
      reduce calling discipline; okm-core untouched. Shipped 2026-09-22
      (Rust-side base): `AccessMethodKind` (Plain/Partial/Func — callables
      return encoded bytes, entry lifecycle stays dynamic-side);
      `DynamicCollection::with_reduces` + the put/overwrite/delete calling
      discipline (`apply_reduces`: put folds, overwrite unfolds old then
      folds new, delete unfolds — one get→callable→put per group, same
      engine instance); byte-transparent acc (host owns the layout, seed
      on first fold). Locked by dynamic_semantics_test.rs: func fan-out
      sweep, partial admission flip, full discipline sequence, and
      reduce-entry layout parity against the Rust-side key format. Shipped
      2026-09-22 (continued): the reduce side converged to ONE host object —
      `okm-dynamic::ReduceLogic` trait (seed/fold/unfold; `seed` is the
      `ReduceCodec: Default` counterpart, called when a group entry is
      missing on the fold arm; unfold hitting a missing entry is a discipline
      violation, not a zero group) replacing the free-callable pair and the
      empty-bytes seed hack. Python binding (`bindings/okm-python`): `Table`
      class wrapping DynamicCollection (embedded mode, Python-owned engine)
      with `add_func_index(slot, func)` / `add_partial_index(slot, fields,
      admits)` / `add_reduce(slot, group_fields, logic)` binding-time
      registration — Python callables bridge into the Rust-side trait
      objects (GIL acquired per call); put/get/delete/scan/reduce_get/
      scan_reduces exposed, schema-coerced. Locked by
      accept_embedded.py: registration → put folds → overwrite no drift →
      cross-group overwrite → delete unfolds → func fan-out sweep →
      partial admission flip. Shipped 2026-09-22 (remote mode, A route):
      the put/delete expansion extracted into a SHARED plan surface
      (`okm-dynamic::plan` — `plan_put`/`plan_delete` are pure functions
      over caller-held state: old document + acc lookup closure; the
      embedded `put`/`delete` is plan + local engine replay, so remote
      plans land byte-identical by construction). Plan-local acc overlay:
      within one plan the fold arm reads the acc the unfold arm wrote
      (same group overwrite) — matches the receiver's in-frame execution
      order. Python binding grows `plan_put`/`plan_delete` (dict in →
      `(wire frame bytes, new_accs receipt)` out; `decode_stored` refills
      the booth's old-document cache) — Python wraps, all encoding lives
      Rust-side. okm-wire gains `OpFrame::write_batch` (the ADR-0010 §2
      Batch→frame mapping; `RemoteStore::commit_batch` now shares it;
      zero new op tags). Locked by accept_remote.py: the same scenario
      (fresh put / same-group overwrite / cross-group move / delete)
      planned Python-side and Rust-side lands hex-identical frames.
      Steel callable surface remains open work.
      - [x] Variable-width trailing index field (2026-10-09, aura
      Phase 4.13's alignment item): `AccessMethod` accepts a variable-
      width (Str) field as the LAST index field — raw UTF-8, no length
      prefix (the `IndexFuncResult for String` text-first regime), the
      fixed-width pkey tail (`schema.key_len`) cuts the segment; the
      order rule "at most one variable-width field, last position only"
      is shared by fields AND includes (`check_field_order`, the derive's
      `okm-derive/src/schema.rs` rule mirrored). `find_field` widens to
      cold fields (Str lives there). Equality probes may exceed the
      fixed-width head exactly when the declaration's single field is
      the trailing variable one. Locked by dynamic_table_test:
      dynamic_variable_width_index_matches_typed_bytes (byte equality
      with the typed path + equality-probe fan-out + overwrite sweep)
      and dynamic_variable_width_index_declaration_errors. Consumer:
      aura scan-route probes match String fields directly (the aura-side
      event-flow.md §8.1 residual closes when aura adopts it).
      - [x] Path indexes (2026-10-14, ADR-0029): declared paths over the
      schema-free zone. `Path` component vocabulary (Field / Index / Key
      — two namespaces, two walker operations, never one niched integer)
      + a self-delimiting canonical encoding with NO leading component
      count (a count would make DEPTH the primary sort key and break
      "a path is a byte prefix of its extensions", which is what keeps a
      subtree scan one prefix scan and array wildcards free); one slot
      per declared path with the path OUT of the entry key; the value
      type declared at declaration (only U8/U16/U32/U64/FixedBytes/Str).
      Maintenance hangs off the DYNAMIC-segment write path
      (the whole-document writes `merge` / `replace` / `purge`, ADR-0035; the
      primary path's `index_entries`
      skips `Path` declarations by construction — one maintainer per
      entry set), and the old map is read only when a path index is
      declared, so the plan surface and every binding call site are
      untouched. Unsigned kinds COERCE at the index boundary: the dynamic
      read side widens every unsigned kind to U64, so a strict variant
      match would make the written map and the read-back map produce
      different entry keys and the overwrite sweep would leave the old
      entry behind. Locked by path_index_test.rs (nested + top-level
      resolution, array fan-out, pinned index, missing path / type
      mismatch = no entry, entry layout lock, overwrite sweep, delete
      sweep) and path.rs unit tests (parse forms, resolution, no-count
      prefix property, compact-lane boundary, encode/decode roundtrip).
      Open (not silently dropped): backfill for a path declared over
      data that already exists (ADR-0020 range scan + one batch is the
      shape), a durable path→slot registry, and a host-language
      declaration surface (okm-python's `add_func_index` /
      `add_partial_index` have no `add_path_index` counterpart yet —
      declarations are Rust-side only for now).
- [x] Multi-tenancy: receiver-side prefix only — a remote OKM instance is
      one application = one domain model = one ns; to the receiver it is
      just another prefix. No app_id layer inside OKM, no multi-level ns
      declaration, no reserved values; internal tenant sharding is a
      plain key field (business concern, same modeling). Receiver key =
      pure concatenation `[receiver prefix][ns 2B][sender payload]`,
      ns opaque to the receiver (ADR-0002 sketch promoted; discipline
      untouched). Shipped 2026-09-12: the mechanism is exactly the
      `NestStorage` derive (one declared executor + prefix per
      application) + `NestStorage`'s concatenating `hosted_key` — there
      is no separate multi-tenant code path. `multi_tenant_test.rs`
      covers one shared engine / two hosts (disjoint prefix segments,
      identical sender keys), internal tenant sharding as a plain key
      field, and the Table write path landing inside the tenant's
      segment.
- [x] Bare shard host: `NestStorage::bare` — NO prefix, frames execute
      byte-identical (the sender's keyspace IS the engine's keyspace).
      Serves sharding of one business domain: N shards = N bare hosts
      behind the orchestrator's partition-key routing (e.g. Aura's);
      OKM adds zero checking or machinery. Prerequisite: domain-model
      consistency across the shard instances pointing at one host —
      guaranteed by deployment (same binary per shard), not decidable
      at runtime. Coexists with hosted hosts on one engine (2-byte
      segment disjointness; the orchestrator allocates hosted segment
      numbers off the bare shards' in-domain ns numbers — an
      allocation duty at the global-view layer, not a runtime check;
      hosted apps may themselves shard as bare-hosted instances).
      Enabler: `SharedVirtualStorage` trait — hosts require genuinely
      shared engines (handle semantics, not deep copies); the test engines share
      via Arc kernels. `NestStorage` (transport-free intake:
      apply) factored out for WS/UDS adapters; mpsc
      pumps remain the reference transport. bare_shard_test locks
      byte-identical execution, shard-table ns segments, and
      bare+hosted coexistence.
- [~] Key-segment composition primitive — REJECTED (2026-09-12, evaluated
      and dropped). The scenario was hypothetical: it existed to justify
      moving `#[ok_ns]` off `KeyEncode` (a key type may legitimately serve
      several tables). With ns now on the row, that justification is
      moot; a composition need, when it actually appears, is trivially
      served by redeclaring the fields (they are 2-3 plain fixed-width
      fields — manual re-encoding is ~zero cost and keeps the flat
      named-field DDL). A `KeySegment` trait + derive recursion was
      prototyped to working width-composition, but dropped: real use
      cases are rare, the trait layer + atomic-segment slicing rules add
      machinery that would sit idle, and the flat key encoding is the
      discipline worth keeping. If a genuine repeated need shows up, the
      prototype design (KeySegment: WIDTH/DESC/put/take, blanket impl
      over KeyEncode) is the starting point.

## Phase 8 — Object model: one encoding, field-name vocabulary, kv_ → ok_ (ADR-0012)

Design locked in [ADR-0012](adr/0012-object-model-and-field-dictionary.md).
Core idea: a declared row IS a doc with an empty dynamic segment — one
encoding, one derive family. No separate document storage mode.

- [x] Rename: attributes `kv_` → `ok_` (`ok_ns`, `ok_index`, `ok_ref`,
      `ok_head`, `ok_subscribe`, `ok_default`, `ok_event_enum`, plus
      `ok_reduce`/`ok_offset`/`ok_layout` caught in the sweep); derive
      `RowEncode` → `ObjEncode`; docs' concept vocabulary follows. Shipped
      2026-09-14 (ec07a33) — landed before crates.io publishing as required.
- [x] Slot allocation revision: slot 1 = obj dynamic segment, slot 2/3 =
      field-name vocabulary (bidirectional), slot 4–13 reserved (two-ended
      growth buffer), slot 14/15 = edge fwd/rev, slot 16+
      indexes/reduces. Existing rows byte-compatible (they use none of
      the new slots). Shipped 2026-09-16 (f4c9136).
- [x] Dynamic segment (slot 1): one entry per obj, value =
      `([field-id][value-type][len u32][bytes])*`; value-type byte is a
      small closed enum (int/uint/float/str/bytes/bool/null/array/
      obj-reserved). Shipped 2026-09-16 (cdefdea): ObjValueType tags
      (EnumTag discipline), obj_dynamic put_frame/decode_variants,
      unknown-type skip, malformed-tail partial results.
- [x] Field-name vocabulary: run-time per-table append-only vocabulary;
      first-seen name claims the next number (single-writer, engine
      mutex); id `0xFF` escapes to `[0xFF][u16 id]` — no renumbering,
      ever; flat bidirectional point lookups (slots 2/3), no trie.
      Shipped 2026-09-16 (d18cb36): VocabCache (by_id/by_name/next_id),
      wholesale lazy load, id_for allocates one batch both directions.
- [x] Indexing rule: declared fields only; a dynamic field becomes
      indexable by being declared (schema evolution, deliberately manual).
      Not a separate mechanism: index entries derive from `R::FIELDS`
      (declared) only; `set_object` routes unknown names to the dynamic
      segment, so nothing undeclared can reach a slot ≥ 16.
- [x] Schema export: CollectionSchema extended with SlotMap (fixed-role slot
      numbers) and the ObjValueTypeSchema tag enum for the dynamic
      reader (okm-dynamic / Python side). Shipped 2026-09-16.
- [x] Read/delete API over the two slots: `get` returns the typed struct
      unchanged (declared fields only); `get_variants(key) ->
      BTreeMap<String, Value>` reads slot 1 with **name keys directly** —
      the id never appears in a public signature (no use case: iteration,
      addressing, and writes are all name-first; the vocabulary is a
      per-table append-only map cached wholesale on first access, so a
      name lookup is one hash probe); `get_object(key) ->
      Option<BTreeMap<String, Value>>` merges declared (FieldDesc names)
      and dynamic (vocabulary names) into one view — a combinator over
      the two, not a base op. `delete` / `delete_by_pkey` cover both
      slots (primary + dynamic segment) in one engine batch; scans stay
      index-slot only (dynamic fields are not indexable).
- [x] Write side mirrors the read side: `set_variants(key, map)` writes
      the slot-1 entry (first-seen names allocate, single engine batch);
      `set_object(key, map)` is the name-keyed whole-obj write — fields
      present in the row struct go to the typed path (slot 0, existing
      put semantics), the rest land in slot 1; unknown names follow the
      dynamic rule (first-seen allocation), not rejection (that rule is
      for the typed decoder, where a mismatched value tree is a caller
      bug).
- [x] `okm-dynamic::Value` gains the dynamic-segment value types
      (I64/F64/Bool/Null added; Array/Obj stay dynamic-segment-only —
      the typed static region rejects them until schema kinds land).
      Nested obj lives in okm-core's `DynamicValue` (wire tag 7), which
      is the only value tree the dynamic segment needs.
- [x] Vocabulary cache is **bidirectional**: `VocabCache { by_id: HashMap<u16,
      String>, by_name: HashMap<String, u16>, next_id: u16 }` under one
      `OnceLock<RwLock<…>>`. Read path needs id→name (slot 2 mirror),
      write path needs name→id (slot 3 mirror), first-seen allocation
      needs `next_id` + one engine batch writing both slots + cache
      update on both sides. Consistency rests on the single-writer
      discipline (engine mutex serializes allocation); multi-process
      shared engines degrade the cache to connection lifetime (reload on
      reconnect) — acceptable.

Unchanged: primary payload layout `[version][hot_len][hot][cold TLV]`
(ADR-0011 full keys; ADR-0006 row model for declared fields).

## Phase 9 — wrappers: `Option<T>`

- [x] `Option<T>` wrapper in `okm-core/src/wrappers/` (family: `Enum<T>` /
      `Offset<T>` / `Quant<P>` / `VarInt<T>`): fixed-width encoding for
      optional declared fields — wire `[present u8][T wire bytes]`,
      total width `1 + T::WIDTH`, None zero-fills the value bytes. Keeps
      hot-segment eligibility (O(1) offsets) for fields that would
      otherwise be forced into cold TLV; distinguishes None from a real
      value (`Some(0)` is not `None`). Shipped 2026-09-16 (2dd5385):
      u8..u64/[u8;N] primitives; Enum/VarInt/Quant/Reverse compose via
      their own contracts. (Derive field-position recognition: see the
      REJECTED entry below.)
- [x] First use cases: watermark/cursor fields where 0 is a real value
  (MQ cursor keeps its 0 semantics — a plain u64 stays correct there;
  `Option` targets genuine None/Some distinctions: config overrides,
  optional foreign keys). Aura MQ audited 2026-09-16: no field needs
  `Option` today — its tables are pure typed-path (put/get/scan), so no
  migration was required by the obj work.
- [x] Derive field-position recognition: REJECTED (2026-09-20) — the
      derive-recognition followup is closed without implementation. The
      wrapper type stays for hand-written OptionalEnc use, but declared
      `Option<T>` fields are not supported and no use case exists
      (aura audit: zero Option fields; the fixed-width presence byte
      buys nothing over a plain field + sentinel for the few genuine
      None/Some cases). Do not re-propose.

## Phase 10 — edge keys via slots: retire the direction-bit niche (high priority)

- [x] Replace ADR-0001's direction-bit niche with plain slot allocation:
      edge forward = slot 14, edge reverse = slot 15 (top of the fixed
      nibble region, growing toward the middle — heap/stack shape; see
      the revised slot table). Header becomes the raw ns big-endian value
      (`head_bytes` = `ns.to_be_bytes()`, `DIR_BIT` deleted) — no
      transform, no hidden halves.
- [x] Full 16-bit ns space shared by tables and edges for real. The ns
      vocabulary is a genuinely shared space (b2bd478).
- [x] Table/edge layout fully uniform: same header discipline, edge just
      declares two slots instead of one. ADR-0001 superseded (rationale
      recorded in the doc header).
- [x] Sweep: `edge.rs` (`head_bytes`/`DIR_BIT` deleted), collection.rs /
      slatedb_backend.rs prefix scans, integration hex locks, ADR-0001
      superseded. Edge key bytes changed — dev stage, no stored data.

## Slot shapes: fixed face / counting family (ADR-0030, decided 2026-10-17)

- [x] Design recorded (ADR-0030): a family that is a single face — primary,
      dynamic segment, both vocabulary directions, the graph's five — gets
      its own 1-byte slot number (0x00–0x0A — the graph's five faces plus its
      predicate vocabulary); the counting families
      (declared index 0x1 / reduce 0x2 / junction 0x3) keep the 2-byte slot,
      the 12-bit counter and their numbers unchanged. Width is decided by the
      first byte's high nibble, so a family-prefix scan stays exact without
      consulting a table (the rejected alternative was a flat 1-byte slot with
      level-1 ranges: a range split presupposes each family's growth ratio).
- [x] Implement the renumbering in code (LANDED 2026-10-17): slot constants
      became fixed faces / counting families in `okm-core::model::index`,
      with `write_slot` / `slot_len` as the single width-aware writer every
      key-assembly site calls (okm-core: document / obj_vocab / reduce /
      junction / graph; okm-dynamic: collection / plan_fields / index /
      reduce / graph; the derive emits `::okm_core::write_slot`). Hex locks
      updated (graph faces one byte, primary head 3 bytes, dynamic face
      table), the Steel binding's primary-width assertion follows, and the
      byte-equality drift locks between the typed and dynamic writers caught
      the one writer that was missed. Caps stay 4095 indexes, 4095 reduces,
      2048 junction discriminators. Pre-release window — the store is
      rebuilt (derived entries rebuild from documents; the dynamic segment
      and the dictionaries ride the rebuild).


## Graph edge: one declaration, a handle, a predicate vocabulary (ADR-0017, decided 2026-10-17)

- [x] Design recorded (ADR-0017, terminal shape): the DECLARATION is the graph
      — `GraphEncode` (one ns via `#[ok_ns]`, the edge's own document payload,
      attribute faces declared EXPLICITLY via `#[ok_index]`, and `type Nodes` =
      a compile-time enum over the member COLLECTIONS). `EdgeEncode` and its
      encoder-only half (`attrs()` / `attr_faces()` / `ATTR_SLOTS`) retire. The
      edge's type is its PREDICATE — a key dimension (2 bytes in the head) with
      its OWN vocabulary (new fixed faces 0x04/0x05), separate from the
      payload's field-name vocabulary (0x02/0x03), so listing a graph's
      predicates is one 0x04 prefix scan and never an inference from the index
      faces. `Graph<S, E>` is a HANDLE that declares nothing (an engine instance
      + the per-instance state: the predicate vocabulary cache, the edge_id
      allocation position) — the same shape as `Collection<S, K, R>`. The member
      set lives in code (never a slot); refs stay self-describing
      `[ns 2B][len varint][pkey]` with membership checked at write and decode;
      `unlink` deletes precisely by decoding the edge body. "kind" is retired as
      the name for the edge's type (the predicate; `edge_id` stays the edge's
      own identity).
- [x] Landing (LANDED 2026-10-19 — code is the new shape):
      · derive: `GraphEncode` replaced `EdgeEncode`; ns from `#[ok_ns]`, faces
        from explicit `#[ok_index]`, and the member set given as
        `#[ok_nodes(Table, …)]` — a list of node TYPES, from which the derive
        generates the `<Graph>Node` enum (SINGULAR: a value names one node
        collection, while the set role lives in `NodeSet::NS_VALUES`) and its
        `NodeSet` impl (ns off each table's
        `#[ok_ns]`; 2026-10-21: it used to name a hand-written enum). Rust has
        no associated-type defaults, so one declaration, one impl. It generates the whole `Document` impl
        (`okm-derive`'s row emitter, with `type Key = EdgeKey`) plus
        `impl GraphEncode`; `attrs()` / `attr_faces()` / `ATTR_SLOTS` /
        `KvGraph` are gone.
      · predicate vocabulary: `PREDICATE_VOCAB` is a `VocabFaces` pair (0x04 id →
        name, 0x05 name → id) and `VocabCache` is parameterized by that pair
        rather than duplicated per vocabulary; `FIELD_VOCAB` (0x02/0x03) stays
        the payload's. Faces moved to 0x06 predicate index, 0x07 out, 0x08 in,
        0x09/0x0A predicate±endpoint; `predicate_vocab_id`/`predicate_vocab_name` are
        `predicate_id`/`predicate_name` everywhere.
      · assembly: `Graph<S, E: GraphEncode>` takes the engine only (ns from
        `E::NS_PREFIX`), checks `E::Nodes` membership at write and at decode,
        encodes the payload itself (`E::encode_payload`), and writes the
        declared faces through `E::index_entries` — the document machinery, so
        an edge's attribute face IS an ordinary declared index.
      · `unlink`: decodes the body, decodes the payload into `E`, re-derives
        the declared entries and deletes them exactly — no prefix sweep.
      · edge head type renamed `EdgeHead { src, dst, predicate }` (the word
        "fact" is k10r's business vocabulary, not okm's; ADR-0017 §3's own
        word for this segment is the head).
      · okm-dynamic's handle: `Graph::new(store, ns, schema)` — the pair, and
        `CollectionSchema` now carries `ns_prefix` so a disagreeing pair is
        rejected instead of overridden, plus `members` (exported by
        `CollectionSchema::of_graph::<E>()` from `E::Nodes::NS_VALUES`), which
        the handle checks the refs' ns against at write and decode — the
        member set was the one declaration half a payload layout cannot carry.
        Its open attributes moved OUT of the
        body into the dynamic segment (fixed face 0x01, one entry per edge) as
        ADR-0017 §3 says, and they allocate names through the FIELD vocabulary
        while predicates allocate through PREDICATE_VOCAB — the collision
        krystallizer worked around is structurally gone.
      · tests: `okm-core/tests/graph_edge_test.rs` (10) pins the new hex lock —
        both vocabulary faces, the five graph faces, the 0x1001 declared face,
        and the absence of any predicate in 0x02/0x03;
        `okm-dynamic/tests/dynamic_graph_test.rs` (5) keeps the static/dynamic
        byte-equality drift lock over faces 0x00/0x04–0x0A.
      · `unlink_into`: the delete set into a caller-owned batch, so a rewrite
        (`unlink_into` + `link_into`, same edge id) is ONE commit — ADR-0010's
        owed item on the k10r side. `unlink` is a thin wrapper over it.
      · still open: krystallizer's follow-up (`EdgeEncode` → `GraphEncode`,
        `kind_id` → `predicate_id`, entity kinds onto its own 0x04/0x05, store
        rebuild) — its ns 37 edges predate this, so the rebuild is mandatory.

## Embedded documents (2026-09-17, shipped)

`Ref<D, K>` and `Refs<D, K>` field types — child documents by key
reference (single / many). Refs renamed from List: the type IS the
foreign-key set of a one-to-many relation (MODELING's one-to-many
section, now with a declarative field-level carrier), so the name says
reference semantics, not container shape:

- Naming line drawn: elements WITH identity (own indexes, sharing,
  independent updates) -> Refs; pure-value elements -> `Vec<T>` fields
  or dynamic `Array` frames (a scalar has no key — a key reference to
  it is a category error).

- Ref wire = child key bytes only (fixed width, hot segment); List wire =
  cold TLV `[count u32][key × n]`. Children are complete documents at
  their own ns/key with their own indexes.
- No attribute: derive recognizes `Ref<D, K>` / `Refs<D, K>` in field
  position from the type itself (same discipline as Reverse/VarInt/Quant).
- Naming: Ref chosen over Embedded/Unit/Record/Object/Dict — the wire
  truth IS a reference; `values: Vec<Option<D>>` keeps dangling refs
  visible. Key discipline: list children carry their own sequence
  identity (OKM never appends positional numbers).
- Memory: `key: K, value: Option<D>`. Write `Some(d)` = child written by
  the parent's put; write `None` = reference an existing child (shared,
  many-to-one). Read: `get` dereferences via the generated
  `__okm_embed_deref` hook — missing child stays `None` (visible
  absence under reference semantics, not a panic).
- `Collection::put` writes `__okm_embed_entries` (child payload + child
  index entries, same store/atomic boundary) and releases stale
  references: keys pointed at by the OLD document but not the new one
  are deleted (no cascade — shared children may survive; owned-cascade
  is a future `#[ok_embed(own)]` option).
- Queries return the nested struct directly (deref on read); the map
  view (to_map) lifts an embedded field to `Bytes(child key)`.

## Terminology (2026-09-17, decided)

Document-oriented naming, one sweep before crates.io:

- `Table` -> `Collection` (module table.rs -> document.rs); `EdgeTable` -> `Edge`.
- `TableSchema` -> `CollectionSchema` (2026-09-22, same sweep carried out:
  the structured schema export is a per-collection declaration; the serde
  JSON field shapes are unchanged in kind, consumers rename the type).
- `ObjEncode` -> `DocumentEncode`; trait `Row` -> `Document` (assoc types too).
- API verbs: `get`/`put` (typed, the struct door) + `fetch` / `merge` /
  `replace` / `purge` (whole document, the document door — dynamic side has
  these and only these, 2026-10-23; this list first landed with
  the slot-split names `get_document` / `put_document` / `delete_document` +
  `get_fields` / `put_fields` / `delete_fields`, retired 2026-10-22; the old `variants`
  name retired — dynamic fields ARE the document's fields).
- Public concept: **document** (OKM is document-oriented; declared static
  fields embed into the dynamic whole). "obj"/"object" retired from docs;
  `DynamicValue` stays (value-type name, unambiguous).
- Record considered and rejected: record-oriented storage is the fixed-
  schema lineage the model is moving away from.
- Aura mq.rs migrated; bindings follow (they reference the codec only).

## Parquet Variant export (shipped, 2026-09-17)

Dynamic segment fields exported to Parquet as the **Variant** type —
the open question and the assessment:

- **What**: `Collection::to_record_batch` currently exports declared
  fields only (key + hot/cold payload columns from `FieldDesc`). The
  dynamic segment (slot 1, name-keyed `DynamicValue` tree with nested
  `Obj`/`Array`) would become one extra `Variant`-typed column —
  per-row, schema-free, queryable by Variant-aware engines (DuckDB
  1.4+, Spark 4, Snowflake natively).
- **Feasibility**: parquet crate 60.0 ships `parquet-variant` /
  `parquet-variant-json` / `parquet-variant-compute`; OKM pins parquet
  54 — the Variant feature requires the 60 line, so this lands as a
  minor breaking bump of the optional `parquet` feature. Mapping is
  mechanical: `DynamicValue` UInt/Int/F64/Bool/Str → Variant scalars,
  Bytes → Variant binary, Null → Variant null, Array → Variant list,
  Obj → Variant object (field names straight through — the vocabulary
  has already resolved ids → names at read time).
- **Why it is reasonable**: Variant is precisely the industry answer to
  "schema-on-read columns inside a schema-on-write table" — the same
  static/dynamic split ADR-0012 encodes at the storage layer, mirrored
  at the analytics layer. Declared fields stay typed Parquet columns
  (zero-copy, predicate-pushdown-able); dynamic fields stay open but
  remain queryable. Neither shape is compromised.
- **Deliberate scope line**: Variant export does NOT pull dynamic
  fields into declared indexes/reduces — dynamic fields stay outside
  the declared access-method/reduce surfaces (ADR-0022's scope line:
  the write-path capabilities there are caller-declared on declared
  fields, via host callables). Export is a read-side projection, not a
  write-path capability.
- **Plan**: bump `parquet` optional dep 54 → 60; build Variant values
  from the dynamic segment's map (names resolved); append as `variant` column
  via parquet-variant's Arrow integration; feature-gate
  `parquet-variant` separately so plain typed export stays on 54 if
  version coupling proves painful. Decide version strategy at
  implementation time.
- **Shipped 2026-09-17**: deps bumped (arrow/parquet 60 + parquet-variant);
  `Collection::to_record_batch_with_variant()` — typed columns unchanged,
  one `variant` Binary column appended (ARROW:extension:name =
  "parquet.variant"), per-row wire = [md_len u32 BE][metadata][value]
  (the metadata dict is self-contained per variant value), rows without
  a dynamic segment are null. Bump exposed three masked bugs, fixed:
  Reverse<T> from_map emitted VarInt::from_dyn (type error, codec_v2_test
  only compiled under parquet); hand-written TRowByOrg marker used stale
  SLOT=1; FieldType::Bytes missing from arrow_type/swap_be/wire_bytes.

## Scalar list fields (closed, 2026-09-21)

Closed in two halves: `Vector<String>` (per-element LV) already covers
the variable-width scalar list — the remaining mismatch was the BYTE
spelling. `Vec<u8>` as a declared field is retired: raw-byte fields are
now spelled `Bytes` (`okm_core::Bytes`), same cold TLV wire, honest name
(a list-shaped name misdescribed byte-string semantics). The old
spelling is rejected at compile time (compilefail/vec_u8_retired.rs);
no consumer declared `Vec<u8>` fields, so there is no migration.

## Junction rename + ns derivation (ADR-0015, decided 2026-09-17)

`EdgeEncode`/`Edge` misnamed — it is the SQL junction table (many-to-
many middle table, double-materialized FWD/REV), not a graph edge.
Decided:

- [x] 4-byte entry head (ADR-0016): `[ns u16][slot u16]`, slot = 4-bit
      segment + 12-bit counter (0x0 document-self, 0x1 index, 0x2 reduce,
      0x3 junction; counters independent). All non-primary entry keys
      +1 byte; hex locks and slot tables rewritten. Renames
      `EdgeEncode` -> `JunctionEncode`, `Edge<S, E>` -> `Junction<S, E>`
      (mechanical: derive, core, tests, docs en/zh). LANDED 2026-09-20 —
      hex locks in integration.rs (`[0,1,0x30,2]` A-side / `[0,2,0x30,3]`
      B-side, low bit of nnn carries dir for self-reflexive junctions).
- [x] Junction fields reference document types (`user: User`, not
      `UserKey`) — the derive resolves `<User as Document>::Key` and
      `NS_PREFIX`; `#[ok_ns]` on junctions removed entirely (ns declared
      once, on the document; the earlier "derive the junction ns from
      endpoint ns values" idea is superseded — no derivation at all,
      the entries live in the endpoints' own ns, ADR-0015/0016).
      Discriminator `#[ok_junction(n)]` fills the segment-0x3 counter
      (separates multiple junctions over one endpoint pair). LANDED
      2026-09-20 — endpoint fields carry `Ref<Doc, Key>` (integration.rs
      UserToSession), derive resolves NS_PREFIX + Key.
- [x] Future (HIGH): `#[ok_relation(JunctionType)]` on a `Refs`
      field — REJECTED (2026-09-20) after analysis, closed without
      implementation. The truth-source argument: Refs works because the
      parent row is the single owner; a junction's endpoints are peers —
      a one-side field declaration leaves the peer's delete unable to
      clean up (reverse RMW chain or stale-key edge resurrection on the
      next put), and two write entry points (field diff + explicit
      link/unlink) fight each other. The imperative pairing (link writes
      both entries, unlink deletes both) stands as the junction's
      interface — the call site is the causal record, no old state is
      reconstructed, the write domain never crosses the peer's ns
      implicitly. Lighter escape if a real consumer surfaces:
      explicit `Junction::sync_from(row)`. Do not re-propose without a
      named consumer. Full argument chain in ADR-0015 Future work §B.
- [x] Future (HIGH) -> LANDED 2026-09-20 (fixed-ontology form): graph
      `Edge` — ADR-0017 (Accepted): the third relation carrier. One ns
      per graph (normal ns vocabulary); self-describing endpoint
      references ([ns 2B][pkey], pkey width via a ns->KEY_LEN registry,
      compile-time `nodes(...)` declaration); EIGHT entry kinds on
      ADR-0016 segments — primary (0x0, edge_id u64) / predicate vocabulary
      (0x2/0x3, VocabCache reused) / kind index (0x4) / out (0x5) / in
      (0x6) / kind+out (0x7) / kind+in (0x8) / one declared-attribute
      face per field (0x1). `GraphEdgeEncode` derive
      (`#[ok_edge(ns = N, nodes(ns = KEY_LEN, ...))]`, fixed-width
      attribute fields only) + `Graph<S, E>` assembly point
      (`link`/`link_into`/`unlink`, typed/untyped/kind/attr scans).
      Node side = standard document layout (node kind face = its own
      declared index). Update 2026-09-20: endpoint refs re-decided to
      self-describing `[ns 2B][len varint][pkey]` — the ns->KEY_LEN
      registry (compile-time nodes(...) literals AND the runtime
      register() table) is deleted; derive renamed GraphEdgeEncode ->
      EdgeEncode. Locked by graph.rs module tests + graph_edge_test
      (six-face byte lock, parallel edges, self-describing ref cuts,
      cross-collection batch, node-kind intersection).
    - [x] Fully dynamic form LANDED 2026-09-20 (okm-dynamic `Graph<S>`):
          empty declarations, attributes as nTLV frames (no 0x1 faces —
          structural), same eight faces / same wire. Byte-equality with
          the fixed-ontology form is the drift lock. MODELING/README
          sections landed.
- Pre-crates.io timing makes the rename free: downstream (aura, k10r)
  currently declares zero junctions.

## Plural modeling types (ADR-0015 §4, 2026-09-17)

The full plural taxonomy (identity axis x homogeneous/heterogeneous):

- [x] **Vector<T>** — homogeneous list, used as a whole. Shipped
      2026-09-19 (P3.5, second pass): variable-length cold TLV frame —
      payload = [count u32 BE] + count x element encoding; fixed-width
      scalars are bare V (zero per-element overhead vs Array's
      per-element frames), dynamic-width elements (String) are
      per-element LV. `FieldType::Vector { elem: "f32" }` in schema
      export (Arrow Binary, byte-opaque). Optional `#[ok_len(N)]` is an
      ENCODE-TIME contract check (embedding dims) — the write boundary is
      where the promise is enforced; decode never checks (bypassing the
      decoder is the reader's own problem, raw frame bytes are as opaque
      as an unrendered image). The contract still travels in
      `FieldSchema::expect_len` for dynamic readers to enforce. Length is
      data, not schema:
      changing embedding models is different frames, no migration.
      Multi-dim shape is application-layer (row-major over the flat
      sequence); okm-vector migrated onto it. Elements are pure
      values — NOT a relation carrier.
- [x] **DynamicValue::Array** — heterogeneous list (shipped with the
      dynamic segment); more general than Vector, per-element type
      tags, slightly higher overhead.
- [x] **Set** — deduplicated element membership. CLOSED 2026-09-21
      without a dedicated type: membership is a cardinality choice
      between two application-layer paths. Low cardinality = the
      multi-value function index IS the inverted index (element -> pkey
      fan-out; point lookup exact; duplicates collapse by put
      semantics; truth source on the row). High cardinality =
      application-level bloom filter stored as a `Bytes` field (O(1)
      per put, false positives traded; hash family / m-n ratio /
      error budget are application parameters — data-quality policy
      stays out of the codec). Bloom is a lossy derivative: it must
      ride with a complete member representation, never stand alone.
      Documented in MODELING "Set membership: inverted index or bloom,
      by cardinality" (en/zh).
- [x] **Refs<D, K>** (one-to-many) / **Junction** (many-to-many) —
      relation carriers, see ADR-0015.

The dividing line: elements with identity -> Ref/Refs/Junction; pure
values -> Vector/Array (a scalar has no key; a key reference to it is
a category error).

## Range scan + lazy iteration (ADR-0020, 2026-09-21)

The engine contract had one ordered-read shape: `scan_suffix(prefix) ->
Vec` — an equality prefix, fully materialized. Range predicates
(`1 < a < 100`) degraded to in-memory filtering, and no early exit:
LIMIT-style consumers paid for the whole result. Byte order == value
order was already a property of every encoding; the trait hid it.

Decision (ADR-0020): ONE ordered-read primitive `scan_range(begin,
end) -> Vec` (full-key `[begin, end)` byte order, None end = unbounded)
plus a streaming form `scan_range_iter -> ScanIter` — an OPAQUE
concrete enum in okm-core (private arms per engine + a buffered arm),
implementing DoubleEndedIterator: fjall and redb iterate backwards
natively; slatedb's forward-only DbIterator buffers the remaining tail
on next_back (laziness lost there, semantics identical). Reverse<T>
stays valid for key-layout descending order; query-time rev() is the
unwrapped alternative. `scan_suffix` becomes a default over
`scan_range` via `prefix_end` (last-byte increment with carry).
Collection gains `scan_range<I>` / `scan_range_iter<I>` splicing bounds
between the entry header and the identity tail (the region
`entry_prefix` fills with an equality prefix); two names stay separate
(scan = equality-prefix semantics, scan_range = caller owns bound
encoding). The lazy mirror's fetch-back captures
`SharedVirtualStorage::shared_handle()` — a view of the SAME physical
engine across the iterator boundary, never a deep copy. Remote path
rides the SAME OP_SCAN frame — bounds encoded in the value segment,
zero wire change; its iter form buffers — NOW SCHEDULED (2026-09-22):
streaming the remote path is the next wire-layer work item; design
drafted as ADR-0021 (see the checklist item below).

Engine survey (see ADR-0020 for the full table): fjall `Keyspace::range`
(lazy Iter, owned 'static), redb `range_owned` (OwnedRange, 'static via
Arc'd txn guard), slatedb `scan_prefix(b"", full-key range)` (DbIterator
'sstatic; sync adapter = Arc<Runtime> + block_on per next; empty Range
panics -> adapters return empty). All native iterators are owned and
'static, so the boxed trait method loses nothing.

- [x] Engine trait: `scan_range` + `scan_range_iter` + `prefix_end`;
      `scan_suffix` demoted to default over range. fjall / redb /
      slatedb (async + sync) / TestStore / RemoteStore (OP_SCAN value
      segment) / test engines implemented.
- [x] Acceptance: `scan_range_test` — inclusive begin / exclusive end,
      key order, unbounded, empty interval, doc fetch-back; per-engine
      (slatedb / fjall / redb) interval tests.
- [x] Collection `scan_range_iter<I>` lazy mirror: fetch-back captures
      `shared_handle()` (`S: SharedVirtualStorage` on the method, not
      the impl block); return type is `impl DoubleEndedIterator`.
      Tests: fwd/rev equivalence (native fjall), last-N via rev(),
      early-abandon take().
- [x] Docs: query-recipes "the forms of WHERE" (prefix = equality,
      range = the interval form of physical WHERE, filter = in-memory)
      + core-stance table row; MODELING "Rows at runtime" cross-reference
      (en + zh).
- [x] Remote streaming scan — ADR-0021 landed (2026-09-22):
      `OP_SCAN_STREAM` (tag 4) — paged request (entries, 0xFF = legacy
      buffered shape), self-contained value-carrying chunks (hits +
      trailing tail byte on `OpResponse`), sender-owned cursor via
      exclusive-begin flag `[0x02]` (receiver-side `prefix_end`
      increment — the sender never parses keys); receiver stays
      stateless, unknown tag falls back to buffered `OP_SCAN`.
      Answers are PREFIX-RELATIVE (receiver strips the hosted prefix
      from scan/stream keys — the sender's key space never sees it),
      and the scan-range end bound is hosted receiver-side like the
      begin. okm-wire: tag 4 accepted, tag 5 still reserved, chunk
      grammar call-site chosen (`decode_chunk`), hex tests lock the
      layout. `ScanIter::Remote` refills one page per round trip;
      `next_back` degrades to buffered (slatedb rule).

## Wire encoding refinements (execution order, 2026-09-17)

UTF encoding-series lessons applied to OKM's wire. Full analysis in
session: the valuable transfers are (1) prefix-monotonic variable-
width integers (fix VarInt's sort-order defect), (2) value-width
tiers for dynamic-segment scalars (space, capped to slot 1), and the
explicitly rejected ones: surrogate-pair-style compensation and
uniform width (O(1) field indexing is already covered by static
offsets).

- [x] P3 — 4-byte head (ns u16 + slot u16) + Junction rename. DONE
      (2026-09-20). Layout groundwork first; ADR-0015 decided. `slot: u8` ->
      `u16` BE, high byte = segment (0x0 document-self / 0x1 indexes /
      0x2 reduces / 0x3 junction / 0x82-0xBF relation reserve / 0xC0-0xFF
      system; 12-bit counter per segment). Junction rename rides the same
      sweep. Hex locks + key-layout + ADR-0012 slot table rewritten.
- [x] P3.5 — Vector<T> typed homogeneous list. DONE (2026-09-19, second
      pass). First pass compiled the dimension into the type (`Vector<T, N>`,
      fixed-width hot segment) — rejected in review: embedding dims change
      per model, and a 1.5 KB field blows up the hot segment for zero gain
      (the offset arithmetic it enables is meaningless for a field accessed
      as a whole). Final form follows the original definition: a variable-
      length cold TLV frame like String, header carries the count, elements
      are bare V when homogeneous (the payoff vs Array) and per-element LV
      when dynamic-width (`Vector<String>`). `view` lifts it to
      `DynVal::Array` — the dynamic layer has no Vector type; Vector is a
      STORAGE-layer format. Embedding vectors are the anchor use case.
      Elements are pure values — the identity line keeps this out of
      Ref/Junction.
- [x] okm-vector search layers (LANDED 2026-10-19). `ExactIndex` — one
      contiguous f32 array, full scan, exact (the small-corpus path and the
      recall reference). `Hnsw` — in-memory graph over quantized codes:
      `Codec::I8` (cosine, 4-byte header, no training) or `Codec::Pq` (k-means
      codebooks, `m` bytes, asymmetric distance); deterministic from
      `(seed, insertion order)` so a rebuild reproduces byte-for-byte; deletes
      are tombstones that keep routing. `VectorIndex` — fnv-1a shard routing,
      fan-out merge, one snapshot blob per shard under fixed face 0x0B
      (`[ns][0x0B][shard u16 BE]`), written in one batch so a reader sees every
      shard from the same build. No core change, no per-row entries, no
      permission filter inside the index: access control stays in the graph
      layer, the caller over-fetches k' > k. Gates: recall@10 >= 0.85 vs exact
      (dim 64, 400 rows, ef 48); PQ recall@10 >= 0.6 (dim 32, m 8, 512-sample
      training); snapshot roundtrip equality; build determinism.
- [x] P1 — VarInt re-encoding: byte order = value order. DONE
      (2026-09-19). Prefix-monotonic encoding shipped: first byte = width
      ((w-1) leading ones + terminator 0), payload big-endian — byte
      comparison equals numeric comparison across width boundaries, so
      VarInt is a legal index-segment field without swap transforms.
      Breaking wire change (LEB128 payloads re-encoded); downstream (aura,
      k10r) declared zero VarInt fields. Acceptance: byte-order test over
      width boundaries passed, hex lock updated.
- [ ] P2.5 — KDL as the CollectionSchema serialization (low priority). The
      dynamic mode's schema serialization is JSON today (serde, bindings
      consume it). KDL would replace it for hand-written declaration
      consistency with the KDL config family; ~150-200 lines of manual
      converter (vs serde's auto-derive) for no new capability. Defer until
      hand-maintained schemas actually exist. Note: `json_schema` (the JSON
      Schema standard output for external systems) is a different thing and
      stays regardless.
- [x] P2 — dynamic-segment frame-length varint. DONE (2026-09-19).
      Scope narrowed during review: UInt already carries minimal-width
      payload (leading zeros stripped, width implied by the frame length —
      shipped with ADR-0012), so the scalar-tiering idea was redundant. The
      real waste was the frame header: `[tag][len u32 BE]` spent 4 bytes on
      lens that are usually 1-2. Now `[tag][len varint]` via the shared
      wire codec (`wrappers/wire.rs` — put_len/take_len, the P1 encoding;
      one implementation, no second codec). Frame headers drop from 5 to
      2-3 bytes on small values; Array element frames and nested Obj frames
      inherit the saving. Declared cold-segment frames
      (`[tag][len u32]`, ADR-0004) were converted the same way in the same
      sweep — one wire discipline for every TLV length. `DynamicValue::UInt`
      minimal-width kept as is.

Execution order was P3 -> P3.5 -> P1 -> P2: P1/P2 produce new wire
bytes; doing them after P3 means hex locks change once, not twice.

## Phase 7 — Two-mode record (ADR-0025, 2026-09-24)

- [x] Architecture record (no code change): OKM carries TWO schema
      carriers over ONE engine contract — static (okm-derive codegen +
      `Collection`, compile-time DDL) and dynamic (okm-dynamic
      `CollectionSchema` + `DynamicCollection`, run-time ns and schema
      data). Byte-identical for the same declaration; mode is the
      WRITER's property, not the data's. Documented in the root README
      ("Two modes"). A draft DynamicCollection in okm-core (order-
      preserving dynamic key frames) was withdrawn: the dynamic mode's
      home is okm-dynamic, and keys are schema-typed there — a second
      key wire would break the byte-equality contract.
- Consumer: aura ADR-0026 type-scoped booth storage — python/steel
  booths execute through okm-dynamic `DynamicCollection` at the type's
  registry-allocated ns; wasm (Rust source) booths use the static path
  (derive + `Collection`) compiled into the module.

## In-browser VirtualStorage family (ADR-0028, 2026-09-29)

- [x] Design record shipped; code shipped (8d00132 + 8a12f81) and consumed
      by the mudra panel (wire side + UI-state `LocalStorageStore`). Two
      browser roles, two traits —
      localStorage as a SYNC `VirtualStorage` engine (feature
      `localstorage`; the only sync browser storage, ADR-0018's
      base64 boundary exception applies), and `WireClient` implementing
      the aligned async surface as the transport-injected generic form of
      the mudra panel's hand-rolled RemoteStore-over-WS (FIFO pairing per
      connection, explicit-Err teardown, no tokio). `VirtualStorageAsync`
      relocates slatedb_backend → storage.rs. Details, rejected options,
      and the panel-migration baseline: ADR-0028.

## Semantic batch writes (ADR-0031, 2026-10-20)

- [x] `BatchStore` (`okm-core/src/engine/storage.rs`): the batch as a
      *writer* — `get` reads the batch's pending ops newest-first, then the
      store (put-then-del and del-then-put both answer correctly), `put`/`del`
      only append, and every scan shape panics with the contract instead of
      answering a partial ordered view. `VirtualStorage`'s writes take
      `&self` (ADR-0026), so the borrowed batch sits behind a `RefCell`; the
      caller keeps ownership of the op list.
- [x] `Collection::save_with_prev_into(batch, key, doc, prev)` (ADR-0031):
      the semantic sibling of `save_into` — primary + faces into the caller's
      batch, the faces `prev` produced that the new document no longer
      produces deleted, embedded children `prev` held released, and the
      reduce fold/unfold applied through `BatchStore` (so two writes of one
      group in one batch accumulate instead of each folding from the
      pre-batch value). `prev = None` reads through the adapter, which also
      sees a row the same batch already wrote. Subscribe emission stays out
      by decision: a batch writer owns its events.
- [x] Tests (`okm-core/tests/save_into_test.rs`): one group written twice in
      one batch counts twice, an in-batch overwrite unfolds the pending row,
      stale faces removed, caller-supplied `prev`, an abandoned batch moves
      neither the row nor the ledger, document + junction + ledger in one
      commit, read-through ordering, scan refusal.
- [ ] Owed: subscribe emission for batch writers (its own decision — fire at
      commit, or stay the caller's); a `save_into` deprecation path once
      `save_with_prev_into` covers every call site; a batch delete has no
      semantic sibling yet (deletes still go through the point path), add one
      when a caller needs atomic delete + reduce.

## Categorical values as data (ADR-0032, 2026-10-20)

- [x] Per-field data vocabularies own two counting families: `0x1nnn`
      (label → id) and `0x2nnn` (id → label), ONE counter per `Vocab<N>` field
      (`data_vocab(i)` → counter 1 + i). Declared indexes moved to 0x3nnn,
      reduces to 0x4nnn, junctions to 0x5nnn (one layout change, no production
      data). The two collection-wide name domains keep their fixed faces
      (0x02/0x03 field names, 0x04/0x05 predicate tokens).
- [x] `Vocab<N>` type (`okm-core/src/model/vocab.rs`): the label in memory, an
      `N`-byte big-endian id on the wire (`VocabId` for u8/u16/u32/u64/u128,
      default `u8`); equality by label; encoding an uninterned value panics (see
      the residue note under ADR-0037: `save_into` is encoding-only by design);
      the vocabulary cache is typed by the same `N`, so exhausting the width is
      `Error::VocabularyExhausted` instead of a wrap. Unit tests cover both a
      1-byte and a 2-byte field.
- [x] `FieldType::Vocab(u16)` (payload = the field's id face; name face = id+1)
      + schema serialization (`"Vocab"`, variant 12) + bridge mapping (Arrow
      UInt8, tooling "integer", export carries ids) + okm-dynamic arms
      (`"U8 (vocab id)"`; decode panics until the read path resolves names).
- [x] Write-path scaffolding: `Document::HAS_VOCAB` (default false) and
      `Document::__okm_intern(&mut self, &mut dyn VirtualStorage, &[u8])`
      (default no-op); `Collection::put` interns on a mutable copy when
      `HAS_VOCAB`, so tables without categorical fields pay nothing but the
      constant. `VocabCache` methods take `?Sized` stores so the `&dyn` hook
      shape works.
- [x] Derive: recognize `Vocab<Domain>` fields — payload 1-byte encode/decode,
      `FieldType::Vocab(id_face)`, `HAS_VOCAB = true`, generated `__okm_intern`,
      `__okm_deref_vocab` (called by the read path after the embedded-ref hook),
      and to_map/from_map lifting the label. `tests/vocab_test.rs` proves two
      categorical fields on one row: per-field id spaces, labels across put/get,
      the map bridge, from_map + re-intern, and the schema's per-field faces.
- [x] `save_with_prev_into` interns INSIDE the batch: the vocabulary's cursor
      (the id face's bare-prefix singleton) makes allocation one point read plus
      three point writes, all of which `BatchStore` serves, so growth + row +
      faces commit together and the signature keeps `&self`. `save_into` stays
      encoding-only and therefore requires pre-interned documents.
- [x] Vocabulary allocation cursor: the id face's FACE-LEVEL SINGLETON —
      `[ns][0x2nnn]` with no suffix, holding the next id. No extra family and no
      extra fixed face: every real entry of that face carries exactly N bytes, so
      a zero-length key cannot be data. It replaces the `max + 1` scan, so (a)
      `put` and `read` on a categorical table use point operations only (proved
      by `vocab_test::a_put_on_a_categorical_table_never_scans`, which runs
      against a store that panics on every scan) and (b) batches can allocate.
      The dynamic plan surface emits the same cursor op, keeping `put_fields` and
      `plan_put_fields` byte-identical. Generalised as a layout rule in ADR-0030:
      any one-value-per-face metadata may take the bare prefix when all of that
      face's entries carry a non-empty suffix — and NOT on the label face (empty
      label) nor on a no-group reduce face (its entry key is the bare prefix).
      The two fixed-name vocabularies use the same shape (no extra fixed faces):
      `[ns][0x02]` for field names, `[ns][0x04]` for predicates / kind tokens,
      each holding the next id in place of its data suffix. Their readers already
      filter by suffix width: `Graph::predicates` (`suffix.len() == 2`),
      `VocabMirror::load` (same check) and `scan_max_plus_one`
      (`VocabId::from_be` rejects any length ≠ W).
- [x] The PREDICATE arbitration cursor has a test of its own:
      `graph_edge_test::the_predicate_cursor_is_the_id_faces_bare_prefix` pins
      `[ns][0x04]` (next id, no suffix), asserts that a predicate allocation
      writes no field-name cursor at `[ns][0x02]`, and links a second predicate
      through a FRESH graph so the resume-past-a-process-boundary case is
      actually crossed. Bites by construction: pointing `PREDICATE_VOCAB`'s id
      face at 0x02 fails it on the first assertion.
- [x] The singleton convention IS part of the exported schema contract — the
      host reads and writes this cursor itself, so it cannot stay a Rust-side
      private habit. Shape: the schema keeps naming both faces of each vocabulary
      pair and NO cursor field (the key is derived — the id face's bare prefix;
      a numbered cursor face is exactly what ADR-0030's "no bands" rule forbids),
      `SlotMap`'s own doc states the derivation and the same-commit rule, and
      `obj_vocab::cursor_key` is its ONE definition — the typed cache, the
      dynamic mirror (`VocabMirror::load`) and the plan surface
      (`plan_put_fields`, whose ops a host replays) all call it. Prose:
      `docs/internals/key-layout.md` + ADR-0030, both languages.
- [x] Arrow/Parquet export of a `Vocab` column: LABELS cross, ids do not.
      The export resolves each categorical field through ONE scan of its id face
      (`arrow_bridge::vocab_labels`) and emits a NULLABLE `Utf8` column; an id
      with no label becomes NULL, never an invented name. The import re-interns
      each label in the DESTINATION vocabulary (`obj_vocab::vocab_id`, dispatched
      by the field's declared width) before the row bytes are assembled, so
      restored rows carry the destination's numbering and are named by
      construction — the id-carrying round trip was the one path that produced
      unnameable values, and it is gone. Files from that older form are refused
      by column type (no compatibility path, by decision). Parquet's physical
      dictionary (default on, one dictionary page per column chunk, 1 MiB limit)
      then gives each chunk a local vocabulary on its own: every distinct label
      is stored once per chunk and the rows carry indices, so the wider value
      form is not paid per row. The export still passes
      `WriterProperties::None`, i.e. dictionary encoding WITHOUT a compression
      codec — a separate knob, left alone.
      Tests: `tooling_test::parquet_roundtrip_carries_labels_not_ids` (column
      type, nullability, the empty label is a value, restore into a fresh store,
      the JSON-schema view) and
      `::an_unnamed_id_exports_as_null_and_import_refuses_it`.
      Recorded: ADR-0032 §7 and ADR-0007's type mapping, both languages.
- [ ] Owed: the vocabulary as Parquet file key-value metadata — a warehouse can
      read the labels already, but a consumer that wants the full id↔label table
      (including labels no row used) has nothing to read. Additive: one footer
      entry per categorical field.
- [x] okm-dynamic: a `Vocab` field decodes to its LABEL. The pure decoder hands
      out the id as raw `Bytes` (it has no store) and the collection's read paths
      run `resolve_vocab_labels` — one point probe per categorical field against
      the field's own vocabulary; an unknown id stays bytes rather than an
      invented label. `okm-dynamic/tests/vocab_read_test.rs` reads a typed-written
      row dynamically and gets both labels back (u8 and u16 fields).
- [x] Retire `Enum<T>` / `EnumTag` / `FieldType::Enum`: the wrapper module is
      gone, every match site (schema serde, Arrow/tooling bridges, okm-dynamic,
      both bindings, the derive arms) is clean, and `wrappers_test` covers the
      same byte layout with a plain `u8` field. Consumer-side enum semantics are
      the consumer's own wrapper.
- [x] Docs: MODELING (both languages) gains "Payload field types and wrappers" —
      the full wire table plus the three rules (categorical values are `Vocab`,
      `Option` is not a field type, interning is a write-path step).
- [ ] Migration note for existing data: old one-byte `Enum` tags decode without
      error into a DIFFERENT category; one layout change, version bump, no
      silent path. Same window as the vocabulary-face move (no production data).

## ADR-0034 — table names and the export shape (2026-10-21)

- [x] `#[ok_name("...")]` → `Document::TABLE_NAME` (trait default `""` = unnamed;
      derive emits the override; both `DocumentEncode` and `GraphEncode` parse it,
      since a graph row goes through the same schema). An unnamed table is refused
      by an export rather than spelled from the struct.
- [x] `parquet_io::export_parquet_with_variant` — the bridge's variant column
      (`Collection::to_record_batch_with_variant`) finally has an export call, so a
      file keeps the dynamic segment instead of only the declared half.
- [x] `Graph::replace_into` / `attrs` (named `put_fields_into` / `get_fields`
      until ADR-0035) own the edge's 0x01 dynamic segment
      (field-name vocabulary, `[ns][0x01][edge_id]`), and `link_into` deletes the
      previous segment in the same batch — a rewrite must not inherit attributes.
      Before this, the consumer wrote that segment by hand: a documented layout with
      no owner.
- [x] `Graph::to_record_batch_table(name_of)` — the edge set as ONE table:
      `[src_table, src_pkey, dst_table, dst_pkey, predicate]`, the declaration's
      fields, then `variant`. Endpoint keys are RAW bytes (an edge table spans node
      collections and cannot decode another table's key); an ns the caller's map does
      not know is NULL, never a guessed name. `Graph::edge_ids()` scans slot 0 only.
- [x] `parquet_io::export_reduces::<_, A>(store, ns, table, path)` — `(table,
      reduce, group, value)`, one row per entry. A restore MUST replay these before
      the next write: a missing watermark hands out ids already in the table.
- [x] Tests: `the_table_name_is_declared_beside_the_ns` (named / unnamed),
      `the_edge_set_exports_as_a_table_with_named_endpoints` (column order, raw key
      bytes, predicate label, unknown ns → NULL),
      `the_reduce_table_exports_as_its_own_file` (slot, groups 1→40 and 2→7).
- [x] Consumer side (Krystallizer) LANDED 2026-10-21 — the three asks, each
      answered on the consumer's side and verified there: `GET /export` is the
      catalog generated from the declarations at request time (`(table, ns)`), so
      the names are declared where the ns is (`#[ok_name]` → `Document::TABLE_NAME`,
      ADR-0034 §1) and k10r keeps no second list; `GET /export/{table}` is the door
      addressed by TABLE NAME, with an unknown name a 404 pointing at the catalog;
      and the FACT table exports through `Graph::to_record_batch_table` as okm's
      edge table (`(src_table, src_pkey, dst_table, dst_pkey, predicate)` + the
      declaration's fields + a props variant) instead of a 501.

## Whole-document API: merge / replace / purge / view (ADR-0035 — A landed 2026-10-22, B/C open)

The slot-split document API (`get`/`put`/`delete_document` +
`get`/`put`/`delete_fields`) made the caller choose a slot, and every
whole-document write cost two engine commits. The surface collapses onto two
axes: the static segment speaks the STRUCT (`get` / `put` / `delete`,
unchanged — whole-struct replace is already the only typed write, and the derive
generates no per-field setters), the whole document speaks a name-keyed map, and
every write is ONE engine commit.

| operation | covers | commits | replaces |
|---|---|---|---|
| `get(key) -> Option<R>` | static segment (typed) | — | unchanged |
| `put(key, &R)` | static segment + faces + ledgers | 1 | unchanged |
| `delete(key, &R)` | static segment + faces + ledgers | 1 | unchanged |
| `view(key) -> Option<BTreeMap<String, DynamicValue>>` | merged projection of both slots | — | `get_document` + `get_fields` |
| `merge(key, &map)` | declared names → row RMW; dynamic names → per-name overwrite of slot 1 | 1 | `put_document` (merge half) + `put_fields` |
| `replace(key, &map)` | row rebuilt from the map (absent declared fields take defaults); slot 1 = exactly the map's dynamic names | 1 | — (stronger than `put_document`) |
| `purge(key) -> bool` | both slots removed | 1 | `delete_document` + `delete_fields` |

Decisions:

- `merge` / `replace` clean the previous document's faces (through
  `save_with_prev_into`). `put` keeps its documented contract — "never cleans;
  `delete(old row)` owns the dangling face" — which `index_test.rs` locks.
  Rationale: `put` is the one write that does not read the row first, so it is
  the only one that can leave a stale face; the whole-document verbs read the
  stored row and sweep it. Two entry points must not have two face lifecycles.
- `merge` vs `replace` split on ABSENT declared fields: merge keeps the current
  value, replace takes the default. This is the distinction `put_document`
  blurred — named put, behaved as merge.
- The document read is named `fetch` (it was `view` until 2026-10-23): a *view*
  is a projection of a row, and the map is not a projection of two slots any
  more — it is the document. `fetch` is the read door of the write verbs it
  pairs with.
- `Graph`'s edge-attribute face drops the slot-carve-out names too, but NOT by
  copying the document verbs: `get_fields` → `attrs` (it answers the attributes
  only — an edge's typed half is the payload, not a map, so `view` would
  over-claim) and `put_fields_into` → `replace_into` (it replaces the segment
  outright, so `merge_into` would describe behaviour it does not have).
- The six old names retire with no aliases.
- Private op builders shared by the writers: `delete_into(batch, key, doc)` and
  `slot1_into(batch, key, map)` — that is what makes a whole-document write one
  `Batch` and one commit.

Tasks:

- [x] A `okm-core`: the whole-document surface — `view` / `merge` / `replace` /
      `purge` beside `get` / `put` / `delete`; every write composes row ops +
      slot-1 ops into one `Batch` (`delete_into`, `slot1_into`, `commit`);
      the six old names retired with no aliases; `Graph` aligned
      (`attrs` / `replace_into`). Landed 2026-10-22 with ADR-0035.
- [x] A tests: `write_commit_test` counts commits and ops per operation (insert
      3, overwrite 4, +1 for a stale-face delete, +1 per slot-1 entry);
      `obj_variants_test` rewritten for the map surface (merge per-name,
      replace = whole document, purge = both slots, dynamic-only documents
      viewable, face cleanup on merge). `remote_test` proves the composed write
      stays one frame. All green: 40 okm-core suites, 64 workspace suites.
- [x] A docs: ADR-0035 (bilingual) + index, MODELING en/zh, README en/zh,
      ADR-0018's reference, this PLAN.
- [x] B `okm-dynamic`: `DynamicCollection` gains `view` / `merge` / `replace` /
      `purge`, retires the slot-1 trio, and its plan surface emits a
      whole-document plan — `plan_merge` / `plan_replace` / `plan_purge`, each
      returning one `PlannedWrite` (ops + acc receipts + vocabulary growth):
      row ops, segment ops and the path-index sweep in one list, one replay.
      Four facts the port settled: (a) `replay` now builds a `Batch` and calls
      `commit_batch`, so a whole-document write is one engine commit, not a loop
      of point writes; (b) `split_names` classifies by the PAYLOAD declaration only — a
      key field's name in a map is a value field (OKM is KV; the key is the
      identity and it is independent of the value, so nothing is checked and
      nothing is rejected), and the row's identity is decoded out of the pkey (a
      map cannot move it);
      (c) `delete` is RETIRED from the dynamic facade — the document is the pair
      of slots, so `purge` is the one deletion verb (a row-less document, which
      `replace` can still produce, purges the same way); (d) `put` now REJECTS a
      name the schema does not declare, on the embedded path and in `plan_put`:
      the map stands in for the struct on this side, so an undeclared name is a
      caller bug, never a value silently dropped — dynamic names belong to
      `merge` / `replace`.
      "Absent declared field" has ONE definition — `decode::fill_schema_defaults`,
      shared by the payload decoder's version migration and `replace`'s map.
      Tests: `dynamic_table_test` rewritten around merge/replace/purge
      (unnamed dynamic names kept by merge, dropped by replace, `replace(empty)`
      == purge, `delete` keeps the segment, one row-less document still views),
      `path_index_test` ported (`put_fields` → `merge`; the segment-drop test
      becomes a declared-only `replace`), `plan_merge_equals_embedded_merge`
      locks the remote/embedded byte equality. Landed 2026-10-23; 64 workspace
      suites green.
- [x] C consumers: aura / krystallizer / effector. Push okm, then
      `cargo update -p okm-core` (and `-p okm-dynamic`); same work session.
      The sweep now covers the dynamic trio as well as the typed six names
      (17 call sites seen on 2026-10-23: aura `realm/src/meta.rs` ×3,
      `realm/src/store_exec.rs` ×3 plus its `coll.delete(&k)` ×1 — now `purge`;
      krystallizer `mem-core` log ×1, channel ×1, graph ×6) plus the typed
      `delete` sites (aura `mq.rs` ×3, `store_exec.rs`'s dynamic `delete` ×1,
      krystallizer `log.rs` ×1, mudra `mudra-store` ×1 → `purge`). Mapping rule,
      per site — no mechanical rename:
      `put_fields(map)` WAS a whole-segment replace, so it is `merge` when the
      map is partial and `replace` when the map is the document;
      `get_fields` + mutate + `put_fields` is `merge` with just the changed
      names (or read `view`, mutate, `replace`); `delete_fields` (drop the
      segment, keep the row) is `replace` with the declared-only map; a
      `put_fields` on a key whose row write is meant to ride along becomes one
      `merge`/`replace` (one commit instead of two).
      Landed 2026-10-23 across six repositories (aura, effector, krystallizer,
      mudra, prism, gravity — every lock on the same rev). Three findings worth
      keeping: (a) TWO consumers encoded the primary-slot prefix by hand as
      `PRIMARY_SLOT.to_be_bytes()` (2 bytes), while the slot has been a 1-byte
      fixed face since ADR-0030 — mudra's `schema_contract` and aura's MQ backlog
      scan both silently found nothing; `okm_core::write_slot` is the fix and the
      only thing either side should use. (b) `put_fields`'s whole-segment
      replace has NO whole-document equivalent for free: dropping a name means
      rebuilding the document from its declared half plus the given map and
      calling `replace`, which reads what the delta verbs never read. aura's
      `PutFields` op was therefore redefined as `merge` (a delta — names not
      given survive), while its `DeleteFields` op keeps the rebuild because the
      document is a pair and only `replace` can leave no segment. (c) `merge` /
      `replace` loaded the field-name vocabulary unconditionally; the mirror is
      the segment arm's business, so both now load it only when a non-declared
      name (or a stored segment) is in play.
- [x] D `delete` retired on BOTH sides (2026-10-23): `purge` is the one removal
      verb. A row cannot outlive its segment, so `delete(&K, &R)` /
      `delete_by_pkey` / the dynamic `delete` were only ways to leave a dangling
      segment. `purge` reads the stored row + the segment, clears both in ONE
      commit, and now carries the Delete event `delete` used to emit (a
      segment-only document has no row to hand the event, so it emits nothing).
      `delete_into` stays as the private op builder. Consequence recorded, not
      hidden: the removal set derives from the STORED row, so a stale face left
      by a bare `put` overwrite is no longer removable — the overwrite path is
      `merge` / `replace`, which sweep the stored row before writing.
      `index_test::partial_index_flip…` now locks both halves (the dangling entry
      survives purge; the same overwrite through `merge` leaves none).
- [x] E the read door is `fetch`, and the dynamic side has no `get` / `put`
      (2026-10-23). `view` named a projection of a row; the map is the document,
      so the read door is `fetch` — the verb its three write partners imply and
      the one thing a caller holding an external map needed. On
      `okm-dynamic` the pair `get` / `put` retired: `put` was `merge` restricted
      to declared names (identical op list), a restriction invisible in the
      signature — the same mistake that made `put_document` a lie. It costs no
      read: every verb reads per ARM (a map of dynamic names never reads the
      row, a map of declared names never reads the segment, both kinds read
      both), and `put` read the same row its declared arm reads. What it does
      cost is the reference-round-trip check `put_rejects_a_name_…`, which
      became `an_undeclared_name_is_a_dynamic_name_on_this_side` — the surface
      no longer has a declared-only door, so nothing on that side checks a typo.
      The typed side keeps `get` / `put`: it has a struct to speak.
      `dynamic_table_test::a_verb_reads_only_the_state_its_arm_consumes` locks
      the read discipline key-by-key (the vocabulary cursor probe is a read too,
      so the test asks which keys, not how many).
- [x] F the same pass found typed `replace` passing `prev = None`, so its face
      sweep never ran: `replace` now reads the stored row in the rebuild arm too
      (`index_test::…` locks that the old entry is gone). Rule 4 said "the
      row's previous faces are removed"; the code had not, and the dynamic side
      always had (it passes the stored row it read).
      Python binding swept in the same pass (`put`/`get`/`delete` →
      `merge`/`fetch`/`purge`, plus the missing `FieldType::Vocab` coercion arm
      that had kept the binding uncompilable).
- [x] G the document surface reaches the READ-side tools (2026-10-23). Three
      facts, one theme — a document is the pair of slots, so anything that walks
      documents owes the second slot: (a) enumeration has two modes —
      `scan_keys` is rows only (one prefix scan), `scan_document_keys` is the
      union of the two slots (that scan plus a key-only scan of the segment slot,
      two-pointer merged in memory, zero point reads); materializing is `fetch`'s
      job, so a key becomes a document with a plain `map` — a union cannot be an
      adapter, because an adapter cannot surface an item that was never scanned;
      (b) `scan_documents_raw` now yields `(suffix, Option<payload>)`, `None`
      meaning "this document has only a segment", and every payload column in the
      Arrow/Parquet export is NULLABLE (key columns stay non-nullable) — the
      segment-only row exports with its key and a Variant, nothing inverted into
      a default; (c) `import_parquet` finally reads the `variant` column back
      (it had exported it and ignored it, so a backup lost every dynamic segment):
      a row whose payload columns are all null imports as a segment alone and
      gains no row, and the segment lands through `merge`, leaving the row alone.
      The reduce ledger needs no change and is now locked by a test: it follows
      the ROW — a segment-only patch folds nothing, a patch writing both halves in
      one call folds ONCE (ADR-0035 rule 9).
- [x] H the write verbs take `&self` (2026-10-23, ADR-0035 rule 10) — the same
      shape the KV beneath already has (`VirtualStorage::put` / `del` are `&self`),
      so a write no longer needs an exclusive binding it never used for
      exclusion. The exclusivity `merge`'s read-modify-write needs is not the borrow: it is the
      collection being `!Sync`, i.e. `epoch: Cell<u64>` (the only mutable state a
      write touches). `VirtualStorage::commit_batch` (sync + async) drops `&mut`
      with it — the engines' own batch APIs are `&self` (`fjall::db.batch()`,
      redb `begin_write()`, slatedb `write()`) and the remote engines hold their
      transport behind `Rc<RefCell<…>>`/channels. `Graph` keeps `&mut self`: its
      predicate vocabulary IS a mutable cache field, a different shape. The
      carrier lost a misleading prefix in the same pass — `MemBatch` → `Batch`
      (every batch is in memory; serialisation is a different type). Locked by
      `write_commit_test::the_verbs_take_a_shared_handle_and_still_commit_once_each`:
      a plain `&Collection` drives put/merge/replace/purge, one commit each.
- [x] `docs/CONCURRENCY.md` (+ zh) records the multi-writer analysis: the two
      layers and which one is single-writer, five mechanisms that assume one
      writer, the four shapes (mutex / wrapper+RwLock / writer thread / channel as
      store), what a frame buys, and the four debts a real multi-writer design owes.
- [ ] The remote frame's mid-frame flush is dead code (2026-10-23, found while
      writing CONCURRENCY §4): `ExecCore::apply` accumulates a frame's writes in a
      local batch committed AFTER the op loop, while the `commit_batch(Batch::default())`
      before an `OP_GET` / `OP_SCAN` commits an EMPTY batch — so a query never sees
      the same frame's pending writes, contrary to the comment there. No production
      caller builds a mixed frame, so nothing is broken today; a mixed frame's
      read-your-own-writes needs the query routed through a `BatchStore` view.
- [x] includes gain their read half (2026-10-24) — both holes closed.
      (a) dynamic side: `scan_access_method_covered` / `decode_includes` (over
      `okm_core::scan_suffix_kv`) read the entry VALUE and decode the segment by
      declared width; exposed as `DynamicCollection::scan_covered`, returning
      `(key fields, covered fields)` as two name-keyed maps.
      (b) typed side: `KvIndex::Covered` + `decode_covered`; the derive generates
      `__OkmCovered_{Row}_{index}` (fields = INCLUDES order, same Rust types;
      `()` when the declaration covers nothing) and `Collection::scan_covered`
      now returns `(PrefixKey<K>, I::Covered)` instead of raw bytes. The segment
      is a bare concatenation located by width — no frames — so the decoder is
      the write walk read backwards; a trailing variable-width include (Str,
      last-only rule) takes the remainder and reuses the payload decoder with
      `len = remainder`. ADR-0005's 2026-10-24 update records the landing and
      corrects this repo's "value = includes TLV" wording, which the code never
      did. Locked by
      `index_test::covered_scan_answers_without_the_primary_table` (the primary
      row is deleted from the store and the covered scan still answers — the
      whole point), `index_test::covered_includes_carry_a_variable_width_tail`,
      `dynamic_table_test::dynamic_covered_scan_answers_without_fetching_documents`
      and `…::dynamic_covered_str_include_decodes_the_trailing_remainder`. The
      bench now measures both doors at every fanout (index carries
      `includes(score)`): 100 hits 515 µs → 104 µs, 10 000 hits 60.8 ms →
      10.7 ms (docs/internals/benchmarks.md).
- [x] Downstream bump after H is complete (2026-10-24): mudra / effector / aura
      ride `ba0fb72` and check clean; krystallizer followed with its own
      `9bee7f5` ("MemBatch is Batch, and the verbs take `&self`"), and no
      `MemBatch` reference remains anywhere under `~/world`; gravity and prism
      check clean through it — the older krystallizer skew
      (`view::DEFAULT_USER_NODE`, `note_verdict`) went away with that commit, so
      nothing was left to patch there. The `&self` change did leave a residue of
      pointless `let mut` bindings at the downstream call sites: 17 in
      `aura-realm` (meta.rs / mq.rs / store_exec.rs) and 9 in `prism`
      (identity.rs / nodes.rs / lib.rs). Cleared — every downstream repo checks
      warning-free again, which is what "the bump is done" has to mean.
- [x] Declined (2026-10-23): a `Hit` / `Prefix` result type that carries the
      verbs — a key does not know its collection, so the wrapper can only hold a
      `&Collection` and take it as a parameter anyway, and it drags the partial-key
      and covered-value states into the type for nothing. Chaining is not a goal;
      the scan result stays what it is. (A separate, still-open question: `scan`
      fetch-backs every hit eagerly — laziness is worth having on its own terms,
      and it does not need a new type.)
- [x] `from_map` refuses a present field it cannot honour (2026-10-24, the
      item queued as "own commit"): absent names still take their declared
      default (the append-only evolution rule), but a name the map CARRIES with
      another kind — or a value that does not fit the declared field — is
      refused by name at run time (`obj_dynamic::from_map_kind_mismatch` /
      `from_map_bad_value`). Closed class-wide rather than only at the reported
      `_ => #dflt`: the two `unwrap_or_else`ed conversions in the same arms were
      silently defaulting too (fixed `[u8; N]` from the wrong length,
      `Reverse<T>` from an out-of-range value), and the `as #t` narrowing casts
      now go through `TryFrom`. `Vocab` accepts BOTH forms `to_map` emits — the
      label, or the unresolved id (narrowed by width, so an id can never wrap
      onto another label). Locked by
      `obj_variants_test::from_map_defaults_an_absent_name`,
      `...::from_map_refuses_a_present_name_of_the_wrong_kind`,
      `...::from_map_refuses_a_value_that_does_not_fit_the_declared_width`.
      Superseding the panic: ADR-0037 puts the refusal on the type —
      `okm_core::Error` (thiserror) + `Document::try_from_map`, with
      `Error::Engine` carrying the engine's own text. The same decision then
      went all the way: EVERY write verb returns `Result` (no write panics) —
      `put`, `upsert_with`, `purge`, `prune_deprecated_slots`, `merge`,
      `replace`, `save_with_prev_into`, the junction's `link`/`unlink`, the
      graph's `link`/`link_into`/`unlink`/`unlink_into`/`replace_into` (whose
      `String` errors became `Edge*` variants). The intern chain is fallible end
      to end (`__okm_intern`, `VocabCache::id_for`, `put_frame*`,
      `NameResolver`), because interning is the one allocation a write performs.
      `a_refused_merge_returns_the_error_and_writes_nothing` locks the "a refusal
      writes nothing" half.
- [x] Read side (ADR-0038, LANDED 2026-10-24): the same disease on the other
      half — every read answered `Option`, so "the row is gone" and "the store
      broke" were the same value, and the wire had no error field at all.
      Now: absence stays `Option`, failure is `Result` (`Result<Option<R>>` /
      `Result<Vec<…>>` at the semantic verbs); `VirtualStorage` /
      `VirtualStorageAsync` read primitives return `crate::error::Result` and
      `ScanIter::Item` is `Result<(Vec<u8>, Vec<u8>)>` (six backends, none
      `expect`s on a read); `OpResponse.fault` gives the wire its third answer
      (`[2][len][utf8]`, `0`/`1` byte-identical, receiver sets it in `OP_GET` /
      `OP_SCAN` / per `OP_SCAN_STREAM` page, sender folds it back to `Err`);
      the three read hooks (`__okm_embed_deref`, `__okm_deref_vocab`,
      `__okm_apply_reduces`) and the junction's generated `get_*` queries are
      fallible; `Error::Corrupt` / `Error::ScanThroughBatch` replace two aborts.
      `decode_payload` still panics by decision (ADR-0038, "deliberately not
      done"). The dynamic plan's `AccOf` closure is fallible too (it was
      `&|ek| store.get(ek).unwrap()`, a panic behind a `Result` the caller had
      already paid for); `okm_dynamic::reduce_get` follows. Suite: okm-core
      225, okm-dynamic 50, okm-query 3; the whole workspace under
      `--all-features` is 327 passed / 0 failed.
- [x] Warning sweep (LANDED 2026-10-24): the twelve clippy warnings the `&self`
      write pass orphaned are gone (six `let mut` on tests, three
      `type_complexity` — `RawDocument` / `EdgeRow` / `GroupAccs` name the
      tuples, three `unnecessary_cast` in the Iceberg bridge). `cargo clippy
      --workspace --all-targets --all-features` is now silent.

### A value too big for the row is its own table (ADR-0036, decided 2026-10-23)

`#[ok_offrow]` is **rejected** — approved 2026-10-22 (fixed face 0x0C, off-row
fields out of the generated type, a per-document tag enum as the field-level
selector), withdrawn 2026-10-23. Three reasons and the comparison table live in
ADR-0036; the modelling recipe is a case in MODELING (en / zh). Nothing to build
here: a second table keyed by the parent's own key type does the job, its write
pair is already one commit through ADR-0003's batch, and `R` stays complete.

Tasks:

- [ ] Publish the batchable removal: `Collection::purge_into(&mut Batch,
      &K) -> bool` over the existing private `delete_into` (reading the stored row
      and segment through a `BatchStore`, so a pending write in the same batch
      counts as the old state), so a parent row and its side table leave in one
      commit. A surface addition, not a mechanism — the builder exists; only the
      door is missing, and the pair's WRITE is already one commit.

### Vocabulary allocation: one rule, two implementations, only one checked

`#[ok_offrow]` is gone; this defect surfaced with it and stands on its own.

The rule is "first-seen claims next, append-only, exhausting the width is a loud
error, never a wrap" (ADR-0012 / ADR-0032), and it lives in two places:

- `VocabCache::id_for` (`okm-core/src/model/obj_vocab.rs`) checks —
  `id.succ() == None` returns `Error::VocabularyExhausted` (a panic until
  ADR-0037; a write verb reports, it does not abort the process).
- `VocabMirror` (`okm-dynamic/src/plan_fields.rs`) does raw `u16` arithmetic at
  three sites: `load` (`next_id = id + 1`), `adopt_entries` (the same line), and
  the `plan_put_fields` resolver (`let id = *next_id; *next_id += 1`).

No `[profile]` section in the workspace, so release keeps overflow-checks off:
65535 + 1 wraps to 0, the next new name takes id 0, that face's "0 → name" entry
is overwritten, and every row already written resolves its frame to a different
name. That is the plausible-but-wrong id ADR-0032 forbids, on the plan surface
the bindings and the remote path use — the one without the check.

The predicate vocabulary has no hole today (it only goes through `VocabCache`,
via `okm-dynamic/src/graph.rs`), but a graph plan surface would copy those three
lines verbatim. So the fix goes in the primitive, not at the call sites:

- [x] One checked successor — one definition, one message — used by the three
      `VocabMirror` sites (`okm-dynamic/src/plan_fields.rs`). `VocabCache::id_for`
      already had it. The rule as landed: LOADING saturates (`saturating_add` —
      a table may already hold the last id, and a read must not panic), CLAIMING
      is checked (`claim_id`, the same refusal the typed cache makes, neither
      handing out the last representable id because the watermark could not move
      past it). Tests: `a_full_vocabulary_dies_loudly_instead_of_wrapping`,
      `a_vocabulary_holding_the_last_id_still_loads` (2026-10-23).
- [x] Slot assembly is checked, not masked (2026-10-24): `index::slot` asserts
      BOTH halves (family ≤ 0xF, counter ≤ 0x0FFF) — masking landed the value on
      another family's legitimate slot (`0x3FFF + 1 = 0x4001` is the reduce
      family), so those entries were read back as a different kind of row. The
      declaration side is checked where the counter is BUILT: the derive's
      `declared_slot` refuses declaration 4096 (ADR-0030's 4095 cap was prose
      until now) and `#[ok_junction(n)]` past 0x07FF is refused at expansion
      time (`compilefail/junction_id_out_of_range.rs`), with
      `KvJunction::dir_slot` asserting instead of masking. Tests:
      `integration::slot_counter_is_checked_not_masked`,
      `...::slot_family_is_checked_not_wrapped`,
      `okm-derive::tests::one_past_the_last_declaration_is_refused`.

## Panic surface audit and the unwrap gate (2026-10-24, gated)

A read-only sweep of the workspace's production sources — 136 `.rs` files, with
`tests/`, `benches/`, `examples/` and `#[cfg(test)]` modules excluded — counts 118
`panic!`/`unreachable!`/`todo!`, 115 `unwrap()`, 98 `expect()` and (inside those
two counts) 24 lock-poison sites, across 41 files. The test surface holds 1064
`unwrap()`s; that is the normal shape of a test crate and needs no work. The
classes below are ordered by what blocks what, not by size.

**The gate's first blocker is discharged.** `VirtualStorage::put` and `del`
(`okm-core/src/engine/storage.rs:31,37`) returned `()`, so a backend had nowhere
to put an I/O refusal: `slatedb_backend.rs` spent 18 `expect`s plus one
`panic!("slatedb iter failed")` on that gap, `redb_backend.rs` 10 more. ADR-0037
gave the typed write verbs their `Result`; ADR-0039 opened the storage trait
(`put`/`del` and their async twins), the slatedb sync path (a reply channel on
`Cmd::Put`/`Cmd::Del`, driver-side durability), `RemoteStore::send_write`, and the
two `WireClient` senders. The READ side followed (ADR-0039's second half), and ADR-0040 closed
the last
runtime class by declaring the CODEC's decode half infallible on purpose, with the
panic lints allowed only in the places it names. So the runtime half of
`okm-core` now reports zero warnings under `unwrap_used` / `expect_used` /
`panic`; what stands between the gate and the tree is the compile-time surface
(`okm-derive` + `build.rs`) and the bindings' own bodies, below.

- [x] Write verbs gain an error face (ADR-0037 for the typed verbs, ADR-0039 for
      the storage trait; both landed 2026-10-24).
- [x] A failing double for the write path, proving a refused write comes back as
      `Err`: `tests/write_refusal_test.rs` (a refusing store through
      `Collection::put`, plus the replayed-batch boundary) and two wire tests
      (dead connection, refused POST). Not covered, and said so in the ADR: fault
      injection into the native engines, and the nest's "executor gone" arm.
- [x] Writes answer (LANDED 2026-10-24, the decision ADR-0039 queued here): every
      well-formed frame answers exactly once — a write's answer is the empty ack, a
      refusal or a panic inside the engine is a `fault` — and both senders wait for
      it (`RemoteStore::round_trip`, `WireClient::ask`). Cost, accepted: a write is
      now a round trip, so the fire-and-forget write shape (ADR-0010 §6, ADR-0028)
      is superseded on the commit path. `None` is reserved for a frame that does not
      decode.
- [x] The read arms, same round: the slatedb driver's `Get` / `ScanRange` /
      `ScanSuffix` / `ScanStream` answer `Result<_, String>` over the channel
      (`SlatedbSync::request` returns `crate::error::Result`; a scan that cannot
      open hands back an iterator whose one item is the error), and redb's
      `scan_range_iter` reports a failed `begin_read` / `range` the same way.
      `localStorage` had no production read panic left.
- [x] The gate (landed 2026-10-24):
      `[workspace.lints.clippy]` denying `unwrap_used`,
      `expect_used`, `panic`, `unwrap_in_result`, `panic_in_result_fn`, inherited by
      every member. Tests keep the vocabulary through one crate-root
      `#![cfg_attr(test, allow(..))]` per crate, plus one `#![allow(..)]` line per
      `tests/` crate: `[lints]` is per-package and covers every target, and neither
      Cargo.toml nor clippy.toml can scope a lint to lib/bins (2026-10-24, verified
      on the seven sibling repos that now carry the gate). All nine members inherit
      it. The three binding crates are separate workspaces (`bindings/*`), so they
      need their own `[lints]` to inherit it; their bodies now carry zero
      `unwrap`/`expect`/`panic` (the 29 measured before the poison pass were 15
      single-line lock sites plus 14 others, all fixed), so turning the gate on
      there is just the manifest edit.
- [x] Poison, 28 production sites (bindings/okm-steel 7, okm-python 8,
      okm-stream 5, `okm-core/build.rs` 3, subscribe 3, nest 2):
      `unwrap_or_else(std::sync::PoisonError::into_inner)`, with the reason stated
      at `subscribe.rs::register` (a sink that panicked mid-delivery leaves
      recoverable state, not a broken invariant — propagating would let one bad
      sink wedge every later write). `build.rs` carried a copy of that same cell in
      its generated-code template and was fixed in the same pass, or the panic
      would have shipped into every consumer.
- [x] Fixed-width conversions and outside-byte decode paths: resolved as a
      decision, not a conversion (ADR-0040). Every one of them sat in an
      infallible-signature decoder (`VectorElem::decode_le`, `ReduceCodec::decode_acc`,
      `EdgeKey::decode`, `offset_decode`, `varint_decode`, `dec_wire`,
      `Vector::decode_payload`), so `?` had nowhere to go; the answer is that the
      codec aborts on bytes it did not write, and the panic lints are allowed there
      and only there. The genuinely foreign input — a Parquet file from another
      producer — was converted instead: column downcasts answer `Err`
      (`tooling.rs::column_as`) and the variant metadata frame reads with
      `first_chunk`.
- [x] (folded into the item above) Decode over outside bytes: the boundary is the
      SOURCE of the bytes. Arrow-export reads (`arrow_bridge.rs:60`, `:436`) read
      frames this SDK wrote, so they abort with a message; the Parquet import path
      reads a foreign file, so it answers `Err`.
- [ ] Bindings' own bodies, 29 sites (`okm-python` 18, `okm-steel` 10,
      `okm-entry` 1): a panic there is a dead interpreter or a dead host process,
      so the error face is `PyErr` / steel's value, not a message on stderr. (The
      host-injected seam itself already answers `Result<_, String>` — commit
      `38c5c6c`; these are the poison and host-call sites inside the bindings.)
- [x] Compile-time code: `okm-derive`'s diagnostics now carry spans. 116 of its
      167 abort sites became `syn::Result` → `compile_error!` on the offending token
      (a malformed `#[ok_index(..)]` reports at the token, not as "proc macro
      panicked" at the derive); 35 live inside `quote!` — that is generated code
      (the decoder's own aborts), not macro aborts — and 16 internal invariants
      stay behind eleven narrow `#[allow]`s, each with its reason. The crate-root
      allow is gone, and the seven trybuild fixtures were re-blessed: only the
      header line and the span changed, every message byte-identical.
      `okm-core/build.rs` keeps its crate-root allow (a build script that cannot
      read its inputs has nobody to tell); its generated-code copy of the sink cell
      was fixed with the poison pass, since that text ships into consumer crates.
- [x] The last 12 sites the gate reported, decided one by one: nine were code with
      no reason to panic and lost the panic arm — `okm-query`'s merge-join run drain
      (`if let Some`), `okm-dynamic`'s two post-length-check conversions and
      `graph.rs`'s suffix decode (`first_chunk`), `encode.rs`'s missing cold tag
      (`CodecError::ColdFieldWithoutTag`, the schema is caller data), a refused
      dynamic write (`replay` answers `Err`, it used to panic), and an exhausted
      field vocabulary (`claim_id` answers `Err` — a new name at runtime is input).
      Two stayed: `okm-vector/src/codec.rs` reads codes this build wrote, so a
      mismatch or a short code aborts, and that file-level allow is registered in
      ADR-0040. The workspace now passes all five panic lints under `-D`.

## 2026-10-27 — the graph faces carry both endpoints (ADR-0041)

- [x] Four faces instead of five, each naming both endpoints in traversal order:
      `[src][dst][edge_id]`, `[dst][src][edge_id]`, `[pred][src][dst][edge_id]`,
      `[pred][dst][src][edge_id]`. The old predicate index retired — its edge set is
      the bare `[predicate]` prefix of the predicate+out face, so one fewer put and
      one fewer delete per link. Slots compacted to 0x06–0x09 with the vector index
      snapshot moved to 0x0A.
- [x] New lookups: `out_edges_to`, `in_edges_from`, `typed_out_to`, `typed_in_from`
      (and their okm-dynamic twins). `typed_out_to` is the assertion identity check
      k10r's `ingest` now uses (k10r ADR-0020) — one seek instead of a scan over the
      source's out-degree.
- [x] `Graph::reindex()`: wipes the four graph slots and re-derives every entry from
      the primary rows. Mandatory rather than optional, because the numbers changed
      too — the retired predicate index sat exactly where out-edges live now, so an
      older store's entries would otherwise be read as something they are not.
- [ ] To pay later: a schema version + background reindex for stores too large to
      rebuild in one batch (nobody needs it yet), and the vector snapshot's own wipe
      after its move (a stale blob fails its magic and reads as a miss today).

## 2026-10-27 — layout versions are stamps (ADR-0042)

- [x] Written down after two changes were shaped by the codec: the entity row's
      usage counters were appended at the tail because "appending is legal", and
      the vector's removal was argued as a migration. Neither is the policy any
      more: fields go where a reader looks, a shape change changes the number
      (incrementing is the ordinary style), and no build owes an old shape a
      reader — migration is export → import (ADR-0042).
- [x] k10r's `EntityRow` reset to `version = 1` with its counters at the FRONT
      (kind, invoke_calls, invoke_ok, last_invoked, then the rest).

## 2026-10-28 — the table's own record (ADR-0043)

- [x] Fixed face **0x0F** = table meta, `[partition?][ns][0x0F]`, no suffix. The
      highest face on purpose: `slot <= 0x0F` is the one/two-byte delimiter, so the
      meta sits on the boundary and 0x0B–0x0E stays the contiguous free band.
- [x] Value = name → bytes map (`[count u16][name_len varint][name][value_len
      varint][value]`), names inline so stamping never touches the field-name
      vocabulary. `version` is the runtime's name (same number as the payload
      header); other names are the caller's.
- [x] `Collection::{meta, ensure_layout, set_meta}` — I/O-free `new` stays as it is;
      `ensure_layout` seeds an absent row, answers a matching one, and refuses
      `LayoutMismatch { stored, current }` otherwise. Detection, not compatibility
      (ADR-0042): nothing is read through it and nothing migrates.
- [x] `okm-core/tests/meta_test.rs` — 7 locks: address hex, seeded payload hex, the
      write-once stamp, a second name beside it, both refusal shapes, and row scans
      never surfacing the entry.
- [ ] `okm-dynamic`: a `DynamicCollection::ensure_layout` through the same helper
      (`okm_core::meta::ensure`, its own `schema.layout_version`). The FORMAT is
      shared already; the verb lands with the first dynamic consumer that attaches
      to a store — the typed assembly point is what stamps today.

## 2026-10-28 — a table is a key type plus a row type (ADR-0044)

- [x] Written down once instead of argued per case: the key is its own declaration
      (`KeyEncode`, fixed width, its own name list) and the row NAMES it
      (`#[ok_ref]` → `Document::Key`) without embedding it. Two consequences that
      used to be implicit now have a stated reason.
- [x] Side tables (ADR-0036): one key type, many row types, byte-identical key
      payload, separate ns and separate commits.
- [x] The same name may appear in the key and in the value, and the two fields are
      unrelated: nothing dedupes, nothing errors, nothing compares them on a read.
      The classifier is the payload declaration, and the key's bytes never enter
      the map `fetch` returns.
- [x] `okm-core/tests/key_row_split_test.rs` — 3 locks: the same name in both
      halves (payload copy may disagree with the address), the shared key type
      across two tables (primary keys differ only in the ns), and a map name that
      matches a payload field writing the row rather than the segment.
- [x] The SQL contrast recorded in the same ADR: a relational engine keeps the key
      inside the row and hides an off-row split as overflow pages (row id + chunk
      ordinal appended); okm has no row outside a key, so the split is a declared
      side table whose ns is PREPENDED to the address with the key payload
      untouched — queryable, separately committed, and composable only by two point
      reads.

## 2026-10-28 — the change log: capture at the commit boundary, drain out of process (ADR-0045)

- [x] `ChangelogConfig { dir, segment_bytes: 64 MiB, segment_seconds: 300,
      buffer_bytes: 1 MiB, sync_data: false }` plus
      `Collection::with_changelog(store, config) -> Result<Self>` — the fallible
      constructor is where the table's directory is opened; `new` stays I/O-free
      (ADR-0043). No cargo feature: the writer needs no dependency, and the cost
      when off is one `Option` check per commit.
- [x] Capture in `Collection::commit`, after `commit_batch` returns `Ok`,
      filtered in memory before it — the only funnel that sees every semantic
      write (`put`/`delete`/`merge`/`replace`/`purge`). `save_into` bypasses it
      (the caller commits to the store directly), so the assembly point also
      gained **`Collection::commit_batch`** — stage, commit, append, same path.
- [x] Cross-table batches: `stage_capture(ops)` stages a collection's own share
      BEFORE the one commit and `flush_capture()` appends after it returned Ok, so
      every participating table's log gets its rows and none of them describes a
      write the engine refused. `commit_batch` stays the single-collection
      shorthand (it captures its own header's keys and drops the rest, which is
      what a one-table publication wants). `changelog_test` locks the split: one
      batch carrying two tables' keys, two stages, one commit, and each log holds
      only its own row.
- [x] Filter is an EXCLUDE list, stated once and failing closed toward the lake:
      carry the set an export carries — primary `0x00`, dynamic `0x01`, field-name
      vocabulary `0x02`/`0x03`, predicate vocabulary `0x04`/`0x05`, and both
      directions of every declared categorical vocabulary (`0x1nnn`/`0x2nnn`, the
      labels are truth because the payload stores ids); drop declared
      index `0x3nnn`, reduce `0x4nnn`, junction `0x5nnn`, graph faces `0x06`–`0x09`
      (`Graph::reindex` rebuilds them from the primary rows, so an edge rides as a
      row), vector snapshot `0x0A`,
      meta `0x0F`, bare-face singleton keys (the vocabulary cursor is a face's
      bare prefix, ADR-0032), and anything unrecognised. A visibly incomplete log
      can be repaired by a reconciliation scan; a lake holding derived state
      cannot.
- [x] Frame = the op-frame encoding (`put_len` + `put_op_header`, the primitives
      `OpFrame::encode` itself uses — `write_batch` would clone every key and
      value) behind an outer length
      prefix — the codec deliberately carries no total-length header (a
      byte-stream transport owns delimiting), so one committed batch is one frame
      and vocabulary growth needs no record of its own.
- [x] Segments: `<root>/<header hex>/<nnnnnnnn>.partial` while being written,
      renamed to `.segment` on seal (the suffix names the artifact, not the
      transport — `.log` reads as an application log); rotation on bytes and on
      wall clock. The next segment number is the highest name in the directory
      plus one, and the offset is the append position — so there is no producer
      counter, no reservation, and no sequence holes.
- [x] The write side has no reader: a collection with the changelog on appends and
      rotates and forgets the file, and the drainer opens a DIRECTORY (never the
      store), so it can be another process on another machine fed by copied or
      synced files. Only two invariants cross: a `.segment` never changes, and
      nothing is deleted before its consumer's watermark passes it.
- [x] Reader: sealed segments in name order, frames in append order, stopping at
      the first incomplete frame of an unsealed segment (a torn tail is a normal
      end; a torn frame inside a sealed segment is an error).
- [x] Schema sidecar: `CollectionSchema::of::<K, R>()` JSON per layout version
      beside the segments, written by `with_changelog` when `schema-serde` is on
      (`schema-v<N>.json`, write-then-rename, idempotent, and a REFUSAL when the
      same version already describes another shape). Names ride in the log; types
      and key layout do not — the sidecar carries `key_len`/`key_fields` (offsets +
      widths), the hot/cold lists with tags, `layout_version` and the slot map.
      Without it the log is readable as bytes and useless as columns.
- [x] Wrapper kinds in the dynamic codec, which had diverged from the typed one
      in three ways: `encode::storage_be_bytes` wrote an 8-byte BE image for
      `VarInt`/`Quant`/`Offset` (the typed side writes a prefix-monotonic varint
      and a 4-byte biased u32); the decoder demanded exactly 8 bytes for all
      three; and `decode_fixed` REJECTED a hot `Quant`/`Offset` field outright, so
      a table declaring one could not be read dynamically at all. Fixed by
      dispatching on the field KIND and letting the value be LOGICAL — the codec
      applies `Quant`'s precision and `Offset`'s base, which the schema already
      carries. `dynamic_cross_test` now locks byte equality with the derive for a
      row of all three kinds (4-byte biased offset, 8-byte fixed-point quant,
      2-byte varint 300) plus both round-trip directions, and the storage-form
      contract is gone from `field_pair` (a quant field fed its wire is a type
      error, not a silent integer). Bindings coerced by kind and were updated to
      hand over f64 / i64.
- [x] Schema-driven columns: `ValueMap` (+ key bytes) → `RecordBatch`
      (`okm_dynamic::arrow::record_batch`, `arrow` feature), reusing
      `arrow_backend::arrow_type` — now public as the ONE `FieldType → DataType`
      mapping — and `dynamic_cross_test`'s sibling `arrow_columns_test` locks it:
      the same rows through the typed bridge and through this builder produce the
      same schema and equal column data, including a row that declares a COLD
      field first and another between hot ones (`ordered_fields` puts the
      hot/cold split back into declaration order, which works because a cold
      field's TLV tag is its declaration index). A `Vocab` column resolves through
      the caller's replayed vocabulary and is NULL — never invented — without one;
      a present value of the wrong kind is an error, not another NULL.
- [x] `okm-drain`: the SECOND layer — `drain(root, cfg)` walks the table
      directories, replays a window of N sealed segments, decodes through the
      sidecar schema and builds ONE Parquet file with the schema-driven columns.
      The header comes from the directory name (it is `<header hex>`), vocabulary
      labels are replayed from the id faces the log carries, and a watermark file
      per consumer moves only AFTER the file exists (atomic rename), so a window is
      never drained twice and never half-recorded. A window is history: the same
      key may appear twice and collapsing it is the sink's compaction, not this
      crate's promise. What it refuses: a window holding a DELETE (the declaration
      has no column for a removal) and a table with no sidecar (bytes, not
      columns) — both stop with the watermark untouched and no artefact written.

- [x] `--features arrow` alone did not build: `bridge::iceberg` (a Parquet
      METADATA writer) and `Graph::to_record_batch_table` (the variant column) were
      gated on `arrow` while using the `parquet` crate. Both are `parquet` now, and
      the imports they used followed.
- [x] Tests (`okm-core/tests/changelog_test.rs`, 7 locks): frame round trip through
      a file; `merge`/`replace`/`purge` captured; derived faces and the vocabulary
      cursor absent; the categorical labels present; unsealed tail ignored;
      sealed-segment truncation refused; both rotation triggers; the
      `commit_batch` verb capturing `save_into`; status clean for an unused log.
- [x] Bench the capture tax: **~+50 to +135 ns/write (~5 to 8%)**, typically ~90 ns,
      on a durable `fjall_tmp` put (1.32–1.55 µs plain); 9 paired rounds x
      5,000 puts, order flipped each round, median of per-round ratios — two fjall
      stores compact on their own schedule, and an unpaired median printed a NEGATIVE
      tax once). Run with `cargo run --release --example changelog_tax -p okm-core
      --features test-engines,fjall`; the numbers sit in
      `docs/internals/benchmarks.md` beside the recorded 1.31 µs semantic put.
      The criterion harness still hangs here before any measurement (reproduced with
      a pure-CPU filter and with `--list`), so the hook in `core_paths.rs` stays a
      baseline for a machine where it runs.
- [x] The in-memory slatedb test engine stalls under volume — FOUND while benching,
      diagnosed and fixed. It was not a deadlock: `SlatedbSync` awaited
      `WriteHandle::await_durable` on every `put`, and durability waits for the next
      WAL flush — slatedb's default `flush_interval` is **100 ms**, so a serial put
      loop ran at 10 writes/second flat (measured 101 ms/put; 300 puts = 30.3 s),
      which reads as a hang under any long loop. `drive()` now takes durability as a
      property of the STORE: the in-memory engine skips the wait (its object store
      dies with the process — nothing to recover), a Db handed in by a host keeps
      it. Same 300-put loop: 30,296 ms -> 5 ms (~17 µs/put). The capture tax is still
      measured on fjall, because a memory store is not an honest arm for a
      durability-related cost.
- [x] Drain-side pieces that stay outside core, built where they are owned: the
      watermark, one file per consumer, replaced by atomic rename and moved only
      after the window's Parquet file exists (`okm-drain`). The repair path landed
      too, on the side that owns the store: `Collection::reconcile()` (whole table)
      and `reconcile_range(begin, end)` re-emit the rows that still exist as the
      log's own PUT frames, so a sink folds them like any other write. What no scan
      can repair is a REMOVAL — a key that is gone looks exactly like a key that was
      never written — and that limitation is written into the method's doc.
- [x] Order is commit-then-append, so the guarantee is at-most-once and never a
      phantom row: `sync_data` narrows the crash window but does not change the
      class, and deleting rows after an export waits for "window sealed and taken"
      plus a reconciliation scan. Written down because the alternative order makes
      the lake disagree with the store, which is the one failure a derived copy
      must not have.
- [x] Output is a WINDOW, not a table format: sealed Parquet files + the schema
      sidecar (`okm-drain` writes the file; the sidecar stays beside the segments
      it describes), plus a MANIFEST per window (`<stem>.manifest.json`: table,
      layout version, the segments covered, row count, columns with their Arrow
      types and nullability, and the file's name) written before the watermark
      moves. Three layers: the store
      produces segments and the sidecar; the DRAINER groups N sealed segments into
      a window (its policy — the sink sets the file target); the ADAPTER loads
      windows into a table format. Semantics do not vary with the format:
      append-only windows, current state via compaction (rewrite) or a sink that
      can express deletes, and no sink may weaken §6's ordering to buy a cheaper
      capture.
- [x] Where each part lives: the drainer is `okm-drain` in this workspace (it only
      needs okm-wire + okm-dynamic + arrow/parquet, all already here) and the sink
      adapter is its own repo (`okm-iceberg`, depending on this one — never the
      reverse). The split is by DEPENDENCY, not by taste: the adapter is what
      pulls in a catalog client, HTTP and auth, so it must not sit in this build.
- [x] Sink capability contract, written once for third-party adapters too
      (`docs/drain-contract.md`): key columns; some route to current state (native upsert key / delete files /
      periodic compaction); the dynamic segment natively as a variant type, or
      degraded to a JSON string under okm's mapping (Binary as base64, `Int`/`UInt`
      kept as numbers, lists and maps as arrays and objects). The mapping is okm's
      to publish — otherwise every adapter invents a dialect.
- [ ] Not in this unit: Iceberg snapshot append with delete files, partitioning
      and file-size targets (`write_table_metadata` writes one snapshot over the
      files it is handed); any Parquet read path; the Iceberg WRITER
      (it goes in its own crate, because `bridge::iceberg` is a zero-dependency
      metadata encoder and must stay one); the SQL-to-okm planner. The
      schema-driven Arrow builder is NO LONGER here: it landed as
      `okm_dynamic::arrow`, reusing `arrow_type`, with the typed bridge as its
      drift lock.

## 2026-10-28 — the append-only log (ADR-0046)

A sibling crate, `okm-series`, for data whose whole life is "append in time order, read
by offset or time, drop the old end". Queues sit above it; time series use it. It is
NOT an okm table and does not implement `VirtualStorage` — the reason is retention:
in an LSM every dropped record leaves a tombstone, and the file is the only unit
that can be dropped in one syscall. Design in ADR-0046. Nothing below is built.

- [x] `okm-series`: `log.meta` (epoch, precision, bucket), `LOCK`, `<bucket>/part-<n>.open`
      → `.log` by rename, retention by `remove_dir_all`. The lock is a pid file: a live
      pid refuses, a stale one is taken over — no `flock`, because a stale lock after a
      crash must not need an operator.
- [x] The record and part headers: `[len][ts delta varint][nTLV body]`, `base_ts` +
      monotone deltas, `first_offset` / `record_count`. **No per-record checksum** — a
      crash can only truncate the tail (nothing is acknowledged before its fsync, and a
      part is only written at its end), so the torn record is the one whose `len` runs
      past EOF. Integrity against a lying storage stack belongs to the storage stack;
      the part footer carries one whole-part digest for copies and exports.
- [x] Blocks: 64 KiB of records compressed per block (zstd level 1; `Raw` when the
      compressed bytes are no smaller), at buffer flush rather than at seal — every
      block, not only the long ones. `Config::dict = None` switches interning off,
      because block compression covers part of what it was for; the choice is a
      measurement, not an argument.
- [x] `part-<n>.idx`: one row per block `[first_ts][first_offset][file_offset][len_c]
      [len_u][records]`, appended when the block is written; seek = parts' first rows,
      then the rows inside the part. Per part, NOT in `log.meta` and not per bucket.
- [x] The seal footer: records, last timestamp, block count, whole-part digest. Not the
      byte sizes (the file has a length, the index has the rows), not the first
      timestamp (the header has it), not the dictionary count (the dict file counts
      itself) — a footer that repeats the disk is a second copy that can disagree.
- [x] Partitioning by one field, declared at creation and never changed: the value of
      a declared body tag becomes `<bucket>/p-<hex(value)>/part-<n>.*`, the dictionary is
      per (partition, part), retention is still per bucket, and a record without the tag
      is refused (`MissingPartition`). Two things the implementation settled that the
      design had wrong or vague:
      **offsets are per stream** (a block header carries only its first offset and the
      reader counts up, so a global counter would make an interleaved part unreadable),
      and **there is one `LOCK` per log, not per partition** (the writer that holds it
      owns every partition; a per-partition lock only matters if two processes write
      different partitions of one log, and nothing asks for that).
- [x] The write time is the log's, not the caller's: `append(body)` takes no timestamp.
      No configuration names a "time field" (it would collide with a real tag and nothing
      can predict that), no per-record clock to trust, and the time axis is ingestion
      order — which means **no backfill**, stated out loud in ADR-0046 §2. A clock that
      steps back is clamped to the stream's newest timestamp instead of failing a write
      the caller did nothing to deserve. Tests drive this through `Config::clock` (a
      test-only seam; the binding deliberately does not expose it — a Python caller
      cannot stamp a record). Reading exposes the stamp as a field name, `_ts` by default,
      configurable to another name or to `None`.
- [x] Fixed while writing the Python binding: monotone time is a property of the STREAM,
      not of a part. A record an hour older than its predecessor lands in an older
      bucket, opens a fresh part with no history, and passed the per-part check — so
      "time cannot go backwards" was only true inside one bucket. The log now remembers
      the newest timestamp each stream has seen (recovered from the parts on open) and
      refuses before anything is written.
- [x] `bindings/okm-series-python`: a PyO3 binding, standalone crate (not a workspace
      member, same posture as `okm-python`), `import okm_series`, `Log(...)` with
      `append`/`append_durable`/`append_many`/`flush`/`seal`/`read`/`read_by_time`/
      `retain_before`/`parts`, bodies as dicts keyed by tag or by the names given to
      `Log(tags=...)`, values as str/int/float/bool/bytes, `OSError` for IO and
      `ValueError` for refusals. Its `host-tests` build links the interpreter, so the
      tests run with `--no-default-features --features host-tests` (the wheel posture
      must not link libpython). Verified end to end by importing the built `.so` from
      Python, not just from Rust.
- [x] Fixed while building partitions: an OPEN part reported its time span from its last
      index row, which is its last block's FIRST record, so a range read could skip a
      part whose later records were in range. The span is now computed at open from the
      last block, and there is a test that reads a range out of an unsealed part.
- [x] `append(body) -> offset`, buffered until a block fills, with `append_durable` as
      the fsync-before-ack path (a block per record, which is the trade the caller
      chose). Single writer enforced by `LOCK`. No timestamp argument: the log stamps the
      write time (see the entry on write-time stamping above).
- [x] `read(from_offset, max)` and `read_by_time(from, to)`: the part is picked by the
      range in its header, then `part-<n>.idx` seeks inside it.
- [x] nTLV string forms: `kind = 0` inline, `kind = 16 + k` = the id of a string in
      dictionary block k, whose width the block header declares. An unresolvable code
      is an error, never a wrong string.
- [x] `part-<n>.dict`: blocks are assembled in memory and written COMPLETE, so a block
      carries an exact count and nothing is ever patched; `prev_offset` chains them
      newest-first, and `<part>.dict.tail` (renamed into place) names the newest. The
      chain widens when a block reaches `block_entries` (65536 by default): u16 → u32 →
      u64. The dictionary is flushed BEFORE the records that refer to it.
- [x] Promotion by tiers of the code's own width, `[[4, 10], [8, 5], [16, 3], [0, 2]]`:
      `L <= W` never, `W..4W` at 10, `4W..8W` at 5, `8W..16W` at 3, `16W` and beyond at
      2. Configuration, not format; counts are per part and restart hourly, which is
      why the short end is the only conservative row.
- [x] `flush()`: the durability boundary for a caller who batches — write and fsync the
      buffered block without a phantom record. `append_durable` remains the per-record
      path (a block per record, the trade the caller chose).
- [x] Measured, three runs, in `docs/internals/benchmarks.md`: 0.5–1.9 µs/record to
      write and 4–21 M records/s to read. The interesting result is that the dictionary
      and the block codec are not interchangeable — on closed-set fields the dictionary
      cuts the bytes 4x on top of compression AND makes the write faster (a smaller body
      gives the codec less to chew), while one unique id per record makes it buy 3% for
      15–25% of the write time. That is the shape `Config::dict = None` is for.
- [x] Tests: a part that survives a torn tail and keeps appending; a string promoted at
      its tenth occurrence with the first nine inline; a long value promoted at its
      second; a string no longer than its code never interned; an escalation to a wider
      block that the reader resolves; retention that drops a bucket directory; a
      dictionary that survives a reopen and still codes the next record.
- [x] Decided: the changelog's segments do NOT ride this substrate. The shared part is
      a convention (a directory of parts, seal by rename, cut a torn tail, ~150 lines);
      the contract runs the other way on three points — a monotone timestamp per record
      (the changelog has no clock, and a capture path that fails because the host's
      clock stepped is worse than none), fsync before the ack (there it acks after the
      commit and repairs holes by rescanning the store), and retention by time (there it
      is a per-consumer watermark). What may still be borrowed, without accepting any of
      that: block compression inside a segment, if segment size is ever shown to matter.

### Owed on `okm-series`

Not bugs found and postponed — the list of things this crate does not do, written where
the next decision can find it.

- [ ] Owed: **a production caller.** Nothing writes to this crate outside its tests, the
      `series_tax` example and the Python smoke script; `okm-drain` still reads the
      changelog's segments. The API has been checked by tests and one binding and has not
      been worn by a real user, so its shape may still be wrong in ways tests cannot see.
- [ ] Owed: **config drift is silent.** `log.meta` pins `precision`, `bucket_seconds` and
      `partition_tag`; `block_bytes`, `zstd_level`, `block_codec` and `dict` are NOT in it,
      so two opens can disagree about them without a word — new parts simply use the new
      values. Decide: pin them (and refuse a mismatch, as `partition_tag` does) or record
      them as "properties of the part, not of the log" and say so in the ADR.
- [ ] Owed: **an idle log's part is never sealed.** Sealing happens on a bucket crossing,
      on `part_bytes`, or on an explicit `seal()`. A log that stops writing keeps its last
      part `.open` forever — no footer, no whole-part digest — and `retain_before` refuses
      to drop that bucket because retention never touches a bucket with a live writer. The
      fix is a seal policy (`seal_after_seconds`, driven by the next append or by a
      caller's tick) and it must not be a background thread (ADR-0046 §3 has no timer).
- [ ] Owed: **reading one stream.** `read(from)` applies `from` to every partition. A
      caller that wants one partition filters `parts()` by hand and reads the files itself.
- [ ] Owed: **`Error::TimeBackwards` is unreachable from the API** — it survives as the
      writer's internal assertion (`part.rs`), and the log clamps instead. Either delete
      the public variant or document it as an internal invariant that no caller can hit.
- [ ] Owed: **no concurrency test.** One writer is enforced by the `LOCK` pid file, and the
      stale-pid takeover is a heuristic: a reused pid makes a legitimate open fail. No test
      covers two openers, two readers during a flush, or a `kill -9` mid-block.
- [ ] Owed: **packaging.** No `maturin build` / `pip install` round trip, so the wheel is
      untested (only a hand-copied `.so` on `PYTHONPATH`), and no CI for either crate.
- [ ] Owed: **`parts_by_offset` opens each part twice** — once for its offset span, once
      to read. Small, but it is the read path's only obvious waste.
- [ ] Owed: **the binding reads everything at once.** `read(from, max)` returns a list;
      there is no iterator/streaming form, no partition filter, and no async surface.
