# 0048 — A derived face carries the number of the declaration that built it

> **Languages:** [English](0048-faces-carry-a-fingerprint.md) (primary) · [中文](0048-faces-carry-a-fingerprint.zh-CN.md)

**Status:** Accepted (2026-10-31) — stamp and rebuild both implemented the same day. **Date:** 2026-10-31

## Context

A collection's derived faces — the `#[ok_index]` slots, the path indexes over the dynamic segment, the graph's five faces — live outside the primary table, each in its own ns (`ns = table_ns + slot`). An entry found under a face carries its key and its value, and nothing else: no record of which declaration produced it.

That is fine while the declaration never moves. It is not fine when it does, and the payload's `LAYOUT_VERSION` does not cover the case at all: adding an index changes no payload byte, so the layout number does not move, `ensure_layout` passes, the store opens, and the new face is empty while the removed one still answers. Nothing anywhere is positioned to notice.

The graph shows what the failure looks like at its worst. ADR-0041 renumbered the graph's faces, so a store written before it holds entries at slot numbers this build reads as something else — the retired predicate index sat exactly where out-edges live now. The repair is `Graph::reindex`, an operator verb somebody has to remember to call; there is one such verb in the whole model layer and it has no callers.

Two halves of the problem were already answered and are worth naming, because they are not this one. `#[ok_index(..., deprecated)]` plus `Collection::prune_deprecated_slots` handles a face that was *removed*: the slot stays reserved and its entries are swept. And ADR-0042's rule ("a shape change changes the number") handles the payload. What neither covers is the case where the bytes are fine and the *set* of faces changed.

## Decision

**A declaration carries a fingerprint of its faces, and the store is stamped with it.**

1. **The declaration table.** `Document::INDEX_DECLS` is emitted by the derive from the same list that allocates the slots — name, slot, indexed fields, covered fields, function path, `deprecated` — so it cannot disagree with the slots themselves. A declaration with no indexes declares an empty table.

2. **The number is computed, never typed.** `Document::FACES` is `const`-evaluated from `INDEX_DECLS`: sixteen bytes, two independent 64-bit FNV lanes over a length-prefixed canonical encoding, in declaration order. A hand-kept number would be a silent-error machine — the whole failure mode is somebody forgetting to bump it — and the payload's `version` gets away with being hand-kept only because forgetting it breaks the next read loudly. This one cannot be forgotten, because it is not written by anyone.

3. **The stamp lives in the table meta row** (ADR-0043's fixed face `0x0F`), under the name `faces`, beside `version`. That row is already a name → bytes map whose whole point is that a new fact rides beside the stamp without a layout decision, and a fingerprint is exactly such a fact: no slot, no fixed face consumed.

4. **`ensure` writes both, compares only `version`.** A fresh store's faces are trivially the faces of the declaration about to write it, so both names land in one write and no store exists with one and not the other. The comparison is deliberately not on that path: `ensure_layout`'s promise is "this build can read these bytes", and every store written before this mechanism existed would fail a check it can never satisfy. A declaration that cannot name its faces — the dynamic mode behind a hand-written schema — passes `None`, and its store reads back as `Unknown`, which is the truth.

5. **`Collection::faces_status()` answers three things, not two.** `Match` — the faces were built from this declaration. `Stale { stored, current }` — they were built from another one, and they answer for a key layout this build does not use. `Unknown { current }` — no stamp was ever recorded. Stale and unknown are not the same failure and must not share an error type: a stale face is repairable drift, while an unknown one is merely older than the mechanism. Folding either into `LayoutMismatch` would turn a maintenance action into an outage.

6. **The export carries both.** `CollectionSchema` gains `indexes` (the same six facts per face) and `faces` (the number), so an outside reader — a binding, a dynamic writer, an operator's script — can compare against a store without re-deriving anything. In the JSON, both default to empty; empty reads as *undescribed*, never as "this table declares no index".

**What the fingerprint is over:** the declared structural faces and nothing else. Not reduce accumulators — they are data, and recomputing one is a different mechanism (a full-table fold) whose cost must not be attached to "an index was added". Not the graph's faces — their layout is fixed by the format, and what drifts there is repaired by `reindex`. Not the member set — it is enforced at write and is not part of a face's key.

## Consequences

- **The stamp cannot rot.** It is a function of the declaration, so a declaration change moves it in the same commit that changes the slots.
- **Opening a store is unchanged.** `ensure_layout` still answers `version` alone; only a caller that asks gets a verdict about faces. Existing stores keep opening; they answer `Unknown` instead of pretending.
- **On disk, one name appeared.** Older builds ignore unknown meta names and keep them across `set_meta`; a store stamped by this build is still readable by a build that predates it.
- **The repair is the other half, and it runs in chunks.** `Collection::rebuild_faces(chunk)` and `Graph::reindex(chunk)` sweep the faces the declaration names and re-derive them from the primary rows, committing every `chunk` ops — so memory is one chunk, not one table, and `usize::MAX` restores the old single-commit shape. The sweep comes first and cannot be skipped: an entry's key encodes the value it indexes, so an entry whose row changed hands answers at a key no row produces any more, and re-deriving writes the new key without ever naming the old one.
- **Interruption needs no state, because the stamp is the state.** Both verbs write `faces` last, so a run that dies between commits leaves a store that still answers `Stale` and a next run that starts over. The price is a window: a reader during the rebuild sees a half-built face. That is the operator contract (do not read while it runs), and it is the whole reason chunking is an explicit verb rather than something `put` does.
- **A host-declared table carries a number too.** `DynamicCollection` has no `const` to compute over, so `faces::fingerprint_specs` is the same encoder behind a runtime shape: the table's access methods are the declaration, and a name is borrowed from the exported schema when it names that slot, synthesized from the kind otherwise (`plain` / `partial` / `func` / `path(<components> → <ty>)`). A dynamic table that mirrors an export reproduces the compiled number exactly; one that names a method the export does not describe legitimately disagrees, and the answer is `Stale` rather than a happy `Unknown` — the path that drifts without a compiler was the one exempt from detection before this.
- **Where structure ends, the caller speaks.** A `Func` / `Partial` callable is a boxed closure, so two different derives at one slot hash the same. `AccessMethod.version` is the answer: a caller-supplied number that rides the fingerprint, so "I changed the derive" becomes a declaration instead of a memory — bump it and the table answers `Stale`, rebuild and the entries are the new function's. It starts at `0` ("no claim"), and a `Plain` / `Path` method may use it too, since a fixed derive is not the only reason entries go stale. An access method still has no deprecation state: a method is declared or dropped.
- **Two faces are outside the verb.** Reduce accumulators are data, not derived structure — recomputing one is a full-table fold with its own mechanism, which is why they were never in the fingerprint either. The vector snapshot cannot be re-derived at all: it needs an embedding model, so its owner wipes and re-adds (`okm_vector::VectorIndex::wipe`).
- **The dynamic mode inherits the rule rather than being exempt.** A table that mirrors an exported declaration stamps the same number a Rust build would, because its names come from the export; a table declaring a method the export does not describe gets a number of its own and legitimately disagrees. Nothing is exempt, and nothing reads `Unknown` for want of a `const`.