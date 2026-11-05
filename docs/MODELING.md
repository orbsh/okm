# Modeling Guide

How to map a domain onto OKM keyspaces: four layers per entity, access methods mandatory, covering indexes restrained, composite keys bounded. This guide is normative for application schema; for mechanism details see the ADRs and the [README](../README.md).

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## The four layers

Every entity is modeled in four layers: **namespace → primary key → primary sort field → access methods**.

1. **Namespace (`ns`)** — which keyspace the entity lives in. One entity
class = one ns. The 2-byte ns already discriminates every entry; do not encode entity type into keys.
2. **Primary key** — identity, and nothing else. Proxy keys (auto-increm
id, UUID) are the default: attributes are payload, not identity.
3. **Primary sort field** — the third segment of the key
(`[ns][pkey...][order]`). Placed at the key tail, it keeps entries under the same pkey physically adjacent and ordered: a prefix scan *is* the timeline / range read. Other sort dimensions stay out of the key and surface through `#[ok_index]`. How physical decisions trade against indirect costs: see "Design constraints on performance".
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

A many-to-many relationship is a table (ADR-0051). Its key is the pair of identities, and its row carries whatever the relationship owns — including the column a reverse read scans; a field that is already in the key, though, is not copied into a column (see the end of "Many-to-many relationships").

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
- `#[ok_ns(1)]` — the table's namespace segment, declared on the ROW (the
row is the table's declaration point: `#[ok_ref]` pins the key type, so the row determines `Collection<S, K, R>` entirely). A key type carries no ns — the same key shape may serve several rows/tables, each with its own ns. `Collection::new(store)` takes no ns argument; the assembly site picks the engine only.
- `fields(...)` — payload fields to sort/group by, declaration order,
first field = the grouping dimension.
- `includes(...)` — covering index: copies payload fields into the entry
value (see "Covering indexes, with restraint").
- `key(...)` — truncates the primary-key tail carried at the entry end to
the named subset (default: full). Truncation changes row-uniqueness, not grouping: the `fields` prefix drives ordering, the key tail distinguishes rows; `key(user_id)` is safe only when the named subset is unique per row — otherwise rows overwrite each other's entries.
- `func(path)` — function index: the data segment is the encoding of
`path(&row)`'s return value, replacing fields. The function is a plain Rust fn (implemented in business code), and the query side calls the **same declared path** on its probe value — encoding and scanning share one definition, so normalization (lowercase, encoding transforms) cannot drift between the two sides. One declaration drives both sides; declaration is registration.
- `where(path)` — partial index: a row for which `path(&row) -> bool` is
false does not exist in this index (a partial index). The predicate receives the whole row and may read any column (not necessarily an indexed one); it composes with plain field indexes and function indexes alike.
- Raw-byte fields: `Bytes` (`okm_core::Bytes`) — a variable-length cold
TLV frame, the same encoding as `String` minus the UTF-8 constraint (hashes, ciphertext, serialized blobs). The retired `Vec<u8>` spelling is rejected at compile time with a pointer here: a list-shaped name misdescribed byte-string semantics.

**Basic use 1: single-value function index (write-side precomputation).** The data segment is the encoding of the function's return value, one entry per row, entries still live and die with the row:

```text
fn lower_name(row: &Doc) -> String { row.name.to_lowercase() }

#[ok_index(by_name { func(lower_name) })]   // "Apple"/"APPLE" co-located
```

**Basic use 2: multi-value function index (one row fans out into N entries).** A `Vec<V>` return fans one row into N entries; the token is the data segment (variable-length, hugging the primary-key prefix), and the read path is identical to a plain index (`scan::<I>`):

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
`fields(name)` with the tail carrying time/identity gives the entry `[name][tail]`, i.e. a `(name, time)` composite order: the carried tail is the only thing allowed to follow a variable segment — its third job.
- **Two text dimensions that must BOTH order the bytes → one computed segment.**
A `func` index makes the data segment a single string with the separator inside the VALUE (`last\x00first`): still one naked variable segment, still no terminator in the key, and the prefix `last\x00` means "everyone with that surname". Choosing the separator is that declaration's contract (the same one-declaration-drives-both-sides rule as probe normalization); it must stay outside the value domain or the classic escaping problem returns — owned by the value-layer function rather than the key layout. The other lookup direction is a second face (a func fan-out).
- **Fixed width before VarInt.** Two `VarInt` fields hit the same bound; spelling
the width out (`u32`/`u64` BE) removes it — fixed width is the norm in a sort segment, VarInt is a space-saving device.
- **The genuine last resort is a self-delimiting frame**: two adjacent naked
variable segments are structurally undecodable; that route works but trades away zero-cost slicing and the compile-time layout lock (ADR-0005's 2026-10-17 update).

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

Declaring rows and indexes (`DocumentEncode` + `#[ok_index]`) is covered in the [Modeling Guide](docs/MODELING.md), "Declaration basics". The index declaration's derivative is generated at the expansion point (this file): `ok_index(by_org ...)` generates the index type `__OkmIndex_User_by_org` (mechanical concatenation, no case conversion); alias it with `use` and it serves as the generic parameter. `Document::collection` builds the assembly point without repeating the key type at the call site:

```rust
use okm_core::TestStore;
use __OkmIndex_User_by_org as ByOrg; // index type: derived from ok_index(by_org)

let t = <User as Document>::collection(TestStore::default());

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 })?;

// Leftmost-prefix scan on any access method, with fetch-back:
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes())?;
for (key, row) in rows {
    // key: decoded UserKey, row: Some(decoded User) when the payload exists
}

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

Beyond the equality prefix, a comparison on the leading index field is a physical key interval (ADR-0020): `scan_range::<I>(begin, end)` / `scan_range_iter::<I>(..)` — byte order == value order for every OKM encoding, so `1 < a < 100` reads only the rows inside `[1, 100)`. Recipes live in the [query guide](query-recipes.md), "the forms of WHERE".

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

- **One key type, many tables.** This is the side-table form (ADR-0036): a second
row type over the parent's key type, in its own ns, with a byte-identical key payload — `ProfileStats { views: u32 }` over `ProfileKey` costs one more point read, not a second key space. Everything else about the two tables is separate, including their commits.
- **`user_id` above is two fields, not one stored twice.** The key's `user_id` is
the address; the row's `user_id` is data that may legitimately differ — a denormalized copy for an index, or so a reader need not re-derive it from the key. Nothing dedupes, nothing errors, nothing compares them on a read.
- **Names are classified by the declaration, not by the key.** In a whole-document
write a name that is a declared payload field updates the row; anything else lands in the dynamic segment (ADR-0012). The key's own bytes never appear in the map `fetch` returns — that map is payload fields plus segment.

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
- **It is already in hand on every read.** `fetch` takes the key the caller passed; `scan` and `scan_covered` return `PrefixKey<K>` beside every hit. A mirrored id hands a caller something it already had.

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

One encoding serves declared rows and external data. Declared fields ride the hot/cold segments as usual; anything else lands in the **dynamic segment** (fixed face 0x01) as n-TLV frames — `[field-id][type][len][bytes]` — with names allocated on first sight in the **field-name vocabulary** (fixed faces 0x02/0x03). Runtime surface on `Collection` — the static segment speaks the struct, the whole document speaks one name-keyed map (ADR-0035). Two doors, and the split is about what a call may look at: `get` / `put` are the **struct door** (the row only — no vocabulary load, no segment access; the compiled-schema path), while `fetch` / `merge` / `replace` / `purge` are the **whole-document door**.

The classifier is the payload declaration — hot or cold — and nothing else. A name that coincides with a key field is not special: OKM is a key-value store, the key and the value are separate things, so such a name is simply a field in the value and lands in the dynamic segment. The identity still comes from the key: no map can move it, and a map naming `org_id` creates no row at `org_id`. Nothing needs checking here, and no confusion needs preventing — a caller that stores a field named like a key field gets exactly what it asked for.

```rust
// The row-map bridge: every field (declared + dynamic) lifts to its logical
// type (Quant -> F64, VarInt -> u64, Enum -> variant name, Offset -> i64).
let fields = t.fetch(&key)?;                   // Option<BTreeMap<String, DynamicValue>>, None = no document

// Partial update: declared names are a row RMW (typed path, previous faces
// cleaned), unknown names overwrite just those names in the dynamic segment.
t.merge(&key, &patch)?;                       // BTreeMap<String, DynamicValue>

// Whole-document replacement: the map IS the document — absent declared
// fields take defaults, unnamed dynamic names are removed.
t.replace(&key, &fields)?;

// Both slots, one commit — the ONE removal verb (a row cannot outlive its
// segment, so there is no row-only delete to get wrong).
t.purge(&key)?;                               // -> bool (something was there)
```

- `DynamicValue` carries the open value vocabulary: `UInt`/`Int`/`F64`/
`Str`/`Bytes`/`Bool`/`Null`/`Array`/`Obj` — nested objects recurse as native frames (type tag 7) sharing the table's vocabulary; no CBOR.
- The value form crosses a boundary as itself, not as a hand-written
`match` per direction: `Serialize`/`Deserialize` for `DynamicValue` ride the same `schema-serde` feature. `Deserialize` goes through `deserialize_any`, so the format must be self-describing (JSON, CBOR — not bincode); in JSON two collapses are unavoidable and documented — `Bytes` renders as a number array (CBOR keeps a byte string), and a non-negative `Int` reads back as `UInt`. The storage encoding is untouched: frames + vocabulary ids remain the representation at rest.
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

### Payload versions and field defaults

The version byte is a STAMP of the shape, never a compatibility promise (ADR-0042): a field is placed where its reader looks for it, the number changes when the shape does (incrementing is the ordinary style, resetting is equally legitimate — its only duty is to tell two incompatible shapes apart), and no build owes a reader for the shape it replaced. A store written by another shape is regenerated, or carried across by export (the build that wrote it) → import (the build that reads it). The rules below describe what the DECODER does; they do not oblige any table to keep an old shape readable.

The payload carries a version byte (`#[ok_layout(version = N)]`, default 1). Decode rule: a payload whose header version is **newer** than the reading schema's is rejected; **older** payloads are accepted, and fields the old payload lacks (added after that version was written) take their defaults:

```rust
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_layout(version = 2)]            // the shape changed: the number changes with it
pub struct User {
    pub org_id: u32,                 // existed since v1
    #[ok_default(100)]               // what the decoder supplies for a v1 payload
    pub reputation: u32,             // the field this shape added
    pub bio_len: u16,                // no #[ok_default] → T::default() (0)
}
```

- `#[ok_default(expr)]` is any expression (literal, constant, function
call); omitting it falls back to `<T as Default>::default()`.
- Defaults apply ONLY to decoding older payloads — a field missing from
the bytes. Newly written payloads always carry every field (the writer stamps its own version), so this is what the DECODER does when an older payload turns up, not a "field default" for absent values.
- Append-at-the-tail is what the DECODER can absorb — a field the reader knows but
finds missing has to be at the tail of its segment (header `hot_len` marks the boundary for hot fields; absent cold TLV frames simply aren't there). It is not a reason to append anything: a table that claims no compatibility (ADR-0042) places fields where they belong, and moving an existing field is a layout change whose number changes with it.
- **A change of FORM is a shape change, and the bytes cannot tell you.** The rules above rescue a
payload that LACKS a field; they cannot rescue one that POSSIBLY MEANS something else. A categorical
field is the case that bites: an `Enum` tag and a `Vocab` id are both a small integer, so an old
payload read under the new declaration decodes without error into a DIFFERENT category — and every
default above is irrelevant to it. The guard is the table's stamp, not the payload's: the
`#[ok_layout(version = N)]` number goes up with the form, and `ensure_layout` at attach/boot then
refuses the whole store with `LayoutMismatch { stored, current }` instead of letting its bytes be
read as something they never said. Rows cross by export → import, which is also the only path that
re-interns their labels into the destination's vocabulary (ADR-0032 §7).
- There is NO upgrade-on-read path: an old payload that happens to decode is a lucky
landing, not a migration. Carrying a corpus across a shape change is export (by the build that wrote it) → import (by the build that reads it).
- A production upgrade is a BLUE/GREEN move, not a reader: the new build runs as a
second process on its own store, the old instance exports while both stay live, the new one imports the bulk, then a short stop covers the incremental catch-up and traffic switches over. Two builds never write one store (a KV store is single-writer), so even a format-compatible upgrade costs a stop-and-swap. SQL can change a schema online (DDL), which is a real edge for DEVELOPMENT; in production the edge shrinks, because the complexity sits in the application — multi-environment testing, a snapshot before the release, a rollback plan. The stop-and-swap itself is paid in seconds: inside the latency users already blame on the network, hidden by a gateway retry, and schedulable in a predicted idle window. Bulk volume belongs on the cold side (a lakehouse export, or an engine that owns its own format), not in a tolerant decoder here.

### The table's own record (fixed face 0x0F)

A table can describe itself. Fixed face **0x0F** holds ONE entry per table — `[partition?][ns][0x0F]`, no suffix — whose value is a **name → bytes map** (ADR-0043). `version` is the name the runtime writes, and its value is the same number the payload header carries; anything else a consumer wants to record about the store rides beside it without a layout decision.

```text
[count u16 BE] ( [name_len varint][name UTF-8][value_len varint][bytes] )*
```

Two names are the runtime's. `version` is the payload's shape. `faces` is the fingerprint of the declaration's derived faces — the `#[ok_index]` slots, which live in their own ns and change without changing a single payload byte (ADR-0048) — and it is computed from the declaration, never typed. `Collection::faces_status()` compares it and answers `Match` / `Stale` / `Unknown`, and `Collection::rebuild_faces(chunk)` / `Graph::reindex(chunk)` are the verbs that repair a `Stale` store in bounded commits; `ensure_layout` never refuses on it, because a store written before faces existed would fail a check it can never satisfy, and a stale face is repairable drift rather than an unreadable row.

`Collection::ensure_layout()` is the first-touch verb: absent → seed this build's version and its faces; matching → answer the row; another version, or a row with no version → `Error::LayoutMismatch { stored, current }` naming both numbers. `set_meta(name, value)` records a caller-owned name (the runtime re-stamps `version` afterwards). Call it at attach: one point read per table, never a per-row cost — and `Collection::new` stays I/O-free because of that.

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
lives in the collection's data vocabulary, so a dynamic writer can introduce a category and a dynamic reader can name it. `N` is the id width and the only knob: `Vocab<u8>` (the default) buys 256 labels per field at one byte per row, `Vocab<u16>` 65536 at two, and so on. Each field owns its own table, so the budget is per field. There is no ordering — a field that must sort semantically declares a separate rank column. Consumer-side enum semantics (match arms, exhaustiveness) belong to the consumer: wrap the field in your own enum and convert at the edge.
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

The cost is semantic overload: `""` cannot distinguish "explicitly cleared" from "never set". When a field genuinely needs that third state, the answer is **not** a nullable wrapper but an explicit enum (`Unset | Cleared | Set(v)`) — one extra tag byte, and the reader stops guessing. `DynamicValue::Null` is already this shape on the dynamic segment: Null is a variant of the open vocabulary, not an `Option` wrapper.

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
#[ok_index(by_member { fields(member_id) })]
struct Membership {
    member_id: Uuid7,      // the column the reverse lookup scans
    nickname: String,      // what the relation KNOWS (ADR-0051)
    since_ms: u64,
}
```

The first key column is the forward direction: one prefix scan lists a channel's members, and the probe comes from the key's own builder — what comes back is the **decoded key**, not bytes:

```rust
for key in memberships.scan_keys(&MembershipKey::prefix().channel_id(id).finish()) { … }
```

The reverse direction is the declared index: `by_member` turns "which channels is this person in" into an index scan, and `scan::<ByMember>(&probe)` hands back a `PrefixKey<MembershipKey>` per hit, plus the row. The entry's tail IS the primary key, so `decoded.channel_id` and `decoded.member_id` read directly, and the fetch-back is `scan`'s own work (`None` = the row went away between the scan and the read). `put` writes the row and the index entry in one batch; `purge` takes both away; `rebuild_faces` reproduces the index from the rows. Nothing here is hand-kept.

There is no junction carrier (ADR-0051). A relationship is data, and this model already has a carrier for data — a collection. What a junction would have had to build by hand is exactly what `put` and `#[ok_index]` do automatically:

- **attributes on the relationship** (`since_ms`, a per-channel `nickname`) are ordinary fields, indexable and exported like any others;
- **the reverse direction** is declared, not hand-written, and is rebuildable (a junction's second entry had no rebuild path);
- **one write verb pair** (`put` / `purge`) instead of `link` / `unlink` remembering to write twice;
- **the row travels**: it is in the changelog and the lake, which junction entries were excluded from.

The relationship's identity is the pair — at most one row per two endpoints — because the pair is the primary key. Adding or removing a membership is `put` / `purge` on that key; identity never moves.

### A field already in the key is not copied into a column

The declaration above is the **copied-column** shape: `by_member`'s data segment comes from a column on the row's payload, the entry's tail comes from the key, and the two are independent storage locations — nothing checks that they agree, so a writer can put a disagreeing pair on disk. The entry is therefore 40 bytes:

```text
[ns 2B][slot 2B][member_id 16B]   [channel_id 8B][member_id 16B]
                 ↑ the row's column     ↑ the primary key (tail)
```

To have that reverse read without the copy, hand-write the access method: `KvIndex::encode_named` receives both key and document, so the data segment can come off `key`; shorten the tail with `key(channel_id)` and the entry drops to 24 bytes (`[member_id][channel_id]`), with `taken` semantics promising the prefix fields and zero-filling the rest.

The test is whether the field is **already in the key**. If it is, do not copy it — the copy saves no bytes (both live in the entry anyway) and adds an error class nothing checks. If it is not (`nickname`, `since_ms`: what the relation itself knows), it belongs on the row and a plain `fields(...)` is right. This guide keeps the copied-column form because it is the shortest shape that explains the mechanism; the rule itself is "do not copy".

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

