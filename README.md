# OKM — Object-Keyspace Mapping

> ORM experience, Redis speed, PostgreSQL durability, and functions without boundaries.

OKM is the KV counterpart of ORM: the logic is defined ORM-style, code generation produces the underlying KV operations. ORM maps objects onto relational tables, OKM maps objects onto KV keyspaces. Declarative derive macros (`#[derive(KeyEncode)]` / `#[derive(JunctionEncode)]`) plus numeric namespace IDs build a zero-cost semantic data layer — as declarative as an ORM at development time, compiled down to pure pointer-offset arithmetic.

Related reading: [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) — underlying architecture and design patterns (encoding principles, index strategies, engine-level trade-offs); [Architecture](docs/ARCHITECTURE.md) — the two-layer declaration machine (derive expansion + constant tables); [Modeling Guide](docs/MODELING.md) — the normative schema-modeling method (four layers, mandatory access methods, covering-index restraint, composite-key boundary); [Query Recipes](docs/query-recipes.md) — composing merge_join / group_by / walk into SQL shapes, the WHERE boundary record.

## Why: code as DDL

SQL's core value is not execution performance — it is the readability, modeling discipline, and team-coordination determinism delivered by the relational model. Raw binary keys degrade into spaghetti: nobody on the team can reason about key layout rules. The solution is to replace SQL DDL with the Rust type system, moving schema correctness from a runtime database engine to the compile-time compiler.

- **Struct as DDL**: SQL defines table structure; a Rust struct defines key encoding. The compiler enforces format consistency — any code attempting to write a malformed key fails at compile time, no runtime validation.
- **Schema-less is a trap**: schema-less stores accept anything (`"25"`, `[25]` for a numeric field), and every consuming service pays for it in defensive parsing. Strong typing fuses invalid data at `cargo check` — it never even gets generated — while keeping KV's hardware-level speed and skipping PG's runtime DDL-lock and SQL-parsing taxes.
- **Hex stability tests**: the last line of defense against encoding drift. Hard-coded historical hex bytes lock the physical key layout; any change to ns, field order, or widths fails CI immediately (see `okm-core/tests/integration.rs`).

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
- `JunctionEncode` — junctions (SQL many-to-many junction tables): one one-way entry per endpoint ns, 4-byte head `[ns u16][slot u16]` (segment 0x3), per-endpoint identity width (`#[ok_head(...)]`), `#[ok_junction(n)]` discriminator, query methods generated onto endpoint key types.
- `Junction<S, E>` — the junction assembly point: engine + junction type = the operation surface of one relationship (`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`).
- `EdgeEncode` + `Graph<S, E>` + `KvGraph`/`NodeRef` — graph edges, the third relation carrier (ADR-0017): open endpoints as self-describing `[ns 2B][len][pkey]` refs (width rides in the ref — no registry), parallel edges with caller-chosen ids, declared attribute fields as 0x1 faces, kind names through the edge collection's dictionary; `link`/`unlink` write/delete all faces in one batch; typed/untyped/kind/attribute scans.
- `DocumentEncode` — one macro declares a row (Node): `#[ok_ref]` identity + TLV payload fields + `#[ok_index(...)]` access methods; the `ValueEncode` derive is absorbed into it.
- Secondary indexes (access methods) — `#[ok_index(name { fields(…), includes(…), key(…) })]` on **row structs**: composite indexes over payload fields (declaration order), no per-index slot/ns — the 2-byte table namespace already discriminates every entry; leftmost-prefix scans with fetch-back; `key(…)` truncates the carried primary-key tail to the named subset (`encode_prefix_named`), full key by default; `includes` covering positioned as a materialized view for high-fanout queries.
- `Collection<S, K, R>` node assembly point — `put`/`delete` write the primary key and every declared index entry in one store instance (the declaration IS the registry); `scan` returns `(Key, Option<Row>)` via leftmost-prefix on any access method.
- **Document API** — `get_document` / `put_document` / `get_fields` / `put_fields` / `delete_fields`: unknown field names allocate in the per-table field-name dictionary and land in the dynamic segment (n-TLV frames, nested objects recursive, no CBOR); the row-map bridge lifts declared fields to logical types (`Quant`→`F64`, `Enum`→variant name, `Offset`→`i64`).
- **Dynamic codec bindings** — schema export drives embedded-language readers: `bindings/okm-python` (PyO3) and `bindings/okm-steel`, byte-identical with the Rust derive.
- Engine backends behind Cargo features: `fjall` (sync `FjallStore`), `slatedb` (async `SlatedbStore` + `AsyncJunction`), plus an in-memory `MockStore` for tests.
- Multi-engine mixing — different engines per ns segment in one process (fjall for transactions, slatedb for logs); atomicity stops at one engine, ns numbering globally unique.
- Field-level encoding wrappers (`Enum<T>`, `Offset<T>`, `VarInt<T>`, `Quant<P>`, `Reverse<T>`, `Option<T>`) and variable-length payload/index fields (`String`), keys stay fixed-width.
- Snapshot export — rows → Parquet, engine-independent (backup / data exchange / lakehouse); ns restored to descriptive text, columns = field names.
- **Document model** — one encoding for declared rows and external data ([ADR-0012](docs/adr/0012-object-model-and-field-dictionary.md)). OKM is document-oriented: a document is logically all-dynamic (any field may appear at run time), and **declared static fields embed into the dynamic whole** — hot/cold segments, indexing, and schema export apply to them as usual. A declared struct is the degenerate document with only the static path; a pure document (MQ payload, JSON) is the degenerate document with only the dynamic path; both are the same encoding. Terms on the embedding spectrum:
  - **variant** — a dynamic blob nested as ONE declared static field (`Bytes` payload); the dynamics live inside a value, not in the key layout.

  Dynamic frames use a CBOR-derived framing, not CBOR itself: only the **major-type pattern** is taken — a type nibble opening each frame — and struct encoding rides the same scheme. CBOR's major types include lists and maps; with a field-name dictionary OKM needs only the list: a map is an n-TLV list (number, type, len, value), with structure information living in the values, not in a per-document schema.

## Usage

### 1. Define endpoint keys and edges (declarations)

The full declaration vocabulary (`KeyEncode` / `JunctionEncode` / `DocumentEncode`,
the `fields`/`includes`/`key` annotations of `#[ok_index]`) is in the
[Modeling Guide](docs/MODELING.md), "Declaration basics". Summary:

```rust
#[derive(KeyEncode)] #[ok_ns(1)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(JunctionEncode)] #[ok_junction(1)]
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}
```

### 2. Runtime: linking and reading/writing (overview)

Full usage (reverse queries, truncated identities, scans with fetch-back,
schema stability tests) is in the [Modeling Guide](docs/MODELING.md),
runtime subsections after "Declaration basics".

```rust
// Junction: two one-way entries (one per endpoint ns) + queries in both directions
let mut edges: Junction<_, UserToSession> = Junction::new(store);
edges.link(&user, &s1);
let sessions = user.get_session(&edges);

// Row: writes the primary key + all index entries; scan by access method
let mut t = <User as Document>::collection(store);
t.put(&user, &user_row);
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
```

`ByOrg` comes from the index name: `ok_index(by_org ...)` generates the
type `__OkmIndex_User_by_org` (mechanical concatenation, no case
conversion); `use __OkmIndex_User_by_org as ByOrg` gives the short form.
For declarations see the [Modeling Guide](docs/MODELING.md), "Declaration
basics".

### 3. Dynamic fields: the document API

One encoding for declared rows and external data. Unknown field names are
normal input: they allocate in the per-table field-name dictionary and
land in the dynamic segment (`[id][type][len][value]` frames, nested
objects recursive). Declared fields stay typed and indexable.

```rust
// read: declared fields lifted to logical types + dynamic fields by name
let (row, dynamic) = t.get_document(&key);
// write: matched names -> typed path; unknown names -> dynamic segment
t.put_document(&key, &fields);
```

Embedded-language readers (Python / Steel Actors) consume the schema
export through the dynamic codec — `bindings/okm-python` (PyO3) and
`bindings/okm-steel` encode/decode byte-identically with the Rust
derive; version-default migration applies on that path too.

### 4. Engine backends

```toml
[dependencies]
okm-core = { version = "0.1", features = ["fjall"] }    # or "slatedb"
```

- **fjall** (sync): `FjallStore::open(path)` — local LSM engine, single `Database` handle, `persist` on demand.
- **slatedb** (async): `SlatedbStore::open(path, Arc<dyn ObjectStore>)` — object-storage-backed; use `slatedb::object_store` re-exports to construct stores so versions always match slatedb's internals. Async traversal goes through `AsyncJunction`.
- **MockStore**: in-memory `BTreeMap` with memcmp ordering — identical iteration semantics to real engines, used by the test suite.

## Project layout

```
okm-derive/        proc-macro crate: KeyEncode, DocumentEncode, JunctionEncode (zero I/O)
okm-core/src/key.rs     KeyEncode trait + PrefixKey
okm-core/src/index.rs   Row + KvIndex traits, index scan helpers
okm-core/src/junction.rs    KvJunction trait + segment-0x3 slot keys
okm-core/src/storage.rs  VirtualStorage trait + MockStore
okm-core/src/table.rs       Collection<S, K, R> node assembly point
okm-core/src/collection.rs  Junction<S, E> junction assembly point
okm-core/src/fjall_backend.rs    fjall adapter (feature "fjall")
okm-core/src/slatedb_backend.rs  slatedb adapter (feature "slatedb")
okm-query/         extension operator crate: merge_join, group_by (consumes scan streams, zero core changes)
okm-core/tests/         integration + index_test (MockStore), fjall_eval, slatedb_eval
docs/adr/          architecture decision records (docs/PLAN.md = implementation plan)
```

## Design notes

- **Namespace stays in code** — the namespace dictionary is compile-time constants, never stored in KV. The access pattern itself lives in code (binary keys, no separators, per-field widths); putting ns in code is the same act as putting the key layout in code. Macros run at compile time when no KV exists to read from — a dictionary in KV is a bootstrap deadlock. Numbers are manually assigned, append-only, never reused; see [ADR-0002](docs/adr/0002-namespace-dictionary.md).
- **Two layout regimes** — primary keys are fixed-width (zero parsing, hot path); secondary indexes are variable-length (text as discriminating prefix, UTF-8 byte order = dictionary scan order, primary-key ID appended at the key tail, value left empty). Width is a property of *structure*, not *data*; the discriminator is access pattern: point-lookup-only may hash to fixed width, anything needing prefix/range scan must keep raw text. Variable-length fields use a length prefix `[len: u16][bytes]` over NUL termination (no escaping burden); a fixed-width field *after* a variable-length one loses its compile-time offset and falls back to a runtime cursor — "fixed-width prefix + variable tail" keeps most of the zero-parsing benefit. Hex stability tests still apply to variable-length keys: what they lock is the encoding scheme itself (prefix layout, length endianness, limits), not specific bytes. The name→id index is the mainstream case and is almost always variable-length, since an index exists to answer prefix/range queries. See the [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) essay for the full argument.
- **Macro layer is deliberately storage-free** — encode/decode are pure `Vec<u8>` in/out functions; engine choice and lifecycle belong to the assembly site (`Junction::new(store)` / `<Document>::collection(store)`). This is what keeps each derive a single-item pure function.
- **Portability**: the paradigm is bytes-level and host-language independent — a Python dataclass with the same `encode()` reproduces the layout, at the price of moving guarantees from compile time to runtime assertions (SlateDB's Python bindings via UniFFI provide the needed primitives: `get` / `scan_prefix` + `KeyRange` / `WriteBatch` / transactions). Portability has a structural cost, however: type errors move from compile time to runtime (`assert` instead of the compiler), encoding is byte-concatenation rather than memcpy-level offsets (1–2 orders of magnitude slower on hot paths), and decorators/metaclass registration is a runtime cost instead of a compile-time expansion. Same paradigm, guarantee level set by the host language.

## Why not just use a (ready-made) database?

A side benefit of OKM: engine selection anxiety disappears. The actual menu is long — PostgreSQL, DuckDB, Lakehouse, SurrealDB… — and OKM speaks plain bytes, so any engine that can put and get them qualifies.


## License

To be decided upon publication.
