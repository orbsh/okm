# OKM — Object-Keyspace Mapping

> ORM experience, Redis speed, PostgreSQL durability, and functions without boundaries.

OKM is the KV counterpart of ORM: the logic is defined ORM-style, code generation produces the underlying KV operations. ORM maps objects onto relational tables, OKM maps objects onto KV keyspaces. Declarative derive macros (`KeyEncode` / `DocumentEncode` / `JunctionEncode` / `GraphEncode`) plus numeric namespace IDs build a zero-cost semantic data layer — as declarative as an ORM at development time, compiled down to pure pointer-offset arithmetic.

Related reading: [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) — underlying architecture and design patterns (encoding principles, index strategies, engine-level trade-offs); [Architecture](docs/ARCHITECTURE.md) — the two-layer declaration machine (derive expansion + constant tables); [Modeling Guide](docs/MODELING.md) — the normative schema-modeling method (four layers, mandatory access methods, covering-index restraint, composite-key boundary); [Query Recipes](docs/query-recipes.md) — composing merge_join / group_by / walk into SQL shapes, the WHERE boundary record; [Graph Query Recipes](docs/graph-queries.md) — the graph's faces composed into Cypher-shaped traversals.

## Why: code as DDL

SQL's core value is not execution performance — it is the readability, modeling discipline, and team-coordination determinism delivered by the relational model. Raw binary keys degrade into spaghetti: nobody on the team can reason about key layout rules. The solution is to replace SQL DDL with the Rust type system, moving schema correctness from a runtime database engine to the compile-time compiler.

- **Struct as DDL**: SQL defines table structure; a Rust struct defines key encoding. The compiler enforces format consistency — any code attempting to write a malformed key fails at compile time, no runtime validation.
- **Schema-less is a trap**: schema-less stores accept anything (`"25"`, `[25]` for a numeric field), and every consuming service pays for it in defensive parsing. Strong typing fuses invalid data at `cargo check` — it never even gets generated — while keeping KV's hardware-level speed and skipping PG's runtime DDL-lock and SQL-parsing taxes.
- **Hex stability tests**: the last line of defense against encoding drift. Hard-coded historical hex bytes lock the physical key layout; any change to ns, field order, or widths fails CI immediately (see `okm-core/tests/integration.rs`).

The verdict: SQL's core value is human-facing structured discipline. By holding to "code as DDL" — strongly-typed key encoding, lazy migration via versioned enums, dual-write key-pointer contracts, and hard-coded hex stability tests — OKM gains all of it at once: compile-time schema safety (the Rust compiler) + runtime performance (LSM-Tree) + zero-downtime evolution (versioned enums) + team maintainability (struct comments as documentation) + drift protection (hex tests). On schema safety it overtakes both SurrealDB (schema-less) and PostgreSQL (runtime DDL locks).

## Two modes: compiled schema and declared schema

OKM carries ONE storage engine contract and ONE key layout, reached by two schema carriers:

- **Static mode (code generation)** — Rust types + `#[derive(...)]` (okm-derive). The compiler IS the schema validator: key widths, offsets, and index declarations are compile-time facts; `Collection<S, K, R>` is the assembly point. For Rust hosts and wasm booths compiled from Rust source.
- **Dynamic mode (schema data)** — a `CollectionSchema` value (exported from the static side by `okm_core::schema::CollectionSchema::of`, or authored as data) + okm-dynamic. `DynamicCollection` takes a run-time ns and the schema value; encode/decode mirrors the derive byte-for-byte. For embedded-language booths (Python/Steel via the bindings), hosts assembling collections at run time (aura ADR-0026: one real ns per booth type, allocated from a registry).

The two modes produce IDENTICAL bytes for the same declaration: same header discipline, same key encoding, same payload frames, same vocabulary behavior. Data written by a compiled collection reads back through the dynamic one and vice versa — mode is a property of the WRITER, not the data. External API is aligned on purpose (put/get/scan/delete + document map), so application code shapes do not fork.

Division of labor across crates: okm-core = the shared runtime (engine contract, `Collection`, document/dynamic-segment machinery, reduce, subscribe); okm-derive = static codegen; okm-dynamic = the schema-driven codec and `DynamicCollection`; okm-wire = the wire format for remote engines; okm-query/stream/graph/vector/ngram = consumer-side operators above the core.

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
- `GraphEncode` + `Graph<S, E>` + `NodeSet`/`NodeRef` — graph edges, the third relation carrier (ADR-0017): ONE declaration is the graph (ns + the edge's own payload + explicit `#[ok_index]` faces + the `#[ok_nodes]` member set), open endpoints as self-describing `[ns 2B][len][pkey]` refs (width rides in the ref — no registry), parallel edges with caller-chosen ids, predicates through the graph's own PREDICATE vocabulary (0x04/0x05 — listing them is one prefix scan), declared attribute faces as ordinary counting-family 0x1 indexes; `link`/`unlink` write and delete every face in one batch, `unlink` decoding the payload to delete exactly; untyped/typed/predicate/attribute scans.
- `DocumentEncode` — one macro declares a row (Node): `#[ok_ref]` identity + TLV payload fields + `#[ok_index(...)]` access methods; the `ValueEncode` derive is absorbed into it.
- Secondary indexes (access methods) — `#[ok_index(name { fields(…), includes(…), key(…), func(…), where(…) })]` on **row structs**: composite indexes over payload fields (declaration order), no per-index slot/ns — the 2-byte table namespace already discriminates every entry; leftmost-prefix scans with fetch-back; `key(…)` truncates the carried primary-key tail to the named subset (`encode_prefix_named`), full key by default; `includes` covering positioned as a materialized view for high-fanout queries; `func(path)` replaces the data segment with a plain fn's return value (probe side calls the same path); `where(path)` is a row-level predicate — a partial index, rows the predicate rejects contribute no entries.
- `Collection<S, K, R>` node assembly point — `put`/`delete` write the primary key and every declared index entry in one store instance (the declaration IS the registry); `scan` returns `(Key, Option<Row>)` via leftmost-prefix on any access method.
- **Document API** — `get_document` / `put_document` / `get_fields` / `put_fields` / `delete_fields`: unknown field names allocate in the per-table field-name vocabulary and land in the dynamic segment (n-TLV frames, nested objects recursive, no CBOR); the row-map bridge lifts declared fields to logical types (`Quant`→`F64`, `Vocab`→label, `Offset`→`i64`).
- **Dynamic codec bindings** — schema export drives embedded-language readers: `bindings/okm-python` (PyO3) and `bindings/okm-steel`, byte-identical with the Rust derive.
- **Path indexes over the dynamic segment** ([ADR-0029](docs/adr/0029-path-index.md)) — `AccessMethod::path(slot, Path::parse("author.name")?, FieldType::Str)?`: nested and array paths declared as access methods, one entry per resolved value (a wildcard is the absence of an `Index` component), maintained on the dynamic-segment write path; the value type is declared, and the entry key carries the slot, never the path.
- Engine backends behind Cargo features: `fjall` (sync `FjallStore`), `redb` (sync `RedbStore`), `slatedb` (async `SlatedbStore` + `AsyncJunction`); tests run the real engines through the `TestStore` matrix (slatedb-mem / fjall / redb), no mock to drift from them.
- **Semantic batch write path** ([ADR-0031](docs/adr/0031-batch-write-semantics.md)) — `BatchStore` (a batch as a writer: point reads see pending ops newest-first, scans refused) + `Collection::save_with_prev_into(batch, key, doc, prev)`: primary + faces into the caller's `MemBatch`, the reduce fold/unfold applied through the adapter, and the faces the new document no longer produces deleted — so a table with a ledger joins a multi-table atomic commit instead of re-implementing it.
- Multi-engine mixing — different engines per ns segment in one process (fjall for transactions, slatedb for logs); atomicity stops at one engine, ns numbering globally unique.
- Field-level encoding wrappers (`Vocab<T>`, `Offset<T>`, `VarInt<T>`, `Quant<P>`, `Reverse<T>`, `Vector<T>`, `Bytes`) and variable-length payload/index fields (`String`), keys stay fixed-width.
- Snapshot export — rows → Parquet, engine-independent (backup / data exchange / lakehouse); ns restored to descriptive text, columns = field names.
- **Document model** — one encoding for declared rows and external data ([ADR-0012](docs/adr/0012-object-model-and-field-dictionary.md)). OKM is document-oriented: a document is logically all-dynamic (any field may appear at run time), and **declared static fields embed into the dynamic whole** — hot/cold segments, indexing, and schema export apply to them as usual. A declared struct is the degenerate document with only the static path; a pure document (MQ payload, JSON) is the degenerate document with only the dynamic path; both are the same encoding. Terms on the embedding spectrum:
  - **variant** — a dynamic blob nested as ONE declared static field (`Bytes` payload); the dynamics live inside a value, not in the key layout.

  Dynamic frames use a CBOR-derived framing, not CBOR itself: only the **major-type pattern** is taken — a type nibble opening each frame — and struct encoding rides the same scheme. CBOR's major types include lists and maps; with a field-name vocabulary OKM needs only the list: a map is an n-TLV list (number, type, len, value), with structure information living in the values, not in a per-document schema.

## Usage

### 1. Declarations: keys, rows, and relations

The full declaration vocabulary (`KeyEncode` / `DocumentEncode` /
`JunctionEncode` / `GraphEncode`, the `fields`/`includes`/`key`/`func`/`where`
annotations of `#[ok_index]`) is in the [Modeling Guide](docs/MODELING.md),
"Declaration basics". The four shapes:

```rust
#[derive(KeyEncode)]                        // identity: fixed-width fields, BE, no separators
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode)]                   // row: identity + payload + access methods
#[ok_ref(UserKey)]                          // the row pins its key type
#[ok_ns(1)]                                 // the ns is declared HERE, not at the assembly site
#[ok_index(by_org { fields(org_id) })]      // a face: entry key = indexed fields + key tail
pub struct User { pub org_id: u32, pub bio_len: u32 }

#[derive(JunctionEncode)] #[ok_junction(1)] // many-to-many with typed endpoints
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}

#[derive(GraphEncode, Clone, Default)]       // ONE declaration IS the graph (ADR-0017)
#[ok_ns(100)]
#[ok_nodes(EmploymentNodes)]                 // the member set: a compile-time enum of collections
#[ok_index(by_since { fields(since_year) })] // attributes are declared faces — nothing implied
pub struct Employment { pub since_year: u16, pub weight: u32 }

pub enum EmploymentNodes { Users, Orgs }
impl NodeSet for EmploymentNodes { const NS_VALUES: &'static [u16] = &[10, 11]; }
```

An index declaration names its access-method type mechanically:
`ok_index(by_org ...)` generates `__OkmIndex_User_by_org` (no case
conversion), which is what `scan::<…>` takes — `use __OkmIndex_User_by_org
as ByOrg` gives the short form.

### 2. Documents at runtime (`Collection`)

```rust
use okm_core::{Collection, Document};

let mut users = <User as Document>::collection(store.clone());
let key = UserKey { org_id: 7, user_id: 101 };

// Write: primary entry + one entry per declared face, one store instance.
users.put(&key, &User { org_id: 7, bio_len: 240 });
let row: Option<User> = users.get(&key);

// Read by access method: leftmost-prefix scan, then fetch-back.
for (pk, row) in users.scan::<ByOrg>(&7u32.to_be_bytes()) {
    // pk.decoded: UserKey, row: Option<User>
}

// A comparison on the leading index field is a physical key interval —
// byte order is value order, so this reads only rows inside [1, 100).
users.scan_range::<ByOrg>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()));

// Delete: primary + exactly the faces this document produced.
users.delete(&key, &row.expect("row"));
```

Overwrite semantics differ by design: `put` unfolds the stored document from
every declared reduce group before folding the new one in, while faces are
per-document derivations — an overwrite whose indexed value changed lands at
a NEW entry key, so the stale face is the caller's to remove (or
`save_with_prev_into`'s, section 5).

A ledger is a declaration line, not a table you maintain (ADR-0023):

```rust
#[ok_reduce(Count { group(org_id) })]          // rows per group, exactly reversible
#[ok_reduce(Sum(bio_len) { group(org_id) })]   // field total; signed and Quant stay exact
```

The accumulator entry is `[ns 2B][reduce slot][group segment]`, read-modify-written
by the same `put`/`delete` that writes the row — one store instance, so the
ledger shares the row's atomicity.

### 3. Dynamic fields: the document API

One encoding for declared rows and external data. Unknown field names are
normal input: they allocate in the per-table field-name vocabulary and
land in the dynamic segment (`[id][type][len][value]` frames, nested
objects recursive). Declared fields stay typed and indexable.

```rust
// Read: declared fields lifted to logical types, merged over dynamic ones.
// One name-keyed map — the object model, not a row plus an attachment.
let obj = users.get_document(&key)?;          // BTreeMap<String, DynamicValue>

// Write: names matching the declaration take the typed path (indexes,
// reduces and events included); unknown names land in the dynamic segment.
users.put_document(&key, &obj);

// The segment alone, when the caller owns both halves separately:
let dynamic = users.get_fields(&key);         // Option<BTreeMap<String, DynamicValue>>
users.put_fields(&key, &dynamic.unwrap_or_default());
```

Embedded-language readers (Python / Steel Booths) consume the schema
export through the dynamic codec — `bindings/okm-python` (PyO3) and
`bindings/okm-steel` encode/decode byte-identically with the Rust
derive; version-default migration applies on that path too.

### 4. Relations at runtime: junction and graph

```rust
// Junction: two one-way entries (one per endpoint ns), queries both ways.
let mut edges: Junction<_, UserToSession> = Junction::new(store.clone());
edges.link(&user, &session);                  // forward + reverse, one instance
let sessions = user.get_session(&edges);      // method generated on the key type
edges.unlink(&user, &session);

// Graph: endpoints are self-describing refs — no registry, no width table.
let mut g: Graph<_, Employment> = Graph::new(store.clone());
g.link(&EdgeHead {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),  // [ns][len][pkey]
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    predicate: "employs".into(),                                  // runtime data, vocabularised
}, &Employment { since_year: 2020, weight: 7 }, 1)?;             // edge id is the caller's

g.out_nodes(&user_ref);                       // one-hop neighbours as NodeRefs
g.typed_out("employs", &user_ref);            // predicate-qualified traversal (0x09 face)
g.in_edges(&org_ref);                         // who points here
g.predicates();                               // every relation type: one 0x04 prefix scan
g.by_attr_face(<BySince as KvIndex>::SLOT, &2020u16.to_be_bytes()); // declared attribute face
g.unlink(1)?;                                 // decodes the body, deletes exactly those faces
```

Division of labour: `Refs`/`Junction` are for fixed, typed endpoint pairs;
a graph edge is for open endpoints, runtime predicates, parallel edges and
attributes. Node "types" stay ordinary document collections — the ns is the
type marker, so node-kind filtering is that collection's own index.

### 5. Multi-table atomic writes: the batch path

One `MemBatch`, one commit, everything or nothing (ADR-0003). Document
writes encode with `save_with_prev_into`, edges and junctions with their
`*_into` siblings:

```rust
let mut batch = MemBatch::default();
users.save_with_prev_into(&mut batch, &key, &new_row, Some(&old_row));
g.link_into(&mut batch, &head, &attrs, 7)?;
store.commit_batch(batch)?;                    // rows, faces and ledgers land together
```

`save_with_prev_into` is the SEMANTIC sibling of `save_into` (encoding only):
the reduce fold/unfold and the stale-face cleanup run through a `BatchStore`
— a read-through view of the batch, so a same-group fold sees the value the
same batch already produced instead of double-counting
([ADR-0031](docs/adr/0031-batch-write-semantics.md)). Subscribe emission stays
out of the batch path by decision: a batch writer owns its events, and
announcing an uncommitted write would announce one that may never land.
Callers do not push ops into the batch by hand — `save_with_prev_into` /
`link_into` encode the keys and faces for them; the op list itself is the
internal seam the engines, the wire frames and the dynamic plan consume.

### 6. Engine backends

```toml
[dependencies]
okm-core = { version = "0.1", features = ["fjall"] }    # or "slatedb" / "redb"
```

- **fjall** (sync): `FjallStore::open(path)` — local LSM engine, single `Database` handle, `persist` on demand.
- **slatedb** (async): `SlatedbStore::open(path, Arc<dyn ObjectStore>)` — object-storage-backed; use `slatedb::object_store` re-exports to construct stores so versions always match slatedb's internals. Async traversal goes through `AsyncJunction`.
- **TestStore**: the test matrix over the real engines (slatedb in-memory, fjall temp dir, redb temp file) — no mock implementation to drift from the real ones.

## Interface layers

Four tiers, each built on the one below it. The rule across all four: **a tier
speaks in the currency of the one below it** — declarations emit bytes, assembly
points call the engine contract, engines see nothing but bytes.

| Tier | The interface | What it commits to |
|---|---|---|
| Declaration | `KeyEncode` / `DocumentEncode` / `JunctionEncode` / `GraphEncode` (okm-derive) | pure `Vec<u8>` in/out — no storage, no I/O, no engine type; that is what keeps each derive a single-item pure function |
| Assembly | `Collection<S, K, R>` / `Junction<S, E>` / `Graph<S, E>`, plus the reduce and subscribe hooks | binds ONE engine instance; `&mut self` writes, `&self` reads; the declaration IS the registry, so there is no runtime schema object to keep in sync |
| Engine contract | `VirtualStorage` / `VirtualStorageAsync` / `SharedVirtualStorage` (`okm-core/src/engine/storage.rs`) | the seven methods over encoded bytes — the only thing a backend must speak |
| Consumers | okm-query / okm-stream / okm-graph / okm-vector / okm-ngram / bindings | sit above the assembly tier and consume its outputs (ordered scan streams, subscribe events, documents) with zero core changes |

Both schema carriers — compiled declarations (okm-derive) and schema data
(okm-dynamic) — reach the same two lower tiers, so "which mode" never changes
the engine contract or the bytes.

### The engine tier in full

One contract, two worlds, one carrier, two adapters:

| Item | Kind | Contract |
|---|---|---|
| `VirtualStorage` | sync contract | `put` / `get` / `del` / `scan_suffix` / `scan_range` / `scan_range_iter` / `commit_batch`; writes take `&self` (ADR-0026); `scan_range` is the ONE ordered-read primitive — `scan_suffix` and `scan_range_iter` default onto it |
| `VirtualStorageAsync` | async mirror | the same surface, awaited (`SlatedbStore`, `WireClient`) |
| `SharedVirtualStorage` | capability | `shared_handle()` returns a handle to the SAME physical keyspace, never a copy; required by `NestStorage` and by fetch-back inside `scan_range_iter` |
| `MemBatch` | carrier | an ordered op list (`ops: Vec<(key, Option<value>)>`: `Some` = put, `None` = del, **no dedupe** — the list is replayed in order). `Default` + `put` / `del` is its whole API, and the assembly points are what fill it; one `commit_batch` flushes it as a single engine-level write |
| `ScanIter` | iterator | opaque enum over each backend's native owned iterator (fjall / redb / slatedb / remote / buffered); `DoubleEndedIterator` wherever the engine can walk backwards |
| `BatchStore<'a, S>` | adapter | a batch as a writer: point reads see pending ops newest-first then the store, writes only append, scan shapes panic (ADR-0031) |

Eight types implement the sync contract — six backends and two adapters:

| Type | Where | Kind | Shared |
|---|---|---|---|
| `FjallStore` | `engine/fjall_backend.rs` | local LSM (feature `fjall`) | yes |
| `RedbStore` | `engine/redb_backend.rs` | COW B-tree (feature `redb`) | yes |
| `SlatedbSync` | `engine/slatedb_backend.rs` | sync view over slatedb; the async face is `SlatedbStore` | no |
| `LocalStorageStore` | `engine/localstorage_backend.rs` | browser localStorage (feature `localstorage`) | yes |
| `RemoteStore` | `engine/nest.rs` | wire client; its server half `NestStorage` is a pump over one engine mutex, NOT a `VirtualStorage` | no |
| `TestStore` | `engine/test_engine.rs` | test matrix over the real engines (slatedb-mem / fjall / redb) | yes |
| `EngineBox` | `bindings/okm-entry` | **adapter**: dispatches to an in-process `TestStore` or a host-injected `Engine` (four byte methods) — what lets a host hold a different okm build | no |
| `BatchStore` | `engine/storage.rs` | **adapter**: decorates an `S`, redirects writes into a `MemBatch`, lands nothing itself | no |

#### `BatchStore` is a façade, not a backend

It implements the trait it decorates (`impl VirtualStorage for BatchStore<'_, S>`),
which is how the reduce read-modify-write runs through a batch without knowing
it is writing into one. What it adds is a redirection, not a capability:

- **writes do not pass through** — `put` / `del` only append to the `MemBatch`;
  **reads do** — `get` checks the batch's newest op for that key, then the store;
- it owns no data and borrows the engine (`store: &'a S`): a temporary view
  bound to the borrowed batch, not something to hold on to or hand out;
- **the commit lives outside it** — `commit_batch` is the caller's call on the
  engine; called on the adapter instead, it only merges the inner ops into the
  outer batch;
- no `SharedVirtualStorage` (so `NestStorage` and `scan_range_iter`'s fetch-back
  refuse it), and every scan shape panics rather than answer a partial view.

Decision records live in [docs/adr/](docs/adr/); implementation status is
[docs/PLAN.md](docs/PLAN.md).

## Design notes

- **Namespace stays in code** — the namespace vocabulary is compile-time constants, never stored in KV. The access pattern itself lives in code (binary keys, no separators, per-field widths); putting ns in code is the same act as putting the key layout in code. Macros run at compile time when no KV exists to read from — a vocabulary in KV is a bootstrap deadlock. Numbers are manually assigned, append-only, never reused; see [ADR-0002](docs/adr/0002-namespace-dictionary.md).
- **Two layout regimes** — primary keys are fixed-width (zero parsing, hot path); secondary indexes are variable-length (text as discriminating prefix, UTF-8 byte order = dictionary scan order, primary-key ID appended at the key tail, value left empty). Width is a property of *structure*, not *data*; the discriminator is access pattern: point-lookup-only may hash to fixed width, anything needing prefix/range scan must keep raw text. Variable-length fields use a length prefix `[len: u16][bytes]` over NUL termination (no escaping burden); a fixed-width field *after* a variable-length one loses its compile-time offset and falls back to a runtime cursor — "fixed-width prefix + variable tail" keeps most of the zero-parsing benefit. Hex stability tests still apply to variable-length keys: what they lock is the encoding scheme itself (prefix layout, length endianness, limits), not specific bytes. The name→id index is the mainstream case and is almost always variable-length, since an index exists to answer prefix/range queries. See the [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md) essay for the full argument.
- **Macro layer is deliberately storage-free** — encode/decode are pure `Vec<u8>` in/out functions; engine choice and lifecycle belong to the assembly site (`Junction::new(store)` / `<Document>::collection(store)`). This is what keeps each derive a single-item pure function.
- **Portability**: the paradigm is bytes-level and host-language independent — a Python dataclass with the same `encode()` reproduces the layout, at the price of moving guarantees from compile time to runtime assertions (SlateDB's Python bindings via UniFFI provide the needed primitives: `get` / `scan_prefix` + `KeyRange` / `WriteBatch` / transactions). Portability has a structural cost, however: type errors move from compile time to runtime (`assert` instead of the compiler), encoding is byte-concatenation rather than memcpy-level offsets (1–2 orders of magnitude slower on hot paths), and decorators/metaclass registration is a runtime cost instead of a compile-time expansion. Same paradigm, guarantee level set by the host language.

## Why not just use a (ready-made) database?

A side benefit of OKM: engine selection anxiety disappears. The actual menu is long — PostgreSQL, DuckDB, Lakehouse, SurrealDB… — and OKM speaks plain bytes, so any engine that can put and get them qualifies.


## License

To be decided upon publication.
