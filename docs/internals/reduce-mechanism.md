# Reduce Mechanism: the ledger, overwrite compensation, and reversibility

> **Languages:** [English](reduce-mechanism.md) (primary) · [中文](reduce-mechanism.zh-CN.md)

Mechanism/implementation document for `#[ok_reduce]` cross-row pre-aggregation: how the fold/unfold hooks drive it, why an overwrite must unfold the old value, and what this invariant constrains on the write path. Declaration and modeling discipline live in [MODELING](../MODELING.md) ("Cross-row pre-aggregation"); the index-side write path is covered in [index mechanism](index-mechanism.md).

## Positioning

A reduce entry has the same shape as an index entry (`[ns][slot][group segment]`), but the opposite semantics: an index entry is a **derived view that is born and dies with its row** (one row, N entries; delete the row, delete the entries); a reduce entry is a **shared mutable ledger** — every row of a group folds into one accumulated value, and when a row is deleted its contribution must be withdrawn. That opposite semantics dictates the constraint unique to it: **the ledger must be identical to the main table's row set**.

## The invariant

```text
acc(group) = Σ fold(row)   for each row in the main table that belongs to this group
```

acc is identically equal to "the sum of fold over every row currently in the main table". What breaks it is not concurrency (under a single-writer engine get→f→put cannot interleave) but the **overwrite**.

## Why an overwrite doubles the count: the ledger has no memory

A reduce entry holds no member list — there is no record anywhere in the KV of which rows contributed to this group, the ledger holds only one accumulated value. The hooks are therefore ignorant of "how many times this row has been written"; the overwrite sequence (before the fix):

```text
put(k1, hits=10)   → fold(10): acc = { count: 1, sum: 10 }
put(k1, hits=15)   → fold(15): acc = { count: 2, sum: 25 }   ← ledger and fact diverge
```

The second put is the same key — the main table logically holds a single row with hits=15, but acc mathematically accounts for two rows (10 and 15), and the hits=10 row no longer exists. The extra contribution is permanent debt: a later delete's unfold subtracts the value stored at that time, so the earlier over-count is never paid back, and the reversibility contract `unfold(fold(a,x)) = a` is broken on this sequence.

The root cause is not a wrong fold but the **conflict between put's append semantics and the ledger's shared mutable semantics**: an index entry moves to a new key on overwrite (the entry key encodes the indexed fields, and the dangling old entry has its own delete backstop — see [index mechanism](index-mechanism.md) on write-path synchronization), whereas the ledger accumulates in place under one group key, and nobody balances it.

## The fix: unfold before fold on overwrite

`Table::put` now reads the main table's old row (one slot-0 point read) and, if it exists, unfolds it from every declared group first:

```text
put(k1, hits=15)   → get k1 → old row hits=10 in hand
                    → unfold(10): acc = { count: 0, sum: 0 }
                    → fold(15):   acc = { count: 1, sum: 15 }   ← invariant restored
```

Points to note:

- **Zero extra cost on the insert path**: a slot-0 miss = no ledger to withdraw, so fold directly. Only an overwrite pays this one point read.
- **The point read is the precondition for the terminal state**: the old row read here is exactly the source of "every write has the old row in hand" (the baseline state of Phase 6's upsert_with) — the field-level diff and the consumer-side change detection freebie both stand on this read; see the PLAN event-granularity decision.
- **upsert_with keeps no books of its own**: the imperative RMW is exactly `get → f(old) → put`, the unfold happens inside put, and the entry point does not rebalance (that would withdraw twice).

## The full write-path sequence

```text
put(key, row)
  ├─ read the main table's old row (overwrite detection + unfold source)
  ├─ old row exists → unfold(old) for each declared group
  ├─ write the main-table entry (slot 0)
  ├─ write index entries (each access method)
  ├─ fold(new) for each declared group
  └─ emit event (epoch +1, best-effort subscribe)
```

`purge` is the unfold branch of the same path (`add=false`, a single call site: it gets the stored row internally and arrives at the same spot).