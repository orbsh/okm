# Modeling Guide

How to map a domain onto OKM keyspaces: four layers per entity, access
methods mandatory, covering indexes restrained, composite keys bounded.
This guide is normative for application schema; for mechanism details see
the ADRs and the [README](../README.md).

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## The four layers

Every entity is modeled in four layers: **namespace → primary key →
primary sort field → access methods**.

1. **Namespace (`ns`)** — which keyspace the entity lives in. One entity
   class = one ns. The 2-byte ns already discriminates every entry; do
   not encode entity type into keys.
2. **Primary key** — identity, and nothing else. Proxy keys (auto-increm
   id, UUID) are the default: attributes are payload, not identity.
3. **Primary sort field** — the third segment of the key
   (`[ns][pkey...][order]`). Placed at the key tail, it keeps entries
   under the same pkey physically adjacent and ordered: a prefix scan
   *is* the timeline / range read. Other sort dimensions stay out of the
   key and surface through `#[ok_index]`. How physical decisions trade
   against indirect costs: see "Design constraints on performance".
4. **Access methods** — declared `#[ok_index]` entries. Each one is a
   standing answer to "how is this entity queried?".

If you cannot name the access methods, the model is not finished — the
questions you cannot name become full scans.

## Declaration basics

The four layers land in code as three derive macros. The full declaration
vocabulary:

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

Primary keys are fixed-width: fields are big-endian encoded in declaration
order, `KEY_LEN` locked at compile time. Keys stay fixed-width; variability
is a property of index entries, not keys.

### Junctions: `JunctionEncode`

```rust
use okm_core::{JunctionEncode, Ref};

/// user → sessions junction (the SQL many-to-many junction table).
///
/// Fields reference DOCUMENT types: the derive resolves
/// `<User as Document>::Key` and `NS_PREFIX` — the ns is declared once on
/// the document (User carries `#[ok_ns(1)]`, Session `#[ok_ns(2)]`); the
/// junction declares no ns of its own.
///
/// Forward: a user's identity is (org_id, user_id) → ok_head(org_id, user_id)
/// Reverse: a session's identity is the full SessionKey (no ok_head).
/// The two directions use different endpoint identity widths —
/// this is how "the primary key changes with direction" is expressed.
#[derive(JunctionEncode, Clone)]
#[ok_junction(1)]
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}
```

`#[ok_junction(n)]` sets the segment-0x3 discriminator, separating
multiple junctions over one endpoint pair. `#[ok_head(field, ...)]`
declares which fields of the endpoint count as its identity **in this
junction**; omitting it means the full key is the identity. Names must
be a declaration-order prefix of the endpoint's fields (compile-time
generated check). One declaration produces the one-way entry in each
endpoint's ns automatically (direction is carried by which ns the entry
lives in — see "Many-to-many relationships" below).

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
  row is the table's declaration point: `#[ok_ref]` pins the key type, so
  the row determines `Collection<S, K, R>` entirely). A key type carries no ns —
  the same key shape may serve several rows/tables, each with its own ns.
  `Collection::new(store)` takes no ns argument; the assembly site picks the
  engine only. A junction declares no ns — each endpoint document carries its
  own (JunctionEncode).
- `fields(...)` — payload fields to sort/group by, declaration order,
  first field = the grouping dimension.
- `includes(...)` — covering index: copies payload fields into the entry
  value (see "Covering indexes, with restraint").
- `key(...)` — truncates the primary-key tail carried at the entry end to
  the named subset (default: full). Truncation changes row-uniqueness, not
  grouping: the `fields` prefix drives ordering, the key tail distinguishes
  rows; `key(user_id)` is safe only when the named subset is unique per row —
  otherwise rows overwrite each other's entries.
- `func(path)` — function index: the data segment is the encoding of
  `path(&row)`'s return value, replacing fields. The function is a plain
  Rust fn (implemented in business code), and the query side calls the
  **same declared path** on its probe value — encoding and scanning share
  one definition, so normalization (lowercase, encoding transforms) cannot
  drift between the two sides. One declaration drives both sides;
  declaration is registration.

**Basic use 1: single-value function index (write-side precomputation).**
The data segment is the encoding of the function's return value, one
entry per row, entries still live and die with the row:

```text
fn lower_name(row: &Doc) -> String { row.name.to_lowercase() }

#[ok_index(by_name { func(lower_name) })]   // "Apple"/"APPLE" co-located
```

**Basic use 2: multi-value function index (one row fans out into N
entries).** A `Vec<V>` return fans one row into N entries; the token is
the data segment (variable-length, hugging the primary-key prefix), and
the read path is identical to a plain index (`scan::<I>`):

```text
fn hour_bucket(row: &Post) -> Vec<u64> {
    vec![row.created_at / 3_600_000]          // ms timestamp -> hour bucket
}

#[ok_index(by_hour { func(hour_bucket) })]   // scan with a bucket id = that hour's posts
```

Time bucketing is this shape at its cheapest — a single-element Vec that
folds the timestamp into a bucket number at write time (fixed-width BE,
byte order = time order); hourly rollups and timelines are one prefix
scan. The same primitive directly covers tokenized full-text search
(a tokenizer returning `Vec<String>`) and multi-valued fields (split
tags). \`okm-core\` ships no tokenizer — splitting/bucketing logic belongs to
the business layer. The func contract is **purity**: delete regenerates
the entry set from the row, so an impure function (clock / randomness /
external state) produces a different set at delete time than at write
time, leaving dangling entries.

`func(path)` and the `#[ok_reduce]` declaration below are the two
external extension mechanisms: func is **per-row derivation** (computed
at write time; entries still live and die with the row), reduce is
**cross-row aggregation** (mutable value, read-modify-write). How the
heavier integrations (FTS / vector / graph algorithms) land on the
primitives is covered in the [integration boundary
doc](integration/EXTENSION-TYPES.md). Implementation details (encoding contract,
`entry_pairs` override, probe normalization) live in the internals doc
[func-index-mechanism.zh-CN.md](internals/func-index-mechanism.zh-CN.md).

Physical index entry layout (ADR-0005):

```
[ ns 2B BE ][ slot 2B BE ][ fields segment BE ][ primary key prefix (default: full) ]   value = included fields TLV (empty when no includes)
```

The discriminator is namespace + slot: the ns segment scopes the table,
the slot's segment number distinguishes entry kinds within it
(primary/index/reduce/junction, ADR-0016); declaration is the
registry, no runtime index bookkeeping. **Index declarations are
append-only**: add at the tail only — never insert into or reorder the
middle. An insertion shifts the slot of every later index; entries
already on disk stay in the old slot position, and after the shift
`scan` reads a new prefix and returns empty results (a silent error,
not a slowdown). Removing a declaration merely leaves a harmless slot
hole (same discipline as ns IDs never being reused, ADR-0002). Also
note there is no index backfill: a newly appended index only sees rows
written afterwards; existing rows get no entries — migrate with
double-writes if existing data must be covered.

The fields segment is semantically an **ordered sequence of dimensions**:
the leading field is the grouping/equality dimension, the rest sort
within it. Two constraints govern it —

- **Decode constraint**: at most one variable-length field, and it must
  sit immediately before the primary-key prefix. The primary key is
  fixed-width (`KEY_LEN`) and is sliced off from the tail; any further
  fixed-width fields are sliced right-to-left; whatever remains is the
  single variable-length segment — its length is never stored, the
  boundary is inferred from the fixed-width anchor on its right. Two
  variable-length segments (e.g. `fields(token, name)`) share no
  boundary byte and are undecodable; the derive rejects this at compile
  time.
- **Query constraint (leftmost-prefix)**: prefix scans specify fields
  fully from the left, in declaration order. A variable-length field
  that is not last breaks prefix semantics — raw bytes `"beijing"` have
  no terminator, so a scan for it also matches `"beijing2"`. Variable
  fields go last; equality/fixed-width dimensions go first.

Note that `includes` is not part of the fields segment — it lives in
the entry value and takes no part in key structure or ordering.
Expressing "carry more data in the entry" by adding fields is a
modeling mistake; the correct outlets are `includes` (copy to skip
table lookups) or nested entries (store together).


### Link, unlink, query (`Junction`)

```rust
use okm_core::Junction;

let store = okm_core::TestStore::default(); // slatedb-mem; also FjallStore / SlatedbStore / RedbStore
let mut edges: Junction<_, UserToSession> = Junction::new(store);

let user = UserKey { org_id: 7, user_id: 101 };
let s1 = SessionKey { org_id: 7, session_id: 1001 };
let s2 = SessionKey { org_id: 7, session_id: 1002 };

edges.link(&user, &s1);   // atomic double write: forward + reverse key
edges.link(&user, &s2);

let sessions = user.get_session(&edges);   // forward: user → [SessionKey]
assert_eq!(sessions, vec![s1.clone(), s2.clone()]);

edges.unlink(&user, &s1); // deletes both directions
```

Physical key layout (two-ns residency, ADR-0015/0016):

```
[ ns_a u16 BE ][ slot u16 BE: 0x3 seg ][ A·identity ][ B·identity ]   in A's collection
[ ns_b u16 BE ][ slot u16 BE: 0x3 seg ][ B·identity ][ A·identity ]   in B's collection
```

The local identity leads (the scan prefix `[ns][slot][local identity]`
must match); the peer identity is the suffix. The discriminator's low
bit carries the direction — needed only for self-reflexive junctions,
where both endpoints share one ns.

Each entry is one-way: the entry in `ns_org` answers "all members of this
organization", the one in `ns_user` answers "all organizations this user
belongs to". Direction is carried by which ns the entry lives in — no
direction slots (the old 14/15 pair is gone). `nnn` is the junction
discriminator (`#[ok_junction(n)]`) for multiple junctions over one
endpoint pair. The junction's fields reference **document types**; the
derive resolves their `Key` and `NS_PREFIX` — ns is declared once, on the
document. See ADR-0015/ADR-0016 and the slot table in
[key-layout](docs/internals/key-layout.md).

### Reverse queries and truncated identities

```rust
// Reverse: session → users. A's identity here is truncated (ok_head),
// so raw bytes are returned for a main-table prefix scan.
let raws = edges.reverse_raw(&s1);

// If A had full identity, reverse() decodes back to the type:
// let users: Vec<UserKey> = edges.reverse(&s1);

// PrefixKey marks how many leading bytes are trustworthy.
for pk in edges.reverse_prefix(&s1) {
    // pk.decoded: decoded struct (prefix fields valid)
    // pk.taken:   bytes consumed by the identity prefix
}
```

The derive macro also generates query methods on the endpoint types themselves (`user.get_session(&edges)`), named after the opposite field (`session_id` → `get_session`).

### Rows at runtime (`Collection`)

Declaring rows and indexes (`DocumentEncode` + `#[ok_index]`) is covered in the
[Modeling Guide](docs/MODELING.md), "Declaration basics". The index declaration's derivative is generated at the expansion point
(this file): `ok_index(by_org ...)` generates the index type
`__OkmIndex_User_by_org` (mechanical concatenation, no case conversion);
alias it with `use` and it serves as the generic parameter. `Document::collection`
builds the assembly point without repeating the key type at the call site:

```rust
use okm_core::TestStore;
use __OkmIndex_User_by_org as ByOrg; // index type: derived from ok_index(by_org)

let mut t = <User as Document>::collection(TestStore::default());

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 });

// Leftmost-prefix scan on any access method, with fetch-back:
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
for (key, row) in rows {
    // key: decoded UserKey, row: Some(decoded User) when the payload exists
}

t.delete(&user); // removes the primary key + all declared index entries
// (multi-value indexes clear entry by entry — no dangling entries)
```

Multi-value function index, declared and queried (inverted-index shape —
the token is the data segment, the primary-key prefix is cut off the tail).
The `Vec` return is contract, not laziness: put consumes the values
immediately, entry by entry, so a lazy iterator buys nothing — one
`collect` for the smallest trait surface:

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
let hits = t.scan::<ByToken>(b"rust");
```

### Dynamic fields: the document API (ADR-0012)

One encoding serves declared rows and external data. Declared fields ride
the hot/cold segments as usual; anything else lands in the **dynamic
segment** (slot 1) as n-TLV frames — `[field-id][type][len][bytes]` —
with names allocated on first sight in the **field-name dictionary**
(slots 2/3). Runtime surface on `Collection`:

```rust
// The row-map bridge: every field (declared + dynamic) lifts to its logical
// type (Quant -> F64, VarInt -> u64, Enum -> variant name, Offset -> i64).
let fields = t.get_document(&key);          // Option<BTreeMap<String, DynamicValue>>, None = no row

// Whole-document write: fields matching the declared struct go to the typed
// path; unknown names allocate in the dictionary and land in slot 1.
t.put_document(&key, &fields);                      // BTreeMap<String, DynamicValue>

// Dynamic-only views (slot 1 directly):
t.get_fields(&key);                             // name-keyed map or None
t.put_fields(&key, &map);                       // whole-entry put, absent fields removed
t.delete_fields(&key);                          // clear the dynamic segment
t.delete(&key);                                   // remove slot 0 + index entries
```

- `DynamicValue` carries the open value vocabulary: `UInt`/`Int`/`F64`/
  `Str`/`Bytes`/`Bool`/`Null`/`Array`/`Obj` — nested objects recurse as
  native frames (type tag 7) sharing the table's dictionary; no CBOR.
- `Bytes` is the **opaque member** of the vocabulary: the store
  interprets nothing — tag and total length only; content and its
  meaning belong to the application. It is the standard escape hatch
  for extension types: a new encoding starts life as Bytes, and
  promoting it to a first-class type later is an additive change. The
  field name carries the semantics the tag deliberately does not
  (`embed_v1`, `attrs_cbor`).
- Unknown names are **normal input** here (external data, MQ payloads);
  the typed decoder's unknown-field rejection applies only to the
  declared path.
- Declared fields are indexable; dynamic fields are not (their names are
  runtime data).
- The schema export (`TableSchema`, serde behind `schema-serde`) drives
  the dynamic codec for embedded-language readers — Python (PyO3) and
  Steel bindings live in `bindings/`, byte-identical with the Rust
  derive (cross tests lock this). Version-default migration works on the
  dynamic read path too: literal `#[ok_default]` travels with the schema.

### Embedded documents (`Ref<D, K>` and `Refs<D, K>`)

A child document embedded **by key reference**: the parent's field
carries only the child's key on the wire (fixed width, hot segment);
the child is a complete document at its own ns/key with its own
indexes. In memory the field is `key` + `Option<value>`:

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
  `put` also writes the child payload + the child's index entries, in
  the same store batch (one atomic boundary).
- **Write, `None`** (`Ref::ref_key(key)`): reference an existing
  child — the parent stores the key and touches nothing else. This is
  the shared, many-to-one form (many users pointing at one address).
- **Read**: `get` dereferences automatically — the child is fetched by
  the stored key and backfilled. A missing child (deleted independently
  under reference semantics) reads back as `value: None` — visible
  absence, not a panic. Queries return the nested struct directly.
- **Overwrite**: changing the key releases the old reference — keys the
  OLD document pointed at but the new one doesn't are deleted. No
  cascade: a shared child survives; an owned-cascade option
  (`#[ok_embed(own)]`) is a possible future extension.
- No attribute is required: the derive recognizes `Ref<D, K>` / `List<D, K>`
  from the field type, the same discipline as `Reverse<T>` / `Quant<P>`.
- **Lists**: `Refs<D, K>` embeds many children — wire is a cold TLV frame
  `[count][key × n]`; memory is `keys` + index-aligned
  `values: Vec<Option<D>>` (dangling refs stay visible). Child keys must
  carry their own list identity (`owner_id + seq`) — OKM never appends
  positional numbers to keys. Stale release covers shortening: keys the
  old list held and the new one doesn't are deleted. `Refs` is the
  declarative carrier of the one-to-many relation above: children stay
  normalized in their own ns, the parent field holds the foreign-key set.
- **The identity line**: elements WITH identity (own indexes, sharing,
  independent updates) belong in `Ref`/`Refs`; pure-value elements
  (`Vec<String>` fields, dynamic `Array` frames) do not — a scalar has no
  key, and a key reference to it is a category error.
- The map view (`to_map`) lifts an embedded field to
  `Bytes(child key)` — the wire truth; the child's value belongs to the
  child collection, not this map.

Embedded is one of three ways fields relate across documents —
`includes` copies values into an index entry (skip the fetch), dynamic
frames nest values inside one payload, and embedding references a
separate document (shared identity, independent indexes, independent
lifecycle).

### Payload versions and field defaults

The payload carries a version byte (`#[ok_layout(version = N)]`, default 1).
Decode rule: a payload whose header version is **newer** than the reading
schema's is rejected; **older** payloads are accepted, and fields the old
payload lacks (appended at the tail after that version was written) take
their defaults:

```rust
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_layout(version = 2)]            // bump when the field set changed
pub struct User {
    pub org_id: u32,                 // existed since v1
    #[ok_default(100)]               // explicit default for pre-v2 payloads
    pub reputation: u32,             // appended at v2's tail
    pub bio_len: u16,                // no #[ok_default] → T::default() (0)
}
```

- `#[ok_default(expr)]` is any expression (literal, constant, function
  call); omitting it falls back to `<T as Default>::default()`.
- Defaults apply ONLY to decoding older payloads — a field missing from
  the bytes. Newly written payloads always carry every field (the writer
  stamps its own version), so this is version-migration semantics, not a
  "field default" for absent values.
- Append-at-the-tail is the only legal way to add fields: a field the
  reader knows but finds missing must be at the tail of its segment
  (header `hot_len` marks the boundary for hot fields; absent cold TLV
  frames simply aren't there). Mid-insertion changes the position of
  existing fields = layout change = version bump + clean rebuild, never
  silent.
- Lazy migration: old records stay in the old format; upgrading happens
  in memory on read. Unread rows never burn write bandwidth.

The dynamic codec (schema-driven encoders in Python/Steel) mirrors this
rule from `TableSchema` — per-field defaults travel with the schema so
the dynamic reader applies the same migration semantics.

### Schema stability tests

Lock the physical bytes with hard-coded hex — any layout drift fails CI:

```rust
let e = UserToSession {
    user: Ref::ref_key(UserKey { org_id: 7, user_id: 101 }),
    session: Ref::ref_key(SessionKey { org_id: 7, session_id: 1001 }),
};
let fk = e.a_side_key(); // A-side (forward) entry: lives in User's ns
assert_eq!(&fk[..4], &[0, 1, 0x30, 2]); // ns=1 (User), slot 0x3002 (junction seg, n=1, dir=0)
assert_eq!(&fk[4..8], &7u32.to_be_bytes()); // local identity leads
let rk = e.b_side_key(); // B-side (reverse) entry: lives in Session's ns
assert_eq!(&rk[..4], &[0, 2, 0x30, 3]); // ns=2 (Session), slot 0x3003 (n=1, dir=1)
// ... full layout assertions in okm-core/tests/integration.rs
```

## One-to-many relationships

Access methods are not only about ordering: **one-to-many relationships
also land as prefix grouping**. A layout like
`[tenant_id][org_id][user_id]` takes all users of one org by prefix
scan — this is the physical form of the relational "foreign key + list".

In OKM it appears as an independent ns plus a secondary index. The index
entry layout is `[ns 2B][slot][fields segment][primary-key prefix]`:
`fields(org_id)` is the grouping/sort segment in the middle of the entry
(payload fields encoded in declaration order, derive-generated,
compile-time layout locked) and provides the grouping prefix;
`includes(...)` is the entry value, copying frequently used fields so
the scan needs no fetch-back. Note this is independent of the primary
key layout: the grouping dimension lives in the index entry — composable,
retirable, with no "nowhere to decode the suffix" problem. You could
also store user data directly inside the list entries (the nested form;
"denormalized storage" in SQL terms). Storing together is efficient but
locks the user to the org — a job change moves data, the same trap as
primary-key composition.

Against SQL normalization: the independent ns *is* the normalized form —
org attributes like parent_id and org_name are stored once on the org
row, not on user rows. Storing together (denormalized) is efficient but
couples; apart (normalized) pays one indirection but keeps identity
singular.

## Many-to-many relationships: junctions

The access methods defined by `#[ok_index]` are **table-local** — their
data source is the row's own payload, kept in sync automatically by
`put`/`delete`. Cross-table relationships (one-to-many, many-to-many)
are facts between two independent entities; a payload index cannot
reach them. One-to-many is carried by `Refs` (see "Embedded documents");
many-to-many is carried by the **junction**:

```text
#[derive(JunctionEncode)]
#[ok_junction(1)]
struct OrgUser {
    #[ok_head(tenant_id, org_id)]   // A-side identity truncated to (tenant_id, org_id)
    pub org: Ref<Org, OrgKey>,
    pub user: Ref<User, UserKey>,   // B side, no annotation = full UserKey
}
```

A junction materializes as one one-way key per endpoint ns (segment 0x3,
direction carried by which ns the entry lives in, ADR-0015/0016): the
A-side prefix `[ns_org][0x3nnn][org]` fetches all members of an org in
one scan (the list side); the prefix in `ns_user` fetches all orgs a
user belongs to. A job change = adding/removing one junction entry;
identity never moves. `#[ok_head(...)]` declares which identity fields
the endpoint is truncated to on that direction (absent = full identity),
letting each endpoint use a shorter prefix — entry key length and
grouping granularity are trimmed per direction, which is how "the
primary key varies by direction" is expressed. `ok_head` selects a
subset of *declared identity fields*, not free bytes; its decidability
follows the same principle as the index tail.

Junctions and indexes are isomorphic at the mechanism level (both are
"secondary key layouts pointing at identity") but their roles cannot be
swapped:

- **Data source**: an index's data source is the row's own payload —
  the user never writes entries by hand. A junction's data source is the
  relationship between two entities — a business fact in its own right,
  requiring explicit double-write and double-delete.
- **Endpoints**: an index's tail points at its own table's primary key;
  a junction points at another collection's entity (two endpoints, one
  one-way entry per endpoint ns).

**Bidirectionality is an obligation, not an option.** A REV entry costs
one extra key of storage (LSM sequential append — the cheapest write
there is); skipping it buys: reverse lookups degenerate into full scans
of the FWD segment (violating access-method discipline), or a later
backfill migration (orders of magnitude more expensive). The contrast
with indexes makes this sharper: an index has only one direction,
because the reverse lookup goes through the primary-key `get`; both
endpoints of a junction are secondary vantage points — neither holds the
primary key — so both directions need an entry. Bidirectionality also
makes unbinding O(1): both keys' identities are in hand, so it is an
exact `delete`, not scan-then-delete. The real restraint is not "do we
need the REV direction" (no choice there) but **whether this junction
should exist at all**: a relationship with no reverse-lookup need and a
small cardinality (a config-style one-to-one) can live in a payload
field, with no junction declared; add one later when needed — junction
double-writes sync automatically, no backfill.

One sentence: **an index is a derived view of one row; a junction is
first-class relationship data**. The convenience of `#[ok_index]`
(declare-to-register, no manual ns numbering — ADR-0005's motivation
was precisely eliminating per-index manual numbering and hole
bookkeeping) is a secondary bonus, not the dividing line; the dividing
line is the data source.

**The interface is the verb pair, not a field** (ADR-0015 §B, decided
2026-09-20). A field-position `#[ok_relation(JunctionType)]` on a
`Refs` field — put-time diff auto link/unlink, generalizing Refs'
stale-release — was analyzed and rejected: Refs works because the
parent row is the single owner (one-directional by structure); a
junction's endpoints are peers, so a one-side field declaration leaves
the peer's delete unable to clean up (a stale field key resurrects a
deleted edge on the next put), and two write entry points (field diff +
explicit `link`/`unlink`) fight each other. The imperative pairing
keeps the call site as the causal record — the code says what happened,
no old state is ever reconstructed, and no write ever crosses into the
peer's ns implicitly. Do not model a junction as a document field; if
set-shaped mutation over one endpoint becomes a real need, the escape
is an explicit command, never put-path magic.

## Graph edges: the third relation carrier

Junctions cover many-to-many with **compile-time typed endpoints**. A
knowledge graph breaks that binding: one graph connects nodes of many
types, edge kinds are runtime data, parallel edges are distinct facts,
and edges carry attributes. The graph edge is the third relation carrier
(ADR-0017) — independent identity, attributes, parallel edges, open
endpoints.

Nodes are ordinary document collections: a node's "type" is its
collection (the ns is the type marker), and node-kind/attribute
filtering rides the collection's own declared indexes and dictionary —
zero layout difference from any document collection. What is new lives
on the edge side:

```rust
use okm_core::{EdgeEncode, EdgeFact, Graph, NodeRef};

/// The edge collection of one graph. Declared attribute fields are
/// edge-own data; endpoints are NOT declared — refs are
/// self-describing ([ns 2B][len][pkey]), so any node collection
/// participates with zero ceremony.
#[derive(EdgeEncode, Clone, Default)]
#[ok_edge(ns = 100)]
struct Employment {
    since_year: u16,   // each declared field = one 0x1 face
    weight: u32,
}

let mut g: Graph<_, Employment> = Graph::new(store);
let attrs = Employment { since_year: 2020, weight: 7 };
g.link(&EdgeFact {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    kind: "employs".into(),
    attrs: okm_core::KvGraph::attrs(&attrs),
}, &attrs, 1).unwrap();

g.typed_out("employs", &user1);   // 0x7 face: kind-qualified traversal
let slot = __OkmEdgeIndex_Employment_since_year::SLOT; // derive-generated 0x1 face slot
g.by_attr_face(slot, &2020u16.to_be_bytes());
```

- **Endpoint references** are `[ns 2B][len varint][pkey]` — the ns is
  the type marker, the pkey width rides IN the ref as a one-byte
  varint. Any collection's document participates with no declaration:
  same node = same bytes (traversal faces prefix-match exactly), mixed
  pkey widths coexist, and there is no registry to declare, maintain,
  or fail on. Graph edges retire the Ref discipline — the reference
  carries its own width, ns knowledge and all.
- **Faces**: one `link` writes primary (slot 0x0, `edge_id` u64) +
  kind index (0x4) + out/in traversal (0x5/0x6) + kind-qualified
  traversal (0x7/0x8) + one declared-attribute face per field (0x1),
  all in one batch; `unlink` deletes the same set. Kind names resolve
  through the edge collection's own dictionary (0x2/0x3,
  first-seen-claims-next).
- **Parallel edges** are separate facts: each link carries a
  caller-chosen `edge_id`; a live id is rejected, never reused.
- **Filtering**: the most selective face first, ids converged in
  memory — no composite faces by default (`(*)-[kind]->(:node_kind)` =
  0x7 scan intersected with the node kind face's pkey set).

Division line: `Refs` = one-to-many; `Junction` = many-to-many, fixed
endpoints, no attributes; **Graph Edge** = independent identity,
attributes, parallel edges, open endpoints.

## Homogeneous lists: `Vector<T>`

The plural-modeling taxonomy (ADR-0015 §4) has a fourth member besides
the two relation carriers and the heterogeneous `DynamicValue::Array`:
the **typed homogeneous list**. `Vector<f32>` declares a list of
floats; `Vector<String>` a list of strings — the element type is the
schema, the length is data.

Storage is a **variable-length cold TLV frame** (same slot as
`String`): payload = `[count u32 BE]` + the elements. Homogeneity is
the payoff — fixed-width scalar elements are **bare V** (zero
per-element overhead; a 384-dim f32 embedding costs 1.5 KB, not the
~10 bytes/element a heterogeneous Array would pay), while dynamic-
width elements (`String`) are per-element **LV**.

```rust
#[derive(DocumentEncode)]
#[ok_ref(DocKey)]
pub struct Doc {
    pub id: u64,
    pub embed: Vector<f32>,        // cold frame, any length
    pub tags: Vector<String>,      // per-element LV
    pub score: u32,
}
```

Length is data, not schema: a different embedding model (384 -> 768
dims) is just different frames — no wire migration. When the
application DOES know the expected count (embedding dims by contract),
`#[ok_len(384)]` adds an encode-time check — the write boundary is
where the promise is enforced; decode never checks (bypassing the
decoder is the reader's own problem — raw frame bytes are as opaque
as an unrendered image). The same contract is exported to the dynamic
reader (`FieldSchema::expect_len`) so the Python side is enforced
identically. It is an application contract, never a format constraint.

Elements are **pure values** — the identity dividing line (§Many-to-many
above) keeps Vector out of the relation carriers: a scalar has no key,
a key reference to it is a category error. One-to-many -> `Refs`; many-
to-many -> `Junction`; order-expressing value lists -> `Vector`;
heterogeneous dynamic lists -> `DynamicValue::Array` (slot 1). On
`get_document` a Vector lifts to `DynVal::Array` — the dynamic layer
has no Vector type; Vector is a storage-layer format. Multi-dimensional
shape is application-layer interpretation (row-major over the flat
sequence); nothing on the wire.

The consumer (okm-vector) builds search on top: the frame bytes are the
function-index data segment (exact match / quantized bucket recall via
prefix scan), distance is rerank-side. Storage stays byte-opaque.

## Cross-row pre-aggregation

Every index entry is an append-only derived view that lives and dies
with its row — delete regenerates the exact set via the same function,
so nothing dangles. One class of requirements sits outside that
discipline by nature: post counts per author, hourly rollups, exact
counters. These are **cross-row** — func's `fn(&Row)` signature sees
one row, and the answer lands on **read-modify-write of the entry
value**, the fourth primitive (mutable aggregation entry).

okm-core's stance splits in two layers: core ships no aggregation semantics
(no built-in counter types, no distributed add protocol), but the
mechanical half is provided as a helper facility, declared like an
index:

```text
#[ok_reduce(AuthorStats { group(author_id) })]
```

`group(...)` takes the grouping segment from row fields (entry =
`[ns][slot][group segment]`, the slot continues the index counter);
`AuthorStats` is a user type implementing `ReduceLogic` — an `Acc`
(the accumulator type, implementing `ReduceCodec` for fixed-width BE
encoding) plus `fold(acc, &row)` (on put) and `unfold(acc, &row)` (on
delete). The write path performs the read-modify-write automatically:
read the current acc, fold or unfold, write back. The read side is
`reduce_get` for one group and `scan_reduces` for all groups. The
mechanics (ledger invariant, the overwrite-unfold compensation, the
write-path sequence) live in the internals doc
[reduce-mechanism.zh-CN.md](internals/reduce-mechanism.zh-CN.md).

Two disciplines of use:

- **Reversibility is the contract**: `unfold(fold(a,x)) = a` must hold
  exactly — count and sum qualify, median and distinct do not;
  non-invertible reduces belong in an OLAP system beside the KV. A
  compound acc (count + sum for averages) implements `ReduceCodec`
  directly; okm-core only stores and fetches the bytes.
- **Single-writer boundary**: the hook is a read-modify-write, safe
  under single-writer engines; multi-writer races and distributed add
  protocols are outside this model (see the integration doc).

Zero-value GC is deliberately omitted: an emptied group keeps its
entry (the acc back at the identity), which avoids tombstone logic;
callers skip identity-element groups as needed.

## Two read-modify-writes: reduce and upsert_with

Besides reduce, the write path has an imperative half —
`Collection::upsert_with(key, f)`: read the old row, compute the new one in
a closure, go through the normal put. Both RMWs share the same
underlying shape (read → compute → write); the division of labor
follows one question: who knows the logic?

- **reduce (declarative)**: `#[ok_reduce(Logic { group(f) })]` pins
  everything at compile time — which rows land in which group, how
  fold/unfold compute — as part of the type declaration, framework-
  driven. Fits aggregations that evolve with the row structure
  (counts, sums).
- **upsert_with (commanded)**: `f(Option<R>) -> R` receives the old
  row at runtime, arbitrary logic. Fits updates only the caller knows
  (balance adjustments, conditional patches). `None` = the key does
  not exist yet (insert path).

Both go through the same write path (put), so index maintenance,
reduce folds and event emission all fire without special-casing. Both
rest on the same correctness boundary: **single-writer**. OKM is an
in-process library with a serial write order (`&mut self`), so
get→f→put cannot interleave — no CAS needed; the same constraint that
backs reduce's exactly-once. The multi-writer future (optimistic CAS)
is a different mechanism, outside this model. The overwrite unfold
compensation happens inside put; upsert_with does not settle the
ledger twice (see the internals doc on the reduce mechanism).

## Write-path events: inline and channel

Reduce answers "what does the accumulated state look like"; a second
class of consumers needs the write itself as an event — cache
invalidation, search-index sync, downstream notifications. The event
layer (ADR-0008) splits those consumers in two, and the split is the
whole design:

- **Inline** (reduce): runs inside the write path, exactly-once by
  construction — the fold IS part of the write. Reduce never consumes
  a channel.
- **Channel** (`#[ok_subscribe]`): best-effort delivery, no guarantee.
  The annotation declares that this row type's write-path events enter
  a channel; there is no handler at the annotation site — the
  processing logic belongs entirely to the consumer:

```text
#[ok_subscribe]                      // bare: the only form; the event enum is
                                     // build.rs-derived (variant = row type
                                     // name, renameable via #[ok_event_enum])
```

Events carry `op` (put/delete), a monotonic **write-batch epoch**, and
the row itself. The epoch is the emitting table's write counter: events
from one table arrive with an exact same-table boundary, so a consumer
combinator can fold glitch-free to the boundary instead of debouncing.
It is in-process only — never persisted, resets on restart — and gives
no cross-table ordering: independent puts have no atomic "both updated"
instant, so multi-table fan-in stays eventually-consistent by
structure.

Two disciplines of use:

- **Never put correctness on the channel.** A full queue drops, a
  missing sink drops silently; anything that must happen exactly once
  (like reduce) belongs inline. The channel is for consumers that can
  tolerate loss and reconcile later.
- **Purity applies to event payloads too**: the row snapshot rides the
  event as-is; deriving extra context at consume time re-reads storage
  the event no longer guarantees to describe.

The transport is an assembly-site decision (`ChannelCell::register`
accepts any sink — tokio mpsc, crossbeam queue, no-op); the core stays
synchronous and knows no executor. Combinators over the receivers
(map/filter/merge/fold to an epoch boundary) are the stream layer's
business, not the model's.

## Access methods are mandatory

Queries go through `get` (primary key) or `scan::<I>` (a named access
method). There is no general-purpose full-scan query surface.

The only raw scans are `scan_rows_raw` / `scan_keys` — maintenance
surfaces for the snapshot exporter and the Arrow bridge, not application
query paths. If application code reaches for them, that is a missing
access-method declaration, not a shortcut.

## Key naming: plain prefixes + composable access methods

A key has three segments: `[ns][pkey...][order]`. The sort field sits at
the key tail (a big-endian u64 timestamp is byte-order = time order, a
natural timeline): entries under the same pkey are physically adjacent
and ordered, and a prefix scan directly *is* the timeline / range read —
restoring a session, fetching the latest N, all one `scan`. Putting the
primary sort field into the key is a physical decision; other sort
dimensions stay in the payload and surface through `#[ok_index]`.

Fancy prefix schemes are not recommended (path-style, semantically
segmented prefixes) — the longer the prefix, the harder to manage; every
prefix is an encoding convention that must be maintained. Prefer
declaring more access methods: each `#[ok_index]` is an independent,
composable, extensible read path — declared to register, one line
deleted to retire.

**Consider a sort key only when the query result is plural.** The value
of the `[order]` segment is gathering "a group of related entries" into
a physically adjacent, ordered span that one prefix scan retrieves
whole; for a single-result (point) query use `get` — hoarding sort
segments in the key is meaningless.

Sort segments in composite keys deserve their own analysis. Start from
the physical property of sort keys: **a sort key like time is highly
selective and has no grouping power** — its values differ on almost
every entry, so anything placed after it lands in groups of one, and
intra-group ordering is impossible. Therefore appending fields after a
highly selective sort key is meaningless: it can only contribute scan
cost, never a read path.

Anti-pattern: `[session_id][user_id][time]` (session messages).

- user_id sits before time, so entries are ordered "grouped by user,
  then by time within the group" — queries that group a session *by
  user* generally do not exist, and the user_id segment yields no read
  path.
- The cost is certain: even the most ordinary session replay (fetching
  all messages) does not come back in time order — the read side must
  re-sort; fetching the latest N means scanning the whole group and
  sorting.
- If occasional per-user filtering is truly needed, a full scan with a
  filter suffices — not worth changing the key layout for.

The correct form is `[ns][session_id][time]`: the pkey carries exactly
one sort key, and session replay, pagination, and range reads are all
one prefix scan away.

Generalized: **a trailing remainder after `[pkey][sort field]` is also
an anti-pattern.** Stuffing a "also query/group by X" dimension into the
key tail is, in essence, hand-maintaining an access method inside the
key. Hand-maintained key prefixes can indeed buy performance in specific
scenarios, but:

- **Scenario-dependent**: the key layout freezes one read path. As
  business complexity grows, most tables need multiple access methods;
  optimizing for just one is of little value.
- **Business-logic-dependent**: whether the optimization holds depends
  on the business happening to support that read pattern — not a
  modeling technique.
- **Poor cost/benefit**: high development cost, and the performance
  gain is small (quantified in the next section, "Design constraints on
  performance").
- **Nowhere to decode the suffix**: a custom segment at the key tail
  needs a matching decode method, which OKM explicitly does not
  provide — the `KeyEncode` derive decodes only the declared full key
  length (`KEY_LEN` locked at compile time), and `PrefixKey` decodes
  only the truncated identity prefix (bytes beyond the prefix are
  garbage by contract). Hand-writing offset arithmetic to decode a
  suffix is complex and error-prone, precisely losing OKM's point:
  zero-cost encode/decode with compile-time locked layouts.

These dimensions all belong to access methods / secondary indexes:
`#[ok_index]` registers on declaration — composable, extensible, added
as the business grows — without touching the primary key layout.

## Two kinds of tails: index tails are decodable, primary-key tails are not

"Carrying data in the key tail" comes in two forms with completely
different decidability, and they are easy to confuse:

```text
index entry     [ns 2B][slot][fields segment][primary-key prefix]  ← tail always decodable
primary entry   [ns][pkey...][custom tail?]                    ← custom tail undecodable
```

**Index tails are decodable**: the tail segment of an index entry key is
the primary-key encoding — a struct declared via `#[ok_ref]`, whose
exact layout the `KeyEncode` derive knows; cut it off the end of the
entry key and decode it back into the primary key (`PrefixKey`; the
ADR-0005 contract: "the tail is always decodable from the last bytes of
the entry key"). The `fields(...)` segment is the same: payload fields
encoded in declaration order, all encode/decode generated by the derive
with compile-time locked layouts.

**Primary-key custom tails are not decodable**: the primary key's layout
is whatever the user's key struct declares; `decode` decodes only the
declared full length. Stuff a "remainder" into the key tail that the key
struct does not contain, and no generated code knows its offset or
width — the `PrefixKey` contract even states that bytes beyond the
truncated prefix are garbage.

The root in one sentence: **a tail is decodable only if its layout is
inside the declaration system.** The index tail (primary-key encoding,
`fields` segment) is inside the declaration system, hence decodable; a
primary-key "remainder" is outside it, hence undecodable — and should
not be stuffed in; the need belongs to `fields(...)`.

Terminology: **"access method" is the modeling-layer word** — a declared
read path; this guide uses it. **"Secondary index" is the
mechanism-layer word** — the index entry layout generated by
`#[ok_index]`; the ADRs and README use it. `#[ok_index]` is the mount
point for both. This document addresses modeling, so the body uses
"access method" throughout and says "secondary index" only when
touching mechanism layout.

## Design constraints on performance

The constraints above are not free, but the cost is bounded and the
structural benefit outweighs it:

**Reading from the key vs reading via an access method.** A remainder
burned into the key rides back for free with every entry (tail bytes in
hand); via an access method, that dimension comes back through one
indirection — a `scan` fetch-back pays one primary-table point read
plus one `decode_payload` per entry; `scan_covered` skips the point
read but still pays one entry-value parse. Per entry, yes, one extra
parse.

**The per-hit cost is small, and amortized.** That indirection is a
routine LSM point get / one TLV parse — not hot-path scale; what it
buys is a key layout that never deforms for every "also query by X"
dimension. As the business grows you add `#[ok_index]` instead of
redesigning keys.

**Bulk reads actually come out ahead.** Fetch-back looks like random
point reads, but both streams are already ordered: index entries are
clustered and ordered by `[pkey][order]`, and the primary table by
`[pkey]` — fetch-back degenerates naturally into a two-pointer merge
(merge-join style), sequential I/O, far cheaper than per-entry random
reads. Hand-optimized key layouts have no extra card to play in the
bulk case.

Conclusion: the indirect cost of an access method is one amortizable
parse plus one mergeable ordered read; bounded, controlled runtime
overhead in exchange for unbounded key-layout complexity — a trade that
only pays more as business complexity grows.

## Covering indexes, with restraint

`includes(...)` copies payload fields into the index entry so
`scan_covered` needs no fetch-back. A real win for high-fanout read
paths — but every put/delete pays a permanent write cost.

Discipline:

- **Default to `fields` only.** Model with sort fields first.
- Add `includes` only after measurements show the fetch-back path is
  hot. It is an optimization for a proven hot path, not a default
  modeling move.

## The boundary of primary-key composition

Primary keys may be composite (see the four layers:
`[ns][pkey...][order]`), but the composed components must be part of
the identity structure — and identity structure is answered by the
business, not by modeling technique.

The canonical case is SaaS multi-tenancy: tenant → org → user, three
levels. tenant_id and org_id are bound (an org belongs to exactly one
tenant); user_id is not bound to an org — a user can change jobs,
changing org while identity stays. In concrete layout terms (square
brackets = key segments, braces = row fields; a field that is both key
and row is written in both places):

```text
Org (with hierarchy; parent_id expresses the tree)
  [ns:org][tenant_id][org_id]{name}{parent_id}

User
  [ns:user][user_id]{name}...{org_id}
```

- The org primary key is the composite `[tenant_id][org_id]`:
  tenant_id draws the physical boundary of data ownership, org_id is
  identity within the tenant — both are identity structure.
- Trees do not cross tenants: the parent referenced by parent_id is in
  the same tenant as the child, so `{parent_id}` on the org row is
  enough to express the hierarchy — no separate adjacency-table entity
  is needed. Level-by-level expansion (BFS/DFS) within the
  `[ns:org][tenant_id]` prefix is just layered `get` by parent_id; one
  access method covers it.
- The user primary key is only `[user_id]`. org_id is a row field —
  compose `(org_id, user_id)` into the primary key and a job change
  means a key change: one physical person split into several
  identities.

Reads by org go through an index: `#[ok_index(by_org {
fields(org_id, ...) })]` answers org-scoped reads with one prefix scan
while identity stays singular.

Ownership questions like "does user-generated data belong to the
company or the person" follow the same logic — they are business
questions, and when the answer changes (data migration, ownership
transfer) identity should not. This is the row model's (ADR-0006)
payload-not-identity rule, restated as modeling discipline.

## Evolution

New modeling principles are appended to this document as sections;
mechanism decisions go into ADRs and are referenced, not duplicated.
