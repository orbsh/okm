# 0076 — The mechanism moves under the declaration; the query surface stays two

Status: decided 2026-11-21; implemented the same day. §1 (one entry producer) and §2 (one ledger orchestration) are in; §3 is complete — every mechanism module moved into core (`encode`, `path`, `access_method`, `plan_fields`, `byte_reduce`, `dynamic_collection`, `dynamic_plan`, `dict_write`, `dynamic_graph`, `dynamic_cluster`). The projection half went to the crate that already projects — `okm_lake::columns` — and `okm-dynamic` is gone: its tests moved next to the code they test, and the lake, the log and the bindings name core's modules directly.

## Context

`okm-dynamic` sits above `okm-core` and re-implements mechanism core already owns. It has its own entry producer (`okm-dynamic/src/index.rs:366`, answering owned `(Vec<u8>, Vec<u8>)` pairs), its own ledger orchestration (`reduce_get` and `scan_reduces` in `okm-dynamic/src/reduce.rs:157/166`), its own field encoders, its own path indexes, and its largest two modules — the schema-free forms (`cluster.rs` 1,986 lines, `graph.rs` 1,094, together 3,080 of the crate's 7,407 over twelve files). Two implementations of one key layout are held together by byte-equality tests, and a test observes drift; it does not prevent it.

Core has meanwhile acquired what the dynamic side needs in order to execute rather than repeat: `CollectionSchema`/`SlotMap`/`TablePlan` (ADR-0074), the payload decoder (ADR-0056), the batch with its op spans, and the reused `Entries` buffers a write appends into. What `TablePlan` does not answer yet is what an entry IS. Today it carries the ns header, the slot bitmap and the slot numbers — that is routing — while entries are still produced by generated code: one marker type per `#[ok_index]` (`KvIndex` impl, `okm-derive/src/document_encode.rs:514`), one aggregate body per document that calls them in slot order (`:1028`).

The next step was already measured and named. A hardcoded plan costs 3.0 ns key encode, 3.0 ns key decode and 15.7 ns payload decode against the derive's 5.2 / 3.0 / 12.1, and the gap to the `Dict` path's 52.4 / 147.2 / 307.1 is the dictionary, not the interpretation (`cargo bench -p okm-core --features fjall --bench core_paths -- plan-tax`). Interpretation is not the cost; two producers are.

## The decision

1. **The plan is the only entry producer.** `TablePlan` grows an `EntryLayout`: each declared face compiled once into field references — a key field's static offset, a hot field's offset, a cold field's tag — plus its includes and its tail. One walk over that layout appends the faces into the collection's `Entries` buffer from the key bytes and the payload bytes the write has already built; a reduce's group segment compiles the same way. The generated `KvIndex` implementations keep exactly what a compiler can check and a runtime cannot: `SLOT`, `FIELDS`, `INCLUDES`, `KEY_PREFIX`, `FUNC`, `admits`, and the per-field const assertions of the probe builder. `Document::index_entries_into` becomes the call that hands its key and document to the walk; the per-index bodies stop producing. Routing stays a const for the typed side (`Document::PLAN`, ADR-0074 §3) and is compiled at open for the dynamic side (`TablePlan::compile`); the entry half is compiled once from the declaration each form owns — the typed side's emitted table, the dynamic side the access methods its caller declared — because the store's face description does not carry it. One producer, two ways to have obtained the declaration.

2. **One ledger; the carrier owns the error.** Core's `reduce_get` / `scan_reduces` / `apply_row` already own the read-modify-write over an accumulator, and the dynamic side repeats it over `Vec<u8>`. The seam exists: `ReduceCodec` (`encode_acc`/`decode_acc`, `okm-core/src/model/reduce.rs:26`), with the byte accumulator as one more implementation. The dynamic fold is fallible because a `Dict` can fail to match the declaration, and because a fold is user code besides. Widening the typed fold to carry an error it cannot produce would be paying, on the write path, for a failure its caller cannot cause; making the dynamic fold total would leave a refusing callable with nowhere to say so. So the orchestration is generic over the carrier's error instead. The read half is `ledger_get` and `ledger_scan` over any `ReduceCodec`. The write half is `apply_group` over an `AccLedger` — the two questions a carrier answers, `get_acc` and `put_acc` — with the engine as one carrier (`StoreLedger`) and the dynamic plan as the other (`PlanLedger`, where a write becomes the ops the caller records). A step that refuses leaves the entry untouched, because the write happens only after the step agrees; the shape check therefore needs no second home. `create` is a parameter rather than a policy here: the typed write seeds an entry even on an unfold, the dynamic plan only on a fold — a difference older than this ADR, kept visible.

3. **Mechanism moves in; projections stay out.** The modules that are mechanism — `collection`, `plan`, `plan_fields`, `index`, `path`, `encode`, `reduce`, and the two schema-free forms `cluster` and `graph` — move into `okm-core`. The projections do not: the schema-driven Arrow column builder (`arrow.rs`) and everything above it (Parquet, the lake's table formats, the snapshot export) stay outside, because core cannot depend on arrow (ADR-0045, ADR-0056) and a projection of a segment is not part of producing one. The projections move to `okm-lake`, which already owns arrow and parquet; the crate itself is dissolved rather than kept as a re-export layer.

4. **The query surface stays two, and that is the decision, not a residue.** The typed side asks by type: `scan::<I>(&prefix)`, the marker type carrying the slot and field list, the probe builder asserting every name against the key's and the document's own tables. The dynamic side asks by number with bytes: `scan(index_slot, encoded_prefix)`, plus `scan_exact`, `scan_covered` and `scan_access_method_keys`, with the prefix encoded by the caller (`encode_fields`). The carriers differ — a compile-time type versus data — and neither absorbs the other: a typed probe cannot be constructed without the type, and a host language can only hand over bytes. What unifies is the stored side (slot, entry layout, ledger), never the asking side.

## What follows from it

- ADR-0074 §5 made both forms *read* one declaration; after this they also *execute* one plan. The byte-equality tests between the derive and the dynamic codec stop being the contract and become a regression check.
- `Entries` — the write's buffer, and the previous row's for a read-modify-write — becomes the single place a write's faces are built, in both forms.
- The bindings and `okm-log replay` keep their entry points; what they lose is a second implementation behind them.
- The PLAN item that measured the walk ("compile `TablePlan` at attach") folds into this one, numbers included.
- The typed write path does not get faster from the walk itself: `put` measures the same with the walk as with it disabled (1.2975 µs against 1.3002 µs), because the document just encoded is already hot. What did move `put` is `BatchStore::put_bytes` copying once into the batch buffer instead of two `to_vec`s plus a copy (1.2975 → 1.2385 µs) — a fix to the batch boundary, not to this decision — and then the one ledger, 1.2385 → 1.2149 µs. The 42% the entry set gains on its own (`entry_layout/compiled` 33.8 ns against `index_entries_into/reused` 58.4 ns) lands where bytes are the only currency: the dynamic form, and any producer whose payload arrives without a typed document beside it.
- A replaced row's faces are cut from the bytes the store holds, not from the row decoded for the reduce hooks: after `__okm_deref_vocab` that row carries labels where the store holds ids, so a face derived from it need not name the entry the write recorded — on a vocabulary field, merge could not delete what put had written.
- The declaration a plan compiles from is not the store's schema, and cannot be yet: `IndexSchema` carries `name/slot/fields/includes/func/deprecated`, so a `key(...)` tail truncation and a `where(...)` predicate are invisible in the stored bytes, and reduce declarations are absent from `CollectionSchema` altogether (`faces` fingerprints declared faces only). Two declarations differing only in a truncation or a predicate therefore describe the same schema, and attach cannot tell them apart. Closing that gap means widening the stored declaration and its fingerprint — a layout generation change (ADR-0042), and a separate decision.

## Rejected

- **Moving the whole crate into core, arrow included.** A core that cannot depend upward cannot host a schema-driven Arrow column builder; the export is a projection of a segment, not part of producing one (ADR-0045; ADR-0056 §8).
- **Unifying the query surface on a runtime, name-keyed probe.** It moves field-name and width errors from compile time to run time on the side that has the types, and gives the dynamic side nothing it does not already have — it encodes bytes today.
- **Keeping two producers and relying on the byte-equality tests.** A test observes drift; it does not prevent it, and the drift would be silent on the read side — an entry that sorts differently is still a well-formed entry.
- **Compiling or caching the plan per collection.** The typed plan is a const; a cache would reintroduce, on the write path, exactly the lookup the plan exists to remove.
- **Deleting `okm-dynamic` and re-exporting core under its names.** The name would outlive the crate and point at a different body, and the projection half still needs a home that a re-export cannot give it.

## Amendment — 2026-11-21: the crate goes, and nothing is re-exported under its name

With the mechanism gone, what was left of `okm-dynamic` was `arrow.rs` and 44 `pub use`
lines. The projection moved to `okm_lake::columns` — the lake already owns arrow, parquet
and every other projection of a segment, so the column builder had a real home waiting —
and the eleven test files moved to the crates they exercise (core's dynamic form, the
lake's baseline and replay).

The re-export half was dropped rather than kept. A crate whose whole body is `pub use`
buys name stability and a curated list of names, and it buys no boundary: `pub use` is
transparent, so a consumer can always name the real path, and every mechanism move has to
be mirrored by hand. That trade is worth it when a surface is versioned for someone
outside the workspace; here the bindings, the lake and the log are compiled at the same
commit as core, so the only thing the layer guaranteed was that a move costs a second
edit. The rejection above ("deleting `okm-dynamic` and re-exporting core under its names")
does not describe this: nothing is re-exported under that name any more.
