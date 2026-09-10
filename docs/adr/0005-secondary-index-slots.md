# ADR-0005: Secondary indexes — item-local slot allocation, no manual ns per index

Date: 2026-09-06
Status: Accepted (design; implementation pending). **Update 2026-09-07, [ADR-0006](0006-row-node-model.md)**: `#[kv_index]` mounts on the **row struct** (`RowEncode`), not the key struct — the index's data source is row attributes, and key structs stay pure identity. Slot numbering, ns derivation, and the 1-byte discriminator below are unchanged; covering indexes (`includes`) are positioned as materialized views for high-fanout queries. **Update 2026-09-10, implementation**: the 1-byte slot byte ships as designed below — entry header is `[table_ns 2B][slot 1B]`, primary = 0, indexes numbered by declaration order. *(A 2026-09-07 implementation briefly replaced this with additive ns derivation — `index_ns = table_ns + SLOT`, no slot byte — before the flaw surfaced; see the second update at the bottom.)* `fields(...)` names **payload** fields (the index's data source is the row); the carried key tail defaults to the full primary key and may be truncated to any named subset via `key(…)` — `encode_prefix_named` now accepts arbitrary named subsets, not just declaration-order prefixes. Entry layout: `[ns 2B][slot 1B][indexed fields][key prefix]`, value = `includes` fields TLV (empty when absent).

## Context

Secondary indexes (including composite ones) need their own keyspaces. Two obvious mechanisms both fail:

- **Field-level attributes** (`#[kv_index]` on each indexed field) cannot express composite indexes — an index spans several fields, so it has no single field to hang on.
- **Manually assigning a namespace ID per index** re-introduces the exact burden ADR-0002 removed: every `CREATE INDEX` becomes a hand-edit of a global numbering table, and every deletion leaves a hole someone must remember not to reuse.

## Decision

Indexes are declared on the **table struct itself** and numbered **inside the item** — this stays within the macro discipline (a macro cannot collect cross-item state, but the attribute list of the item it is expanding is item-local and fully visible):

```rust
#[derive(KeyEncode)]
#[kv_ns(9)]                              // one manual ID per TABLE, not per index
#[kv_index(
    by_name    { fields(name) },          // single-field index
    by_age_org { fields(org_id, age) },   // composite index = field list
)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
    pub name: [u8; 16],
    pub age: u8,
}
```

The derive macro, at expansion time:

1. **Assigns the index slot** by attribute order (0, 1, 2, …) — an item-local counter, not cross-item collection. Index ns is derived mechanically: `table_ns` + slot (or bit-packed `table_ns << k | slot`).
2. **Encodes the composite index as a `encode_prefix_named` field prefix** — the existing primitive, no new machinery. Index key = `[header][indexed fields BE][primary key ID]`, value empty.
3. **Generates one `IndexEncode` struct per index** (invisible to the user), carrying its derived ns and field codec.

## Physical layout

```
table key:   [header = table_ns·0][all fields BE]
index key:   [header = table_ns·slot][indexed fields BE][primary key ID]   ← value empty
```

- **Scan one index (leftmost-prefix match)**: header + slot + indexed-field prefix — composite leftmost-prefix matching falls out of prefix scanning for free.
- **Scan everything of one table (table + all its indexes)**: prefix = the `table_ns` segment — contiguous, because slots share the table's segment.

## Consequences and boundaries

- **Slot width is a global decision**: bit-packing with 4 bits caps 15 indexes per table. Preferred alternative: a 1-byte discriminator after the header (header becomes 3 bytes: `[table_ns 2B][slot 1B]`) — ns space untouched, 256 indexes per table, header stays fixed-width; cost is 1 byte on every key. Chosen: 1-byte slot.
- **Deleted indexes leave holes**: slots are never reused (same discipline as ns IDs, ADR-0002). Holes are harmless.
- **Fixed-width fields only** for this regime: variable-length indexed fields (e.g. `String`) belong to the secondary-index layout regime (discriminating text first, primary key ID carried at the tail, value empty — see kv-storage-engine.md). The slot mechanism is identical across both regimes. *(Shipped 2026-09-09 — see the update below; the rule sharpened to a positional constraint.)*
- **`#[kv_ns]` semantics return to its cleanest form**: one number per table; indexes, composites and slots are mechanical macro expansion. Humans only allocate table IDs.

## Update 2026-09-09: variable-length fields and function indexes shipped

The text-first regime is implemented; the constraint sharpens from "fixed-width fields only" to a positional rule.

**Variable-length fields (`String`, `VarInt<T>`) in `fields()`/`includes()`**: allowed at most once, and only in the **last** position. The wire is the raw encoding — UTF-8 bytes, no length prefix: a `len` prefix would sort by length before bytes and destroy dictionary order, which is the entire value of the text-first regime. Sort order = byte-wise lexicographic; fields *before* the variable-length one stay fixed-width and locatable, so `(city, name)` composite prefixes work (`WHERE city = ? AND name LIKE 'ab%'` falls out of prefix scanning). The exact-match cost: `"ab"` scans into `"abc"` — disambiguation is the trailing primary key + fetch-back, which is the regime's accepted price. A variable-length field followed by anything else is a compile-time panic (fields after it cannot be located — no static width).

**Function indexes** (`func(path)`): the entry's sort segment is `path(&row)`'s result instead of payload fields — `fields`/`includes` stay empty and mixing them is a compile-time panic. The same declared function is what the query side calls on its probe value: one declaration drives both encode and scan (normalization like `to_lowercase` cannot drift between sides). The result type must implement `IndexFuncResult` (`okm` crate): `String` → raw UTF-8 bytes (dictionary order), unsigned integers → BE bytes (numeric order); anything else fails to compile at the generated impl.

## Update 2026-09-10: slot byte restored

The 2026-09-07 implementation replaced the slot byte with additive ns derivation (`index_ns = table_ns + SLOT`, `wrapping_add`) — one byte shorter, one less layer. The flaw that surfaced: **table ns allocation stopped being self-contained**. Allocating ns=256 meant reserving headroom for "how many indexes this table might ever have"; adding an index could overflow into the next table's segment, and the error was silent (overlapping segments still scan fine — entries just cross-read). `#[kv_ns]` regressed from "one number per table" to "one number plus an unbounded reservation".

The byte saved was a local, negligible gain; the guarantee given up was global. Implementation-time simplifications must be checked against this ADR's consequences list — the additive variant was considered and rejected here for exactly this class of reason. Restored: entry header `[table_ns 2B][slot 1B]` on every entry (primary slot 0), +1 byte per key, ns dictionary back to one number per table, 255 indexes per table.
