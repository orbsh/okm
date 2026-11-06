# ADR-0005: Secondary indexes — item-local slot allocation, no manual ns per index

Date: 2026-09-06
Status: Implemented.

## Context

Secondary indexes (including composite ones) need their own keyspaces. Two obvious mechanisms both fail:

- **Field-level attributes** (`#[ok_index]` on each indexed field) cannot express composite indexes — an index spans several fields, so it has no single field to hang on.
- **Manually assigning a namespace ID per index** re-introduces the exact burden ADR-0002 removed: every `CREATE INDEX` becomes a hand-edit of a global numbering table, and every deletion leaves a hole someone must remember not to reuse.

## Decision

Indexes are declared on the **row struct** — an index's data source is row attributes, and key structs stay pure identity — and numbered **inside the item**, which stays within the macro discipline (a macro cannot collect cross-item state, but the attribute list of the item it is expanding is item-local and fully visible):

```rust
#[derive(DocumentEncode)]
#[ok_ns(9)]                              // one manual ID per TABLE, not per index
#[ok_ref(UserKey)]                       // the key type: identity only, ns-free
#[ok_index(
    by_name    { fields(name) },          // single-field index
    by_age_org { fields(org_id, age) },   // composite index = field list
)]
pub struct User {
    pub org_id: u32,
    pub user_id: u64,
    pub name: [u8; 16],
    pub age: u8,
}
```

`#[ok_ns]` mounts on the row struct too, and the derive emits it as `Row::NS_PREFIX` (a `&'static [u8]` big-endian `[ns 2B]`); `Table::new(store)` takes no ns parameter. The row is the table's declaration point (`#[ok_ref]` pins the key type, so the row fully determines `Table<S, K, R>`), and a key type carries no ns so the same key shape can serve several rows/tables, each with its own declared ns. Edge structs keep declaring `#[ok_ns]` on the edge (unchanged `EdgeEncode` path).

The derive macro, at expansion time:

1. **Assigns the index slot** by attribute order (1, 2, 3, …) — an item-local counter, not cross-item collection. The slot is ADR-0030's counting family `0x3nnn`: two bytes, family nibble plus declaration order (counter from 1, 0 reserved), 4095 declared indexes per table. The primary table sits at fixed face `0x00`, one byte.
2. **Encodes the composite index as an `encode_prefix_named` field prefix** — the existing primitive, no new machinery. The indexed-field segment is the declared field list (or the `func` result, below); the carried key tail defaults to the full primary key and may be truncated to any named subset via `key(…)`, because `encode_prefix_named` accepts arbitrary named subsets, not just declaration-order prefixes. The entry value is the `includes` fields' payload encodings concatenated bare — no tag byte, no length prefix, laid down in `INCLUDES` order by `entry_value` — and empty when the declaration has no `includes`. Locating a field by its declared width is what makes the value decodable without a frame, the same trick the indexed segment uses.
3. **Generates one `IndexEncode` struct per index** (invisible to the user), carrying its slot and field codec.
4. **Variable-length fields** (`String`, `VarInt<T>`) in `fields()`/`includes()` are allowed at most once, and only in the **last** position. The wire is the raw encoding — UTF-8 bytes, no length prefix: a `len` prefix would sort by length before bytes and destroy vocabulary order, which is the entire value of the text-first regime. Sort order is byte-wise lexicographic; fields *before* the variable-length one stay fixed-width and locatable, so `(city, name)` composite prefixes work (`WHERE city = ? AND name LIKE 'ab%'` falls out of prefix scanning). The exact-match cost: `"ab"` scans into `"abc"` — disambiguation is the trailing primary key plus fetch-back, the regime's accepted price. A variable-length field followed by anything else is a compile-time panic (fields after it cannot be located — no static width).
5. **Function indexes** (`func(path)`): the entry's sort segment is `path(&row)`'s result instead of payload fields — `fields`/`includes` stay empty and mixing them is a compile-time panic. The same declared function is what the query side calls on its probe value: one declaration drives both encode and scan, so normalization like `to_lowercase` cannot drift between sides. The result type must implement `IndexFuncResult` (okm-core): `String` → raw UTF-8 bytes (vocabulary order), unsigned integers → BE bytes (numeric order); anything else fails to compile at the generated impl. The return form is an iterator of that type — one element for one entry, a `Vec`/slice/iterator for a fan-out — as decided in ADR-0055.
6. **Partial indexes** (`where(path)`): a row-level predicate, so a rejected row produces no entries. Legal on plain and function indexes alike; expanded as a `KvIndex::admits` override consulted at the head of `entry_pairs`. Entry layout, slot numbering and scan paths are untouched.

## Physical layout

```
table key:   [ns 2B][0x00][all fields BE]
index key:   [ns 2B][0x3nnn][indexed fields][key prefix]   ← value = includes, bare
```

- **Scan one index (leftmost-prefix match)**: header + slot + indexed-field prefix — composite leftmost-prefix matching falls out of prefix scanning for free.
- **Scan everything of one table (table + all its indexes)**: prefix = the `table_ns` segment — contiguous, because slots share the table's segment.

**The entry key carries the address header exactly once, and the primary key rides bare in the tail.** Stripping-and-rejoining at the engine level was considered and rejected (ADR-0011); repeating `[ns][0x00]` inside each index entry would be pure duplication — the header is a property of the table the entry belongs to, and the entry key already carries it. Lookup flow: a scan hits the entry key, decodes the primary key from the tail segment, and hands it to `Table::get`, whose `primary_key` is the **single** place that composes `[NS_PREFIX][0x00][payload]` for the primary table. One composition point per direction: entry construction composes once, primary lookup composes once, nothing repeats.

**The tail does three jobs at once, which is why the identity must not move into the entry VALUE.** (1) Entry uniqueness: one row = one entry, so `put` replaces the whole entry and `delete` regenerates the entry set from the row — no read-modify-write, no tombstone. Move the primary key into the value and two rows with equal indexed fields collide on one entry key; the escapes are declaring those fields unique (no runtime check, silent row loss) or turning the value into an id set — a cross-row read-modify-write on a shared entry, exactly what per-row derived entries exist to avoid, and already owned by another primitive (`#[ok_reduce]`: group in the key, accumulator in the value, fixed-width `Acc`, exactly-once fold/unfold). (2) Identity without reading the value: the tail decodes to a `PrefixKey` — that zero-copy slice is also why the probe/tail judgement below costs nothing — and a truncated `key(...)` tail stays a trustworthy prefix, which is what a relation table's reverse index hands back to continue on the main table. (3) Order: inside an equality prefix the tails ARE the main-key bytes in order, the fetch-back locality property.

Graph edges are the instructive counterexample rather than a refutation: an edge keeps its endpoints in the BODY because the edge's own identity is its surrogate `edge_id`, and the addressed direction carries the ref in the face key — identity sits in the key on the side being addressed.

**Two encodings for mid-segment variable-length data are examined and NOT adopted.** (1) A length prefix inside an index segment fails structurally: the segment IS a sort segment, so a leading length sorts by length before bytes and destroys the vocabulary order the text-first regime exists for (see the positional rule in decision 4). (2) A C-style terminator with order-preserving escaping (`0x00`-terminated, `0x00` → `0x00 0xFF`) does keep byte order and would allow more than one variable segment — a self-delimiting form already ships inside okm, where the graph faces put `NodeRef = [ns 2B][len varint][pkey]` mid-key and pay ~1 byte per ref for the boundary living in the wire. It is a trade, not an upgrade: encoding stops being compile-time-locked zero-copy slicing and becomes a runtime codec with escaping, range bounds must become terminator-aware, and what it buys is decodability alone — never selectivity, because leftmost-prefix is a query-semantics rule and extra variable dimensions still mean extra declared access methods. A separator IS legal one layer down, where nothing has to be decoded: a `func` index may build a single composite text segment with the separator inside the VALUE (`last\x00first`), the application owning that contract — the KEY still carries no terminator. The rule: NAKED bytes cannot sit mid-segment; a SELF-DELIMITING frame can.

## Read path

**Prefix and exact matching share one length judgement.** The entry's indexed-field segment and the carried key tail have no boundary byte between them, so without a judgement a probe could be completed by TAIL bytes: a stored value `ad` whose key begins with `0x31` matched the probe `ad1`. Values mix letters and digits, so that data is legal — the defect is in the read path, never in the input.

The judgement is ONE length compare against the tail's width (`key_prefix_width()`), and it yields the exact-match form as well:

- `remain <  tail width` → the stored value is SHORTER than the probe: not a match, dropped.
- `remain == tail width` → the probe consumed the whole segment: full-field equality, i.e. `scan_index_exact` / `Collection::scan_exact`.
- `remain >  tail width` → the stored value EXTENDS the probe: prefix semantics — a hit for `scan`, not for `scan_exact`.

Prefix and exact share ONE code path, and every entrance applies it: the prefix scan, the covered scan, the lazy range iterator, the range scan and the dynamic counterpart (`okm_dynamic::scan_access_method_exact`, `DynamicCollection::scan_exact`, the Python binding's `scan_exact`). Because the test runs before any fetch-back, exact matching needs neither row validation nor `includes`.

`VirtualStorage::scan_suffix`'s default forwards `scan_range`'s keys with the prefix stripped, per ADR-0020's statement of the default's obligation; a default forwarding full keys would leave `remain` carrying a whole address header and make the judgement silently inert. `scan_suffix_contract_test.rs` locks the contract.

## Covering reads

- `KvIndex` gains `type Covered` and `fn decode_covered(b: &[u8]) -> Self::Covered`. Per covering declaration the derive generates a struct `__OkmCovered_{Row}_{index}` whose fields are the `INCLUDES` names in order, typed exactly as in the document, and the decode walks the segment by width: fixed-width fields cut at their declared width, the trailing variable-width field (at most one, last only — decision 4) takes the remainder. A declaration without `includes` gets `Covered = ()`: nothing was copied, so there is nothing to decode.
- `Collection::scan_covered` returns `(PrefixKey<K>, I::Covered)` instead of `(PrefixKey<K>, Vec<u8>)`. The raw-bytes form is gone on purpose: bytes are what `scan_suffix_kv` hands out, and a covered scan that stops at bytes has not answered anything.
- Dynamic side: `DynamicCollection::scan_covered` (over `okm_dynamic::scan_access_method_covered` / `decode_includes`) returns `(key fields, covered fields)` as two name-keyed maps. Two maps, not one, because a key field name and a payload field name may collide and the key is the identity rather than payload data.

The covered segment is derived state, regenerated wholesale on every write: `entry_pairs` is the single entry producer, `put` lands the row and every entry in ONE batch/commit, and `merge` / `replace` / `purge` reach the same producer through the stored row. No second writer, no incremental bookkeeping, no extra commit — the cost is bytes in the entry, worth paying only for a measured high-fanout read path, which is what the covered door is for.

## Retiring a declaration

Slots are positional: deleting a MIDDLE declaration shifts every later slot down one, so those slots' new writes land on the previous declarations' stale entries — silent corruption, the same class as an additive ns derivation that overflows into the next table's segment. Removing a declaration is therefore NOT a supported operation (same discipline as reordering: a layout change = clean rebuild or migration double-write). The supported retirement path is a deprecation marker:

```rust
#[ok_index(by_old { fields(legacy) }, deprecated)]   // slot stays reserved
```

The derive keeps the slot (later declarations keep their slots), but generates no marker struct, no write path, and no scan surface for it. Stale entries from before the deprecation are cleared explicitly: `Table::prune_deprecated_slots()` prefix-scans `[ns][deprecated slot]` and deletes (idempotent; returns the count). `Row::DEPRECATED_SLOTS` carries the derive-emitted slot list.

## Consequences and boundaries

- **Slot width is a global decision**: a declared index spends a counting-family slot (two bytes, 4095 per table) and the ns segment is untouched, so ns allocation stays one number per table — it never has to reserve headroom for how many indexes the table might ever declare. Packing the slot into the ns byte instead (bit-packed, or additive `table_ns + slot`) caps the indexes at 15 and lets an extra index overflow into the next table's segment, where overlapping segments still scan fine and the corruption is silent.
- **Slot numbers are permanent**: a retired declaration keeps its slot (see Retiring a declaration), so slots are never reused and never renumbered — the same discipline as ns IDs (ADR-0002).
- **Sort order over a variable-length indexed field is byte-wise lexicographic**, and exact matching there is resolved by the trailing primary key plus fetch-back (decision 4).
- **Covering indexes are materialized views for high-fanout queries**: the write side copies the covered fields into every entry, so the cost is paid only where a measured read path consumes it (`scan_covered`).
- **`#[ok_ns]` semantics return to its cleanest form**: one number per table; indexes, composites and slots are mechanical macro expansion. Humans only allocate table IDs.
