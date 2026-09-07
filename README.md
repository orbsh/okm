# OKM — Object-Keyspace Mapping

OKM is the KV counterpart of ORM: ORM maps objects onto relational tables, OKM maps objects onto KV keyspaces. Declarative derive macros (`#[derive(KeyEncode)]` / `#[derive(EdgeEncode)]`) plus numeric namespace IDs build a zero-cost semantic data layer — as declarative as an ORM at development time, compiled down to pure pointer-offset arithmetic.

Related reading: [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) — underlying architecture and design patterns (encoding principles, index strategies, engine-level trade-offs).

## Why: code as DDL

SQL's core value is not execution performance — it is the readability, modeling discipline, and team-coordination determinism delivered by the relational model. Raw binary keys degrade into spaghetti: nobody on the team can reason about key layout rules. The solution is to replace SQL DDL with the Rust type system, moving schema correctness from a runtime database engine to the compile-time compiler.

- **Struct as DDL**: SQL defines table structure; a Rust struct defines key encoding. The compiler enforces format consistency — any code attempting to write a malformed key fails at compile time, no runtime validation.
- **Schema-less is a trap**: schema-less stores accept anything (`"25"`, `[25]` for a numeric field), and every consuming service pays for it in defensive parsing. Strong typing fuses invalid data at `cargo check` — it never even gets generated — while keeping KV's hardware-level speed and skipping PG's runtime DDL-lock and SQL-parsing taxes.
- **Hex stability tests**: the last line of defense against encoding drift. Hard-coded historical hex bytes lock the physical key layout; any change to ns, field order, or widths fails CI immediately (see `okm/tests/integration.rs`).

## Physical gains

- **85% prefix compression**: a 14-byte string prefix becomes 2 bytes of numeric namespace.
- **100% fixed-width keys** (for full-identity keys): every field's absolute byte offset is compile-time locked; decoding is pure pointer slicing, zero parsing.
- **32768 namespaces**: u16 with the top bit niched as the edge direction bit (see [ADR-0001](docs/adr/0001-direction-bit-niche.md)).
- **Zero runtime overhead**: no regex, no split, no AST — the query path is machine code after `cargo build --release`.

## Status

Implemented:

- `KeyEncode` — fixed-width key encoding (`u32` / `u64` / `[u8; N]`), big-endian, compile-time `KEY_LEN` / `FIELD_WIDTHS`, `encode_prefix_named` truncation primitive.
- `EdgeEncode` — bidirectional edges with per-endpoint identity width (`#[kv_head(...)]`), 2-byte direction-bit header, query methods generated onto endpoint types.
- `Collection<S, E>` — the assembly point: engine + edge type = the operation surface of one relationship (`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`).
- Engine backends behind Cargo features: `fjall` (sync `FjallStore`), `slatedb` (async `SlatedbStore` + `AsyncCollection`), plus an in-memory `MockStore` for tests.

Roadmap (design locked, not yet implemented — [ADR-0004](docs/adr/0004-value-side-and-wrappers.md), [ADR-0005](docs/adr/0005-secondary-index-slots.md)):

- `ValueEncode` — versioned value payload (lazy migration) and TLV extension section.
- Field-level encoding wrappers (`Enum<T>`, `Offset<T>`, `Delta<T>`, `VarInt<T>`, `Reverse<T>` …).
- Secondary indexes — `#[kv_index(name { fields(…) })]` on table structs: composite indexes, item-local slot numbering (one manual ns per **table**), 1-byte slot discriminator, leftmost-prefix scans.
- Variable-length key fields (`String` with `[len: u16]` prefix), for secondary indexes.

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
use okm::Collection;

let store = okm::MockStore::default(); // or FjallStore / SlatedbStore
let mut edges: Collection<_, UserToSessionEdge> = Collection::new(store);

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

### 5. Engine backends

```toml
[dependencies]
okm = { version = "0.1", features = ["fjall"] }    # or "slatedb"
```

- **fjall** (sync): `FjallStore::open(path)` — local LSM engine, single `Database` handle, `persist` on demand.
- **slatedb** (async): `SlatedbStore::open(path, Arc<dyn ObjectStore>)` — object-storage-backed; use `slatedb::object_store` re-exports to construct stores so versions always match slatedb's internals. Async traversal goes through `AsyncCollection`.
- **MockStore**: in-memory `BTreeMap` with memcmp ordering — identical iteration semantics to real engines, used by the test suite.

### 6. Schema stability tests

Lock the physical bytes with hard-coded hex — any layout drift fails CI:

```rust
let fk = edge.forward_key();
assert_eq!(&fk[..2], &[0, 8]); // ns=4, FWD — direction bit in the low bit of the BE pair
assert_eq!(&fk[2..6], &7u32.to_be_bytes());
// ... full layout assertions in okm/tests/integration.rs
```

## Project layout

```
okm-derive/        proc-macro crate: KeyEncode, EdgeEncode (zero I/O)
okm/src/key.rs     KeyEncode trait + PrefixKey
okm/src/edge.rs    KvEdge trait + direction-bit header
okm/src/engine.rs  KvEngine trait + MockStore
okm/src/collection.rs  Collection<S, E> assembly point
okm/src/fjall_backend.rs    fjall adapter (feature "fjall")
okm/src/slatedb_backend.rs  slatedb adapter (feature "slatedb")
okm/tests/         integration (MockStore), fjall_eval, slatedb_eval
docs/adr/          architecture decision records (docs/PLAN.md = implementation plan)
```

## Design notes

- **Namespace stays in code** — the namespace dictionary is compile-time constants, never stored in KV. The access pattern itself lives in code (binary keys, no separators, per-field widths); putting ns in code is the same act as putting the key layout in code. Macros run at compile time when no KV exists to read from — a dictionary in KV is a bootstrap deadlock. Numbers are manually assigned, append-only, never reused; see [ADR-0002](docs/adr/0002-namespace-dictionary.md).
- **Two layout regimes** — primary keys are fixed-width (zero parsing, hot path); secondary indexes are variable-length (text as discriminating prefix, UTF-8 byte order = dictionary scan order, primary-key ID appended at the key tail, value left empty). Width is a property of *structure*, not *data*; the discriminator is access pattern: point-lookup-only may hash to fixed width, anything needing prefix/range scan must keep raw text. See the [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) essay for the full argument.
- **Macro layer is deliberately storage-free** — encode/decode are pure `Vec<u8>` in/out functions; engine choice and lifecycle belong to the assembly site (`Collection::new(store)`). This is what keeps each derive a single-item pure function.
- **Portability**: the paradigm is bytes-level and host-language independent — a Python dataclass with the same `encode()` reproduces the layout, at the price of moving guarantees from compile time to runtime assertions.

## License

To be decided upon publication.
