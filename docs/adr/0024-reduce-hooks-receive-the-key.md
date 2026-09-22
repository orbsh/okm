# ADR-0024: Reduce hooks receive the decoded key — fold(acc, key, item)

> **Languages:** [English](0024-reduce-hooks-receive-the-key.md) (primary) · [中文](0024-reduce-hooks-receive-the-key.zh-CN.md)

**Status:** Accepted (2026-09-22); implementation pending, see Consequences. Amends the "not carried here" note in ADR-0023 (Honest semantic cost, third bullet): the key-side quantity problem is solved by THIS ADR, and ADR-0023's preset combinators may aggregate key fields once it lands.

## Context

The reduce hook contract (ADR-0008) is `fold(acc: &mut Acc, item: &Self::Document)` — the accumulator folds over PAYLOAD fields only. The grouping segment (GROUP) is likewise encoded from payload fields via the named-field walk. The primary key (`Key`) is invisible to the hook.

The first production consumer (aura's state table, ADR-0018 step 2) hit the constraint squarely: the aggregated quantity — the per-type proxy-id watermark for id assignment — lives in the KEY (`InstanceStateKey { type_id, instance_id }`). Since fold cannot see the key, the payload had to carry a mirror field (`instance_id` written twice: key + payload), and the reduce aggregates the mirror. Three costs: two sources of truth for one value, redundant bytes, and a read-side rule ("addressing uses the key; the payload copy is only for the hook").

### The key is already in hand, already decoded

The hook chain never lacks the key:

- Every write-side call site (`Collection::put` / `delete_by_pkey` / `upsert_with`) invokes `R::__okm_apply_reduces(&mut self.store, key, document, &header, add)` — the generated hook's signature already carries `_key: &Self::Key`; the derive simply does not pass it on to fold.
- The key at that point is the CALLER'S DECODED, TYPED reference (`put(key: &K, ..)`'s own argument), not raw bytes. `Key::decode` is only used on the INDEX side (recovering a key from entry-suffix bytes); the reduce write path is not on that path.

So "pass the key to fold" means handing an existing object reference one level deeper — zero serialization, zero decoding, zero copies. The user's framing holds: item is already the un-serialized object reference (`&Document`); key joins it in the same form (`&Key`).

### Group bytes can reference key fields with no new encoding

`KvIndex` already defines the two-source named walk: `encode_named(key, document, names, buf)` — index `fields()`/`includes()` may name key OR payload fields, encoded per source. The reduce group walk (`__okm_encode_named(document, GROUP, buf)`) is the payload-only half of the same pattern. Relaxing GROUP to key fields means routing each declared name to its source's encoder — the index layer's byte layout (and the dynamic side's `ReduceSpec.group_bytes`, which today explicitly rejects key-field names) is the reference.

## Decision

**`ReduceLogic`'s hooks receive the decoded key as a typed reference: `fold(acc: &mut Acc, key: &Self::Key, item: &Self::Document)` and `unfold` likewise. GROUP declarations may name key fields; each group field encodes from its own source (key via `KeyEncode`, payload via the payload walk) — the `KvIndex::encode_named` two-source rule, reused.**

- **References, not bytes, at every point.** The write path holds `&Key` and `&Document` already; the hook change is a parameter pass, never a decode. (`Key::decode` remains index-scan-side only.) The okm-dynamic twin passes the decoded forms too: document as `&ValueMap` (already the case), key as the decoded key-field map per the schema — no byte-level handoff to host callables.
- **GROUP name resolution becomes two-source.** `#[ok_reduce(Logic { group(a, b) })]` may name key and payload fields; validation (schema.rs, okm-dynamic's `ReduceSpec`) drops the key-field rejection and routes per source. Byte compatibility: the group segment's layout rule is unchanged — declared names in order, each field's fixed-width BE encoding; the ENCODER per name is the only difference. A group name that exists in both sources is a compile error (ambiguous source; payload wins by convention is rejected — ambiguity should not be silently resolved).
- **No accumulator-shape changes.** `ReduceCodec` (`u64` BE, `Vec<u8>` escape hatch) is untouched; ADR-0023's preset combinators declare their aggregated field by name and that name may now be a key field — the mirror-field pattern disappears for the standard shapes without any combinator-level special case.
- **Exactly-once unchanged.** The fold still runs inside the same engine-locked read-modify-write, fed by the same call sites; passing one more existing reference does not alter the atomicity surface (ADR-0008's invariant is about state visibility, not signatures).

## Honest semantic cost

- **Breaking signature change for every `ReduceLogic` impl.** In-tree: `reduce_test.rs` (`AuthorStats`), `subscribe_common.rs` (`CounterTotals`), aura's `MaxInstanceId` — a mechanical `_key`-prefixed parameter; the migration note goes in the changelog. Out-of-tree users of reduce (the feature is young) pay the same one-time rename.
- **okm-dynamic's `ReduceLogic` trait changes in the same batch** (`fold(&self, acc, key, document)`): the dynamic key form must be decided here — it is the decoded key-field map (schema-driven), not raw bytes, so host-language callables see one consistent object model. Python `add_reduce` callables gain a key argument in the same release.
- **Ambiguity rule is a compile error, not a resolution.** A group field name present in both key and payload structs fails the derive. The alternative (payload silently wins) would make group encoding depend on a shadowed name — the same class of trap the document get-path documents for declared/dynamic collisions.
- **`scan_reduces` returns group segments, not decoded keys.** A group segment built from key fields is decodable by the caller (field widths are in the schema), but the read-side helper does not hand back a typed key — that convenience, if wanted, is a separate follow-up, not smuggled into the signature change.

## Consequences

- The mirror-field pattern is retired: aggregated quantities may live where they belong (the key), with one source of truth and no hook-visible duplication.
- ADR-0023's combinators aggregate key fields by name (`MaxKeep::<instance_id>`-shaped usage) — aura's `MaxInstanceId` hand-written logic collapses into a preset declaration.
- The hook signature reaches its intended shape while reduce adoption is still young; later would multiply the migration cost.
- Implementation: okm-core trait + derive pass-through + two-source group validation (schema.rs, okm-dynamic `ReduceSpec`) + okm-dynamic/Python callable signatures; tests extend `reduce_test.rs` (key-field group, key-field fold) and the dynamic semantics tests; docs pass INTEGRATION/MODELING. Wire format, slot allocation, and entry layout are untouched.
