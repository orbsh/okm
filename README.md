# OKM — Object-Keyspace Mapping

> ORM experience, Redis speed, PostgreSQL durability, and functions without boundaries.

OKM is the KV counterpart of ORM: ORM maps objects onto relational tables, OKM maps objects onto KV keyspaces. Declarative derive macros (`#[derive(KeyEncode)]` / `#[derive(EdgeEncode)]`) plus numeric namespace IDs build a zero-cost semantic data layer — as declarative as an ORM at development time, compiled down to pure pointer-offset arithmetic.

Related reading: [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) — underlying architecture and design patterns (encoding principles, index strategies, engine-level trade-offs).

## Why: code as DDL

SQL's core value is not execution performance — it is the readability, modeling discipline, and team-coordination determinism delivered by the relational model. Raw binary keys degrade into spaghetti: nobody on the team can reason about key layout rules. The solution is to replace SQL DDL with the Rust type system, moving schema correctness from a runtime database engine to the compile-time compiler.

- **Struct as DDL**: SQL defines table structure; a Rust struct defines key encoding. The compiler enforces format consistency — any code attempting to write a malformed key fails at compile time, no runtime validation.
- **Schema-less is a trap**: schema-less stores accept anything (`"25"`, `[25]` for a numeric field), and every consuming service pays for it in defensive parsing. Strong typing fuses invalid data at `cargo check` — it never even gets generated — while keeping KV's hardware-level speed and skipping PG's runtime DDL-lock and SQL-parsing taxes.
- **Hex stability tests**: the last line of defense against encoding drift. Hard-coded historical hex bytes lock the physical key layout; any change to ns, field order, or widths fails CI immediately (see `okm/tests/integration.rs`).

The verdict: SQL's core value is human-facing structured discipline. By holding to "code as DDL" — strongly-typed key encoding, lazy migration via versioned enums, dual-write key-pointer contracts, and hard-coded hex stability tests — OKM gains all of it at once: compile-time schema safety (the Rust compiler) + runtime performance (LSM-Tree) + zero-downtime evolution (versioned enums) + team maintainability (struct comments as documentation) + drift protection (hex tests). On schema safety it overtakes both SurrealDB (schema-less) and PostgreSQL (runtime DDL locks).

## Physical gains

- **85% prefix compression**: a 14-byte string prefix becomes 2 bytes of numeric namespace.
- **100% fixed-width keys** (for full-identity keys): every field's absolute byte offset is compile-time locked; decoding is pure pointer slicing, zero parsing.
- **32768 namespaces**: u16 with the top bit niched as the edge direction bit (see [ADR-0001](docs/adr/0001-direction-bit-niche.md)).
- **Cache locality**: bloom-filter rejection plus in-memory seeks keep the hot path CPU-cache friendly.
- **Zero runtime overhead**: no regex, no split, no AST — the query path is machine code after `cargo build --release`.

## Status

Implemented:

- `KeyEncode` — fixed-width key encoding (`u32` / `u64` / `[u8; N]`), big-endian, compile-time `KEY_LEN` / `FIELD_WIDTHS`, `encode_prefix_named` truncation primitive.
- `EdgeEncode` — bidirectional edges with per-endpoint identity width (`#[kv_head(...)]`), 2-byte direction-bit header, query methods generated onto endpoint types.
- `EdgeTable<S, E>` (formerly `Collection`) — the edge assembly point: engine + edge type = the operation surface of one relationship (`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`).
- `RowEncode` — one macro declares a row (Node): `#[kv_ref]` identity + TLV payload fields + `#[kv_index(...)]` access methods; the `ValueEncode` derive is absorbed into it.
- Secondary indexes (access methods) — `#[kv_index(name { fields(…), includes(…), key(…) })]` on **row structs**: composite indexes over payload fields (declaration order), no per-index slot/ns — the 2-byte table namespace already discriminates every entry; leftmost-prefix scans with fetch-back; `key(…)` truncates the carried primary-key tail to the named subset (`encode_prefix_named`), full key by default; `includes` covering positioned as a materialized view for high-fanout queries.
- `Table<S, K, R>` node assembly point — `put`/`delete` write the primary key and every declared index entry in one store instance (the declaration IS the registry); `scan` returns `(Key, Option<Row>)` via leftmost-prefix on any access method.
- Engine backends behind Cargo features: `fjall` (sync `FjallStore`), `slatedb` (async `SlatedbStore` + `AsyncEdgeTable`), plus an in-memory `MockStore` for tests.
- Multi-engine mixing — different engines per ns segment in one process (fjall for transactions, slatedb for logs); atomicity stops at one engine, ns numbering globally unique.

Roadmap (design locked, not yet implemented — [ADR-0006](docs/adr/0006-row-node-model.md), [ADR-0004](docs/adr/0004-value-side-and-wrappers.md)):

- Field-level encoding wrappers (`Enum<T>`, `Offset<T>`, `Delta<T>`, `VarInt<T>`, `Reverse<T>` …) and variable-length payload/index fields (`String`), keys stay fixed-width.
- Snapshot export — rows → Parquet, engine-independent (backup / data exchange / lakehouse); ns restored to descriptive text, columns = field names.

## Usage

### 1. Define endpoint keys

```rust
use okm::{EdgeEncode, KeyEncode};

/// A user within an org. `org_id` is the organizational prefix;
/// `user_id` is the identity endpoint.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(1)] // compile-time namespace, folded into the key as big-endian bytes
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(2)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}
```

Physical layout of `UserKey { org_id: 7, user_id: 101 }`:

```
[ org_id: 4B BE ][ user_id: 8B BE ]   = 12 bytes, zero padding
```

### 2. Declare an edge between them

```rust
/// user → sessions edge.
///
/// Forward direction: a user's identity is (org_id, user_id) → kv_head(org_id, user_id)
/// Reverse direction: a session's identity is the full SessionKey (no kv_head).
///
/// The two directions of one edge use different endpoint identity widths —
/// this is how "the primary key changes with direction" is expressed.
#[derive(EdgeEncode, Clone)]
#[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}
```

`#[kv_head(field, ...)]` declares which fields of the endpoint count as its *identity* for this edge; omitting it means the full key is the identity. Names must be a declaration-order prefix of the endpoint's fields (compile-time generated check).

### 3. Link, unlink, query

```rust
use okm::EdgeTable;

let store = okm::MockStore::default(); // or FjallStore / SlatedbStore
let mut edges: EdgeTable<_, UserToSessionEdge> = EdgeTable::new(store);

let user = UserKey { org_id: 7, user_id: 101 };
let s1 = SessionKey { org_id: 7, session_id: 1001 };
let s2 = SessionKey { org_id: 7, session_id: 1002 };

edges.link(&user, &s1);   // atomic double write: forward + reverse key
edges.link(&user, &s2);

let sessions = user.get_session(&edges);   // forward: user → [SessionKey]
assert_eq!(sessions, vec![s1.clone(), s2.clone()]);

edges.unlink(&user, &s1); // deletes both directions
```

Physical key layout (forward):

```
[ head 2B: (ns<<1 | dir) BE ][ A·identity ][ B·identity ]
```

`ns = 4`, FWD → head `[0x08, 0x00]`; REV → `[0x08, 0x01]`. The direction bit is niched into the top bit of the namespace field — see [ADR-0001](docs/adr/0001-direction-bit-niche.md).

### 4. Reverse queries and truncated identities

```rust
// Reverse: session → users. A's identity here is truncated (kv_head),
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

### 5. Rows: declare a table with indexes

```rust
use okm::{RowEncode, Row};

/// A user row hangs off UserKey via #[kv_ref]; payload fields are TLV-encoded.
/// Each #[kv_index] declares an access method over PAYLOAD fields —
/// identity belongs to the key (a surrogate id), business dimensions to the row.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_index(by_reputation { fields(reputation) })]
#[kv_index(by_org { fields(org_id, created_at), includes(bio_len) })]
pub struct User {
    pub org_id: u32,
    pub created_at: u64,
    pub reputation: u32,
    pub bio_len: u16,
}
```

`fields(...)` names payload fields to sort/group by (declaration order, first
field = the grouping dimension); `includes(...)` copies extra payload fields
into the entry value — a covering index, positioned as a materialized view for
high-fanout queries; `key(...)` truncates the primary-key tail carried at the
key end to the named subset (default: the full key). Physical index entry
layout (ADR-0005):

```
[ ns 2B BE ][ indexed fields BE ][ primary key prefix (default: full) ]   value = included fields TLV (empty when no includes)
```

The 2-byte namespace is the only discriminator — no slot byte; every index
gets its own `ns` derived from the table's. `Table::put` writes the primary
key (value = TLV payload) and one entry per declared access method in the same
store instance — the declaration is the registry, no runtime index
bookkeeping. `Row::table` builds the assembly point without repeating the key
type at the call site:

```rust
use okm::{MockStore, Row};

let mut t = <User as Row>::table(MockStore::default(), 9);

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 });

// Leftmost-prefix scan on any access method, with fetch-back:
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
for (key, row) in rows {
    // key: decoded UserKey, row: Some(decoded User) when the payload exists
}

t.delete(&user); // removes the primary key + all declared index entries
```

Truncated keys change row-uniqueness, not grouping: the `fields` prefix drives
ordering, the key tail distinguishes rows. `key(user_id)` (dropping `org_id`
from the tail) is safe only when the named subset is unique per row —
otherwise rows overwrite each other's entries.

### 6. Engine backends

```toml
[dependencies]
okm = { version = "0.1", features = ["fjall"] }    # or "slatedb"
```

- **fjall** (sync): `FjallStore::open(path)` — local LSM engine, single `Database` handle, `persist` on demand.
- **slatedb** (async): `SlatedbStore::open(path, Arc<dyn ObjectStore>)` — object-storage-backed; use `slatedb::object_store` re-exports to construct stores so versions always match slatedb's internals. Async traversal goes through `AsyncEdgeTable`.
- **MockStore**: in-memory `BTreeMap` with memcmp ordering — identical iteration semantics to real engines, used by the test suite.

### 7. Schema stability tests

Lock the physical bytes with hard-coded hex — any layout drift fails CI:

```rust
let fk = edge.forward_key();
assert_eq!(&fk[..2], &[0, 8]); // ns=4, FWD — direction bit in the low bit of the BE pair
assert_eq!(&fk[2..6], &7u32.to_be_bytes());
// ... full layout assertions in okm/tests/integration.rs
```

## Project layout

```
okm-derive/        proc-macro crate: KeyEncode, RowEncode, EdgeEncode (zero I/O)
okm/src/key.rs     KeyEncode trait + PrefixKey
okm/src/index.rs   Row + KvIndex traits, index scan helpers
okm/src/edge.rs    KvEdge trait + direction-bit header
okm/src/engine.rs  KvEngine trait + MockStore
okm/src/table.rs       Table<S, K, R> node assembly point
okm/src/collection.rs  EdgeTable<S, E> edge assembly point
okm/src/fjall_backend.rs    fjall adapter (feature "fjall")
okm/src/slatedb_backend.rs  slatedb adapter (feature "slatedb")
okm/tests/         integration + index_test (MockStore), fjall_eval, slatedb_eval
docs/adr/          architecture decision records (docs/PLAN.md = implementation plan)
```

## Design notes

- **Namespace stays in code** — the namespace dictionary is compile-time constants, never stored in KV. The access pattern itself lives in code (binary keys, no separators, per-field widths); putting ns in code is the same act as putting the key layout in code. Macros run at compile time when no KV exists to read from — a dictionary in KV is a bootstrap deadlock. Numbers are manually assigned, append-only, never reused; see [ADR-0002](docs/adr/0002-namespace-dictionary.md).
- **Two layout regimes** — primary keys are fixed-width (zero parsing, hot path); secondary indexes are variable-length (text as discriminating prefix, UTF-8 byte order = dictionary scan order, primary-key ID appended at the key tail, value left empty). Width is a property of *structure*, not *data*; the discriminator is access pattern: point-lookup-only may hash to fixed width, anything needing prefix/range scan must keep raw text. Variable-length fields use a length prefix `[len: u16][bytes]` over NUL termination (no escaping burden); a fixed-width field *after* a variable-length one loses its compile-time offset and falls back to a runtime cursor — "fixed-width prefix + variable tail" keeps most of the zero-parsing benefit. Hex stability tests still apply to variable-length keys: what they lock is the encoding scheme itself (prefix layout, length endianness, limits), not specific bytes. The name→id index is the mainstream case and is almost always variable-length, since an index exists to answer prefix/range queries. See the [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) essay for the full argument.
- **Macro layer is deliberately storage-free** — encode/decode are pure `Vec<u8>` in/out functions; engine choice and lifecycle belong to the assembly site (`EdgeTable::new(store)` / `<Row>::table(store, ns)`). This is what keeps each derive a single-item pure function.
- **Portability**: the paradigm is bytes-level and host-language independent — a Python dataclass with the same `encode()` reproduces the layout, at the price of moving guarantees from compile time to runtime assertions (SlateDB's Python bindings via UniFFI provide the needed primitives: `get` / `scan_prefix` + `KeyRange` / `WriteBatch` / transactions). Portability has a structural cost, however: type errors move from compile time to runtime (`assert` instead of the compiler), encoding is byte-concatenation rather than memcpy-level offsets (1–2 orders of magnitude slower on hot paths), and decorators/metaclass registration is a runtime cost instead of a compile-time expansion. Same paradigm, guarantee level set by the host language.

## Why not just use a (ready-made) database?

A side benefit of OKM: engine selection anxiety disappears. The actual menu is long — PostgreSQL, DuckDB, Lakehouse, SurrealDB… — and OKM speaks plain bytes, so any engine that can put and get them qualifies.


## License

To be decided upon publication.
