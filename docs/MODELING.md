# Modeling Guide

How to map a domain onto OKM keyspaces. The order is the argument: the substrate's properties first, the constraints they force second, the shapes that satisfy them third — four layers per entity, access methods mandatory, covering indexes restrained, composite keys bounded. This guide is normative for application schema; for mechanism details see the ADRs and the [README](../README.md).

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## The substrate: what a KV store gives you, and what it does not

Every rule in this guide is one of the facts below applied to a shape, so they are stated once, here. A discipline whose reason is not at the front decays into a convention that the next reader argues with.

**What it gives you.**

- **Key order is scan order.** A prefix is a group and a range is a range read; `.rev()` is the reverse page. There is no ORDER BY to add later — the order you can read is the order you wrote into the bytes.
- **One commit covers any set of keys.** Atomicity is per commit, not per row: a side row, a relation edge and a reduce ledger move together or not at all (ADR-0003, ADR-0031).
- **A table is a key space.** No catalog to register in and no planner to feed: a table is a namespace plus two declarations (ADR-0044), so the ceiling is per table, never per system.
- **A value is your bytes.** No text container, no base64, no escaping (ADR-0018).

**What it does not give you.** Each of these is a constraint on the model, not a gap to route around.

- **No partial read.** `get` answers the whole value as owned bytes, and there is no projection door for one field. So whatever shares a key is read together: a 2 KB embedding beside an 8-byte counter is paid by every reader of the counter, and the only way out is another key — the side table ([below](#a-field-too-large-the-side-table)). It is also why a `Ref` is not the escape: its auto-deref restores exactly that read.
- **No partial write.** `put` replaces the whole value, so writing any field re-encodes all of them. Write frequency therefore decides what may share a key as much as size does: a small counter that changes on every call must not sit beside a large column that hardly ever moves.
- **No secondary index of its own.** Nothing maintains one for you; every index is a key you write and keep in step with the row. An access method is thus a declaration you owe ([the fourth layer](#the-four-layers)) and a piece of derived state with a fingerprint and a rebuild story (ADR-0048 faces, ADR-0029 path indexes).
- **No optimizer and no field-level knowledge.** The engine sees bytes: no predicate pushdown, no column pruning, no statistics. A query is a key range you chose, a filter you run, or a full scan; an aggregate is a maintained ledger ([pre-aggregation](#cross-row-pre-aggregation)) or a scan; and analysis leaves the store for a columnar form (ADR-0058).

**What each property forces, and where this guide carries it.**

| property | the constraint it forces | carried by |
|---|---|---|
| key order is scan order | every dimension you order or page by is encoded into a key, and a variable-width field goes last (it has no terminator) | [the four layers](#the-four-layers) · [design constraints](#design-constraints-on-performance) · [composite key bounds](#the-bounds-of-a-composite-primary-key) |
| one commit, any keys | pairs and ledgers are written in one batch; the semantic layer is single-writer | [rows and indexes](#rows-and-indexes-documentencode) · [two read-modify-writes](#two-read-modify-writes-reduce-and-upsert_with) |
| a table is a key space | a second table over the parent's key type, never an overflow position inside the row | [a field too large](#a-field-too-large-the-side-table) |
| whole-value read | big-and-cold data gets its own key; a read that wants one column pays for all of them | [a field too large](#a-field-too-large-the-side-table) |
| whole-value write | split by write frequency, not only by size; a patch verb exists because RMW must read first | [when to split](#when-to-split) |
| no secondary index | access methods are mandatory; every face is derived state, fingerprinted and rebuildable | [reverse queries](#reverse-queries-the-declared-index) · [declaration basics](#declaration-basics) |
| no optimizer | ordering and filtering are the key's job; aggregation is a ledger; analysis goes to Parquet/Arrow | [pre-aggregation](#cross-row-pre-aggregation) · [read paths](#read-paths-a-key-a-declared-face-or-a-point) |
| a value is bytes | no serialized-text storage; bytes stay bytes end to end | [payload types](#payload-field-types-and-wrappers) |

After one comparison — the two families that make the same trade — the rest of this guide is cases. The four layers are the first two rows in declaration form; the relation carriers are the second; the side table is the third and fourth.

## Where this sits: SQL, wide-column, and here

SQL hands the layout to the database. You declare normalized tables and a planner derives access paths; a secondary index is a database object, and denormalizing is an exception you justify. The wide-column family (Cassandra and its kin) makes the opposite trade and is the closer relative: a table is designed for one query, the partition key groups and the clustering key orders, and a second access path is a second table carrying a copy of the row — because there is no join and no free secondary index. OKM is in that family by construction: the key order *is* the read order, a face is a table you keep in step, and the four layers are the design instead of the plan.

What differs is how much is still yours. A wide-column engine owns compaction, partitions, tombstones and a query surface; OKM gives keys and commits, and the declaration layer is the whole model. That is the trade, stated without a soft edge: **manual layout is more capable and less forgiving.** Nothing fails loudly when a shape is wrong — it surfaces as a scan you did not intend, an index nobody maintains correctly, or a value two readers pay for and one wanted. Knowing which shape costs what is experience rather than syntax, and it is why this guide exists at all.

One piece of arithmetic removes a common worry. A key space repeats its prefix in every entry — the ns, the pkey, the sort field — and a second face repeats it again. That looks like storing the key many times, and in bytes it is; in cost it is not. The engines here are block-structured: entries live in compressed blocks behind a block index, most apply prefix compression inside a block, and a bloom filter usually fronts point lookups, so a shared prefix costs a few bytes per entry rather than a copy of the data. What is expensive to duplicate is a *value*, never a key — which is exactly the split [the side table](#a-field-too-large-the-side-table) makes.

## The four layers

Every entity is modeled in four layers — and the first thing to notice is that they are not conventions: each is a substrate property turned into a declaration, the order of the key for scanning and the absence of a maintained index. **namespace → primary key → primary sort field → access methods**.

1. **Namespace (`ns`)** — which keyspace the entity lives in. One entity
class = one ns. The 2-byte ns already discriminates every entry; do not encode entity type into keys.
2. **Primary key** — identity, and nothing else that can CHANGE. Proxy keys
(auto-increment id, UUID) are the default: attributes are payload, not identity;
the immutable dimensions a scan orders by belong to layer 3.
3. **Primary sort field** — the third segment of the key
(`[ns][pkey...][order]`). Placed at the key tail, it keeps entries under the same pkey physically adjacent and ordered: a prefix scan *is* the timeline / range read. Other sort dimensions stay out of the key and surface through a declared face — `#[ok_index]` over a payload column, or `#[ok_lead]` when the dimension is identity the key already holds. How physical decisions trade against indirect costs: see "Design constraints on performance".
4. **Access methods** — declared `#[ok_index]` entries. Each one is a
standing answer to "how is this entity queried?".

If you cannot name the access methods, the model is not finished — the questions you cannot name become full scans.

## Declaration basics

The four layers land in code as three derive macros. The full declaration vocabulary:

### Endpoint keys: `KeyEncode`

```rust
use okm_core::KeyEncode;

/// A user within an org. `org_id` is the organizational prefix;
/// `user_id` is the identity endpoint.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}
```

Physical layout of `UserKey { org_id: 7, user_id: 101 }`:

```
[ org_id: 4B BE ][ user_id: 8B BE ]   = 12 bytes, zero padding
```

Primary keys are fixed-width: fields are big-endian encoded in declaration order, `KEY_LEN` locked at compile time. Keys stay fixed-width; variability is a property of index entries, not keys.

### A relationship: the pair is the key

A many-to-many relationship is a table (ADR-0051). Its key is the pair of identities, and its row carries only what the relationship owns: a reverse read orders by identity through a lead face (ADR-0054), not by a copy of it. And because a key here is an ADDRESS rather than a uniqueness constraint, it may carry more than the two identities — see "The key carries more than a primary key".

```rust
use okm_core::{DocumentEncode, KeyEncode, Uuid7};

/// channel ↔ member: one row per pairing, the key IS the identity.
#[derive(KeyEncode)]
struct MembershipKey { channel_id: u64, member_id: Uuid7 }

#[derive(DocumentEncode)]
#[ok_ref(MembershipKey)]
#[ok_ns(110)]
#[ok_index(by_member { fields(member_id) })]
struct Membership {
    member_id: Uuid7,   // the column the reverse direction scans
    nickname: String,   // what the relation knows
    since_ms: u64,
}
```

The first key column is the forward direction; the declared index is the reverse. `#[ok_head]`
does not exist any more: it truncated an endpoint's identity to a shorter scan prefix, which a
key layout expresses directly by which fields the key names.

### Rows and indexes: `DocumentEncode`

```rust
use okm_core::DocumentEncode;

/// A user row hangs off UserKey via #[ok_ref]; payload fields are TLV-encoded.
/// Each #[ok_index] declares an access method over PAYLOAD fields —
/// identity belongs to the key (a surrogate id), business dimensions to the row.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(1)] // the table's namespace — declared on the row, not the key
#[ok_index(by_reputation { fields(reputation) })]
#[ok_index(by_org { fields(org_id, created_at), includes(bio_len) })]
pub struct User {
    pub org_id: u32,
    pub created_at: u64,
    pub reputation: u32,
    pub bio_len: u16,
}
```

- `#[ok_ref(UserKey)]` — which primary key the row hangs off; identity
belongs to the key, business dimensions to the row.
- `#[ok_ns(1)]` — the table's namespace segment, declared on the ROW (the row is the table's declaration point: `#[ok_ref]` pins the key type, so the row determines `Collection<S, K, R>` entirely). A key type carries no ns — the same key shape may serve several rows/tables, each with its own ns. `Collection::new(store)` takes no ns argument; the assembly site picks the engine only. It is **required**: a row without it does not compile, because a table with no ns has no place of its own in the key space and its keys would land next to whatever else was written there. And **one ns, one row**: the derive emits a marker const whose name is the ns, so a second row claiming the same ns in the same module is a build error (`the name __OKM_NS_7_IS_UNIQUE is defined multiple times`). The exception is spelled, not assumed — `#[ok_shared_ns]` on the second declaration, for a generation pair (one table before and after a layout change, ADR-0056). That check cannot leave the module, and a table may be declared in another one, so the STORE checks too: the first touch records the table's `#[ok_name]` at this ns, and a different name already sitting there is refused by name (ADR-0043).
- `fields(...)` — payload fields to sort/group by, declaration order,
first field = the grouping dimension.
- `includes(...)` — covering index: copies payload fields into the entry
value (see "Covering indexes, with restraint").
- `key(...)` — truncates the primary-key tail carried at the entry end to
the named subset (default: full). Truncation changes row-uniqueness, not grouping: the `fields` prefix drives ordering, the key tail distinguishes rows; `key(user_id)` is safe only when the named subset is unique per row — otherwise rows overwrite each other's entries.
- `func(path)` — function index: the data segment is the encoding of
`path(&row)`'s return value, replacing fields. The function is a plain Rust fn (implemented in business code), and the query side calls the **same declared path** on its probe value — encoding and scanning share one definition, so normalization (lowercase, encoding transforms) cannot drift between the two sides. One declaration drives both sides; declaration is registration. The return value must be iterable of the segment type — `[v]` for a single value, a `Vec`/slice/iterator for many (ADR-0055).
- `where(path)` — partial index: a row for which `path(&row) -> bool` is
false does not exist in this index (a partial index). The predicate receives the whole row and may read any column (not necessarily an indexed one); it composes with plain field indexes and function indexes alike.
- Raw-byte fields: `Bytes` (`okm_core::Bytes`) — a variable-length cold
TLV frame, the same encoding as `String` minus the UTF-8 constraint (hashes, ciphertext, serialized blobs). The retired `Vec<u8>` spelling is rejected at compile time with a pointer here: a list-shaped name misdescribed byte-string semantics.

**Basic use 1: single-value function index (write-side precomputation).** The data segment is the encoding of the function's return value — one element, one entry per row, entries still live and die with the row. The return form is `IntoIterator` (ADR-0055), so a single value is written as a one-element array rather than a bare value:

```text
fn lower_name(row: &Doc) -> [String; 1] { [row.name.to_lowercase()] }

#[ok_index(by_name { func(lower_name) })]   // "Apple"/"APPLE" co-located
```

**Basic use 2: multi-value function index (one row fans out into N entries).** Same declaration, one element per entry: return a `Vec`, a slice or any iterator of `V`. One form covers both uses — the writer asks for an iterator and does not care whether it yields one value or fifty (ADR-0055). What `V` may be is narrow, and identical in both uses: `String` (raw UTF-8) or `u8`/`u16`/`u32`/`u64` (big-endian, fixed width), nothing else — `&str`, `Option<T>`, `Uuid`, `[u8; N]` and `f32` are not function-index result types (a payload field may be a `Uuid` or an `f32`, but the entry segment has no order-preserving encoding for them). An empty iterator is zero entries. A `Vec<V>` return fans one row into N entries; the token is the data segment (variable-length, hugging the primary-key prefix), and the read path is identical to a plain index (`scan::<I>`):

```text
fn hour_bucket(row: &Post) -> Vec<u64> {
    vec![row.created_at / 3_600_000]          // ms timestamp -> hour bucket
}

#[ok_index(by_hour { func(hour_bucket) })]   // scan with a bucket id = that hour's posts
```

Time bucketing is this shape at its cheapest — a single-element Vec that folds the timestamp into a bucket number at write time (fixed-width BE, byte order = time order); hourly rollups and timelines are one prefix scan. The same primitive directly covers tokenized full-text search (a tokenizer returning `Vec<String>`) and multi-valued fields (split tags). \`okm-core\` ships no tokenizer — splitting/bucketing logic belongs to the business layer. The func contract is **purity**: purge re-derives the entry set from the stored row, so an impure function (clock / randomness / external state) produces a different set at delete time than at write time, leaving dangling entries. An empty `Vec` is one way to filter at row level (the row produces no entries at all), but a row-level condition reads better as a declared `where` predicate.

**Basic use 3: partial index (a `where` predicate).** Only rows the predicate admits are indexed — when most of the stored rows never use the access method (a ticket table that is 99% closed, say, indexing only the open ones), entry volume and maintenance cost are paid on the admitted subset:

```text
fn is_open(row: &Ticket) -> bool { row.status == 0 }

#[ok_index(open_by_assignee {
    fields(assignee_id, created_at),
    includes(title_len),
    where(is_open),
})]
```

The predicate sits at the same level as `func` and carries the same purity contract: purge re-derives the removal set from the stored row, so a predicate reading a clock or external state produces a different set at delete time than at write time, leaving dangling entries. The predicate's guarantee ends at the write side — entries are addressed by the indexed fields, and the predicate only decides "write it this time or not", so an overwriting `put` does not clean the old entry: when a row goes from admitted to rejected, the old entry dangles (the same pre-existing contract as "a changed value leaves a dangling entry"), and no verb can clear it any more — `purge` derives from the STORED row, which is no longer the one that produced the entry (ADR-0035, 2026-10-23). The overwrite path is `merge` / `replace`: they sweep the stored row's faces before writing. Nothing checks completeness on the read side either — a partial index is complete only for queries whose condition implies the predicate, okm has no query planner, and choosing the index is the caller's job. That discipline belongs to modeling, not to the mechanism.

`func(path)` and the `#[ok_reduce]` declaration below are the two external extension mechanisms: func is **per-row derivation** (computed at write time; entries still live and die with the row), reduce is **cross-row aggregation** (mutable value, read-modify-write). How the heavier integrations (FTS / vector / graph algorithms) land on the primitives is covered in the [integration boundary doc](integration/EXTENSION-TYPES.md). Implementation details (encoding contract, `entry_pairs` override, probe normalization) live in the internals doc [func-index-mechanism.zh-CN.md](internals/func-index-mechanism.zh-CN.md).

Physical index entry layout (ADR-0005):

```
[ ns 2B BE ][ slot 2B BE ][ fields segment BE ][ primary key prefix (default: full) ]   value = included fields TLV (empty when no includes)
```

The discriminator is namespace + slot: the ns segment scopes the table, the slot's segment number distinguishes entry kinds within it (primary/index/reduce, ADR-0016); declaration is the registry, no runtime index bookkeeping. **Index declarations are append-only**: add at the tail only — never insert into or reorder the middle. An insertion shifts the slot of every later index; entries already on disk stay in the old slot position, and after the shift `scan` reads a new prefix and returns empty results (a silent error, not a slowdown). Removing a declaration merely leaves a harmless slot hole (same discipline as ns IDs never being reused, ADR-0002). Also note there is no index backfill: a newly appended index only sees rows written afterwards; existing rows get no entries — migrate with double-writes if existing data must be covered.

The fields segment is semantically an **ordered sequence of dimensions**: the leading field is the grouping/equality dimension, the rest sort within it. Two constraints govern it —

- **Decode constraint**: at most one variable-length field, and it must
sit immediately before the primary-key prefix. The primary key is fixed-width (`KEY_LEN`) and is sliced off from the tail; any further fixed-width fields are sliced right-to-left; whatever remains is the single variable-length segment — its length is never stored, the boundary is inferred from the fixed-width anchor on its right. Two variable-length segments (e.g. `fields(token, name)`) share no boundary byte and are undecodable; the derive rejects this at compile time.
- **Query constraint (leftmost-prefix)**: prefix scans specify fields
fully from the left, in declaration order. A variable-length field that is not last breaks prefix semantics — raw bytes `"beijing"` have no terminator, so a scan for it also matches `"beijing2"`. Variable fields go last; equality/fixed-width dimensions go first. The mirror family — a probe LONGER than the stored value, its bytes completed by the carried key tail — is not a match and is dropped by the scan's length test ([index-mechanism.zh-CN.md](internals/index-mechanism.zh-CN.md)); full-field equality is `scan_exact`.

Note that `includes` is not part of the fields segment — it lives in the entry value and takes no part in key structure or ordering. Expressing "carry more data in the entry" by adding fields is a modeling mistake; the correct outlets are `includes` (copy to skip table lookups) or nested entries (store together).

**Several text dimensions do not violate this constraint — they change shape.** The bound is on NAKED variable-length segments (one, last), not on how many text dimensions an entity may have. When several are needed, pick a shape in this order:

- **Hierarchical dimensions become proxy ids (fixed width).** Province / city /
district is `fields(province_id, city_id, district_id)`: three fixed-width segments, legal, and `[province]`, `[province][city]`, `[province][city][district]` are each one prefix scan — how deep the grouping goes comes free from declaration order. Names ride the open-vocabulary set (row payload + a `by_name` face). It buys more than legality: a rename never moves the key, keys never grow with the text, and no terminator ambiguity exists.
- **One face per text dimension** (`by_name`, `by_city`, …): each face holds that
single variable segment, legal — at the price of one more entry per row, i.e. a permanent write cost per face.
- **A fixed-width sort dimension AFTER a text one rides the key tail.**
`fields(name)` with the tail carrying time/identity gives the entry `[name][tail]`, i.e. a `(name, time)` composite order: the carried tail is the only thing allowed to follow a variable segment — its third job, per ADR-0005.
- **Two text dimensions that must BOTH order the bytes → one computed segment.**
A `func` index makes the data segment a single string with the separator inside the VALUE (`last\x00first`): still one naked variable segment, still no terminator in the key, and the prefix `last\x00` means "everyone with that surname". Choosing the separator is that declaration's contract (the same one-declaration-drives-both-sides rule as probe normalization); it must stay outside the value domain or the classic escaping problem returns — owned by the value-layer function rather than the key layout. The other lookup direction is a second face (a func fan-out).
- **Fixed width before VarInt.** Two `VarInt` fields hit the same bound; spelling
the width out (`u32`/`u64` BE) removes it — fixed width is the norm in a sort segment, VarInt is a space-saving device.
- **The genuine last resort is a self-delimiting frame**: two adjacent naked
variable segments are structurally undecodable; that route works but trades away zero-cost slicing and the compile-time layout lock (ADR-0005).

Every extra face is paid on the write side: one more entry set to maintain per put.


### Put, purge, query

The verb pair is `put` / `purge` on the pair key — no `link` / `unlink`, because the row and its
index entry are written by the same batch:

```rust
let members: Collection<_, MembershipKey, Membership> = Collection::new(store);

// write a membership: the row and the by_member entry land together
members.put(
    &MembershipKey { channel_id: 7, member_id: u },
    &Membership { member_id: u, nickname: "zhang".into(), since_ms: now },
)?;

// forward: the channel's members, one prefix scan over the primary key
let probe = PrefixProbe::<MembershipKey>::new(7u64.to_be_bytes().to_vec());
let rows = members.scan_keys(&probe);

// reverse: the channels this person is in, one index scan + fetch-back
let channels = members.scan::<__OkmIndex_Membership_by_member>(u.as_bytes())?;

// delete: both entries leave in the same batch
members.purge(&MembershipKey { channel_id: 7, member_id: u })?;
```

The relationship's identity is the pair: at most one row between two endpoints, enforced by the
key and nothing else. Adding or removing a membership is one call; identity never moves.

### Reverse queries: the declared index

There is no "truncated identity" to decode any more (ADR-0051). The reverse direction is an
index scan, and every hit comes back with its key and, with `scan`, with the row itself:

```rust
// Which channels is this member in? One index scan, each hit points at its row.
for (key, row) in members.scan::<__OkmIndex_Membership_by_member>(member_id.as_bytes())? {
    let channel_id = key.decoded.channel_id;   // the pair, decoded from the entry's tail
    let since = row.map(|r| r.since_ms);        // fetch-back (回表); `None` if the row is gone
}
```

`KeyEncode` derives the prefix encoder too, so a composite key gives you the forward scan
without writing a probe by hand: `K::prefix().field(..).finish()` builds a `PrefixProbe` that
matches only the leading fields you named.

### Rows at runtime (`Collection`)

Declaring rows and indexes (`DocumentEncode` + `#[ok_index]`) is covered in the [Modeling Guide](#declaration-basics), "Declaration basics". The index declaration's derivative is generated at the expansion point (this file): `ok_index(by_org ...)` generates the index type `__OkmIndex_User_by_org` (mechanical concatenation, no case conversion); alias it with `use` and it serves as the generic parameter. The name is spelled out rather than hidden because the caller has to write it — a generated item is guessable, not hygienic (ADR-0053); hygiene is what the generated function bodies use for their own locals, so a field named `b` cannot break the decoder. `Document::collection` builds the assembly point without repeating the key type at the call site:

```rust
use okm_core::TestStore;
use __OkmIndex_User_by_org as ByOrg; // index type: derived from ok_index(by_org)

let t = <User as Document>::collection(TestStore::default());

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 })?;

// Leftmost-prefix scan on any access method. The type parameter after the
// access method says WHAT TO READ out of each hit, in the caller's order:
// (fields, key, row) here — drop `Row` and no payload is read at all, which is
// what a join does (intersect the entries, then fetch only the survivors).
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes())
    .collect::<Result<Vec<_>>>()?;   // a scan is an iterator; this is the mirror
for (fields, key, row) in rows {
    // fields: the index fields, decoded (org_id here) — the column you scanned BY
    // key:    decoded UserKey (PrefixKey: full when the entry carries it)
    // row:    Some(decoded User) when the payload exists
}

let entries = t.scan::<ByOrg>(&7u32.to_be_bytes())
    .parts::<(parts::Fields, parts::Key)>()
    .collect::<Result<Vec<_>>>()?;

t.purge(&user)?; // removes the primary key + all declared index entries (+ the dynamic segment)
// (multi-value indexes clear entry by entry — no dangling entries)
```

Multi-value function index, declared and queried (inverted-index shape — the token is the data segment, the primary-key prefix is cut off the tail). The `Vec` return is contract, not laziness: put consumes the values immediately, entry by entry, so a lazy iterator buys nothing — one `collect` for the smallest trait surface:

```rust
fn tokens(row: &Doc) -> Vec<String> {
    row.text.split_ascii_whitespace().map(String::from).collect()
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_index(by_token { func(tokens) })]
pub struct Doc {
    pub text: String,
}

use __OkmIndex_Doc_by_token as ByToken;

// Write: one row of "rust kv" fans out into two entries (rust → key, kv → key)
// Query: prefix scan by token → fetch-back, exactly like a plain index
let hits = t.scan::<ByToken>(b"rust")?;
```

A multi-value face is also the primitive under full-text search, and that use carries one discipline worth knowing before declaring one — what recall gives, and where frequency comes from: see *Inverted index: recall from the index, frequency from the row*. The entry it produces is the size of one token plus a key tail, not the size of a document — which is why the write cost tracks the token count rather than the row size.

Beyond the equality prefix, a comparison on the leading index field is a physical key interval (ADR-0020): `scan_range::<I>(begin, end)` / `scan_range_iter::<I>(..)` — byte order == value order for every OKM encoding, so `1 < a < 100` reads only the rows inside `[1, 100)`. Recipes live in the [query guide](query-recipes.md), "the forms of WHERE".

### The engine is an assembly-point choice

`Collection::new(store)` takes a store, and nothing on any type says which engine it is (ADR-0010 — the assembly site picks the engine, the declaration owns the ns). Every table therefore has this freedom, not only a side table: one application's tables need not share an engine. The hot table can sit on a small, fast one, the payload table on an LSM tuned for volume, and a read-mostly export table on whatever dumps cheaply. In a relational engine the same wish is a foreign server or a linked table; here it falls out of the model, because a table already IS an independent key space with its own handle — nothing has to be bridged for it to live somewhere else.

The freedom stops at exactly one line: **a commit is per store.** Two collections on one engine write one `Batch` and commit once — that is how a parent row and its side table stay atomic; two collections on different engines are two commits, so a pair that must be atomic lives on one engine. Splitting by access pattern is free; splitting by engine is a durability and atomicity boundary, and the boundary follows the store rather than the ns: capture and restore are whole-store too, so a table that moved to another engine is captured by that store's log.

### A table is a key type plus a row type

Two declarations, not one (ADR-0044). The key is the address — `KeyEncode`, fixed width, ordered bytes. The row is the value — `DocumentEncode` — and `#[ok_ref(ProfileKey)]` BINDS it to the key without embedding it: no key bytes live in the row.

```rust
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct ProfileKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ProfileKey)]
#[ok_ns(41)]
pub struct Profile {
    pub user_id: u64, // same NAME as a key field: a different, unrelated field
    pub bio: String,
}
```

`Collection<S, K, R>` carries both halves and `R::Key = K` pins them together at compile time; `Document::collection(store)` reads `K` off `R`, so the call site never restates the key type. Three consequences, all deliberate:

- **One key type, many tables.** Because a read takes the whole value and a write replaces it, "one entity, two halves" becomes "one key type, two tables" (ADR-0036): a second row type over the parent's key type, in its own ns, key payload byte-identical — `ProfileStats { views: u32 }` over `ProfileKey` costs one more point read rather than a second key space, and its writes never re-encode the row. Everything else about the two tables is separate, including their commits.
- **`user_id` above is two fields, not one stored twice.** The key's `user_id` is
the address; the row's `user_id` is data that may legitimately differ — a denormalized copy for an index, or so a reader need not re-derive it from the key. Nothing dedupes, nothing errors, nothing compares them on a read.
- **Names are classified by the declaration, not by the key.** In a whole-document
write a name that is a declared payload field updates the row; anything else lands in the dynamic segment (ADR-0012). The key's own bytes never appear in the document map — that map is payload fields plus segment.

The boundary with SQL is structural, and it sits here. A relational engine hides the key inside the row, so a value split off into overflow pages is an engine detail addressed by the row id with a chunk ordinal appended. okm has no row outside a key — the table IS a key space — so the same split is a DECLARED side table whose ns sits at the FRONT of the address while the key payload stays byte-identical. Hence: the side table is still scanned by the parent's key prefix, it commits on its own, and nothing can join the two halves back — composition is the application's two point reads (ADR-0036).

#### Case: the key is the identity — prism's rewrite, and the five mirrors that stay

The relational instinct is that a row owns an id: a surrogate key in a column, an index over that column, and every read handing the id back out of the row. okm has no row outside a key, so the instinct costs a second copy of the address and creates key-versus-payload state that only the writer keeps honest. prism's identity tables were rewritten to delete exactly that (`crates/prism/src/identity.rs`, 2026-10-30):

```rust
// before: a u64 counter in the key, mirrored into the row so a reduce could fold over it
pub struct AccountKey { pub id: u64 }
#[ok_ref(AccountKey)]
#[ok_index(by_name { fields(username) })]
#[ok_reduce(HighWater(id) { group(global) })]   // next id = MAX over the payload
pub struct Account { pub id: u64, pub username: String, pub nickname: String, pub password_hash: String, pub global: u32 }

// after: the key IS the id, and the row says only what the key cannot
pub type UserId = Uuid7;
#[ok_ref(Uuid7)]
#[ok_index(by_name { fields(username) })]
pub struct Account { pub username: String, pub nickname: String, pub password_hash: String }
```

All three tables (`Account` ns 50, `Device` ns 51, `Token` ns 53) now carry no id field. Three pieces of machinery left with it: the mirrored field, the `HighWater` reduce that issued u64 ids, and the constant `global` group column that existed only because a reduce needs a group. A `Uuid7` issues itself, so nothing has to count.

The key is not the cheap substitute for a column. It is stronger in four ways, and none of them is available to a field inside the row:

- **It is a read path, not only an address.** "Every device of this user" is key composition, not an index: with `[org][user][device]`, `scan_keys(&DeviceKey::prefix().org(7).user(11).finish())` is the whole query — one prefix scan over the primary table, nothing declared, and lazy (`.take(n)` a LIMIT, `.rev()` a newest-first page). This is where the model departs from SQL: there an index is a COPY of the data that the planner may or may not use, here the ordering the read needs IS the storage order, so the bytes that identify a row also group it. A face is declared only for the dimensions the key does not order (ADR-0049).
- **It is ordered.** Key bytes ARE the sort order of every scan (BE fixed width, prefix-monotonic VarInt). `Uuid7` in the key makes creation order readable to the millisecond with no timestamp field and no index — ties inside one millisecond sort by their random tail, and that is enough for insert locality, which is why a btree's hot page stays hot. `Uuid4` for the token says, by choice of type, that a credential has no order worth keeping.
- **It cannot drift.** One writer builds the address once. There is no second copy to update and no read path that could compare two ids and disagree.
- **It is already in hand on every read.** A point read takes the key the caller passed; `scan` and `scan_covered` return `PrefixKey<K>` beside every hit. A mirrored id hands a caller something it already had.

What a value SHOULD carry is the id of something else — `Device.user`, `Token.device`: a foreign relationship the key does not encode. That is data, it may change, and no read can recover it from the address.

A pass over every declared row in the family (aura, prism, krystallizer, mudra, effector, gravity) finds five mirrors, and each one has its reason written beside it:

| site | mirrored | why it is there | retires when |
|---|---|---|---|
| aura `meta.rs` `BoothName.id` | the issued proxy id | the `HighWater(id)` reduce folds over payload fields | ADR-0024's key-field groups reach the derive (aura's own note) |
| aura `mq.rs` `InstanceKeyRegistry.id` | the issued proxy id | the same reduce | the same |
| aura `mq.rs` `EventRoute.booth_id` | a key segment | `ok_index` fields must be payload fields, and `by_booth` scans this one | an index can name a key segment |
| aura `meta.rs` `CodeBlob.sha256` | the whole key | observability: a raw scan shows its own content address (this table declares no index) | whenever — the cheapest of the five |
| prism `nodes.rs` `Node.id` | the issued node id | `HighWater(id)` again | when node ids stop being a local counter |

None of the five is SQL habit; each is a workaround for something the model does not yet allow, and four are written down with their retirement condition. The habit worth watching for is the sixth kind — a field that is always equal to the key above it and has no condition attached. That field is the address said twice: delete it, do not add a check that compares the two.

The far end of the same spectrum is aura's `MqData`, whose row type is empty (`pub struct MqData {}`): event, instance key and sequence are the key, depth and write head are reduces, and nothing needs saying twice.

### Dynamic fields: the document API (ADR-0012)

One encoding serves declared rows and external data. Declared fields ride the hot/cold segments as usual; anything else lands in the **dynamic segment** (fixed face 0x01) as a block table over a value area — `toc_len u16 | blocks | values`, an entry being `[field-id][head]` (one byte: length, type, composite), nested blocks breadth-first (ADR-0057; the bytes and the size envelope are in [dynamic-segment.md](internals/dynamic-segment.md)) — with names allocated on first sight in the **field-name vocabulary** (fixed faces 0x02/0x03). Runtime surface on `Collection` — the split is about what a call may LOOK AT, and it is spelled in the type: `get_row` is the **struct door** (the row only — no vocabulary load, no segment access; the compiled-schema path), `get::<(parts::Row, parts::Value)>` names the halves it wants and pays for those, and a write is ONE verb whose payload picks the door — `put(&key, &R)` is the struct door, `put(&key, &map)` and `put(&key, (&map,))` are the whole-document door (ADR-0035 as amended).

The classifier is the payload declaration — hot or cold — and nothing else. A name that coincides with a key field is not special: OKM is a key-value store, the key and the value are separate things, so such a name is simply a field in the value and lands in the dynamic segment. The identity still comes from the key: no map can move it, and a map naming `org_id` creates no row at `org_id`. Nothing needs checking here, and no confusion needs preventing — a caller that stores a field named like a key field gets exactly what it asked for.

Writing is one verb, and the payload type chooses the path. The three face lifecycles never followed from a verb name anyway — they follow from what the payload IS:

| payload | path |
|---|---|
| `&R` | the row and its faces. The fresh-row fast path: it does NOT clean the previous values' faces (that is `purge` / `save_with_prev_into`'s job) |
| `&Variant::Map(map)` (or `&Dict` on the dynamic side) | partial document: a declared name is a row read-modify-write (superseded face swept, ledger compensated), every other name overwrites itself in the segment, names the map omits keep their stored values |
| `(&Variant::Map(map),)` (or `(&Dict,)`) | whole document: the map IS the document — an omitted declared field takes its default, an omitted dynamic name is removed |
| `purge` | both slots, one commit — the ONE removal verb |

A scalar or array payload is refused (`Error::NotADocument`): a scalar is a field VALUE, not a document. The payload is caller data, so a refused write writes nothing.

Reading is parts, and the composition is the caller's. There is deliberately no assembly function: a row converts with `Document::to_value` (its name map as one `Variant::Map`) and two values compose with `Variant::merge` / `merged` — objects merge name by name, nested objects recursively, and anything else replaces the left side (an array is a value, not a list of names).

```rust
// A point read names its parts, in the caller's order, and only those are
// decoded: Row is slot 0, Value is slot 1 as Variant::Map.
let (row, dyn_) = t.get::<(parts::Row, parts::Value)>(&key)?;  // (Option<R>, Option<Variant>)
t.get_row(&key)?;                              // sugar for get::<(parts::Row,)> — Option<R>
t.get::<(parts::Value,)>(&key)?;               // slot 1 alone; the row is never decoded

// The document: the row as one value, the segment laid over it. `None` only
// when BOTH slots are empty — a dynamic-only document still answers.
let doc = match (row, dyn_) {
    (None, None) => None,
    (row, dyn_) => Some(
        row.map(|r| r.to_value())   // Option<Variant::Map>
            .unwrap_or_default()    // an absent half contributes no names
            .merged(dyn_.unwrap_or_default()),
    ),
};

// Partial write: declared names are a row RMW (typed path, previous faces
// cleaned), unknown names overwrite just those names in the dynamic segment.
t.put(&key, &patch)?;                          // Dict / Variant::Map

// Whole-document replacement: the map IS the document — absent declared
// fields take defaults, unnamed dynamic names are removed. The one-element
// tuple IS the whole-document arm.
t.put(&key, (&doc.unwrap_or_default(),))?;

// Both slots, one commit — the ONE removal verb (a row cannot outlive its
// segment, so there is no row-only delete to get wrong).
t.purge(&key)?;                               // -> bool (something was there)
```

- `Variant` is the tagged union of the value kinds a document can hold: `U8`/`U16`/
`U32`/`U64`/`I64`/`F64`/`Str`/`Bytes`/`Bool`/`Null`/`Array`/`Map` — `Map` is the
name-keyed object (a document is one), and nested `Map`s recurse as native frames
(type tag 7) sharing the table's vocabulary; no CBOR.
- The object is `okm_value::Dict`, a type and not an alias: an insertion-ordered
name → value map whose surface is an explicit method list (`get`, `insert`, `len`,
`iter`, …) written on the type and forwarded to the backing map. There is no
`Deref`, so the container's name appears nowhere in the API, coercion cannot hand
a `&Dict` to a `fn f(&<container>)`, and no backing method the contract does not
list is reachable; swapping the container rewrites that one impl block and nothing
else. The contract is therefore the type's: iteration is
INSERTION order (an overwrite keeps a name's place), equality ignores order, and a
site needing a canonical order sorts explicitly. `Array` stays a plain `Vec` because
a vector has one canonical implementation; an ordered map's implementation — and the
ordering it implies — is a design choice the model must own.
- The value form crosses a boundary as itself, not as a hand-written
`match` per direction: `Serialize`/`Deserialize` for `Variant` ride the same `schema-serde` feature. `Deserialize` goes through `deserialize_any`, so the format must be self-describing (JSON, CBOR — not bincode); in JSON two collapses are unavoidable and documented — `Bytes` renders as a number array (CBOR keeps a byte string), and a non-negative `Int` reads back as `UInt`. The storage encoding is untouched: the block table's entries plus vocabulary ids remain the representation at rest (ADR-0057).
- `Bytes` is the **opaque member** of the vocabulary: the store
interprets nothing — tag and total length only; content and its meaning belong to the application. Opaque means the CONTENT: the frame's `[len]` prefix is payload delimiting (the encoding layer's job — how multi-field payloads are sliced), not interpretation; `String` carries the same header. It is the standard escape hatch for extension types: a new encoding starts life as Bytes, and promoting it to a first-class type later is an additive change. The field name carries the semantics the tag deliberately does not (`embed_v1`, `attrs_cbor`).
- Unknown names are **normal input** here (external data, MQ payloads);
the typed decoder's unknown-field rejection applies only to the declared path.
- Declared fields are indexable through their own 0x1 faces. A dynamic
field is indexable through a path index over its declared path (ADR-0029: one entry per resolved value, the value types with a storage-form encoding — U8/U16/U32/U64/FixedBytes/Str), landed on the schema-driven (`DynamicCollection`) write path; a typed collection's own dynamic segment has no such maintenance wired yet. What dynamic names can never be is KEY material: a name is runtime data, so a dynamic path is addressed by its declaration, never by the key.
- The schema export (`CollectionSchema`, serde behind `schema-serde`) drives
the dynamic codec for embedded-language readers — Python (PyO3) and Steel bindings live in `bindings/`, byte-identical with the Rust derive (cross tests lock this). Version-default migration works on the dynamic read path too: literal `#[ok_default]` travels with the schema.

### Path indexes over the dynamic segment (ADR-0029)

An access method can name a PATH into the schema-free zone instead of a declared field:

```rust
use okm_dynamic::{AccessMethod, FieldType, Path};

// nested object field
let by_author = AccessMethod::path(0x1001, Path::parse("author.name")?, FieldType::Str)?;
// array path: one entry per element (the fan-out regime)
let by_tag = AccessMethod::path(0x1002, Path::parse("tags")?, FieldType::Str)?;
// pinned element: exactly one
let by_first_href = AccessMethod::path(0x1003, Path::parse("refs[0].href")?, FieldType::Str)?;
```

- **Components**: `Field(name)` — an object key, interned through the table's
field-name vocabulary; `Index(u32)` — an array position (data: never interned, and not stable across mutations); `Key(bytes)` — a literal key, no interning. The first pair and `Index` are two namespaces and two walker operations (map lookup vs subscript), not one niched integer.
- **Resolution**: any component after an array fans out over every element
(a wildcard is the ABSENCE of `Index`), a path ending at an array fans out over its scalar leaves, and an `Index` pins exactly one element. A missing name, an out-of-range index, or a path ending at an object resolves to nothing.
- **The value type is declared** (`FieldType`): an entry segment is
byte-ordered and a path has no schema field to consult. Unsigned kinds also accept the widened form the dynamic read path produces (a `U32` written value reads back as `U64`), which is what keeps an entry's bytes a function of the stored value — the overwrite sweep recomputes exactly the keys the write built, so no entry survives a changed value.
- **Maintenance rides the whole-document write path** (`merge` / `replace` /
`purge`). The primary document path skips path declarations by construction: a depth-1 path over the primary document is what a plain index already is, and one entry set has exactly one maintainer.
- A value whose variant does not match the declared type produces no entry
and no error — the "absent attribute values are empty results" rule.
- **Queries** are the ordinary leftmost-prefix scans (`scan(slot, prefix)`),
ADR-0020's range scan covering ranges over the value. The canonical path encoding (self-delimiting, no leading component count) is the form paths take where they are DATA — declaration carriers and exported schemas; the entry key carries the slot, never the path.

### Embedded documents (`Ref<D, K>` and `Refs<D, K>`)

A child document embedded **by key reference**: the parent's field carries only the child's key on the wire (fixed width, hot segment); the child is a complete document at its own ns/key with its own indexes. In memory the field is `key` + `Option<value>`:

```rust
#[derive(KeyEncode)]
pub struct AddressKey { pub owner_id: u64, pub kind: u8 }

#[derive(DocumentEncode)]
#[ok_ref(AddressKey)]
#[ok_ns(42)]
pub struct Address {          // a complete document of its own
    pub city: String,
    pub zip: u32,
}

#[derive(DocumentEncode)]
#[ok_ref(OwnerKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub address: Ref<Address, AddressKey>,  // no attribute needed
}
```

- **Write, `Some(value)`** (`Ref::own(key, value)`): the parent's
`put` also writes the child payload + the child's index entries, in the same store batch (one atomic boundary).
- **Write, `None`** (`Ref::ref_key(key)`): reference an existing
child — the parent stores the key and touches nothing else. This is the shared, many-to-one form (many users pointing at one address).
- **Read**: `get` dereferences automatically — the child is fetched by
the stored key and backfilled. A missing child (deleted independently under reference semantics) reads back as `value: None` — visible absence, not a panic. Queries return the nested struct directly.
- **Overwrite**: changing the key releases the old reference — keys the
OLD document pointed at but the new one doesn't are deleted. No cascade: a shared child survives; an owned-cascade option (`#[ok_embed(own)]`) is a possible future extension.
- No attribute is required: the derive recognizes `Ref<D, K>` / `List<D, K>`
from the field type, the same discipline as `Reverse<T>` / `Quant<P>`.
- **Lists**: `Refs<D, K>` embeds many children — wire is a cold TLV frame
`[count][key × n]`; memory is `keys` + index-aligned `values: Vec<Option<D>>` (dangling refs stay visible). Child keys must carry their own list identity (`owner_id + seq`) — OKM never appends positional numbers to keys. Stale release covers shortening: keys the old list held and the new one doesn't are deleted. `Refs` is the declarative carrier of the one-to-many relation above: children stay normalized in their own ns, the parent field holds the foreign-key set.
- **The identity line**: elements WITH identity (own indexes, sharing,
independent updates) belong in `Ref`/`Refs`; pure-value elements (`Vec<String>` fields, dynamic `Array` frames) do not — a scalar has no key, and a key reference to it is a category error.
- The map view (`to_map`) lifts an embedded field to
`Bytes(child key)` — the wire truth; the child's value belongs to the child collection, not this map.

Embedded is one of three ways fields relate across documents — `includes` copies values into an index entry (skip the fetch), dynamic frames nest values inside one payload, and embedding references a separate document (shared identity, independent indexes, independent lifecycle).

### Layout generations: the segment says which bytes these are

A payload has no header. The bytes are the two segments back to back — `[hot segment][cold TLV]` — and the boundary between them is a compile-time fact of the declaration (`HOT_WIDTH`), so a row repeats nothing its reader already knows, and an empty document is zero bytes.

The generation lives one level out, in the **segment** header (`layout_version`), and `#[ok_layout(version = N)]` (default 1) is the declaration's half of that number: an identity, never a compatibility promise (ADR-0042). Nothing is compared per row, because nothing may read a row without the log it came from — the bytes never have to vouch for themselves:

- **A segment is read under the declaration its own header names** — `schema-v<N>.json`, chosen per segment by the `layout_version` in the segment header. A generation whose sidecar is missing is a stop that names the number; a neighbouring declaration is never borrowed, because borrowing is how a payload gets read as the wrong row. There is no second reader and no "read it as best you can".
- **The directory is permanent, so it accumulates generations.** The changelog path never changes (ADR-0056 §5/§6): an upgrade appends its own segments beside the older ones, the older sidecars stay where they are, and nothing refuses a directory because its newest segment was written by another build. A **window** is one generation, since it is decoded with a single declaration.
- **The identity is per table.** One build, one store, one process; each table's log states its own shape, so generations can be cut over table by table.
- **What survives without a new generation is the COLD half only.** An absent cold TLV frame is a field that was never written, and it takes its declared default:

  ```rust
  #[ok_default(100)]  pub reputation: u32,   // any expression; absent → T::default()
  pub bio_len: u16,                          // no attribute → T::default() (0)
  ```

  A **hot** append moves the boundary and shifts every cold frame after it, so it is a shape change — a new generation, not an append a decoder may absorb. `#[ok_default]` is about a field being absent, not about a reader tolerating a shape it does not have.
- **A change of FORM is a shape change, and the bytes cannot tell you.** A categorical field is the case that bites: an `Enum` tag and a `Vocab` id are both a small integer, so old bytes read under a new declaration decode without error into a DIFFERENT category. Neither a default nor the boundary helps; the guard is the segment's number.
- **No in-place migration, and no upgrade-on-read.** A payload that happens to decode is a lucky landing, not a migration. Rows cross by export (the build that wrote them) → import (the build that reads them), which is also the only path that re-interns their labels into the destination's vocabulary (ADR-0032 §7).
- **An in-place binary swap is free exactly when the layout did not change**: the numbers match, there is nothing to move, and the downtime is a restart. When the layout DID change, in-place is not cheaper — the store must be read and rewritten with the service down, or rebuilt beside the old one while it serves. Blue-green costs a second copy of the data for the length of the cutover; a table-by-table cutover keeps the peak at "everything + the largest table" instead of double.
- `ensure_layout` at attach records the declaration's face fingerprint (ADR-0048) and answers the row it finds; it compares no version.

### The table's own record (fixed face 0x0F)

A table can describe itself. Fixed face **0x0F** holds ONE entry per table — `[partition?][ns][0x0F]`, no suffix — whose value is a **name → bytes map** (ADR-0043). `version` is the name the runtime writes, and its value is the same number the payload header carries; anything else a consumer wants to record about the store rides beside it without a layout decision.

```text
[count u16 BE] ( [name_len varint][name UTF-8][value_len varint][bytes] )*
```

Two names are the runtime's. `version` is the payload's shape. `faces` is the fingerprint of the declaration's derived faces — the `#[ok_index]` slots, which live in their own ns and change without changing a single payload byte (ADR-0048) — and it is computed from the declaration, never typed. `Collection::faces_status()` compares it and answers `Match` / `Stale` / `Unknown`, and `Collection::rebuild_faces(chunk)` / `Graph::reindex(chunk)` are the verbs that repair a `Stale` store in bounded commits; `ensure_layout` never refuses on it, because a store written before faces existed would fail a check it can never satisfy, and a stale face is repairable drift rather than an unreadable row.

`Collection::ensure_layout()` is the first-touch verb, and it stamps ONE name now — `faces`. Absent → seed the fingerprint of this declaration's derived faces; present → answer the row untouched, because repairing it would erase the `Stale` signal the maintenance verbs act on. There is no version name to compare: the payload's generation is the segment header's business, and a store the current declaration cannot read is carried over by replay, not detected at attach. `set_meta(name, value)` records a caller-owned name and nothing else. Call `ensure_layout` at attach: one point read per table, never a per-row cost — and `Collection::new` stays I/O-free because of that.

Three boundaries. The entry is **not a row**: row scans and the export path filter by slot, so 0x0F is skipped like 0x01/0x02. It does **not** travel in export/snapshot: it describes the WRITER's shape, and the importing build stamps its own. And it is a **detection** device, not a compatibility promise (ADR-0042): it turns a wrong-shape store into a refusal at attach instead of a decode failure on the first row, and it never makes an old shape readable.

The dynamic codec (schema-driven encoders in Python/Steel) mirrors this rule from `CollectionSchema` — per-field defaults travel with the schema so the dynamic reader applies the same decoder rule.

### Payload field types and wrappers

Every declared field is one of these wire shapes. This is the map from the Rust type in the struct to the bytes in the hot or cold segment; the derive recognizes wrappers by name, so the spelling matters.

| Rust type | Wire | Width | Use for |
|---|---|---|---|
| `u8` `u16` `u32` `u64`, `i8` … `i64` | BE fixed-width | 1/2/4/8 | numbers, ids, counters |
| `[u8; N]` | raw N bytes | N | digests, fixed blobs |
| `String` | UTF-8, TLV frame | 0 (cold) | text, free-form labels |
| `Bytes` | raw bytes, TLV frame | 0 (cold) | opaque payloads |
| `Vocab<N>` | an N-byte id into that field's data vocabulary (`N` = `u8`…`u128`, default `u8`) | 1/2/4/8/16 | categorical values whose LABELS the data grows (an imported status column, a tag, an event name) |
| `VarInt<T>` | prefix-monotonic variable-length integer | 0 (cold), byte order = value order | integers that must sort in an index but often stay small |
| `Quant<P>` (bare `Quant` = P3) | fixed-point `i64` | 8 | decimals stored exactly |
| `Offset` + `#[ok_offset(base = N)]` | `u32` displacement from `base` | 4 | timestamps or monotonic values in a known window |
| `Reverse<T>` | bit-flipped `T` | width of `T` | descending order in an index without a second column |
| `Vector<T>` | `[count u32 BE][elements]` | 0 (cold) | homogeneous numeric lists (embeddings) |
| `Ref<D, K>` / `Refs<D, K>` | the child key bytes | key width / 0 | a document relation carried in the payload (`Embedded documents`) |

Three rules follow from the shapes:

- **A categorical value that arrives as a LABEL is `Vocab<N>`, never a Rust enum.** The label↔id mapping
lives in the collection's data vocabulary, so a dynamic writer can introduce a category and a dynamic reader can name it. `N` is the id width and the only knob: `Vocab<u8>` (the default) buys 256 labels per field at one byte per row, `Vocab<u16>` 65536 at two, and so on. Each field owns its own table, so the budget is per field. The id→label direction is the field's own cache on the collection's handle, so a label seen once costs one probe for the life of that handle, not one per document. There is no ordering — a field that must sort semantically declares a separate rank column. Consumer-side enum semantics (match arms, exhaustiveness) belong to the consumer: wrap the field in your own enum and convert at the edge.
- **`Option<T>` is not a field type.** Absence is a sentinel value chosen per
field; the derive has no `Option` arm (see the next section).
- **A `Vocab` field is interned on write.** `Collection::put` and
`save_with_prev_into` intern labels before encoding; `save_into` is encoding-only and therefore requires a document already interned through the collection. A full field vocabulary is a write error (`Error::VocabularyExhausted`) — narrow the domain, or model that value as `String`. The capacity is one short of the width: allocation needs the next id to exist so the cursor can advance, so `Vocab<u8>` holds 255 labels, not 256.

#### Case: a log table

High-volume append-only logs want the widest fields to pay for themselves. This declaration follows from the table above:

| Field | Type | Why |
|---|---|---|
| `ts` | `u64` | milliseconds since epoch: no wrapper, no conversion, and it sorts in an ordinary index |
| `level` | `u8` | 0–5 is a CODE, not a label — the consumer owns the mapping, and it stays one byte |
| `event` | `Vocab<u16>` | an open-ended label set (`http.request`, `db.timeout`, …) a dynamic writer can extend without a rebuild; `u16` because 256 event types run out fast |
| `module`, `path` | `Vocab<u16>` | same reasoning when the row carries a source module or an access path; omit the field entirely when it does not |
| `note` | `String` | free text, never a vocabulary — those values are not enumerable |
| everything else | the dynamic segment | shape varies per event type; the declared fields are the ones every query filters or groups on |

The rules behind it: put a field in the payload when you filter, sort or group on it AND its type is stable; make it categorical when the value is a label from a set that keeps growing; leave it dynamic when the shape varies per record. The vocabulary must be shared by the table, so it cannot live in the dynamic segment (those values are per row).

#### Case: a protocol enum is a code, not a vocabulary — k10r's `role`

k10r's channel log stores `role: u8` (0 user / 1 assistant / 2 tool / 3 system) and `kind: u8` beside `content: String`. Four stable values is the strongest temptation in the table above — it looks exactly like what a `Vocab<u8>` is for — and it is the case where the wrapper is wrong.

- **There is nothing to intern.** A vocabulary exists to turn a repeated STRING into a fixed-width id. `role` is already that id, and the wire width is one byte either way (`Vocab::WIDTH` = `N::WIDTH`), so the row saves nothing by wrapping it.
- **Both paths get more expensive.** A `Vocab` field is interned before encoding (a `VocabCache::id_for` lookup, an allocation on first sight, and a document that cannot be encoded until it has been interned) and dereferenced after decoding (`name_for`) — with one cache built per row per field. Every projection read would pay a lookup for a value the schema already owns.
- **The byte stops being semantic.** Today `role == 3` means system in every database, at a fixed hot offset, so anything reading rows outside the Rust API — an iceberg reader, a viz client, a `grep` over an export — decodes it with no side table. A vocabulary id is allocated in first-write order: the same logical role can be a different byte in two stores.
- **Allocating labels is policy.** An unknown code today surfaces as the caller's bug (k10r's projection answers `null` rather than guessing at it). A vocabulary would mint an id for `role: "assistent"` and absorb the typo into the table's domain.

So the boundary is not "is it categorical" — it is **where the value comes from**. A label from a set the DATA grows (an imported status column, a tag, an event name) needs `Vocab<N>`: the store must learn the label and hand it back. A code the CONTRACT fixes (a protocol enum, a level 0–5, a kind byte) is a number the schema already owns; wrapping it buys a lookup and a second source of truth. The giveaway is this question: could any writer legally send a label the schema does not know? If it cannot, there is nothing to intern.

#### Case: a queue as generations

A queue's retention is a boundary, and its design usually forgets it. aura's `MqData` is the worked example: key `[event_id][instance_key_id][seq]`, the sequence issued by a `HighWater` reduce, delivery a range scan past a cursor, depth a `Count` reduce — and compaction calls `delete_before`, which scans the slice prefix and purges row by row. Each purge is its own commit that also unfolds the `Count` reduce, so dropping consumed data costs O(n commits x (primary + reduce read-modify-write)), forever, on a table whose only visible change is shrinking.

The reframe is that a queue's address is **(generation, sequence)**, not "a row I may delete": a generation is a contiguous key prefix, so old data is a number.

| Table | Key | Value |
|---|---|---|
| `MqData` | `[event][instance][gen u16][seq u64]` | payload |
| `MqSliceHead` | `[event][instance]` | `{ gen, floor, high_water }` |
| `MqCursor` | `[event][instance][booth]` | `{ gen, seq }` |

`gen` before `seq` is the whole mechanism: one generation of one slice is one contiguous prefix.

- **Rotate** = the head update and the first record of the new generation in ONE
batch. One commit, so there is no window where a message exists and the head does not point at it.
- **Read** = from the cursor to the end of its generation, then the next generation
from its `floor`. Sequential across a boundary, no merge sort.
- **Depth** = `sum over live generations of (high_water - max(cursor, floor))`. The
`Count` reduce disappears, and with it a read-modify-write on every append.
- **Drop** = when every cursor has passed generation G, delete the prefix
`[event][instance][G]` in one batch (`Batch` + `commit_batch`; `prune_deprecated_slots` is the precedent).

No new annotation is needed and that is the point: `gen` is an ordinary key field, the head is an ordinary row, the cursor grows two fields, and rotation rides the batch that already exists.

What it does not buy: the drop still writes one tombstone per row. The *shape* changes — one commit instead of n, no reduce unfold, a contiguous range instead of deletes interleaved with appends. A drop that costs nothing needs an engine that can forget a prefix in one step, and none here can (fjall 3.1.10 has no partition drop, slatedb none either), which is why `okm-series`' parts are files: a file is the only unit that can be dropped in one syscall with no engine cooperation.

When not to: with few consumers and a small backlog, per-row `purge` is simpler and honest. The protocol costs a key field, a head point read and a generation-carrying cursor on every read and write.

### Optional fields: sentinel values, not `Option<T>` (ADR-0027 audit)

Declared `Option<T>` fields are **not supported** — the derive-recognition followup was closed without implementation (PLAN.md Phase 9, REJECTED 2026-09-20: no audited table needs it, and a fixed-width presence byte buys nothing over a plain field + sentinel). The idiom instead is **sentinel values drawn from the type's identity element**:

- `String` / `Bytes` cleared → the empty value (length 0 — the identity
of concatenation). Clearing = writing the identity, idempotent, no separate null marker.
- Timestamps (`updated_at`-style) never updated → `0` ("did not happen";
mudra's lifecycle fields `opened_at`/`closed_at`/`deleted_at` all use it, and rebinding re-clears `closed_at` back to 0 — the sentinel also carries the reopened state).
- "No parent" ids → `0` where real ids start at 1.
- Flags → `u8` 0/1, not `bool` (no payload `Option` in the wire;
2..=255 stays free for future states).

The cost is semantic overload: `""` cannot distinguish "explicitly cleared" from "never set". When a field genuinely needs that third state, the answer is **not** a nullable wrapper but an explicit enum (`Unset | Cleared | Set(v)`) — one extra tag byte, and the reader stops guessing. `Variant::Null` is already this shape on the dynamic segment: Null is a variant of the open vocabulary, not an `Option` wrapper.

Rule of thumb: if the field's empty state coincides with the type's identity element, use the sentinel; if "cleared" and "never set" must differ, declare a state enum — do not encode state in sentinels.

### Schema stability tests

Lock the physical bytes with hard-coded hex — any layout drift fails CI:

```rust
let e = UserToSession {
    user: Ref::ref_key(UserKey { org_id: 7, user_id: 101 }),
    session: Ref::ref_key(SessionKey { org_id: 7, session_id: 1001 }),
};
let fk = e.a_side_key(); // A-side (forward) entry: lives in User's ns
assert_eq!(&fk[..4], &[0, 110, 0x00, 0x00]); // the relation table's ns, primary face
assert_eq!(&fk[4..8], &7u32.to_be_bytes()); // local identity leads
let rk = e.b_side_key(); // B-side (reverse) entry: lives in Session's ns
assert_eq!(&rk[..4], &[0, 110, 0x06, 0x00]); // same ns, reverse face (a graph's 0x06)
// ... full layout assertions in okm-core/tests/integration.rs
```

## One-to-many relationships

Access methods are not only about ordering: **one-to-many relationships also land as prefix grouping**. A layout like `[tenant_id][org_id][user_id]` takes all users of one org by prefix scan — this is the physical form of the relational "foreign key + list".

In OKM it appears as an independent ns plus a secondary index. The index entry layout is `[ns 2B][slot][fields segment][primary-key prefix]`: `fields(org_id)` is the grouping/sort segment in the middle of the entry (payload fields encoded in declaration order, derive-generated, compile-time layout locked) and provides the grouping prefix; `includes(...)` is the entry value, copying frequently used fields so the scan needs no fetch-back. Note this is independent of the primary key layout: the grouping dimension lives in the index entry — composable, retirable, with no "nowhere to decode the suffix" problem. You could also store user data directly inside the list entries (the nested form; "denormalized storage" in SQL terms). Storing together is efficient but locks the user to the org — a job change moves data, the same trap as primary-key composition.

Against SQL normalization: the independent ns *is* the normalized form — org attributes like parent_id and org_name are stored once on the org row, not on user rows. Storing together (denormalized) is efficient but couples; apart (normalized) pays one indirection but keeps identity singular.

## Many-to-many relationships: a table

The access methods defined by `#[ok_index]` are **table-local** — their data source is the row's own payload, kept in sync automatically by `put` / `purge`. Cross-table relationships are facts between independent entities, and a payload index keeps to one table. One-to-many is carried by `Refs` (see "Embedded documents"); many-to-many is carried by **a table whose key is the pair** — the same shape SQL uses, because SQL has nothing else and neither does this model:

```text
#[derive(KeyEncode)]
struct MembershipKey { channel_id: u64, member_id: Uuid7 }

#[derive(DocumentEncode)]
#[ok_ns(110)]
#[ok_lead(by_member { fields(member_id) })]
struct Membership {
    nickname: String,      // what the relation KNOWS (ADR-0051)
    since_ms: u64,
}
```

The first key column is the forward direction: one prefix scan lists a channel's members, and the probe comes from the key's own builder — what comes back is the **decoded key**, not bytes:

```rust
for key in memberships.scan_keys(&MembershipKey::prefix().channel_id(id).finish()) { … }
```

The reverse direction is a declared **lead face** (ADR-0054): `by_member` orders the same rows by an identity the key already holds, so `by_member::prefix().member_id(id).finish()` scans "which channels is this person in" and `scan::<ByMember>(&probe)` hands back a `PrefixKey<MembershipKey>` per hit, plus the row. Its entry is `[ns][slot][member_id][channel_id][member_id]` — the segment is sliced out of the key's own bytes, so the row needs no column for it and the two can never disagree. The tail IS the **whole primary key** (`taken == 24`), so `decoded.channel_id` and `decoded.member_id` both read directly and the fetch-back is `scan`'s own work (`None` = the row went away between the scan and the read). `put` writes the row and every face in one batch; `purge` takes them away; `rebuild_faces` reproduces them from the rows. Nothing here is hand-kept.

For a relation table this is the DEFAULT shape of a reverse read, not one option among two: the identities are already in the key, so a face over them is free of payload and cannot drift. A copied column is the exception, for the shapes a lead face refuses — the next subsection lists them.

There is no junction carrier (ADR-0051). A relationship is data, and this model already has a carrier for data — a collection. What a junction would have had to build by hand is exactly what `put` and the declared faces do automatically:

- **attributes on the relationship** (`since_ms`, a per-channel `nickname`) are ordinary fields, indexable and exported like any others;
- **the reverse direction** is declared, not hand-written, and is rebuildable (a junction's second entry had no rebuild path);
- **one write verb pair** (`put` / `purge`) instead of `link` / `unlink` remembering to write twice;
- **the row travels**: it is in the changelog and the lake, which junction entries were excluded from.

The relationship's identity is the pair — at most one row per two endpoints — because the pair is the primary key. Adding or removing a membership is `put` / `purge` on that key; identity never moves.

#### The key carries more than a primary key

In SQL a column in the primary key is a **uniqueness decision**: adding one changes what "duplicate" means, so the key stays as narrow as the constraint allows and every other sorting dimension becomes a column plus an index. Here the key is an **address**. All it must guarantee is one row per address; uniqueness over the identities is a modeling choice (put them at the front), and every further dimension carries no constraint semantics at all.

A lead face turns that difference into a design allowance: any **immutable, identity-shaped** sorting dimension — a join time, a sequence number, the second endpoint, a tenant id — can live in the key, and a face re-reads the same bytes in any order without copying one field into the payload.

```text
[channel_id][joined_ms][member_id]      members of a channel, in join order
  forward:  prefix (channel_id) into joined_ms — a range read, no index at all
  by_member: fields(member_id)       → the reverse read, no member_id column
```

Both reads hit one stored fact — nothing copied, nothing to keep in sync. The boundary is immutability, and it is the same boundary identity has always had: a key segment cannot change, so a dimension that a writer will update (`status`, a counter, a score) cannot go in the key at all — changing it would move the row's address. Those stay payload columns with an `#[ok_index]`, which is the honest cost of mutability.

What a wider key costs is bytes and write locality, not correctness: the key appears in the primary entry and again as each face's tail, so when a face does not need the whole key the tail is truncated with `key(…)`. That makes the trade worth making deliberately — put in the key the immutable dimensions your scans ORDER by, not every field you happen to have.

The rule: immutable identity goes in the key and is ordered by lead faces; what the payload computes, derives, mutates or covers stays a column; a copied column survives only for the shapes a lead face refuses.

### When the reverse order has to come from a column

A lead face can only order by identity, and identity is fixed-width and ascending: `KeyEncode` refuses `Reverse` (descending), `String`/`VarInt` (variable width) and the wrapper codecs, and a lead face has neither `includes` nor `func`. When the reverse scan needs any of those, the endpoint is an ordinary **payload column** and the face is an ordinary `#[ok_index]` (ADR-0052):

```text
#[ok_index(by_member { fields(member_id), includes(nickname) })]
struct Membership {
    member_id: Uuid7,      // the column the reverse lookup scans
    nickname: String,
    since_ms: u64,
}
```

The entry then holds the column's bytes and, with `includes`, the covered fields — `[ns][slot][member_id][nickname] [channel_id][member_id]` — while `key(channel_id)` narrows the tail to the other endpoint (28 bytes, keys only, continue with a primary prefix read).

The price is the copy. That column is a second fact, and nothing in the model makes it equal to the key segment it repeats: a map write (`merge` / `replace`) can set it to something else while the key stays put, and the reverse scan then answers "rows whose column matched", not "rows whose identity matched". Take the column when the query needs what identity cannot express (a descending order, text, a computed value, covered fields); take the lead face when the question is "the same relation, read from the other end".

### The other key: an allocated id and two permutation indexes

The same table can be keyed by an allocated `id` instead of the pair, with each direction declared as its own index over both columns:

```text
#[derive(KeyEncode)]
struct MembershipKey { id: u64 }          // allocated by the caller

#[derive(DocumentEncode)]
#[ok_ns(110)]
#[ok_index(by_channel { fields(channel_id, member_id) })]
#[ok_index(by_member  { fields(member_id, channel_id) })]
struct Membership {
    channel_id: u64,      // endpoint, ordinary column
    member_id: Uuid7,     // endpoint, ordinary column
    nickname: String,     // what the relation KNOWS
    since_ms: u64,
}
```

Two orders of one field set are two declarations, not one declaration used twice. Each takes its own slot in declaration order (`0x3001`, `0x3002` here), its own keyspace, and its own typed probe builder — `by_channel::prefix().channel_id(id).finish()` and `by_member::prefix().member_id(id).finish()` cannot be confused, and neither direction reads the other's bytes. What the second declaration buys is exactly what a shared index could not give: the same rows under a different physical order, so "members of channel X ordered by member" and "channels of member X ordered by channel" are both one scan.

The trade, one property at a time:

**Uniqueness: free, or the caller's rule.** With the pair as the key, "at most one membership per pair" is the key's own guarantee and `put` is idempotent on it — no read before write, no race between two writers. With a surrogate key the pair is payload: two rows may carry the same `(channel_id, member_id)` and nothing objects. OKM has no unique index and no runtime uniqueness check (ADR-0005 names "declare those fields unique" as an escape whose cost is silent row loss), so deduplication becomes a read and then a write in the caller, with the window between them owned by the application.

**Entries: one per row, or two, and where the forward direction physically lives.** The pair-key row derives one entry (the reverse one); its forward direction is a prefix scan over the primary slot, so a channel's members sit contiguously in the table's own key order and enumerating them costs no index at all. The surrogate row derives two entries and its forward direction is a separate keyspace, so a `put` writes three entries instead of two, and the two directions of one relation no longer sit in one key range. In the example above the derived bytes go from 44 B per row (`[ns 2][slot 2][member_id 16][tail 24]`) to 72 B (two entries of `[ns 2][slot 2][24][tail 8]`).

**Payload: the surrogate stores each endpoint once.** In the pair-key shape the indexed endpoint must also be a column (ADR-0052: index fields come from the payload), so `member_id` exists twice per row and the two copies are trusted to agree. In the surrogate shape both endpoints are payload-only and the key is eight bytes of id — nothing is duplicated. This is the surrogate's one structural gain beyond the stable handle, and it is small next to the uniqueness loss.

**The handle is the actual reason to take it.** An `id` is an address for the *relation row itself*: another table can point at one membership (a `Refs` field holding `MembershipKey { id }`), a log can key by it, and editing `channel_id` does not move it. With the pair as the key, an external reference holds the pair, so re-pointing an endpoint is a delete plus an insert and every reference to the old address is stale. That is the right semantics when the relation's identity IS the pair ("the same membership, moved" is a different fact) and the wrong one when the relation row is an entity the rest of the model names.

**Choosing.** Take the pair as the key (ADR-0051) unless something outside the relation must name a single row of it; that requirement is what the surrogate buys, and it is paid for in dedup discipline plus one extra entry per row. Wanting both uniqueness and a stable handle does not make the surrogate right: the pair key plus a `Refs` field holding the pair keeps uniqueness, and a surrogate cannot carry it at all.

**Two rules the second declaration inherits.** Slots are positional, so declaration order is a persistent contract and the permutations are append-only — adding one later is fine, removing or reordering either is a rebuild (ADR-0005). And the variable-length rule is stricter here than on a single-field index: an endpoint stored as `String`/`VarInt` must be the **last** field of every declaration that names it, so one text endpoint admits only the permutation that puts it last, and two text endpoints admit no permutation pair at all. Identity endpoints are fixed-width by construction (`u64`, `Uuid7`, `[u8; 16]`), which is why this shape is safe for a junction and not a general replacement for the pair key.

### Refs or a relation table: deciding, and migrating when the answer changes

The two carriers answer different ownership questions — pick by asking which side needs set-shaped lookup:

- **"Which sessions does this user hold?"** — a one-directional need.
The relation is an attribute of the user row: `Refs` in field position, the user row is the single truth source, sessions live in their own ns and never point back. A sync-shaped mutation (an import pushes the whole list, a form submits the new set) is naturally served by the put diff — the field IS the state, overwrite-and-diff is already the mechanism.
- **"AND: which users follow this session?"** — both directions are
now first-class set queries. The relation has outgrown any single row: a user-side field cannot answer the session-side question without a full scan, so the fact needs its own table, read from both ends — a relation collection, written with `put`/`purge` as explicit events.

The second question arriving later is a **schema migration, not a retrofit**: promoting a `Refs` field to a relation table means (a) declare the collection keyed by the pair, with an index on the column the new direction scans; (b) backfill by walking the parent collection and `put`ting each row's current key set (a new table only sees writes made after it exists); (c) drop the field from the document in the same layout-version bump, since the field and the table would otherwise be two truth sources fighting over the same fact. Reverse migration applies when one endpoint's lookup need disappears — the reverse-scan demand was the only reason the fact left the row.

### Relation table or edge: the second question

Both carry a relationship, and both let you read it from either side. They answer different
questions, and the deciding one is what the relation KNOWS.

| | relation table | graph edge |
|---|---|---|
| Endpoints | two document types, fixed at compile time | `NodeRef`s — any collection, any mix, resolved at runtime |
| The relation's name | the collection: one table per relation | every edge carries a `predicate`, resolved through the graph's vocabulary |
| Identity | the endpoint pair — at most one row of that table between two nodes | `(src, dst, predicate, edge_id)` — parallel edges are distinct facts |
| Payload | ordinary fields: defaults, codec placement, indexes | a dynamic value with no declared schema |
| Reverse read | the declared index over the scanned column | the graph's `0x06`/`0x07` faces, both materialised |
| Cost | a table: one ns, one declared index | a graph: four faces, a predicate vocabulary, ref decoding |

So: **a relation table is a table about one kind of relation; an edge is a fact inside a graph.**
Use a table when the pair of types is closed and the relation is a fact in its own right
(membership, ownership, follow with a since-date). Use an edge when the node types are open,
the relation's name is data, several different relations connect the same two nodes, or the
facts need to be traversed (out/in, chains, timelines). A relation that starts closed and
later grows predicates or parallel facts is a migration to an edge, not a flag.

The two do not compete for the same storage: one is a collection and the other is a graph, and
they live in different namespaces.


## Graph edges: the third relation carrier

A relation table binds its many-to-many to **compile-time endpoint types**. A knowledge graph breaks that binding: one graph connects nodes of several types, the edge **predicate** is runtime data, parallel edges are distinct facts, and an edge carries attributes. The graph edge is the third relation carrier (ADR-0017) — independent identity, attributes, parallel edges, open endpoints, and a declaration that **is** the graph.

Nodes are ordinary document collections. When the type set is closed at compile time, a node's "type" is its collection (the ns is the type marker); when kind only arrives at runtime, kind is an indexed field inside one collection (the k10r case below). In both forms node-kind and attribute filtering go through that collection's own declared indexes and vocabularies — zero layout difference from any document collection. Everything new sits on the edge side:

```rust
use okm_core::{Graph, GraphEncode, NodeRef, NodeSet};

/// The graph's edge collection — the declaration **IS** this graph (ADR-0017 §4):
/// one ns, the edge's own payload (an ordinary document payload: hot/cold
/// placement, codecs generated in both directions), the attribute faces it
/// **declares explicitly**, and the node set it admits. Endpoints are not
/// declared one by one — a reference is self-describing (`[ns 2B][len][pkey]`) —
/// but "which collections may be nodes" is.
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(100)]
#[ok_nodes(User, Org)]                         // the member set: node TABLES, by type
#[ok_index(by_since { fields(since_year) })]   // faces are declared, never generated
struct Employment {
    since_year: u16,
    weight: u32,
}

// Generated by the derive — not hand-written, and nothing to keep in sync:
//   enum EmploymentNode { User, Org }
//   impl NodeSet for EmploymentNode { const NS_VALUES: &'static [u16] = &[10, 11]; }
// The enum is singular (graph name + `Node`): one type name names one value, and
// one value names one COLLECTION (membership = is that ns in the set); "these
// collections are members of this graph" is `NodeSet::NS_VALUES`. Each ns comes
// from that table's own `#[ok_ns]` — adding a member is one more type name in the
// edge declaration.

let mut g: Graph<_, Employment> = Graph::new(store);
let attrs = Employment { since_year: 2020, weight: 7 };
g.link(&EdgeHead {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    predicate: "employs".into(),   // the edge's type: a head segment + the graph's own vocabulary
}, &attrs, 1).unwrap();            // the payload encodes itself from the declaration (ADR-0017 §3)

g.typed_out("employs", &user1);   // face 0x08: predicate-filtered traversal
use __OkmIndex_Employment_by_since as BySince;   // the face type the declaration generated
g.by_attr_face(<BySince as KvIndex>::SLOT, &2020u16.to_be_bytes());
```

- **An endpoint reference** is `[ns 2B][len varint][pkey]` — the ns acts as the type marker, and the pkey width lives inside the reference as a one-byte varint. A document of any collection joins without a declaration: the same node is the same bytes (a traversal face hits an exact prefix), different pkey widths coexist, and there is no registry to declare, maintain or fail on. The graph edge retires the `Ref` discipline — the reference carries its own width, and its ns knowledge with it. The member set is the only thing declared, as a list of **tables** (`#[ok_nodes(User, Org)]`); the derive generates the `XxxNode` enum and the `NodeSet` impl from it, each ns read off that table's own declaration and applied as one check at write and one at decode. Width is still wire data; membership is a declaration.
- **Faces**: one `link` writes the primary table (fixed face 0x00, `edge_id` u64) + out/in traversal (0x06/0x07) + predicate-qualified traversal (0x08/0x09) + one face per **declared** `#[ok_index]` (counting family 0x3nnn), in one batch. Every traversal entry carries **both** endpoints (`[src][dst][edge_id]`, predicate first when qualified), so "this pair" is addressable and asserting identity is one seek; **there is no separate predicate index** — 0x08's bare `[predicate]` prefix is it. `unlink` decodes the edge body and deletes exactly those entries (declared faces are re-derived from the decoded payload, no prefix sweep); the all-dynamic form writes open attributes into the dynamic segment (fixed face 0x01, one entry per edge). Predicate names go through the **graph's own predicate vocabulary** (fixed faces 0x04/0x05, first sight gets the number) — separate from the payload's field-name vocabulary (0x02/0x03) and from any node field's vocabulary, so "list this graph's predicates" is one 0x04 prefix scan and never inferred by walking the index faces.
- **The handle declares nothing.** `Graph<S, E>` binds one engine instance and its per-instance state (the predicate vocabulary cache); the ns comes from the declaration and the member set from `E::Nodes` — the same shape as `Collection<S, K, R>`, which also takes only the engine.
- **Parallel edges** are separate edges: every link carries a caller-chosen `edge_id`, and `link` refuses a live id. Rewriting one edge in place is the only exception, and it has exactly one entry point: `unlink_into` + `link_into` fold the delete and the write of the same id into one batch, so at commit the edge was never absent and a second id is never born — k10r's functional arbitration and its counter write-back both go this way (ADR-0010's owed item). A deleted id is never reused.
- **Filtering**: walk the most selective face first and converge the ids in memory — no composite face is declared by default (`(*)-[predicate]->(:node kind)` = an 0x08 scan intersected with the node-kind face's pkey set).

The dividing line: `Refs` = one-to-many; a relation table = many-to-many, fixed endpoints, attributes the relation owns; **a graph edge** = independent identity, attributes, parallel edges, open endpoints.

### Predicates: an edge's type

The relation's name lives in the edge's **head**, beside the endpoints — `EdgeHead { src, dst, predicate }` — and it is the only one of the three with no compile-time declaration. This is the rung a relation table cannot reach: a relation table's type is its Rust struct (`PageTag`), fixed when the code is written; an edge's type is **data**.

- **Interned in the graph's own vocabulary**: name → `u16` id, append-only, first sight gets the number, never reordered. The predicate face and the traversal faces are keyed by that id, so every "filter by relation" read is a prefix scan on an id rather than a payload comparison.
- **Two directions, one vocabulary** (`name → id` and `id → name`): "which relations does this graph have" is one prefix scan, so the predicate set is enumerable metadata instead of a guess reconstructed from the edges.
- **Adding a relation is not a migration.** No per-predicate declaration, no code change, no ns per relation: one edge collection holds `employs`, `reports_to` and `blocked_by` at once. The id space is partitioned per graph, so a predicate can never collide with a node kind, a field name, or another graph's predicate.
- **A predicate is a relation, not an attribute.** Making `is_admin` a predicate that points at a `true` literal node is the classic failure shape; that flag is a field on the row. A predicate answers "how are these two things related"; "what is this node" belongs to the node's own declaration.
- **A node's KIND is a field, not the predicate vocabulary.** Kind is a categorical value in the payload — `kind: Vocab<u16>`, indexed by `by_kind` (ADR-0032) — the label set belongs to that field, the width is the declaration, and no further rule is needed. Predicate = the edge's type in the **head**; kind = the node's type in the **payload**. The field is named `kind`, not `kind_id`: under `Vocab<N>` the value **is** the label and the id is only its encoding, so an `_id` suffix would claim the caller is handling numbers.
- **Direction is part of the assertion.** `link` writes src → dst; the reverse relation is another predicate (or the same predicate with the endpoints swapped), and the library never derives it. Two edges over the same pair are two distinct assertions, told apart by their predicates and `edge_id` — which is what parallel edges are for.

**Why a `Vocab` field cannot replace it.** A `Vocab<N>` field interns a label, its faces are only the ones `#[ok_index]` declares, and it lives in the **payload**, behind the payload's own frames. A predicate cannot move there: the traversal faces are keyed by it (`[predicate_id][src NodeRef][edge_id]`, `[predicate_id][dst …]`), and the endpoints are **head** bytes rather than payload fields, so no payload index can produce those entries; the id also has to sit at an offset computable in the edge body, so `unlink` can delete an edge without decoding the payload first; and each edge has exactly one predicate, which is what lets it go straight into the key with no existence check. What the two share is the mechanism underneath — the same intern-style label ↔ id protocol (append-only, first sight gets the number, two faces) — so this is one primitive in two positions, not two mechanisms: a vocabulary owned by the graph's **key** versus one owned by a payload **field**.

### What belongs in a graph

Three modeling judgements sit on top of the mechanism — open endpoints make "put everything in the graph" easy, and these three are the lines that stop it:

- **One graph = one trust domain plus semantically related relations, merged rather than split.** A graph's boundary is its ns: one edge collection per graph (ADR-0017), **not** a global implicit edge pool (SurrealDB's `RELATE`, where every `RELATE` quietly joins one database-wide graph). PGQ's "merge, don't split" test carries over unchanged: same trust domain, semantically related relations = one graph; split into two only when the semantic domains are truly separate and never queried across. Predicates inside a graph are runtime data — one edge collection carries many relation types, which is PGQ's "one graph, many relations" position without its per-pair declaration cost.
- **Type and state apart: high-churn instance state does not enter the graph.** A task's template, preconditions and references are graph nodes (slow variables, worth traversing); its instance rows (`items`, `claimed_at`, short-lived, queried by exact enumeration) are an ordinary document collection. Approximate traversal is not the right carrier for exact enumeration — "search a memory" must not be mixed into "count a state". Application aggregate state that must be listed or counted exactly is a collection with an access method, not a node.
- **Across graphs: compose through `NodeRef`, not graph syntax.** A `NodeRef` is self-describing, so one document collection can take part in several graphs at once (PGQ's "a table belongs to several graphs", here several edge collections referencing the same nodes). Genuinely separate domains reach each other by collecting `NodeRef`s from one graph and probing another's faces, or by falling back to ordinary document reads — there is no cross-graph match syntax, and none is needed.

#### Case: a tag forest is rows plus a relation table, not a graph

mudra's tag forest is exactly the shape that invites a graph: several roots, no single root, one node living in several trees at once. It is built from two carriers, and neither is an edge collection.

```rust
#[derive(DocumentEncode, Clone, PartialEq, Eq, Debug, Default)]
#[ok_ref(TagKey)]
#[ok_ns(1)]
#[ok_index(by_parent { fields(parent_id) })]   // the tree: -1 = a root
#[ok_index(by_name { func(tag_name) })]        // bare-name addressing
struct Tag { parent_id: i32, name: String, rank: i32, deleted: u8, /* … */ }

/// A relation table (ADR-0051): the key is the pair, the row holds what the
/// relation owns.
#[derive(KeyEncode, Clone, Debug)]
#[ok_ns(110)]
struct PageTagKey { page_id: Uuid7, tag_id: i32 }

#[derive(DocumentEncode, Clone, Debug)]
#[ok_ref(PageTagKey)]
#[ok_ns(110)]
#[ok_index(by_tag { fields(tag_id) })]
struct PageTag { tag_id: i32 }
```

**The tree is a self-referencing row.** A tag carries `parent_id: i32` with `-1` as the root sentinel — "several roots" needs no virtual super-root; one scan on `by_parent` returns a node's children. The reverse ("who are my children") needs no edge rows at all, because it **is** that index. This is also where a tree and a many-to-many part company: a parent pointer's reverse is the index itself, while a many-to-many's reverse needs an index on another table.

Two consequences worth copying: the key stays proxy-shaped (`TagKey { id }`), so sibling order cannot ride key order — it is a `rank` payload field, sorted in memory after the scan; and there is no path or closure table, so a path is derived by walking parent pointers upward.

**Membership is a relation table.** page↔tag is many-to-many with endpoints fixed at compile time, so it is a collection keyed by the pair: one `put` writes the row and its index in one batch, both directions are first-class scans (primary-key prefix + the `by_tag` index), and the row still carries columns the relation owns.

**Why not a graph edge.** These edges have no attributes, no identity of their own, no parallel edges, and endpoints closed at compile time — not one of the four properties that make an edge the third carrier (ADR-0017) is used. A forest that uses none of them is "row pointers + a relation table"; forcing the third carrier onto it is precisely the "everything is a graph" failure the section above drew its line against.

One benefit worth stating in passing: a coarse-grained membership bit ("favorited" / "starred") therefore needs no new home — it is one edge to a `rank` child of an existing axis, so it needs no collection of its own.

#### Case: k10r's fact graph — the shape that earns the edge carrier

k10r (a memory service) stores what it extracts from conversations as facts: one fact = two nodes + one edge. An entity is one node collection (ns 36), a user another (ns 42), a fact an edge collection (ns 37) — three declarations, and every property the tag forest had no use for carries weight here.

```rust
/// Nodes: one collection, kind as a field.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[ok_ns(36)]
struct EntityKey { scope: u64, node_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_index(by_kind { fields(kind) })]                  // "every node of a kind"
#[ok_index(by_name { fields(name) })]                   // the exact entry point
#[ok_index(by_community { fields(community_id) })]      // bucket reads
struct EntityRow {
    kind: Vocab<u16>,       // the categorical field: its label is interned on write
    literal_num: i64,       // a literal's integer projection (0 otherwise)
    community_id: u64,      // a write-side bucketing result
    name: String,           // an attribute, never key material
    embedding: Vector<f32>, // no declared face: a vector has no key order
}

/// Users: a collection of a closed type — people are registered, not discovered.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[ok_ns(42)]
struct MemberKey { channel_id: u64, member_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MemberKey)]
#[ok_ns(42)]
struct MemberRow { display_name: String, role: u8 }  // no kind field, no vector

/// The Facts graph: one declaration **is** this graph.
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(37)]
#[ok_nodes(EntityRow, MemberRow)]                 // the member set: node TABLES
#[ok_index(by_since { fields(since) })]           // the time entry point
#[ok_index(by_scope_weight { fields(scope, weight) })] // the progressive-read entry point
struct Facts { /* scope, since, confidence, weight, three counters, two clocks, an 8-slot recent ring */ }

// The member set is GENERATED from that list: `FactsNode { EntityRow, MemberRow }` — one
// variant per node collection (membership = is that ns in the set), each ns read off that
// table's own `#[ok_ns]` (36, 42). The enum is singular (graph name + `Node`): one value
// names one collection, and "this group of collections is a member" is `NodeSet::NS_VALUES`.
// Membership is a compile-time fact, checked once at write and once at decode; the reference
// is still wire data. People and things live in different collections, inside one graph.
```

**Node kind is runtime data, so kind is a field, not a collection.** The ingestion side receives an open type vocabulary: no one has declared the kind a caller's fact tuple names — a term, a concept, a date. A collection, meanwhile, is a compile-time `#[ok_ns]` plus a derive. So every entity lives in one collection, and `kind: Vocab<u16>` is an ordinary declared field: its label is interned on write, `by_kind` indexes it, and a reader decodes the label back through that field's own vocabulary — no allocation code, no reserved face, no rules of its own, and width as the only knob (two bytes, 65536 kinds). The field is named `kind`, not `kind_id`: with `Vocab<N>` the value **is** the label and the id only its encoding, and an `_id` suffix would claim the caller is dealing with numbers. The test is: closed type set → one collection per type; open kind set → one collection, kind as an indexed `Vocab` field.

**The people in the domain are nodes too — and not the extracted kind.** k10r's users are a closed type, so they get a collection of their own (ns 42), and they show what the member set is for: this graph is not "the set of things I extracted". A user row is administered (a display name, a human or an agent), never extracted, and has no `embedding` — the landing index is built from the entity collection only, so a query never lands on a person. Retrieval lands on knowledge; people are entered by traversal, and the context header is one of those walks. A profile fact is `user -[PREFERS]-> iced_americano`: head in ns 42, tail in ns 36, one graph. A member names itself on the wire (`member-{channel}-{member}`), and the core resolves that into a reference — the string is never stored as a node name, so a rename moves no fact. The two node collections meet because a reference carries its own ns: no copying the identity row into the extracted collection, no union node type, and a rename touches one field on the row and no edge.

**A relation is the edge's predicate, so adding one is adding a name.** Ingestion hands over the relation's name (`PREFERS`, `RUNS`, `BLOCKED_BY`) and the graph interns it into a predicate id — no field, no enum, no declaration, no migration. One edge collection thus holds every relation this memory can express, and "which relations did this corpus use" is one scan of the graph's predicate vocabulary, not a walk over the edges. The twin on the node side is `kind`: the same label, the same interning, but it is a `Vocab` field in the node's payload — kind types the **node**, a predicate types the **assertion**.

**Attributes are the reason the edge exists.** A fact carries its own assertional context: `since` (the caller's clock), `confidence`, and the three counters and two access clocks the weighting system folds into `weight`; the relation's open props (cause, condition, strength, a per-assertion `functional` marker) land in the edge's dynamic segment. A relation table has no identity of its own, and none of this would have a place to sit.

**Recent access lives in a fixed-length ring, not in a counter.** A count answers "how many times" and two clocks answer "how recently", and neither answers "what shape does this distribution have" — while the decay bill is decided by exactly that distribution. So the row gains `recent: Vector<u32>` at its tail: 8 slots, second offsets from 2026-01-01, ascending, `0` meaning unused, shifted one position right on write, and an older row missing this frame is filled to 8 slots by the declared default. Why not `Offset`, not `Bytes`, not a cursor and not a wrapper is the case under "Homogeneous lists".

**Parallel edges are facts.** Two assertions between the same pair are two truths — different sources, different clocks, different confidence — so multi-valued is the default and each takes its own `edge_id`. A single-source-per-value relation is declared by **each assertion itself** (the `functional` prop), because dropping a fact is a net loss while an extra edge can be reclaimed. The arbitration is the mechanism's in-place rewrite: incumbent and challenger judge each other (`since >`, or `confidence >=`), and the winner's row is written with `unlink_into` + `link_into` in the same batch, along the **same** id — at commit the edge was never absent. Counter write-back goes the same way.

**Which side of the fact an attribute lands on is decided by what it describes.** Props describing the **relation** land on the edge; props describing the **entity** land in the node row's dynamic segment and merge — a prop of the same name takes the newer value, the rest survive. An attribute shared by several facts about one entity therefore has exactly one home.

**A tenant is not the graph's boundary.** `scope` (one user's corpus) leads the node key — `[scope 8B][node_id 8B]` — while the edge key carries no scope at all, because the edge collection builds its own keys. Scope rides on the **endpoint**: an entity's primary key carries it, and a traversal that starts at a scoped node stays inside that scope. The edge row keeps a redundant `scope` field for one reason only: a sort face can only be probed by what it encodes. The user node is the one endpoint **without** a scope (a channel belongs to one scope, so a member row is store-global), and it is the edge row's own copy that keeps such a fact inside the scope it was written in. One semantic domain = one graph; a partition **inside** a domain is a key prefix plus endpoints, never a second graph — splitting ns per tenant would copy the declarations, the predicate vocabulary and the code paths once per user.

**Not every relation is an edge.** A fact can name another fact's id in a prop's `ref` — one id, or a list — when that fact is not central enough to deserve a seat in the head. Nothing is validated: the reader decides whether to follow (`fact_in_scope`, one id), and a dangling link is the reader's 404. A pointer available on demand, never a subtree the writer forces in.

**Derived things do not enter the graph.** `embedding` is a `Vector<f32>` with no declared face — a vector has no key order — so search is okm-vector's `ExactIndex`: a derived index rebuilt from the table the vectors live in, invalidated by a write and rebuilt as a block, never maintained entry by entry (its home is a side table, see "A field too large", so a write of the primary table does not invalidate it). A face is declared only when there is a reader: two on the edge, `by_since` and `by_scope_weight`, each naming its scanning side. A face with no scanner is a write paid on every touch for nothing.

Every property the tag forest had no use for is load-bearing here: per-assertion identity, attributes on the assertion, parallel edges as distinct facts, and endpoints carrying their own ns — one person and one thing inside the same graph. That is the whole difference between reaching for the third carrier and misusing it.

## Set membership: an inverted index or a bloom filter, by cardinality

There is no dedicated `Set` type — membership is carried by two application-level paths, chosen by the set's cardinality:

**Low cardinality — a multi-value function index IS an inverted index.** `#[ok_index(by_tag { func(tags) })]` fans one row into one entry per element (`element -> pkey`), so "which rows contain X" is one prefix scan and "does row R contain X" is one point read — exact, no false positives, and the inverted face collapses duplicates through put semantics alone (same element + same pkey = same entry key). The truth source stays on the row (the func re-derives the entry set from it), and the purity contract keeps delete correct. Write cost is linear in set size — affordable at tens of elements, worth weighing at thousands. What that face answers and what it cannot is its own case below: *Inverted index: recall from the index, frequency from the row*.

**High cardinality — an application-level bloom filter, stored as a `Bytes` field.** When per-put fan-out is unacceptable (a 100k-element set would write 100k entries on every put), switch to a membership bitmap: k hashes set m bits, O(1) per put, at the price of false positives. The storage layer sees only opaque bytes — the hash family, the m/n ratio and the false-positive budget are all application parameters; carrying them into the codec would put a data-quality policy in the wrong layer. Two disciplines keep it in bounds:

- **Truth-source discipline**: a bloom is a lossy derivative — delete cannot unfold membership from it. It must travel beside a complete membership representation (a `Vector<T>` on the row, or the set stored off-row), and must never be the only record of membership.
- **Query semantics**: "possibly in the set" (verified through the complete representation when it matters) versus the inverted path's "definitely in the set". The two paths are complements, not substitutes.

**Why not a bitmap index.** The tempting third form is one entry per value holding a bitmap of the rows that carry it, so membership intersections become machine-word ANDs — exact, and with no ordering to establish. It is not available here, for the reason the same shape is rejected inside the index mechanism: an entry that many rows write is a cross-row read-modify-write. Every put and every purge would re-encode a value shared by all members of that value's group, which breaks the invariant that an entry's key and value are pure functions of the row that produced it (the deleted set is re-derived from the row, never from bookkeeping). It also needs row ordinals to address bits: a dense id space plus a translation back to keys — the surrogate id this document spends a section deleting. Its one unique win — order-free intersection — is real enough (`intersect` needs its inputs ordered, so a bitmap AND would not), but it is the smallest of the three considerations and it does not outweigh an entry that many rows must rewrite. Bloom filters stay because they are per-row — one opaque value written by that row's own writer — so they pay recall instead of write amplification.

## Inverted index: recall from the index, frequency from the row

The low-cardinality path above is the raw material of full-text search, and the entry layout decides the one thing that is easy to get wrong: **what recall gives you, and what it cannot**.

What the entries give — all of it structural, none of it a feature:

- the posting list for a token `t` is the prefix `[ns][slot][t]`, so one range scan returns every row that contains `t`, ordered by primary key;
- two posting lists for one token both run in primary-key order (the token is fixed, so the tail IS the key), so intersecting them is `intersect`'s shape exactly: a lazy single pass, no hash table, no per-row lookup, and no sort to pay because the order is already there;
- and the entries are readable without the payload at all (`scan_entries`), which is what makes that intersection cheap: keys first, rows afterwards, only for the survivors;
- `df(t)` is the length of that range, for free;
- "does row R contain t" is one point read.

What it cannot give is `tf`. Two occurrences of a token in one row produce the same entry key — same token, same tail — so `put` overwrites and the second is invisible. The mechanism that makes the entry idempotent and the delete pure is the same one that discards the count; set semantics are not an omission.

So the discipline is: **recall from the index, frequency from the row.** Posting lists decide the candidate set; ranking fetches each candidate's row (needed for the document length anyway) and counts occurrences in the text. The index answers "who might match", the row answers "how much". The cost sits in the ranking step, once per query, and never on the write path. The only quantity that needs a bound is the candidate set — a top-k by `df`, or a bounded range scan.

Storing `tf` anyway is possible, and it costs what "one fact, one place" costs everywhere else in this document:

- **A second table**, `(token, pkey) -> { tf }`, written in the same `Batch` as the row: `tf` in O(1), plus a second representation to delete, rebuild and keep in step — exactly the bookkeeping the declared index performs for free.
- **A count inside the segment**, `[token][tf u16][key tail]`. Mechanically legal (a variable-width field followed by a fixed-width one still leaves the segment boundary recoverable from the tail width), but it kills `scan_exact` — a probe can no longer name a whole segment, because it does not know the count — and the segment is not readable through `scan`, which returns `(decoded key, taken)` and drops the entry value. That is a new read surface in the core, not an extension of a declaration.

The text side stops at tokens too. `okm-ngram` supplies what is library work rather than store work: `ngrams(text, n)` — character n-grams, so CJK is one gram per two characters rather than byte noise — and `bm25_score`. Normalisation is the caller's, and it is not optional: `"are you ok"` at `n = 2` yields `"ar" "re" "e " " y" "yo" "ou" "u " " o" "ok"`, and the grams that straddle a word boundary are why real tokenizers exist — and why a whitespace tokenizer is the better default for words where n-grams are the better one for CJK. No positions are stored, so there are no phrase queries; n-grams buy substring recall, not phrase matching. Write cost is linear in the row's token count: tens is nothing, a thousand is a modeling decision.

## Homogeneous lists: `Vector<T>`

The plurality taxonomy (ADR-0015 §4) has a fourth member beside the two relation carriers and the heterogeneous `Variant::Array`: the **typed homogeneous list**. `Vector<f32>` declares a list of floats, `Vector<String>` a list of strings — the element type is schema, the length is data.

Storage is a **variable-length cold TLV frame** (the same placement as `String`): payload = `[count u32 BE]` plus the elements. Homogeneity is the value — a fixed-width scalar element is **bare V** (zero per-element overhead; a 384-dimension f32 embedding is 1.5 KB, against roughly 10 bytes per element for a heterogeneous array), and a dynamic-width element (`String`) is **LV** per element.

```rust
#[derive(DocumentEncode)]
#[ok_ref(DocKey)]
pub struct Doc {
    pub id: u64,
    pub embed: Vector<f32>,        // a cold frame, any length
    pub tags: Vector<String>,      // LV per element
    pub score: u32,
}
```

The length is data, not schema: swapping embedding models (384 → 768 dimensions) is just a different frame, with no wire migration. When the application does know the expected length (a contract-fixed embedding dimension), `#[ok_len(384)]` adds an encode-time check — the write boundary is where the promise is enforced; decode does not check (bypassing the decoder is the reader's own problem — raw frame bytes are as opaque as an unrendered image). The same contract is exported to dynamic readers (`FieldSchema::expect_len`) and enforced consistently on the Python side. It is an application contract, never a format constraint.

Elements are **pure values** — the identity line (in the many-to-many section above) keeps a `Vector` out of the relation carriers: a scalar has no key, and a key reference to it is a category error. One-to-many → `Refs`; many-to-many → a relation table; an ordered list of values → `Vector`; a heterogeneous dynamic list → `Variant::Array` (fixed face 0x01). `to_value` lifts a vector to `Variant::Array` — the dynamic layer has no vector type, a vector is a storage-layer format. A multi-dimensional shape is application interpretation (a flat sequence read row-major); nothing about it is stored on the wire.

The consumer side (okm-vector) layers search on top: an exact contiguous scan (the small-scale path, and the recall reference), an in-memory HNSW over quantized codes (int8 cosine or product-quantization codes), and shard-organized handles — one snapshot per shard under fixed face 0x0A, a cache, droppable at any time. The graph yields code-level distances; re-scoring against the stored f32 values is the caller's job. Storage stays byte-opaque: the corpus is still just those frame bytes.

#### Case: a fixed-length access ring lives in `Vector<u32>`, not in a counter

k10r's fact row has to answer a question a count cannot: are these 50 reads packed into one week, or spread over two years? The weighting system's decay is billed on exactly that distribution, and `read_count` plus "last access" leaves two numbers. The shape it calls for is a fixed-length 8-slot ring on the row:

```rust
pub struct Facts {
    // … hot segment: three counters + two access clocks …
    #[ok_default(::okm_core::Vector::new(vec![0u32; RECENT_SLOTS]))]
    #[ok_len(8)]
    pub recent: Vector<u32>,   // cold frame: the last 8 accesses, oldest first
}
```

Each slot is a **second** offset from `1_767_225_600` (2026-01-01T00:00:00Z); ascending, `0` = unused (that instant is never a real access, so it is a natural sentinel); a write shifts the whole ring one position right, so the tail is the last access. The window covers only the most recent 8, and older volume is still summarized by the counters — a fixed price paid in row width for fidelity, not an omission.

Four rejections, each of them the cheaper wrong answer:

- **Not `Offset`.** That is the spelling for scalar fields (`#[ok_offset(base)]` stores an i64 as a u32 displacement), while a `Vector`'s elements are only `f32/u32/i32/u64/i64/String` — the base could only live as a constant on the consumer side, with okm seeing a plain `u32`.
- **Not `Bytes`.** `Vector<u32>` already gives the zero-overhead bare-V layout for fixed-width elements, the `FieldType::Vector` column type, and the `#[ok_len]` / `expect_len` write-boundary contract; with opaque bytes all three would be rewritten in the application, and neither the dynamic reader nor the arrow bridge would understand it.
- **No cursor.** Every touch of `weight` changes the volume, so the whole row is re-encoded anyway and the 8 slots are written however they are arranged — shifting costs nothing; a cursor would only buy a rotated view on read and a `0` sentinel whose position is no longer fixed.
- **Not a wrapper.** The ring has no new wire shape (it is `[count][u32 × 8]`), and k, the base and the seconds unit are all application parameters — lifting them into the codec would make every consumer inherit clock semantics it never asked for, the same discipline as the bloom filter.

Adding the field needs no store wipe on its own: an old row missing this cold frame takes 8 zeros from `#[ok_default]`, and `#[ok_len(8)]` only constrains newly written rows. Whether the store is wiped is decided by whether this table has a written compatibility claim (ADR-0042), not by the codec's tolerance. The one trap to remember is on the application side: `Default` has to be written by hand, because the derived `Default` gives an empty `Vector` and the first encode panics at the write boundary.

Nor is it a candidate for "let the library count for me": a reduce is a cross-row GROUP-BY (its accumulator's unit is "how many rows are in the group"), and subscribe only emits writes as events and never changes a field. "One per access" is the caller's read-modify-write, and the single-writer discipline keeps it from interleaving.

## A field too large: the side table

One column's volume can exceed the rest of the row by orders of magnitude — a blob, a rendered document, an embedding. Left in the payload it pays both substrate constraints at once (whole-value read, whole-value write): every read of the row carries it, and a write of **any** field re-encodes it. The answer is not another storage position inside the row codec (ADR-0036) but a side table whose key type is the parent table's key type.

```rust
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct PostKey { pub id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_ns(1)]
#[ok_name("posts")]
pub struct Post { /* hot columns only: author_id, ts, title */ }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]          // the same key type: the key payload is byte-identical
#[ok_ns(44)]                // an ns of its own — that is the whole cost
#[ok_name("post_payloads")]
pub struct PostPayload { pub mime: String, pub body: Bytes }
```

| What you want | How |
|---|---|
| read one column | `payloads.get_row(&key)` — one point read; the row is never decoded |
| write one column | `payloads.put(&key, &payload)` — one row write; the hot row is never re-encoded |
| write both atomically | each collection's `save_with_prev_into` into one `Batch`, then one `commit_batch` (ADR-0003, ADR-0031) |
| the whole document | two reads, on demand — that is what the split bought |
| "is there a payload" | whether the side row is present; no `Option` field, no sentinel |

Deletion is the caller's: there is no cascade in this model (a relation table has the same shape), so the caller deletes the pair. The write pair is one commit; the delete pair is one commit as well, through `purge_into` into the same batch.

### When to split

Two triggers, either one is enough:

- **Large, and accessed on its own.** Downloads, thumbnails, re-computing one batch of embeddings — those readers want that column and not the row.
- **Asymmetric write frequency.** A row is written as a whole document (ADR-0035), so a write of **any** field re-encodes the large column. When the same row also holds something that changes faster — a counter, a clock, a status — the frequent writer pays for the large field every time, and the split is worth it even if every read wants both. The test is not "is it accessed on its own" but "is anything in this row changing faster than this column": k10r's node vectors (384 dimensions = 1.5 KB, 512 = 2 KB) sharing a row with a usage counter are exactly this case (k10r's own ADR-0019).

Do not split: every access reads the row anyway **and** nothing in it is written more often — that pays a point read for nothing, and the payload is the cheaper place.

### How to split

- **The key type is the parent's key type.** The key payload is therefore byte-identical and the pairing is pinned by the compiler; only the ns and the table name differ — the side table takes an ns of its own, and that is the whole cost.
- **Existence is the side row's presence.** No `Option` field and no zero-length sentinel: "is there one" is a state, not an absence.
- **Write the pair into one batch** (each collection's `save_with_prev_into`, one `commit_batch`) and delete the pair — there is no cascade in this model, so deletion is the caller's; `purge_into` puts the delete in one commit as well.
- **The parent row stays complete.** `R` is still the document's full declared field set; the side table is its second table, not its patch — so the document composition (a point read, `to_value`, `merge`) and `put` semantics do not change by a word.
- **The side table can have everything a table has**: its own declared faces, reduce groups, its own `#[ok_layout]` version, its own engine (any table may — see *The engine is an assembly-point choice*), and export/import for free as a `(K, R)` pair — because it is an ordinary table, not a position inside the row codec.

### The three shapes it is not

- **An off-row annotation inside the row codec** (`#[ok_offrow]`, ADR-0036): the value stays under the same row's key, only on another face, with a per-document tag enum as a field-level selector. The cost is a fifth storage position threaded through `FieldDesc`, `FieldSchema`, `split_names`, the four whole-document verbs and every walk of the derive, and it **forbids** indexing or reduce grouping on that column — all to save at most one round trip on a whole-document read.
- **Stuffing it into the dynamic segment (slot 1)**: slot 1 is rewritten whole, exactly like the row, so it isolates nothing; and it is the schema-free zone, not where a declared, typed column belongs.
- **A `Ref<D, K>` on the parent row**: that type says "the child document belongs to the parent" — identity, lifecycle and automatic dereference bound together — while what is wanted here is "two parts of one identity, each with its own writer". Four costs: `get` dereferences automatically, so every read of the parent also reads the large column (the read amplification comes back); `Ref::own` makes a parent write also write the child (the write coupling comes back); `purge` removes only the primary entry, the faces and the reduce ledger and never touches the child, so deleting the parent leaves the side row as an orphan; and the release on overwrite does not distinguish `own` from `ref_key`, so it deletes a child that other parents still reference.

### Indexes and derivatives

A table's index entries are derived from the **whole row** and live and die with it, so writing a row recomputes the new row's entire entry set and writes it; `merge` / `replace` then use the old row to delete the entries it no longer produces, and `put` leaves dangling entries to `purge`. There is no "which field changed, so which faces change" mapping, because no write path writes a single field — the off-row shape is precisely what needs that mapping.

The side table's indexes are its own: derived from the side table only, so a side-table write does not disturb the parent's indexes and a parent write does not disturb the side table's. A column with no key order and therefore no declared face (a vector) goes further: its retrieval index is derived state, rebuilt as a whole from the table it sits in, invalidated by a write and rebuilt as a block, never maintained entry by entry.

## Cross-row pre-aggregation

Every index entry is an append-only derived view that lives and dies with the row — purge re-derives the removal set with the same function, so nothing ever dangles. A class of needs falls outside that discipline by nature: a post count per author, an hourly rollup, an exact counter. They are **cross-row** — the func signature `fn(&Row)` sees one row — so the answer lands in a **read-modify-write** of the entry value: the fourth primitive (a mutable aggregate entry).

okm-core's position on it is a two-layer split: the core builds in no aggregation semantics (no built-in counter type, no distributed accumulation protocol), while the mechanical half is provided by the derive, declared exactly like an index:

```text
#[ok_reduce(AuthorStats { group(author_id) })]
```

`group(...)` takes the group segment from row fields (entry = `[ns][slot][group segment]`; the slot comes from the reduce family's own counter, independent of the index counter, ADR-0030); `AuthorStats` is a user type implementing `ReduceLogic` — `Acc` (the accumulator type, implementing `ReduceCodec` for fixed-width BE encoding) + `fold(acc, &row)` (on put) + `unfold(acc, &row)` (on delete). The write path does the read-modify-write automatically: read the current acc, fold/unfold, write it back. On the read side `reduce_get` takes one group and `scan_reduces` scans every group. Mechanism details (ledger invariants, the unfold compensation on overwrite, write-path ordering) live in the internals' [reduce mechanism](internals/reduce-mechanism.zh-CN.md).

Two disciplines of use:

- **Reversibility is the contract**: `unfold(fold(a,x)) = a` must hold exactly. count/sum can; median/distinct cannot — an irreversible aggregate belongs in an OLAP system beside this one. A compound acc (count+sum for a mean) implements `ReduceCodec` directly; okm-core only stores and fetches bytes.
- **The single-writer boundary**: the hook is a read-modify-write, safe under a single-writer engine; multi-writer races and distributed accumulation protocols are outside this model (see the integration doc).

Zero-value reclamation is deliberately not done: an emptied group keeps its entry (the acc returns to the identity), which saves the whole tombstone logic; a caller skips identity groups as it likes.

## Grouping is maintained, not queried

There is no query planner here and no runtime aggregation. A query is a key range you chose, and the engine answers with what the bytes say; nothing on the read path groups, folds or counts. An aggregate therefore takes one of two forms — a ledger the write path maintains, or a scan whose result you fold yourself ([pre-aggregation](#cross-row-pre-aggregation)) — and the set operators do not add a third: `intersect` and `difference` put **selection** on the read path, over streams already ordered, while aggregation stays on the write path.

Grouping is the maintained half. `#[ok_reduce(AuthorStats { group(author_id) })]` makes the group part of the entry's address — the entry key is `[ns][slot][group segment]` — and `put` performs the read-modify-write: read the accumulator, unfold the old row's contribution, fold the new one's, write it back; `purge` runs the same path on its unfold branch. So a group is a stored answer at a known key: `reduce_get` fetches it, `scan_reduces` walks every group, and reading it costs a point read rather than a pass over the rows. What the ledger does not hold is a member list — there is no record in the KV of which rows contributed — so "which rows are in group X" stays a question about rows, answered by the key layout (put the group field first in the index `fields` and one group is one contiguous run) or by a scan. The invariants that keep the ledger equal to the row set, chiefly the unfold on an overwrite, are in the internals' [reduce mechanism](internals/reduce-mechanism.zh-CN.md).

`group_by` in `okm-query` is a different thing under a similar name: it folds consecutive runs of an iterator **already in hand**, taking the group key from a closure and returning a `Vec` of `(group, acc)`. It folds rows that were already read — ordered input is what makes it a single pass — so it can shape a small result you have just produced (the survivors of an intersection, a page you fetched), but it cannot answer "which rows are in group X", because until those rows are read it has no input at all. It is the tail of a read already paid for, not a substitute for the maintained view.

## Reading one reduce

The write path maintains the ledger; the read side is two functions, and both take a **type marker** rather than a slot number — the derive generates one marker per `#[ok_reduce]` declaration in declaration order, carrying that reduce's slot, its group fields and its group encoder. The marker is named `__OkmReduce_<Row>_<n>`: the row type it belongs to, and the declaration's index.

```text
reduce_get::<Store, __OkmReduce_Post_0>(store, ns_prefix, &key, &row) -> Result<Option<Acc>>
scan_reduces::<Store, __OkmReduce_Post_0>(store, ns_prefix)           -> Result<Vec<(group segment, Acc)>>
```

- **The entry key is `[ns][slot][group segment]`** — the group IS the identity, with no key tail. The last two arguments (`&key`, `&row`) do exactly one thing: encode the group segment's bytes (group field names in declaration order, each encoded from its own source — the two-source rule). To read, pass a value that carries those group fields.
- **`None` is a real answer**: the group was never created. No state is garbage-collected — an emptied group leaves its entry at the accumulator's identity value — so distinguishing "absent" from "empty" is the caller's policy, not the core's.
- **Slots are allocated by family and only ever appended**: a reduce's slot comes from the reduce family (`0x4nnn`), independent of the index counter and never reused — so a declaration's slot is stable for the table's lifetime, and two reduces on the same table are told apart by it.
- **`scan_reduces` returns the group segment, not a decoded key** (ADR-0024): the field widths are in the caller's hands and it can decode them; the read-side helper deliberately does not hand back a typed key.

## Two read-modify-writes: reduce and `upsert_with`

Beside reduce, the write path has an imperative half — `Collection::upsert_with(key, f)`: read the old row, let the closure compute the new value, go through the normal put. The two read-modify-writes share one underlying shape (read → compute → write) and split the work by "who knows the logic":

- **reduce (declarative)**: `#[ok_reduce(Logic { group(f) })]` is fixed at compile time — which rows land in which group and how fold/unfold computes are part of the type declaration, and the framework drives it. Fits aggregates that evolve with the row's structure (counts, sums).
- **upsert_with (imperative)**: `f(Option<R>) -> R` receives the old row at runtime, arbitrary logic. Fits updates only the caller knows (a balance adjustment, a conditional patch). `None` = the key does not exist (the insert path).

Both share one write path (put), so index maintenance, reduce folding and event emission all fire as usual, with no special case. They also share one correctness boundary: **single writer**. OKM is an in-process library with one holder per collection and verbs taking `&self`, so the type carries the boundary: the epoch counter is the only mutable state a write touches, it is a `Cell`, and the collection is therefore `!Sync`, so a second holder cannot step in between get → f → put. No CAS is needed, which is the same property that makes reduce exactly-once. A multi-writer future (optimistic CAS) is a different mechanism, outside this model. The unfold compensation on an overwrite happens inside put; `upsert_with` keeps no separate books (see the internals' reduce mechanism).

## Write-path events: reduce, subscribe, and the change log

A committed write can be consumed in three ways, and the choice is not "who runs first" — it is **where the consumer's code lives**.

| | `reduce` | `subscribe` | change log |
|---|---|---|---|
| the consumer's code | inside the write; `fold` is part of it | same process, registered at one point | another process; a reader loop |
| what arrives | one event per row, the typed document | `op`, `epoch`, the key, the decoded document | bytes: `op`, key, value |
| why it exists | cross-row aggregation | decoupling — the reaction is written once | replay, migration, another format |
| a lost event costs | a wrong ledger (corruption) | one resync | one reconciliation scan |
| cost on the write | one read-modify-write | one atomic load, zero with no sink | one memcpy, plus files and retention |

The first two are the mechanisms (ADR-0008); the log is a directory of files plus a reading convention, not a third mechanism (ADR-0045 §10). The line an application writes itself right after `put` is the fourth option and needs no surface at all — it is covered at the end of this section.

**`reduce`** (`#[ok_reduce]`) runs inside the write path: `fold` IS the write's own bookkeeping, so the accumulator answers "what does the accumulated state look like", grouped. Its reversibility contract (`unfold(fold(a,x)) = a`) only means anything under exactly-once, which is why a reduce never consumes a subscription.

**`subscribe`** (`#[ok_subscribe]`) is about decoupling, not delivery. Without this interface, "when this data changes, do something" is copied into every place that changes the data — one copy per call site, silently missing wherever someone forgets — or the reaction becomes a reader of the change log: open the segments, decode them against a sidecar, keep a watermark, and only then react. The declaration names the topic, the consumer registers one sink, and the reaction is written once; the code that writes the data does not know who is listening. That is the observer pattern, and the name records it: the mechanism was renamed from a channel to a subscription because it is an interface to register at, not a transport. Delivery and latency do differ from the log (best-effort vs at-most-once, microseconds vs milliseconds), but that is rarely the point: nobody notices in normal use, and a lost event can be reconciled.

```rust
// Declaration: bare — this row type's write events enter the subscription;
// the event enum (variant = the row type's name, `RowEvent` by default) is
// collected and generated by build.rs
#[derive(DocumentEncode, Clone, Debug)]
#[ok_ref(AccountKey)]
#[ok_subscribe]                        // Account now emits Put/Delete events
#[ok_ns(21)]
pub struct Account {
    pub balance: u64,
}

// Consuming: one sink registered at assembly time
// (tokio mpsc, crossbeam, a no-op — the transport is not the model's business)
okm_subscribe::SINK_ROWEVENT.register(move |ev: okm_subscribe::RowEvent| {
    let okm_subscribe::RowEvent::Account(e) = ev else { return false };
    index_sync(e.op, &e.key, &e.document);   // e.g. search-index sync:
    e.epoch == last_epoch(e.key)             //   lossy, resyncable
});
```

What the consumer receives: `op` (Put/Delete), the row's `epoch` (the emitting collection's monotonic write counter — an exact same-table ordering boundary), the key, and the full row snapshot. `register` returns nothing, and delivery is best-effort: a full queue drops, a missing sink drops, and the write path never notices. That is the contract — a consumer that cannot tolerate a dropped event belongs inside the write (reduce), not here.

Which writes announce: `put`, `upsert_with`, `merge` and `replace` announce the row they leave (`Put`), and `purge` — or a `replace` whose map names no declared field — announces the row it takes (`Delete`). Silent by construction: a `merge` / `replace` that only touches the dynamic segment (there is no row for the event to carry), and every batch writer (`save_with_prev_into` / `commit_batch`) — a batch owns its own events (ADR-0031 §5), and its durable face is the change log (ADR-0047).

The stream has two properties independent of the consumer: events carry a monotonically increasing **write-batch epoch** (same-table events therefore carry an exact batch boundary, and a combinator can fold exactly to it, glitch-free rather than by a debounce heuristic), and it only means anything in-process — it is not persisted, it resets on restart, and it offers no cross-table order (independent puts have no atomic "both updated" instant, so a multi-table fan-in is eventually consistent by construction).

**The change log** (ADR-0045) is the opposite shape: files under one directory, appended only after the commit returned `Ok`, read by another process — `okm-drain` for a lake, a replay tool for a migration. It has no registration point: a consumer there owns a reader loop, and what it gets is bytes (`op`, key, value), decoded against the schema sidecar beside the segments. Choose it when the consumer lives in another process, or when the record must survive a restart and be replayed; it is also where a batch's writes are visible, since a batch announces nothing in-process (ADR-0047).

**The fourth option** is the code an application writes itself, right after the write returns — no mechanism, no declaration surface. It runs inside the caller's write sequence, at most once, with no transaction boundary and no undo: "already notified" and "already wrote the other table" cannot be taken back, and a failure neither rolls back the write nor earns a retry. Nothing that must happen exactly once should hang off it.

```rust
// The caller's own put wrapper: the row write stays exactly-once, the
// notification goes out beside it, best-effort, never retried.
fn save_config(store: &mut impl VirtualStorage, cfg: &Config) -> Result<()> {
    let t = Collection::new(store.shared_handle());
    t.put(&cfg.key(), cfg)?;                                // the write: exactly once
    let _ = METRICS_GAUGE.set(cfg.name(), cfg.ttl_secs());  // side effect: at most once
    let _ = notifier.send(cfg.name());                      // side effect: fire and forget
    Ok(())
}
```

The line between this and a subscriber: the former runs **inside the caller's write sequence** (its latency counts against the write, its panic propagates); the latter runs **after the write returns**. If it must observe every write under any load, it has to be a sink consumer with a durable supply, not a few lines beside `put`.

**Choosing** is two questions: does the consumer live in the same process, and does it need the decoded document? Yes to both — subscribe. Otherwise — the change log. If the reaction must be exact, neither is right: it belongs inside the write (reduce).

Two rules of use apply, and both are about subscribe:

- **Correctness never rides subscribe.** A full queue drops, and a missing sink drops silently; anything that must happen exactly once (a reduce, say) belongs inside the write. Subscribe's place is the consumer that tolerates loss and can reconcile afterwards.
- **The purity constraint applies to the event payload too**: the row snapshot travels as it is; deriving extra context at consumption time is re-reading a store the event no longer promises to describe.

Transport is an assembly-point decision (`SinkCell::register` accepts any sink — a tokio mpsc, a crossbeam queue, a no-op); the core stays synchronous and knows no executor. Combinators over the receiver (map/filter/merge, folding to an epoch boundary) are the stream layer's job, not the model's.

## Read paths: a key, a declared face, or a point

A query takes one of exactly three doors, and which one is a modeling decision rather than the optimizer's: `get` (single-key point read), `scan_keys(probe)` (primary-key prefix), `scan::<I>` (a named access method). SQL gives you one door — the query — and lets the optimizer pick the access path underneath; here the access path **is** the declaration: the order a prefix read needs is already the storage order, so a grouping that lives in the key costs nothing extra to read, and a grouping that does not must be declared as a face.

That is the precise meaning of "access methods are mandatory": they are mandatory for the dimensions **not in the key**. When a predicate falls on a leading key segment, `scan_keys(&DeviceKey::prefix().org(7).finish())` is the answer — one prefix scan, zero declarations, and the probe names fields in declaration order, so inserting a field fails to compile at the call site instead of silently rebinding to other bytes. Every other dimension must declare an `#[ok_index]`, because there is no general full-table query surface: a predicate no door can answer means a declaration is missing, not that a `.filter()` should be added.

The index side mirrors the primary side exactly (ADR-0049): `ByOrg::prefix().org_id(7).finish()` builds an `IndexProbe<ByOrg>` in the order of that access method's declared `fields(...)`, and `finish()` at any depth is a legal probe — the depth IS the prefix depth. The index's marker type pins those bytes to one access method, so an `org` probe cannot be spent on `by_label`. Raw encoded bytes are still accepted (`ProbeBytes`), for the case where a probe is assembled from data rather than from the declaration.

### What this model gives up

Prefix reads are less abstract than SQL predicates, and less flexible. This section exists so that a reader need not discover that during a data migration.

SQL's abstraction is: the predicate names only the result, and the optimizer finds the path. Many engines do not honour that promise, and they break it in the most useless way — not with an error, but by turning physical layout into part of the query. Wherever an index is order-sensitive (MySQL's composite indexes are the standard example), the left-prefix rule means whether `INDEX(a, b, c)` can serve `WHERE b = 1` depends on the field order chosen when the table was created. Developers end up maintaining a picture of a B+ tree's shape inside a declarative language, with field order decided by query usability rather than by meaning. That is an abstraction leak, not an abstraction, and the cost is paid both in flexibility and in intuition.

This model takes the other fork: there is no optimizer, so there is no promise to break. The key's layout is the read path, a prefix probe is the WHERE, and the cost is exact — a predicate the declaration cannot answer does not exist rather than being "unoptimized"; adding a dimension means changing the declaration, rebuilding the face and migrating the data; access patterns are fixed at modeling time instead of per query.

What that buys is the other half of the same ledger: read paths are predictable (no optimizer, and no surprises from stale statistics); cost is reasoned about rather than guessed (a prefix hit is a range hit, and one scan costs the range's size times the per-entry cost); and the semantics are honest (no query that reads like a filter and runs like a full scan).

The remaining raw door, `scan_documents_raw`, is the maintenance surface for the snapshot exporter and the arrow bridge, not an application query path.

Enumeration has two modes, because a document is **two slots** (ADR-0035): `scan_keys` enumerates **rows** (one prefix scan — a document with only a segment is not there), while `scan_document_keys` enumerates **documents** (the row slot and the segment slot merged in memory, which is also why a `put` carrying no declared name shows up). Materialization belongs to neither mode — a key becomes a document with a point read plus the caller's composition; and the merge cannot be an adapter, because an adapter cannot emit something that was never scanned.

Both doors take a prefix probe, and a full-table scan is the **empty probe** — there is no separate full-table door any more (ADR-0049). A composite primary key's leading segments are themselves a read path, not just an address: `scan_keys(&DeviceKey::prefix().org(7).user(11).finish())` answers "all devices of this user" with one prefix scan, declaring no access method. The probe is typed and ordered — the derive emits one builder method per depth, and fields can only be given in declaration order; inserting a field between two others fails to compile at the call site instead of quietly constraining other bytes. `finish()` at any depth is a legal probe, and `PrefixProbe::empty()` constrains nothing. `scan_keys` is lazy — `.take(n)` is a LIMIT and you pay only for reads that execute; `.skip(m).take(n)` is paging; `.rev()` walks the same window backwards — while `scan_document_keys` stays eager, because merging two slots needs both sides in hand. The index-side dual is `scan_prefix_iter::<I>(encoded)`, the streaming form of `scan`, with identical entries and fetch-back.

This door answers one shape only: read by key **prefix**. A dimension that is not a leading segment of the primary key does not become queryable through it — that is a key-layout question (move the dimension forward) or an access-method question; if a segment must stay in the key and also be read in another shape, that is the mirror workaround, and its retirement condition is an `#[ok_index]` that can name a key segment.

## A join is set algebra: `merge`, `intersect`, `difference`

There is no join operator here, and no join planner. An index answers a set of keys in key order; a join is the composition of two ordinary reads — intersect the key streams the indexes answer, then fetch a row for each surviving key. The set step is a trait, not a verb added to the storage API: `OrderedSets` in `okm-query` puts `merge`, `intersect` and `difference` on anything that yields keys (`Vec`, array, a scan iterator), and each is an adapter over a stream already ordered by the join key.

`merge` is the general alignment: it walks both streams by key and answers one tuple per distinct key — `(Some, None)`, `(Some, Some)`, `(None, Some)`, in key order, and never `(None, None)`, because an alignment only reports keys that exist. That one pass is a full outer join: three answers for both sides, plus "what each side has that the other does not", with no second operator. `intersect` and `difference` are this stream's two named filters, named because those two questions are asked far more often than "all three cases".

Both are set-level, not multiset-level: a key that either side repeats is answered once, with one representative per side, and multiplicity is left to the step that reads rows. They are lazy and allocate nothing, need no `Clone` on the key, and `take(1)` consumes only what one answer needs.

Chaining is the composition, and any number of sides works, because the item stays a key: `a.intersect(b)`, `a.intersect(b).intersect(c)`, `a.intersect(b).difference(c)`. Direction lives in the receiver — `a.difference(b)` is `a \ b`, the same set as `a − (a ∩ b)`, reached without computing the intersection.

**Outer joins are compositions, not operators.** Left-only is `a.difference(b)`, right-only is `b.difference(a)`, both at once is `merge` — or the two differences side by side, which is the same answer partitioned. For N sides, intersect once and subtract per side.

**m×n belongs to the step after.** Set algebra works at key level, so a repeated key answers once; the expansion happens when rows are fetched for a surviving key, where the loop over that key's entries really is m rows against n. There is no pair-expanding operator beside it, and that is deliberate: `intersect` answers the key set and one fetch per survivor does the rest, which beats materializing both sides unless nearly every key is shared. The one-pass alternative is `merge` — one tuple per distinct key, `(Some, Some)` being the key that meets — and the caller expands that tuple into pairs in its own loop, which is also where the rows are read. A dedicated join would be a second semantics for the same thing, and it is the shape that carries k² copies for two runs of length k.

**Ordered input is a contract, not a runtime check.** Verifying it would mean reading a stream to its end before answering with it — exactly the materialization these adapters exist to avoid. So unordered input does not raise: it silently misses keys. The contract is free to meet where it matters: an index scan's order is the entry tuple `(fields…, key)`, so a projection onto the leading fields is still ordered, and a join on the first declared field needs no sort. When the join dimension is not the leading field, sort explicitly and the sort stays visible in the caller.

```rust
// a, b, c: key streams already ordered by the join field — an index scan
// projected onto it, or any Vec/array of the same key type.
let shared: Vec<u32> = a.clone().intersect(b.clone()).collect();       // a ∩ b
let only_left: Vec<u32> = a.difference(b.clone()).collect();           // a \ b
let both_but_not_c: Vec<u32> = a.intersect(b).difference(c).collect(); // (a ∩ b) \ c
```

Because the join is those two steps and nothing else, its result is ordinary code — a vector to go on folding, a prefix of it, a key set counted before any payload is read — rather than a restricted join form or a query string with a planner behind it. The only restriction is the key layout, and that was settled at declaration. The cookbook form, with the scans in place and the fetch loop spelled out, is in the [query guide](query-recipes.md), "The JOIN recipe: intersect keys, then fetch".

## Key naming: a plain prefix plus composed access methods

A key is three segments: `[ns][pkey...][order]`. The sort field goes at the tail (a u64 big-endian timestamp is the byte order of time order, a timeline by nature): entries under one pkey are physically adjacent and ordered, and a prefix scan IS the timeline / range read — restoring a session, taking the latest N, are one `scan` each. The primary sort field going into the key is a physical decision; other sort dimensions stay out of the key, live in the payload, and surface through `#[ok_index]`.

Fancy prefix schemes (path-like, semantically segmented prefixes) are not advised — the longer the prefix, the harder it is to manage, and every prefix is an encoding convention to maintain. Declare more access methods instead: each `#[ok_index]` is an independent, composable, extensible read path; declaration is registration, and deleting the line retires it.

**A sort key is worth considering only when the query result is plural.** The value of the `[order]` segment is to pull "a group of related entries" into a physically adjacent, ordered run that one prefix scan returns whole; a single result (a point read) uses `get`, and a sort segment in the key buys nothing.

A sort segment inside a composite key also needs thinking through. Start from the sort key's physical nature: **a sort key like time is highly selective and groups nothing** — its values are nearly all distinct, so any field placed behind it is already "one group per row" and ordering inside a group is meaningless. Adding fields to the key behind a highly selective sort key therefore buys nothing; it can only add scan cost, never a read path.

Anti-pattern: `[session_id][user_id][time]` (session messages).

- `user_id` before `time` orders entries "grouped by user, then by time within the group" — there is generally no query that groups by user inside one session, so the `user_id` segment yields no read path.
- The cost is certain: even reading every message (the most ordinary session restore) does not come back in time order, so the reader sorts again; taking the latest N means scanning the whole group and then sorting.
- Genuine occasional filtering by user is a full scan plus a filter, not worth a key-layout change.

The correct shape is `[ns][session_id][time]`: one sort key after the pkey, and session restore, paging and range reads are each one prefix scan.

Generalized: **`[pkey][sort field][remaining identifier]` — a remaining-identifier tail is also an anti-pattern.** Stuffing a dimension that "also needs a query or a group by X" into the key tail is, in essence, maintaining an access method by hand inside the key. A hand-maintained key prefix can buy real performance in one specific scenario, but:

- **It depends on the scenario**: the key layout freezes one read path. Once the business grows, most tables need several access methods, and optimizing for one of them is worth little.
- **It depends on the business logic itself**: whether the optimization holds depends on the business happening to support that read pattern, which no modeling technique can decide.
- **The cost-benefit is disproportionate**: high development cost, and little performance gain (the quantification is in "Design constraints on performance" below).
- **The suffix has nowhere to decode**: a custom segment at the key tail needs a matching decoder, which OKM explicitly does not support — the `KeyEncode` derive decodes only the declared full key (`KEY_LEN` locked at compile time), and `PrefixKey` decodes only the truncated identity prefix (beyond the prefix, bytes are garbage by contract). Hand-writing offset arithmetic to decode a suffix is complex and error-prone, and throws away exactly what OKM is for: zero-cost encode and decode with a compile-time-locked layout.

Every such dimension belongs to an access method / secondary index: `#[ok_index]` is declaration-as-registration, composable, extensible, appended as the business grows, with no change to the primary key layout.

## Open vocabularies: a name to a proxy id

A key segment is a fixed-width byte identity — `KeyEncode` rejects `String` at compile time. A name arriving from outside the schema (a user id, a tenant label, an entity kind, a routing value) has no fixed width and a cardinality equal to the data: it can neither be declared as a key field nor given a compile-time constant per value. Three pieces turn it into a key:

```text
row      [ns][id u32]                    → name + payload
index    by_name { fields(name) }        → name → id (the string lives only here)
reduce   HighWater(id) { group(global) } → id issuance (point-read the water mark, +1)
```

- **The row is keyed by a proxy id, with the name as a payload field.** The name is the only variable-length part and never enters the key.
- **The `by_name` index is the name direction** (`name → id`). An index's text segment carries neither a length prefix nor a separator (ADR-0005), so a bare prefix scan also hits longer names ("add" hits "add_to_cart") — that is prefix semantics, not a false hit. The equality entry point is `scan_exact::<I>`: a probe that consumes the whole fields segment is an exact hit and needs no fetch-back verification; the family whose probe is completed by primary-key tail bytes (see the index mechanism) is dropped by the scan's length test.
- **The `HighWater(id)` reduce issues ids**: read the water mark, +1, write back. The water mark never falls (unfold is a no-op), so withdrawing a row never lowers the next id — the ledger's one deliberate departure from being equal to the row set, and exactly why this preset can serve as an allocator.

Two disciplines:

- **Ids only increase, and are never reused.** Reusing a proxy id reads an old row as a new entity — the same rule as namespaces.
- **The allocator is a single writer.** The water mark's read-modify-write sits under the same single-writer boundary as every other read-modify-write in this document.

The reverse (`id → name`) is one `get` on the primary table — which is where a vocabulary beats a hash: hashing a name into a fixed-width segment collides (a collision = silently addressing another entity's row), and a hash cannot be reversed, so operations cannot list the vocabulary in human words.

Closed versus open: a **closed** vocabulary — the kinds the system declares itself — gets a real ns (`#[ok_ns]`); an **open** one — names arriving as data — gets a proxy id plus a text index. okm's own namespace vocabulary and field-name vocabulary are both this shape.

## Two kinds of tail: an index tail decodes, a primary-key tail does not

"Carrying data at the key tail" has two shapes with completely different decodability, easily confused:

```text
index entry       [ns 2B][slot][fields segment][primary-key prefix]   ← the tail always decodes
primary entry     [ns][pkey...][custom tail?]                        ← a custom tail has nowhere to decode
```

**An index tail decodes**: an index entry's tail is the primary-key encoding — a struct declared by `#[ok_ref]`, whose exact layout `KeyEncode` knows, so slicing it off the end of the entry key decodes the primary key back (`PrefixKey`; ADR-0005's contract: "the tail is always decodable from the last bytes of the entry key"). The `fields(...)` segment is the same: payload fields encoded in declaration order, with encode and decode both derive-generated, compile-time-locked code.

**A custom primary-key tail does not decode**: the primary key's layout is decided by the user's key struct, and `decode` decodes only the declared full length. A "remaining identifier" stuffed at the key tail that does not exist in the struct has no offset or width any generated code knows — and `PrefixKey`'s contract states outright that bytes beyond the truncated prefix are garbage.

The root in one sentence: **a tail decodes only if its layout entered the declaration system.** An index tail (the primary-key encoding, the `fields` segment) is inside the declaration system, so it decodes; a "remaining identifier" at the primary-key tail is outside it, so it has nowhere to decode — nor should it be stuffed there; that need belongs to `fields(...)`.

Terminology: **"access method" is the modeling-layer word** for a declared read path — this guide uses it; **"secondary index" is the mechanism-layer word** for the entry layout `#[ok_index]` generates — the ADRs and the README use it. `#[ok_index]` is the mount point for both. This document is about modeling, so its prose says "access method" throughout and falls back to "secondary index" only when the mechanism's layout is the subject.

## Design constraints on performance

These constraints are not free, but their cost is bounded and the structural gain outweighs it:

**Taking a value from the key versus through an access method.** A remaining identifier burned into the key comes back free with the entry on a scan (the tail bytes are already in hand); through an access method the dimension costs one indirection — `scan` adds one primary-table point read and one `decode_payload` parse per hit, while `scan_covered` skips the point read and pays instead for decoding the includes segment in the entry value (a `Covered` struct). One parse per row remains either way.

**The per-hit cost is small, and amortized.** That indirection is a routine LSM point lookup or one TLV parse, not on the scale of the hot path; what it buys is a key layout that need not deform for every "also queried by X" dimension, and a business that adds an `#[ok_index]` instead of redesigning its key as it grows.

**Batch reads come out better when an equality prefix fixes the leading fields.** The fetch-back is one point read per hit; what helps is that those hits ARRIVE in primary-key order. Entries cluster by `[index fields][primary-key prefix]`, so inside one equality group the tail IS the primary-key bytes, and the group's hits walk the primary table monotonically instead of scattering — the reads land in ascending block order, which is why a batch is far cheaper than per-hit random reads. Read the claim narrowly, because this is where it is easy to overreach: **an index scan is ordered by the index fields first, and by the primary key only INSIDE one fixed field tuple.** A range across the leading fields breaks it (hits then sort by the varied dimension and every hit is an independent point read, locality ≈ the number of distinct leading values in the range), a probe that pins only some leading fields has one such order per group, a `func` index repeats one row's key across several values, and a truncated `key(...)` makes tails tie. None of that makes a result wrong once the caller sorts: the key-stream operators take an order they do not check, so an `intersect` fed a stream that is not ordered by the join key is the one shape here that FAILS SILENTLY (it misses keys), which is why the recipe says to sort explicitly when the join column is not a prefix. What those shapes break is this locality argument, and the sort becomes unavoidable. A dimension that needs range reads belongs in the primary key's `[order]` segment (a range read is then a sequential scan of the primary table itself), or goes through `scan_range_iter`, stopping early and fetching back only for survivors. Hand-optimizing the key layout has no extra card to play in the batch case.

Conclusion: an access method's indirect cost is one amortizable parse plus one mergeable ordered read — bounded, controlled runtime overhead traded for unbounded key-layout complexity, and the trade only improves as the business grows.

## Covering indexes, with restraint

`includes(...)` copies payload fields into the index entry, so `scan_covered` skips the fetch-back and returns the decoded covered fields directly (`I::Covered`, one struct per declaration that has includes). That is a real gain on a high-fan-out read path — but every put and delete pays a permanent write cost.

Discipline:

- **Use `fields` only by default.** Model with the sort fields first.
- Add `includes` only after measuring that the fetch-back path is hot. It is a read-path optimization for a proven hot spot, not a default move at modeling time.

## The bounds of a composite primary key

A primary key may be composite (see the four layers: `[ns][pkey...][order]`), but its components must be part of the identity structure, and the identity structure is answered by the business, not by modeling technique.

The standard case is SaaS multi-tenancy: tenant → org → user, three levels. `tenant_id` and `org_id` are bound (an org belongs to exactly one tenant), while `user_id` is not bound to an org — a user can change employers, changing org without changing identity. In a concrete layout (brackets = key segments, braces = row fields; a field in both is written in both places):

```text
org (with hierarchy, parent_id expressing the tree)
  [ns:org][tenant_id][org_id]{name}{parent_id}

user
  [ns:user][user_id]{name}...{org_id}
```

- The org primary key is the composite `[tenant_id][org_id]`: `tenant_id` draws the physical boundary of data ownership, `org_id` is identity within the tenant, and both are identity structure.
- The tree does not cross tenants: the parent an org's `parent_id` names belongs to the same tenant, so `{parent_id}` inside the org table is enough to express the hierarchy — no separate adjacency entity. Expanding level by level (BFS/DFS) is a `get` per `parent_id` inside the `[ns:org][tenant_id]` prefix, so one access method covers the walk.
- The user primary key is `[user_id]` only. `org_id` is a row field — folding `(org_id, user_id)` into the primary key means a job change moves the key and splits one physical person into several identities.

Reads by the org dimension go through the index: `#[ok_index(by_org { fields(org_id, ...) })]` answers them with one prefix scan, and identity stays singular.

Ownership questions like "does the data a user produces belong to the company or to the person" work the same way — they are business questions, and when the answer changes (a data migration, a change of ownership) the identity should not. That is the row model's payload-not-identity rule (ADR-0006), landed as a modeling discipline.

## Decisions land at declaration time

Every discipline in this document comes from one position: structural decisions are settled once, at declaration time, and the runtime only executes the declaration. That position is why this document keeps coming back to "declare it, then leave it alone".

- **There is no interpreter at runtime.** okm has no query planner: one read is one prefix scan, and whether a question can be answered is decided by the declaration-time order of the data segments (the leftmost-prefix constraint above), not by choosing a path at query time. A dimension you cannot name an access method for is a full scan.
- **One declaration expands into two synchronized products.** The derive layer produces code that touches bytes directly — that is what runs — and, in a second layer, constant tables (`PAYLOAD_FIELDS`, `FIELDS`, `HOT_WIDTH`, `NS_PREFIX`, each access method's `SLOT`). Every non-hot-path consumer (schema export, dynamic readers, the arrow/Parquet bridge, snapshot tooling) reads those tables instead of re-deriving the declaration: declaration is registration, with no runtime registry (see the [architecture overview](ARCHITECTURE.zh-CN.md)).
- **Position is identity.** An access method's slot is its position in the declaration, and a field's vocabulary number is the order of its first appearance. Both only increase and are never reused (above): nothing in a layout is "just a number" that can be rearranged at will.
- **Illegal shapes are unrepresentable at compile time.** Two variable-length fields in an index segment have no boundary byte between them, so the derive rejects the declaration outright; a variable-length field that is not last breaks prefix semantics. Both are compile-time refusals, not runtime checks: an illegal model does not compile, rather than failing at some later moment.
- **The address space is a budget.** ns is a 2-byte identity — one ns per entity class, never reused; the slot space is one byte for a fixed face and two for a counting family (a 4-bit family plus a 12-bit counter), with retired numbers never reissued (ADR-0030); vocabulary numbers only grow. Allocating them is declaration-time work — when several instances share one engine, ns allocation is a deployment decision.
- **A layout change is a full rebuild, not a migration.** Index entries are derived state: they travel in neither export nor snapshot, and an import re-derives them from the rows with `put`. Changing the key layout or the segment order means re-declaring and rebuilding the store, never migrating in place. What a payload can change is its **shape** — and the whole shape changes together: a new shape carries its own number, and rows of the old shape are regenerated or carried across by export → import (ADR-0042). Key layout, face numbers and slots never move.
- **A declaration has two timing registers, and the runtime one covers only the data shape.** The typed path (compile-time derive) and the schema path (runtime `CollectionSchema`, which the Python and Steel bindings use to build collections with no Rust type in sight) produce the same bytes by contract. The difference between them is the payload shape alone; key layout, face numbers and slots are still decided by the declaration.

## Evolution

New modeling principles are appended to this document as subsections; mechanism decisions go into an ADR and are cited, not repeated.