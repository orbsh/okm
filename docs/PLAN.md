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
      in-process use for embedded-language Actors. Permanent capability
      ceiling: no reduce/subscribe (Rust compile-time logic; dynamic rebuild
      would break exactly-once). Core shipped 2026-09-12: `okm-core::schema::
      TableSchema::of` (structured export, serde behind `schema-serde`) +
      `okm-dynamic` crate (Value tree; encode/decode mirroring the derive's
      byte layout; version gate + unknown-tag skip); cross-language byte
      equality locked by dynamic_cross_test. Remaining: PyO3/Steel bindings;
      version-default migration on the dynamic read path — `#[ok_default]`
      values must travel in TableSchema (derive exports them; decode applies
      defaults for fields missing from older payloads, mirroring the Rust
      decode rule); schema reverse-import (Python-declared key/row/table →
      Rust runtime execution) deferred.
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
- [x] Schema export: TableSchema extended with SlotMap (fixed-role slot
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
      their own contracts. Remaining: derive field-position recognition
      (`Option<T>` in a struct field → OptionalEnc impl + width).
- [ ] Derive support: field-position recognition like the other wrappers
      (width = `1 + T::WIDTH` in FieldDesc), `[ok_default]` interplay
      documented (default decides what a missing pre-v2 payload decodes
      to; `Option` decides presence within a payload — orthogonal).
- [x] First use cases: watermark/cursor fields where 0 is a real value
  (MQ cursor keeps its 0 semantics — a plain u64 stays correct there;
  `Option` targets genuine None/Some distinctions: config overrides,
  optional foreign keys). Aura MQ audited 2026-09-16: no field needs
  `Option` today — its tables are pure typed-path (put/get/scan), so no
  migration was required by the obj work.

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

