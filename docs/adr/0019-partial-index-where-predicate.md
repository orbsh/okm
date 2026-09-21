# ADR-0019: Partial indexes — a declared `where(path)` predicate, not an Option-returning function

Date: 2026-09-21
Status: Implemented. `where(path)` is a clause of `#[ok_index]`, legal on plain field indexes and function indexes alike; it expands into a `KvIndex::admits` override and short-circuits `entry_pairs`.

## Context

An index entry is paid for by every write, forever. In the distributions that motivate a filter — a ticket table that is 99% closed, a queue table that is 99% drained — most of the stored rows never use a given access method, and indexing them is pure overhead in both key space and per-write cost. The need is a predicate: "this row belongs in this index, that one does not".

Three mechanisms were on the table.

1. **A function returning `Option<R>`**: `None` = the row contributes no entry.
2. **A declared predicate**: `where(path)`, `bool` over the whole row.
3. **Do nothing** — the function-index regime already covers it: a `func` returning an empty `Vec` produces zero entries, so an impure-free function whose body decides membership is already a partial index in everything but name.

## Decision

The predicate is **declared**: `where(path)` sits beside `fields`/`includes`/`key`/`func` in the `#[ok_index]` body, and applies to both index forms — the filter is a property of "which rows are in this index", not of "how the sort segment is produced".

```rust
#[ok_index(open_by_assignee {
    fields(assignee_id, created_at),
    includes(title_len),
    where(ticket_is_open),      // fn(&Ticket) -> bool
})]
```

Mechanically it is one seam, not a subsystem: the derive emits a `KvIndex::admits` override (`fn admits(document: &Document) -> bool`), the trait default is `true`, and the write path consults it once per document at the head of `entry_pairs` — the single place entries are generated, shared by `put`, `delete` and `save_into`. Nothing else in the write path can create an index entry, so the filter is complete by construction: no new key format, no scan-side change, no runtime registry.

**Rejected: a function returning `Option<R>`.** Since the predicate exists, `Option` would be a second spelling of the same row-level condition — and the less visible one: the declaration would say nothing about which rows are in the index, while the function body silently decides it. An index declaration in this system is meant to be readable end to end (slot, fields, includes, key prefix are all declared); "which rows are covered" is the dimension most easily gotten wrong (query completeness depends on it), so burying it in a function body is the worst place for it.

Two further reasons stand on their own:

- **The cheap path has to stay cheap.** A filter's real use is usually a condition on an ordinary column, not on a computed value. Routing it through `func` would force a function call and an allocation for a trivial predicate, and would forfeit `includes` (the function regime is exclusive with `fields`/`includes` — a parse-time check, not a mechanism constraint) and the compile-time width checks that make `fields(...)` safe. It would also be a strange trade: converting a plain filtered index into the func regime changes not just the declaration's shape but the write cost of every subsequent write.
- **Mixing row membership into the value function breaks func's own invariant.** The documented justification of the func regime is "one declaration drives both sides": the query side calls the same path on its probe value, so normalization cannot drift. A function that decides membership as well as projecting the value breaks that in a subtle way — the probe side must mirror the value half *only*, and telling which half is which requires reading the function body. With a declared predicate the split is explicit: the value half mirrors, the predicate half does not participate in probes by definition.

**The undeclared path stays as the per-value tool.** A `func` returning an empty `Vec` still contributes no entries; that is the per-value drop (one token out of many is filtered, the rest kept), which `where` cannot express because `where` is row-level. The two mechanisms are documented with that division: `where` = row condition (declared, available to plain indexes too), in-body filtering = per-value drop.

## Consequences and boundaries

- **Purity extends to the predicate.** `delete` re-derives its removal set from the row through the same `entry_pairs`; a predicate reading a clock or external state generates a different set at delete time than at write time and leaves dangling entries. Same contract shape as `func`, now covering one more function.
- **A predicate flip dangles an entry, and this is the pre-existing overwrite contract.** Entry addresses are decided by the indexed fields and the primary key; the predicate only decides whether the entry is written this time. So `put` over a row that stops being admitted does not remove the old entry — exactly as a changed indexed value does not remove the old entry today (document.rs states this outright; `delete` exists because of it). The difference the filter makes is frequency: "row leaves the index" becomes a normal event rather than an incident. Removal requires deleting with the row that generated the entry; a test locks both the dangling behaviour and the working order. No new cleanup machinery was added — inventing one would contradict the single-generation-point design.
- **No read-side mechanism, no completeness check.** A partial index is simply sparser: `scan::<I>` is unchanged and the predicate is never consulted at read time. Results are complete only for queries whose condition implies the predicate; okm has no query planner and the caller chooses the index, so this is modeling discipline, stated in the Modeling Guide, not something the mechanism can verify.
- **The predicate is not part of the address.** The entry key stays `[ns 2B][slot 2B][indexed fields][key prefix]`. A rejected row's entry key is still computable — it is the row's derived address, the write path simply never wrote it.
- **Cost is one predicate call per document**, not per entry: a fan-out (multi-entry) index evaluates the predicate once and then fans out. An index without a `where` clause pays nothing (the trait default returns `true` inline).
- **Unchanged**: slot allocation and the append-only declaration discipline, entry layout, `scan`/`scan_covered`, `save_into`'s encoding-surface role, `key(...)` truncation, and `func`'s exclusivity with `fields`/`includes`. A `deprecated` declaration still generates nothing — a `where` clause on it is inert.