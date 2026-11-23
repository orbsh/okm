> **Languages:** [English](0080-the-index-layer-stays-the-engines.md) (primary) · [中文](0080-the-index-layer-stays-the-engines.zh-CN.md)

# ADR-0080 — The index layer stays the engine's, because the cost is the engine

**Status:** Decided 2026-11-23 (no code). Closes the two proposals recorded 2026-11-12. **Update 2026-11-23, the variable-width side:** the split this ADR could not answer when it closed now has a shape — the improved encoding for keys whose layout is predictable from the address, the present framing for keys whose trailing field is variable. It is a per-declaration split known at compile time, not a runtime branch, and it removes the premise objection while moving neither number (§1, §3). The census is in `docs/ideas/schema-aware-index.md`: across five repos, 78 index declarations, 13 (16.7 %) carry a variable-width field in their segment, none in their key, and the workload this ADR's measurement used is entirely on the improved side — so the hybrid narrows coverage and leaves the 1.20× ceiling where it was.

## 1. What was proposed

Two external proposals, one premise, two layers: an in-process schema-aware prefix tree, and a schema-aware block index inside the SSTable. Both argue that OKM locks its key layout at compile time — `ns` is constant, index field widths are known per access method, `segment_start` / `key_prefix_width` are already measuring the layout rather than assuming it — so an index need not pay the generality tax of "I do not know what a key looks like".

The premise holds for fixed-width keys and weakens for the variable-width trailing `Str` field: that field is bare UTF-8 with no framing, and a truncated `key(...)` prefix carries only part of the primary key (`PrefixKey` has `taken < KEY_LEN` for exactly this). For such entries the path no longer reproduces the key, and everything argued underneath "the bytes are known from the address" weakens to "roughly known". The proposals do not say what happens on that side.

The split itself is available, and it is per declaration rather than per row: the derive knows the field widths, so an index either has `key_prefix_width() == KEY_LEN` (fully predictable) or it does not, and `model/index.rs` already reads exactly that number when it splits an entry into `segment` + a `tail` of `taken` bytes. So the encoding can be chosen where the declaration is: fully predictable keys take the improved layout, keys with a variable `Str` tail keep today's framing. There is no lower KV to hand the second case to — an index entry is a key in the same keyspace, written through the same `VirtualStorage`, so "keep the present scheme" *is* the delegation. What this does not do is move either number: the improved set is a subset of what §2 measured, so the ceiling stays at or below 1.20×, and none of the block-layout items become OKM's to change.

## 2. What was measured

A 200 000-row fjall corpus with the memtable sealed, one 1 000-entry index query with fetch: **943.8 µs**. Of that, the engine's index read is 16.7 % — and the part an index rewrite would actually touch, the search itself (one descent through the block index, measured as the empty-probe cost), is **2.76 µs, 0.3 %**. The rest of the 16.7 % is reading the 1 000 entries the query legitimately asks for and splitting them; no tree shape makes those bytes unnecessary.

So the end-to-end ceiling for rewriting the index layer on this workload is **1.20×**, and the realistic figure is close to **1.0×**, because the search component is 0.3 %. A point read is 622.8 ns, of which the document layer is 32 ns: there is no tree shape that improves taking a row.

The proposal's own headline case is already answered: an absent tenant served by a prefix Bloom is **2.76 µs** on the existing engine, roughly 110× cheaper than scanning that tenant's data — removing it entirely saves 0.3 % of a query.

What the measurement did support was elsewhere and already exists: a covering index (193.6 µs against 943.8 µs, **4.87×**), and the operator layer, where gallop on skewed input is **35×**.

## 3. Why the cost is the whole engine, not a feature

Every item in the block-index list — packed fixed-width entries for a zero-parse binary search, a per-ns segment directory in the footer, prefix Bloom filters, in-block key/value separation, shortest-separator truncation — is a change to block layout, filter placement and separator encoding. In fjall those are implementation internals, not knobs; the same is true of the filter and separator seams in the other engines. There is no configuration that reaches them.

The tree is worse in kind rather than degree: ordering, durability, recovery, compaction, snapshots and the seek contract all belong to the engine today (fjall / redb / slatedb). A prefix tree above the document layer is not a feature of OKM, it is **a new engine** — and once OKM owns one, it owns all of the above, permanently.

ADR-0010 made the engine a boundary precisely so that this ownership stays outside. Both proposals cross it.

## 4. The premise is OKM's, not the engines'

A compile-time-locked key layout is the unusual part of this system; a general KV engine cannot assume it, which is why its index pays the tax. That makes the proposal coherent — and it also makes it inapplicable to any engine OKM does not write. Since the gain is ~1.0× and the cost is a fork or a new engine, the premise being true does not make the trade worth taking. A true premise is not a purchase order.

## 5. Decision

The index layer stays the engine's. Neither proposal is designed, scheduled, or kept warm as a half-finished alternative; the record is `docs/ideas/schema-aware-index.md` and the measurement is `docs/internals/query-cost-attribution.md`.

**Reopen gate:** both conditions, not either — a measured workload where the *search* component is what costs (not the fetch it guards), and a decision to own the storage engine. Until both hold, this is a plan to rewrite someone else's component for a 0.3 % line item.

If ownership ever happens, the block index stays the low-risk shape of the two: it keeps the SSTable's page discipline and changes encodings, where the tree has to redo node encoding, splitting and serialization.

## 6. Refused alternatives

- **Configure fjall/RocksDB to get part of it.** Not available: block layout, filters and separators are internals of the crate actually in use.
- **Tree in the document layer.** A new engine outright, bought for a 0.3 % component.
- **Block index alone, as a bounded change.** Still a fork of the engine; bounded only in the sense that it is less new code, not no new code.
- **Keep the idea open as "recorded, not designed".** It was recorded and measured; leaving it open implies the measurement is pending when its answer is 1.20× ceiling, ~1.0× realistic.