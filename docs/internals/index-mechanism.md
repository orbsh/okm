# Index Mechanism: entries, scans, and the row fetch

> **Languages:** [English](index-mechanism.md) (primary) · [中文](index-mechanism.zh-CN.md)

Mechanism/implementation document: what an `#[ok_index]` declaration expands into, how an entry is encoded, and how `scan::<I>` and `scan_covered` work. Modeling discipline lives in [MODELING](../MODELING.md) (access methods mandatory, covering indexes with restraint); the slot/ns derivation lives in [slot mechanism](slot-mechanism.md).

## Declaration expansion

`#[derive(DocumentEncode)]` generates an **index type** (a marker struct, at the expansion point) for every `#[ok_index(name { ... })]`:

```text
__OkmIndex_{Row}_{name}      // mechanical concatenation, no case conversion
```

The type implements the `KvIndex` trait (`okm-core/src/model/index.rs`), carrying compile-time constants and methods:

```text
type Key / type Document     the associated primary-key and document types
const SLOT                   declaration-order position (0x4001, 0x4002, …; primary = 0x0000), 2 bytes BE after the 2-byte ns header
const FIELDS                 index fields (sort order)
const INCLUDES               covered fields (into the value, not part of the sort)
const KEY_PREFIX             truncated subset of the primary key's tail (empty = full length)
const FUNC                   function-index path (empty = plain field index; a declaration-time constant)
fn encode_named(...)         encodes the payload segment in field-name order
fn admits(...)               partial-index predicate (default true = a full index)
```

The query side takes the type as a generic parameter: `t.scan::<ByOrg>(&prefix)`; `use __OkmIndex_User_by_org as ByOrg` is only a spelling alias and plays no part in the mechanism.

## Entry encoding

```text
entry key   [ ns 2B BE ][ slot 2B BE ][ index fields BE ][ primary-key prefix ]
entry value [ includes fields TLV ]                (no includes = empty)
```

- **Index field segment**: `FIELDS` are encoded one by one in declaration order, and the first is the grouping dimension (the physical basis of leftmost-prefix matching). **At most one variable-width field, and it must sit immediately before the primary-key prefix**: the fixed-width part is cut from the right (the primary key's `KEY_LEN` provides the right-hand anchor), and the whole remaining block is that single variable-width segment — its length is derived from the fixed-width boundary on its right and never stored. There is no boundary byte between two variable-width segments, so the derive refuses such a declaration at compile time (MODELING, "Rows and indexes"). A variable-width segment that is not last breaks prefix semantics (raw bytes carry no terminator, so `"beijing"` matches `"beijing2"`).
- **Primary-key prefix**: the full primary-key encoding by default (`KEY_LEN` locked at compile time); `key(...)` truncates it to a named subset. The tail is always decodable — it IS the primary-key encoding, laid out inside the declaration system.
- **Function indexes** (`func(path)`): every element of `path`'s return value is encoded through `IndexFuncResult` into the data segment of one entry — the return shape is an `IntoIterator` (ADR-0055). A single value is written as a one-element array (one entry, the classic function index such as a normalization); a `Vec<V>` / slice / iterator fans out into N entries (the multi-value regime: tokenized inverted indexes, multi-value fields, time bucketing); an empty iterator means the row produces no entry at all. The query side calls the same path on its probe, so one declaration drives both sides. Expansion details in [function index mechanism](func-index-mechanism.md).
- **Partial indexes** (`where(path)`): a row-level predicate; a row whose `path(&document)` is false does not exist in this index (a Postgres-style partial index). Both index shapes can carry one — `fields(...)` and `func(...)` each add the predicate on top, and the columns the predicate reads are unrestricted (they need not be index fields).

## The tail's three jobs

The entry tail carries the full primary key by default (`key(...)` can truncate it to a named subset), and three things hang on it at once — take one away and this layout does not stand:

1. **Entry uniqueness**: one row, one entry; `put` replaces the entry wholesale and `purge` regenerates the whole group of entries from the stored row, so there is no read-modify-write and no tombstone. This is the one most easily taken for granted — until index fields take duplicate values, as in the next section.
2. **Identity without reading the value**: the tail slices out a `PrefixKey` with zero copy, so a probe or a tail criterion costs no extra read; a truncated `key(...)` prefix is still trustworthy, and it is the exit through which a relation table's reverse direction walks back to the primary table.
3. **Order within a group**: inside an equality prefix the tail IS the primary-key bytes, so one equality group's hits arrive in primary-key order. The fetch is still one point read per entry — what the order buys is that this batch of point reads advances monotonically through the primary table (neighbouring reads, increasing block order), not a merge. It does not hold outside a group: a range across leading fields is ordered by the dimension being ranged, a `func` multi-value index repeats a row under several values, and a `key(...)` truncation lets tails tie ([MODELING, "Design constraints on performance"](../MODELING.md)).

### Duplicate index fields: three storage options, each with a debt

Index fields are not required to be unique — many rows sharing a value is the normal case, not a degenerate one: in 10 rows `gender` has only two values, five rows each. The entry key carries no identity, so those five rows land on the same key.

- **one key, one value** — five `put`s overwrite the same entry, and only the last row survives in the index. Rows are lost silently, and discipline cannot forbid it: the declaration never required unique index fields, and the compiler has nothing to check.
- **an id set in the value** — every `put` / `purge` is a cross-row read-modify-write on a shared entry. That shape already has an owner: `#[ok_reduce]` (group in the key, accumulator in the value, exactly-once fold/unfold); a derived entry does not build a second copy of it.
- **a `seq` in the key** (`[gender][seq]`) — five rows do get five keys, but `seq` is an address the store allocates, not a value the row holds, and it owes three debts. On the write side every group maintains a counter (itself another read-modify-write). On the delete side the address has to be reconstructible from the row, because the delete set is derived from the stored row (`purge` does an internal get) and the row does not carry `seq` — so either write `seq` back into the row, or build a separate `(group, pkey) → seq` mapping. And within a group the order degrades to insertion order: the tail is no longer primary-key bytes, and the fetch degrades from a sequential merge to one point query per row.

Putting identity in the tail owes none of the three: the discriminator is the value the row already carried, an overwrite computes the same key, `purge` regenerates the same group of keys from the same row, and one prefix scan returns the whole group in primary-key order. What is saved is not a field but **keeping an entry's address a pure function of its row** — `entry_pairs` is the only producer, the write set and the delete set share one source, and the index has no runtime bookkeeping (see "Keeping the write path in sync").

## Partial indexes: entry_pairs is the predicate's only application point

`where(path)` expands into a `KvIndex::admits` override:

```text
fn admits(document) -> bool { path(document) }      // no where → the trait default, true
```

The write side's only entry producer is `entry_pairs` (put / delete / save_into all go through it), and the predicate short-circuits at the top of the function:

```text
if !Self::admits(document) { return Vec::new(); }
a plain index uses the trait default (which carries the same line); a function index uses the same line and then fans out
```

Three properties follow:

- **Symmetry**: the write set and the delete set share one source (the same `entry_pairs`), so as long as the predicate is pure, delete removes exactly the entries put wrote.
- **The predicate is evaluated once per row**: a fan-out index does not multiply the predicate by N entries.
- **No new mechanism on the read side**: a partial index is merely sparser, `scan::<I>` is unchanged, and the predicate never enters the entry key (the key is still decided by the index fields and the primary key alone).

A predicate flip (a row going from admitted to rejected) is outside the predicate's reach: the indexed fields decide the entry's address and the predicate only decides "write this time or not", so a `put` overwrite does not clean up the old entry (the same existing contract as "a changed value leaves a dangling entry", see [MODELING](../MODELING.md)). The delete set is derived from **the stored row** (`purge` does an internal get), so cleaning up a dangling entry needs "the row that produced it", and since 2026-10-23 no verb takes that row — overwrites go through `merge` / `replace`, which sweep the stored row before replacing.

## `scan::<I>`: prefix scan + fetch

```text
t.scan::<I>(&encoded)
  1. entry_prefix = [ ns 2B ][ slot 2B ] + encoded     table and slot picked at compile time, bytes concatenated at runtime
  2. store.scan_suffix(&p)                             engine range scan, returns suffixes with the prefix stripped
  3. length criterion discards: suffix.len() < taken is not a hit (probe bytes satisfied by the primary-key tail)
  4. field segment → I::Fields                         derive-generated decoder (fixed widths cut, trailing variable field takes the rest)
  5. tail sliced into the primary-key encoding → PrefixKey<K> { decoded, taken }   width known (key_prefix_width)
  6. fetch: taken == KEY_LEN → get(decoded) + decode_payload
          taken <  KEY_LEN → row = None (a truncated prefix cannot fetch)
```

Scanning has only three verbs: prefix `scan`, equality `scan_exact`, range `scan_range` — all return iterators (one `Result` per entry); `collect` it yourself if you want the batch. They return the parts the caller names: the scan verb's type parameter is a **payload tuple** (`parts::Fields` / `parts::Key` / `parts::Row` / `parts::Include` — a part the caller does not name is simply not in the tuple), in the order the caller gives. `(Fields, Key, Row)` is one complete fetching scan; `(Fields, Key)` reads no payload — the first step of a join needs only these two; `(Key, Include)` is the covering scan. A part nobody names is not in the monomorphized function body, so a read-then-discard order cannot happen unless the caller asked for `Row`.

The first element is the value decoded from **the segment this entry is ordered by**: a `fields(...)` declaration yields the derive-generated field struct (each field decoded by its type in the row); a `func(...)` index's segment is the function's output, whose type the declaration cannot name, so it hands back `RawFields` (raw bytes), and the `ok_lead` face is `RawFields` too (those are the key's own bytes, and the key type can already decode them). It exists so that a caller does not have to "scan by a column and still fetch — or add `includes` — to read that column": joining on an index field therefore needs no extra read.

An index with a truncated `key(...)` gets only the trustworthy prefix fields; the caller takes `decoded` and continues a prefix scan on the primary table.

**Step 3 is this family's only criterion.** There is no boundary byte between the field segment and the primary-key tail, so a probe's bytes can be made up by "a shorter value + the tail" (stored `"ad"`, tail starting `0x31`, probe `"ad1"`; values and primary keys may both mix letters and digits, and that is legal data, not input that discipline can forbid). The tail's width is known, so the field segment's length is `suffix.len() - taken` (the probe itself included): **a remainder shorter than the tail's width ⇒ the field segment is shorter than the probe ⇒ the value cannot start with the probe**, so discard it. This criterion is one length comparison on the scan side; no fetch, no value read. "A longer value matches too" is not a false hit (that is prefix semantics itself); this family is the only false-hit class.

The fetch is one point read per hit, and what helps is that hits **arrive in primary-key order**: entries cluster by `[index fields][primary-key prefix]` and the primary table clusters by `[primary key]`, so **when the equality prefix pins the leading fields** this group's point reads advance monotonically through the primary table (increasing block order) instead of scattering. The scan order is still by index field first — only inside one fixed field tuple is it by primary key — so this is local order, not "one stream ordered by primary key": a range read across leading fields, a group that pins only some leading fields, a multi-value `func` index and a `key(...)` truncation all fail it. Where a merge is needed the caller sorts explicitly on the join key (`okm_query`'s `merge` / `intersect` take "the input is ordered" as a contract and do not check it; unordered input silently misses keys), so those shapes need an explicit sort and the locality argument does not apply. Quantification in [MODELING, "Design constraints on performance"](../MODELING.md).

## `scan_exact`: equality across every field

The same length quantity with a different comparison: `suffix.len() == taken` ⇒ the probe consumes the whole field segment ⇒ **the value is exactly the probe**.

```text
t.scan_exact::<I>(&encoded)
  1~2 as above (the engine's scan range is identical to the prefix scan's)
  3. suffix.len() == taken ? keep : discard   —— a longer value (a prefix-scan hit) and
                                          "the probe gnawing into the tail" are both excluded
  4~5 as above (tail decode + fetch)
```

It is **exact matching without a fetch**: no row check, no `includes(that field)` to verify against. The precondition is that the probe names **every field** of the index's data segment — naming only leading fields is group semantics (that is `scan::<I>`), and under the equality form those group hits are all rejected.

The dynamic side is the same criterion, word for word: `okm_core::model::access_method::scan_access_method` / `scan_access_method_exact` → `DynamicCollection::scan` / `scan_exact` → the Python binding's `Collection.scan` / `scan_exact` (`accept_embedded.py` locks one spill assertion).

## `scan_covered`: the covering scan

`includes(...)` fields are copied into the entry value, and `scan_covered::<I>` skips the fetch:

```text
store.scan_suffix_kv(&p)            key and value together
  → the tail is still sliced out of the key (width as above)
  → the value segment is cut into includes fields by width (bare concatenation, no tag, no length prefix)
```

Returns `(I::Fields, PrefixKey<K>, I::Covered)` — the field segment comes from the same place as in a fetching scan, and `Covered` describes only the entry value. `Covered` is the struct `__OkmCovered_{Row}_{index name}` the derive generates for every declaration that has includes: the fields are in `INCLUDES` order with the same Rust types, so the read side gets field values, not bytes. A variable-width field can only come last (the same declaration-time rule as `fields`), and decoding gives it the remaining bytes of that entry's value; a declaration without includes has `Covered = ()` — nothing was copied, so there is nothing to decode.

The dynamic side's counterpart is `DynamicCollection::scan_covered`, which returns two name maps, `(key fields, covered fields)`, separately — a key field name and a payload field name can collide, and the key is identity, not payload.

It is the materialized view for high-fanout queries — every put/delete pays a permanent write cost (`|includes|` bytes per entry; the entry is rewritten wholesale anyway, so the op count and the commit count are unchanged). Discipline in [MODELING, "Covering indexes, with restraint"](../MODELING.md).

## Keeping the write path in sync

`Table::put` is a single write: the primary entry (slot 0) plus every declared index's `entry_pairs()` entries (one pair for a plain index; N pairs for a multi-value function index), inside one store instance; `purge` deletes all of the same. There is no runtime index bookkeeping — `index_entries()` (derive-generated) statically expands into calling `entry_pairs` for each index and flattening, so the declaration is the registry. **Entries and declarations therefore never disagree**: a store holds entries under an ns segment's index exactly when the source declares that index. On an overwrite the reduce ledger's unfold/fold compensation is a separate constraint, see [reduce mechanism](reduce-mechanism.md) — index entries move with the overwrite naturally (a dangling old entry is caught only by `merge` / `replace`'s sweep; a bare `put` overwrite has no net), so the ledger must balance in place.

## The split between the two kinds of one-to-many

Same-table payload dimensions → `fields(...)` (the mechanism in this document); cross-table entity relations → a relation table, whose key is the pair of identities and whose reverse direction is a declared index (see [MODELING, "Many-to-many relationships"](../MODELING.md)). The two mechanisms are isomorphic (a secondary key layout pointing at an identity); the boundary is the data source: an index is a derived view of this row's payload, a relation table is first-class relational data.