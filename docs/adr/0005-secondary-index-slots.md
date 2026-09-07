# ADR-0005: Secondary indexes — item-local slot allocation, no manual ns per index

Date: 2026-09-06
Status: Accepted (design; implementation pending). **Update 2026-09-07, [ADR-0006](0006-row-node-model.md)**: `#[kv_index]` mounts on the **row struct** (`RowEncode`), not the key struct — the index's data source is row attributes, and key structs stay pure identity. Slot numbering, ns derivation, and the 1-byte discriminator below are unchanged; covering indexes (`includes`) are positioned as materialized views for high-fanout queries.

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
- **Fixed-width fields only** for this regime: variable-length indexed fields (e.g. `String`) belong to the secondary-index layout regime (discriminating text first, primary key ID carried at the tail, value empty — see kv-storage-engine.md). The slot mechanism is identical across both regimes.
- **`#[kv_ns]` semantics return to its cleanest form**: one number per table; indexes, composites and slots are mechanical macro expansion. Humans only allocate table IDs.
