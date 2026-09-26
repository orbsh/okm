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
- [x] `Table<S, K, R>` assembly point: plain generics, no assembly macro;
      holds the shared engine instance; `put(row)` = primary key write +
      index entries in one engine batch; `Row::table(store, ns)` constructor.
- [x] `save_into(batch, key, value)` cross-collection atomic path (ADR-0003
      mechanism note): encode into an externally owned engine batch without
      touching the collection's internal buffer; for primary + index across
      two assembly points (single-table case already covered by `put`).
      Landed 2026-09-11: `KvBatch` trait + `KvEngine::batch`/`commit_batch`
      (default carrier MemBatch — op list replayed; fjall overrides
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
      (a prefix would sort by length first and destroy dictionary order);
      exact matching resolves via the trailing primary key + fetch-back.
      Function indexes (`func(path)`) shipped alongside: the sort segment
      is the declared function's result (`IndexFuncResult` — String →
      UTF-8 dictionary order, uints → BE numeric order); the query side
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

## Phase 5 — Event layer (ADR-0008): reduce rename, subscribe channels, okm-stream

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
- [x] Channel payload form: the event Enum is generated by build.rs from
      `#[ok_subscribe]` (bare) declarations — the variant IS the row type
      name, the enum name is `RowEvent` by default and renameable per-row
      via `#[ok_event_enum(Alias)]` (multiple aliases = multiple enums,
      each with its own `CHANNEL_<ENUM>` cell). Zero hand-written
      mapping: send site and enum are derived from the same declaration,
      so drift is a compile error and match exhaustiveness is free. The
      per-row-type bare channel (degraded fallback) is explicitly
      UNSUPPORTED (user decision 2026-09-11) — `#[ok_subscribe]` rejects
      arguments, the derive emits no per-row statics, and
      `ChannelCell`/`Event` remain core library types for hand-assembled
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
      generated per-enum channels (each `CHANNEL_<ENUM>` is its own
      stream head); merge/fan-in combinators over multiple heads land
      with the first consumer that needs them.
- [x] Doc pass: INTEGRATION/MODELING twins updated for the event layer
      (inline exactly-once vs channel no-guarantee boundary; reduce
      never a channel consumer; trigger asymmetry). MODELING.md /
      MODELING.zh-CN.md landed 2026-09-11 ("Write-path events: inline
      and channel" — epoch semantics, both disciplines, transport
      boundary); INTEGRATION twins cover the trigger asymmetry.
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
matching the best-effort channel contract (precision-bound consumers use
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
      maintenance, reduce hooks and (after the event layer) channel emission
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
      — insert, RMW, index scan, reduce fold, channel emission).
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
transport-free surface (Some = get/scan, None = put/delete); the mpsc
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
- [x] Remote backend (sender side): impl VirtualStorage; write = MemBatch
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
      is the transport-free intake; prepend declared prefix → plain
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
      MemBatch→frame mapping; `RemoteStore::commit_batch` now shares it;
      zero new op tags). Locked by accept_remote.py: the same scenario
      (fresh put / same-group overwrite / cross-group move / delete)
      planned Python-side and Rust-side lands hex-identical frames.
      Steel callable surface remains open work.
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

## Phase 8 — Object model: one encoding, field-name dictionary, kv_ → ok_ (ADR-0012)

Design locked in [ADR-0012](adr/0012-object-model-and-field-dictionary.md).
Core idea: a declared row IS a doc with an empty dynamic segment — one
encoding, one derive family. No separate document storage mode.

- [x] Rename: attributes `kv_` → `ok_` (`ok_ns`, `ok_index`, `ok_ref`,
      `ok_head`, `ok_subscribe`, `ok_default`, `ok_event_enum`, plus
      `ok_reduce`/`ok_offset`/`ok_layout` caught in the sweep); derive
      `RowEncode` → `ObjEncode`; docs' concept vocabulary follows. Shipped
      2026-09-14 (ec07a33) — landed before crates.io publishing as required.
- [x] Slot allocation revision: slot 1 = obj dynamic segment, slot 2/3 =
      field-name dictionary (bidirectional), slot 4–13 reserved (two-ended
      growth buffer), slot 14/15 = edge fwd/rev, slot 16+
      indexes/reduces. Existing rows byte-compatible (they use none of
      the new slots). Shipped 2026-09-16 (f4c9136).
- [x] Dynamic segment (slot 1): one entry per obj, value =
      `([field-id][value-type][len u32][bytes])*`; value-type byte is a
      small closed enum (int/uint/float/str/bytes/bool/null/array/
      obj-reserved). Shipped 2026-09-16 (cdefdea): ObjValueType tags
      (EnumTag discipline), obj_dynamic put_frame/decode_variants,
      unknown-type skip, malformed-tail partial results.
- [x] Field-name dictionary: run-time per-table append-only vocabulary;
      first-seen name claims the next number (single-writer, engine
      mutex); id `0xFF` escapes to `[0xFF][u16 id]` — no renumbering,
      ever; flat bidirectional point lookups (slots 2/3), no trie.
      Shipped 2026-09-16 (d18cb36): DictCache (by_id/by_name/next_id),
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
      addressing, and writes are all name-first; the dictionary is a
      per-table append-only map cached wholesale on first access, so a
      name lookup is one hash probe); `get_object(key) ->
      Option<BTreeMap<String, Value>>` merges declared (FieldDesc names)
      and dynamic (dictionary names) into one view — a combinator over
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
- [x] Dictionary cache is **bidirectional**: `DictCache { by_id: HashMap<u16,
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
      dictionary is a genuinely shared space (b2bd478).
- [x] Table/edge layout fully uniform: same header discipline, edge just
      declares two slots instead of one. ADR-0001 superseded (rationale
      recorded in the doc header).
- [x] Sweep: `edge.rs` (`head_bytes`/`DIR_BIT` deleted), collection.rs /
      slatedb_backend.rs prefix scans, integration hex locks, ADR-0001
      superseded. Edge key bytes changed — dev stage, no stored data.


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
- API verbs: `get`/`put`/`delete` (typed, unchanged) + `get_document` /
  `put_document` / `delete_document` (both slots) + `get_fields` /
  `put_fields` / `delete_fields` (dynamic segment; the old `variants`
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
  Obj → Variant object (field names straight through — the dictionary
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
  from `get_fields`' map (names resolved); append as `variant` column
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
      per graph (normal ns dictionary); self-describing endpoint
      references ([ns 2B][pkey], pkey width via a ns->KEY_LEN registry,
      compile-time `nodes(...)` declaration); EIGHT entry kinds on
      ADR-0016 segments — primary (0x0, edge_id u64) / kind dictionary
      (0x2/0x3, DictCache reused) / kind index (0x4) / out (0x5) / in
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
      when dynamic-width (`Vector<String>`). `get_document` lifts it to
      `DynVal::Array` — the dynamic layer has no Vector type; Vector is a
      STORAGE-layer format. Embedding vectors are the anchor use case.
      Elements are pure values — the identity line keeps this out of
      Ref/Junction.
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
