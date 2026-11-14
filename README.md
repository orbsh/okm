# OKM — Object-Keyspace Mapping

> And what, SQL, must we give in return?

[中文版](README.zh-CN.md)

The application is the only reader of its data, so the database should not be a network service. OKM is an embedded, type-safe, compile-time-optimised storage engine — no query language, no optimiser, no run-time plan, because your application code already knows the shape of every query, and the only thing left is to let the compiler know it too.

```rust
use okm::prelude::*;

#[derive(KeyEncode)]                  // identity declared on its own: fixed width, BE, no separators
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode)]             // row: identity + payload + access methods
#[ok_ref(UserKey)]                    // the row pins its own key type
#[ok_ns(1)]                           // the ns is declared HERE, not at the assembly site
#[ok_index(by_plan { fields(plan) })] // one face: entry key = indexed field + key tail
pub struct User {
    pub email: String,
    pub plan: Plan,
}
```

## SQL is dynamic; the SQL in your application is not

SQL was designed as a language interpreted at run time: a query is a string, its structure unknown before execution, so the database has to carry a parser, an optimiser and a plan cache — the only reason those mechanisms exist is that it does not know what the next query looks like.

Your application is not like that. Every query is written in code, its parameters come from variables, and its shape is fixed at compile time: the shape of `WHERE plan = $1` is always "fetch rows by plan", and only the value of `$1` changes. What gets called a "dynamic query" is, in the overwhelming majority of cases, **the same static shape with a different argument**; the small remainder that really does change structure with user input — WHERE clauses assembled in a BI panel, report generators, throwaway admin queries — is exactly SQL's home ground, and exactly where an embedded engine should not pretend it can win. The honest boundary runs here.

So the trade states in one sentence: **take the premise "the query is only known at run time" out of the engine**. Once the premise is gone, a whole string of mechanisms that existed for it goes with it: the parser, the optimiser, the plan cache, parameter sniffing, the prepared-statement lifecycle, the connection pool, and the cache layer that existed to route around all of it. They are not replaced by better implementations — they **lose the reason they existed**.

The cost is just as explicit, and has to be stated: the query set is closed. A schema change means a recompile; ad-hoc analysis takes another path (CDC into Delta Lake, letting the analytical side use whatever engine it likes); and "just connect with psql and take a look" is gone. What static-ness saves is not some constant factor but the maintenance tax on **a whole set of capabilities that never actually paid off for you**.

## Why not SQL + Redis?

The default architecture of a modern backend is "PostgreSQL holds the truth, Redis makes it fast":

```
app ──network RTT──→ SQL    (parse → plan → execute)
app ──network RTT──→ Redis  (cache, invalidated by hand)
```

Each layer pays for the other's existence: Redis exists because every SQL read costs a network round trip, a parse and a plan; SQL is deployed centrally because queries are dynamic strings that need a separate parsing service. The two are each other's cause, which is also why both disappear together.

| | SQL + Redis | OKM |
|---|---|---|
| read one row | 0.5–2 ms (RTT + parse + plan) | ~1 µs (in-process B-tree) |
| cache layer | required (reads are too slow) | none (a read is already memory speed) |
| cache invalidation | an entire engineering discipline | nothing to invalidate |
| deployment manifest | app + database service + connection pool | one binary |
| query validation | run-time error | compile-time error |
| business boundary | two systems, no cross-system transaction | one type domain |

Redis's performance story holds in a world where the network is a given premise. OKM's unified claim is to make that premise disappear — the cache layer is not replaced, it is pulled out entirely.

## Two-step semantics: query = set algebra + fetch

Every query reduces to two primitives:

```rust
// Step one: key-set algebra (pure memory, O(min(|A|,|B|)), optimal as cardinality shifts)
let active: KeySet<UserId> = users.by_plan().contains(&[Plan::Pro]);
let paid: KeySet<UserId>   = users.by_email().prefix("team");

// Intersection, union, difference — all structured values: serialisable, pushable
let target = active.intersect(&paid);

// Step two: fetch (batch read of whole rows by primary key)
let rows: Vec<User> = users.fetch(&target);
```

- **Large cardinality is not an optimisation problem, it is an `if` statement**: build a hash from the small set and scan the large one; which is smaller is a run-time fact, the code decides on the spot, and no optimiser has to guess.
- **A key set is a serialisable intermediate representation**: crossing a process, a network or an engine ships the result of set algebra rather than the raw data.
- **Dynamic queries have their place too**: a filter a user assembles in a UI has the natural shape "predicate → key set → fetch", so the dynamism is confined to the input side of set algebra.

## Faces and engines: shape fixed at declaration, engine chosen at assembly

An index is not a separate object; it is **a face on the row**: `#[ok_index(...)]` is written in the row's declaration, and the entry key is the indexed fields plus this row's key tail. So there is no such thing as "build another index table" — the face's shape is compiled together with the row, its key is the row's key type, and one key type can be shared by several rows and several faces.

Engine selection happens at the assembly site, one decision per collection:

```rust
// production: persistent LSM
let users: Collection<_, UserKey, User> = Collection::new(Fjall::open("data/users")?);

// tests: pure memory, zero I/O
let users: Collection<_, UserKey, User> = Collection::new(Mem::default());

// archive: the same declaration, another engine instance
let archive: Collection<_, UserKey, User> = Collection::new(Fjall::open("data/archive")?);
```

Every engine implements the same behavioural contract (see `engine_matrix_test`); swapping engines does not touch application logic — where the bytes live is an implementation detail, and location is not in the type.

## The layers beyond the engine

An honest world, each layer minding its own business:

- **Ad-hoc analysis / BI** → CDC drains the business log continuously into Delta Lake; the analytical side uses any engine (DuckDB / Spark / Trino).
- **Schema evolution** → a schema change is a compile-time event: blue and green processes each run their own version over shared COW snapshots.
- **Dynamic mode** (tenant-defined fields) → okm-dynamic: the structure is data rather than a type, and outer access still takes the statically optimised path.

## Status

In production use in several internal projects. API stability is not yet promised; at this stage it suits evaluation and experiment, not external production dependency.

## The optimiser's shrinking role

This is the hole that erodes the optimiser most. The optimiser exists because "the query is unknown, so the engine must be clever". In a static world, anything an optimiser can do can be done ahead of time in code, with no estimation error:

- predicate pushdown → the programmer computes the set first and then fetches, which is pushdown by construction
- join order → the order of set operations is the call chain, and the programmer knows the cardinalities
- index selection → the access method is bound when the declaration slot is written
- cardinality estimation → no guessing needed; whoever wrote the code has seen the data

This is not the claim that "the programmer is always smarter than the optimiser"; it is that when the query set is closed, the optimiser's work becomes a one-off development-time decision.

## Against the historical precedents

- **ORM query builders**: TypeORM/Diesel's compile-time query builders half-admit this, but they still compile to text SQL, wasting the static-ness at the last step — compiling to a string and handing it to a dynamic engine loses on both sides.
- **Stored procedures**: logic moves to the data side, but the language is closed, the ecosystem absent and debugging is hell; OKM moves the data to the logic side (embedding) and keeps the whole Rust ecosystem.
- **Embedded KVs such as Sled/redb**: they admit the embedded advantage but stop at the byte layer, with no type layer.
- **Supercompilation (Futamura projections)**: in theory the first projection of "specialise the interpreter into a compiler". A static query is that projection specialisation done by hand.

## Overall assessment

| Property | How it is implemented | Design intent | Status |
|---|---|---|---|
| ORM experience | Rust type system + derive macros | generate access code at compile time, avoiding run-time parsing | ✅ core feature |
| Redis speed | a lean, tuned KV store | embedded, no network overhead, close to memory access | ✅ target performance |
| PostgreSQL durability | business log + persistent storage | no data lost after a system fault | ✅ design goal |
| Functions without a boundary | embedded calls into Rust functions | complex logic implemented directly in application code | ✅ distinctive advantage |
| Two-step semantics | set algebra separated from fetch-back | matching separated from materialisation | ✅ core innovation |

**Core advantages**: compile-time type safety (a mismatched query shows up at compile time), zero-cost abstraction (after compilation it degrades to plain pointer offsets), a clear model (relational algebra) and a small API.

**Current challenges**: ecosystem, production validation, complex queries (across several entities), team collaboration (the community is zero).

**Fit**: high (read/write performance beyond Redis and traditional stores, in a Rust stack); situational (fast concept validation); unsuitable (where a ready ecosystem and community support matter).

## Comparison with other approaches

| Approach | Kind | Core strength | Core weakness | Fit |
|---|---|---|---|---|
| OKM | embedded Rust ORM | performance potential, extensibility, type safety | empty ecosystem | performance-sensitive work, Rust exploration |
| Diesel | Rust ORM | mature and stable, compile-time checks | limited async, steep curve | traditional web backends |
| SQLx | async SQL toolkit | async first, compile-time checks, flexible | not an ORM, relations handled by hand | async services, microservices |
| SeaORM | dynamic ORM | flexible, dynamic, feature-rich | the abstraction layer costs performance | fast REST/GraphQL development |
| rusqlite | SQLite bindings | light, dependency-free, simple | synchronous API, SQLite's limits | CLI tools, desktop apps |
| Redis | in-memory database | extremely fast, rich data structures | not persistent, needs network access | caching, sessions, real-time analytics |

## Summary and advice

OKM represents a contrarian line of database design: map the query patterns of a relational database onto a KV store's structure, and replace run-time querying with ORM-style type definitions and compile-time code generation.

What deserves warning is the price this project charges: static-ness is not free — run-time freedom (BI tools, ad-hoc queries, dynamic reports) is genuinely given up. That settles OKM's position as an application database, not an analytical one; ad-hoc is where SQL really is at home.

For the great majority of projects, adopting OKM is not worth it today: its ecosystem is zero and every component has to be built. It is better suited as a technical exploration and an experimental platform for validating its design ideas; if embedding is what you need, rusqlite (SQLite) is the safer choice; if high performance is what you need, Redis (with persistence) is still the mainstream answer; if maturity is what you need, SeaORM or Diesel fit better.

OKM's value lies in the design ideas it starts from, "functions without a boundary" and "two-step semantics" in particular, which may offer a new paradigm for future database design.

## Usage

### 1. Declarations: keys, rows, and relations

The full declaration vocabulary (`KeyEncode` / `DocumentEncode` / `GraphEncode`, and the `fields`/`includes`/`key`/`func`/`where` annotations of `#[ok_index]`) is in the [Modeling Guide](docs/MODELING.md), "Declaration basics". Three shapes: **identity**, **row**, **graph**. Many-to-many has no carrier of its own any more — it is an ordinary collection (the key is the pair of identities) plus one `#[ok_lead]` reverse face.

```rust
#[derive(KeyEncode)]                        // identity: fixed-width fields, BE, no separators
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode)]                   // row: identity + payload + access methods
#[ok_ref(UserKey)]                          // the row pins its key type
#[ok_ns(1)]                                 // the ns is declared HERE, not at the assembly site
#[ok_index(by_org { fields(org_id) })]      // a face: entry key = indexed fields + key tail
pub struct User { pub org_id: u32, pub bio_len: u32 }
```

A face declaration mechanically generates its access-method type: `ok_index(by_org ...)` produces `__OkmIndex_User_by_org` (no case conversion), and that is what `scan::<…>` takes — `use __OkmIndex_User_by_org as ByOrg` gives the short name.

### 2. Documents at runtime (`Collection`)

```rust
use okm_core::{Collection, Document};

let users = <User as Document>::collection(store.clone());
let key = UserKey { org_id: 7, user_id: 101 };

// Write: primary entry + one entry per declared face, one store instance.
// The verbs take `&self` (same shape as the put/del beneath them); exclusivity
// is the collection being `!Sync`.
users.put(&key, &User { org_id: 7, bio_len: 240 });
let row: Option<User> = users.get_row(&key);

// Read by primary-key prefix: the key itself IS the access path. A composite
// key groups by construction, so "every user of this org" needs no index and
// no declared query — the probe names leading fields in declaration order.
let org_users = users.scan_keys(&UserKey::prefix().org_id(7).finish());
// Lazy: `.take(n)` is a LIMIT, `.skip(n)` a page, `.rev()` walks backwards;
// each item is a `Result<UserKey>` (keys only — a document read fetches it).

// Read by access method: the dimension is NOT in the key, so it is a declared
// face — leftmost-prefix scan, then fetch-back. The probe mirrors the key's:
// `ByOrg::prefix()` names the index's declared `fields(...)` in order, and
// `finish()` yields exactly the bytes a caller would hand-encode.
for (pk, row) in users.scan::<ByOrg>(&ByOrg::prefix().org_id(7).finish()) {
    // pk.decoded: UserKey, row: Option<User>
}

// A comparison on the leading index field is a physical key interval —
// byte order is value order, so this reads only rows inside [1, 100).
users.scan_range::<ByOrg>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()));

// Remove: primary + exactly the faces this document produced.
users.purge(&key);   // primary + faces + dynamic segment, one commit; the deleted set derives from the stored row
```

The three read doors are the modeling decision SQL hides: a predicate on a leading key segment is a prefix scan on the primary table (no index, no declaration), every other dimension is a declared face, and one row is a point read. An index here is not a second copy the optimiser may or may not choose — the ordering a prefix read needs is already the storage order, so the key that addresses a row also groups it.

A prefix read is less abstract than a SQL predicate, and less flexible. That is the trade, stated plainly. A SQL predicate is a promise that an optimiser will find an access path; many engines do not keep it, and the way they break it is worse than an error — wherever the index depends on key order (MySQL's composite indexes being the canonical case), the left-prefix rule makes physical layout part of the query: whether `INDEX(a, b, c)` serves `WHERE b = 1` depends on the field order chosen at create-table time. That is an abstraction leak, not abstraction, and it costs intuition as well as flexibility. OKM takes the other fork: no optimiser, and therefore no promise to break — the key layout IS the read path, a prefix is a range in storage order, and the price is that access patterns are settled at modeling time rather than once per query.

Overwrite semantics divide in two by design: `put` first unfolds the stored document out of every declared reduce group and then folds the new one in, while faces are per-document derivations — change an indexed value and the entry lands at a new key, so the old face is the caller's to remove (or `save_with_prev_into`'s, section 6).

A ledger is a declaration line, not a table you maintain (ADR-0023):

```rust
#[ok_reduce(Count { group(org_id) })]          // rows per group, exactly reversible
#[ok_reduce(Sum(bio_len) { group(org_id) })]   // field total; signed and Quant stay exact
```

The accumulator entry is `[ns 2B][reduce slot][group segment]`, read-modify-written by the same `put`/`purge` that writes the row — one store instance, so the ledger shares the row's atomicity boundary.

### 3. Dynamic fields: the document API

Three doors, and the distinction is about what a call may see, not about convenience:

| door | sees | pays |
|---|---|---|
| `get` / `put` (payload `&R`) | the declared row only (`R`), never the dynamic segment | no field-name vocabulary load, no segment read — the compiled-schema path |
| `get::<(parts::Row, parts::Value)>` | exactly the slots it names | only the slots it names |
| `put` (payload is a map) | the whole document: both halves | only the arms the names actually touch |

Writing is one verb on both sides, and the **payload** picks the route — the call site never translates its data into a verb name:

| payload | path |
|---|---|
| `&R` (typed side only) | the row and its faces; the fresh-row fast path, so the previous values' entries are **not** swept |
| `&Dict` / `&Variant::Map(map)` | partial document: a declared name is one row RMW (its superseded face swept, the ledger compensated), every other name overwrites that name in the dynamic segment, and names the map omits keep their stored values |
| `(&Dict,)` / `(&Variant::Map(map),)` | whole document: the map IS the document — an omitted declared field takes its default, an unnamed dynamic name is removed |
| `purge` | both slots — the only removal verb, on both sides |

The map those rows name is `okm_value::Dict`: an insertion-ordered name → value map, the only spelling of a document object (`Variant::Map` carries it, and insertion order is the contract — ADR-0063).

The payload is caller data, so a write is fallible (`okm_core::Error`): a declared name carrying the wrong kind is `Error::KindMismatch`, never a silent default, and nothing is written. The absent half costs nothing: the schema decides which arm the plan takes, so a map with no declared name never reads the row, and a map with no dynamic name never reads the segment or loads the vocabulary.

Reading is parts, and composition belongs to the caller: a point read answers each slot on its own, a row converts to one value (`Document::to_value`), and two values merge (`Variant::merge` / `merged` — objects merge name by name, nested objects recursively, anything else replaces the left side). There is deliberately **no** assembly function: the two primitives are the whole story, and the composition is two lines at the call site.

One encoding serves declared rows and external data. Unknown field names are normal input: they allocate a number in the per-table field-name vocabulary and land in the dynamic segment (`[id][type][len][value]` frames, nested objects recursive). Declared fields stay typed and indexable.

```rust
// Read: name the parts. Row is slot 0, Value is slot 1 as an object; each is
// its own Option, so a dynamic-only document answers (None, Some(obj)).
let (row, dyn_seg) = users.get::<(parts::Row, parts::Value)>(&key)?;

// Compose the document: the row as one value, the segment laid over it. `None`
// only when BOTH slots are empty — a dynamic-only document still answers.
let doc = match (row, dyn_seg) {
    (None, None) => None,
    (row, dyn_seg) => Some(
        row.map(|r| r.to_value())
            .unwrap_or_default()
            .merged(dyn_seg.unwrap_or_default()),
    ),
};

// Write: one verb, and the payload picks the path.
// Merge — only the names given are touched; a declared name takes the typed
// path (faces, reduces, events as one), unknown names join the dynamic segment.
users.put(&key, &patch)?;                       // Dict

// Replace — the map IS the document (absent declared fields default, unnamed
// dynamic names are removed). The one-element tuple is the whole-document arm.
users.put(&key, (&doc.unwrap_or_default(),))?;

// A value already in hand writes as itself; a non-object payload is refused,
// because a scalar is a field VALUE and not a document.
users.put(&key, &Variant::Map(patch))?;

// Purge: both slots together, one commit.
users.purge(&key)?;
```

Embedded-language readers (Python / Steel booths) consume the dynamic codec through the schema export — `bindings/okm-python` (PyO3) and `bindings/okm-steel` are byte-identical with the Rust derive, and version-default migration applies on that path too.

### 4. Engine backends

```toml
[dependencies]
okm-core = { version = "0.1", features = ["fjall"] }    # or "redb"; the async engines live in okm-async
```

- **fjall** (sync): `FjallStore::open(path)` — local LSM engine, single `Database` handle, `persist` on demand.
- **slatedb** (async): `SlatedbStore::open(path, Arc<dyn ObjectStore>)` — object-storage backed; construct stores from the `slatedb::object_store` re-exports so versions always match slatedb's internals. Async traversal goes through the matching async iterators.
- **TestStore**: the test matrix over the real engines (slatedb in-memory, fjall temp dir, redb temp file) — no mock implementation to drift from the real ones.

### 5. Relations at runtime: the graph

A graph's declaration is the whole graph: `#[ok_ns]` sets the trust domain, `#[ok_nodes(User, Org)]` lists the member **tables** (the derive generates the enum and its `NodeSet` from them, one variant per collection), and `#[ok_index]` declares faces explicitly.

An edge has no number (ADR-0067): its identity is the tuple `(src, predicate, dst)`, so `0x06` IS the edge row (`[ns][0x06][src][pred][dst] -> payload`) and writing the same tuple twice rewrites that row instead of minting a second one.

```rust
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(100)]
#[ok_nodes(User, Org)]
#[ok_index(by_since { fields(since_year) })]
struct Employment {
    since_year: u16,
    weight: u32,
}
```

Writing has one verb, and a fragment (`Fragment`) is its only input: the declaration collects nodes and edges, optionally chains a cluster and three conflict hooks, and a `None` part means it is not done.

```rust
let mut x = Fragment::<Employment>::new([
    Decl::Node(EmploymentNode::User, user_key),          // endpoints created on demand
    Decl::Edge(EdgeHead { src, dst, predicate: "employs".into() }),
]);
x.cluster(Cluster::new(Kind::EVENT, payload))            // form a cluster (optional)
 .on_node_conflict::<User>(|old, _declared| User { seen: old.seen + 1 })   // updates go through a hook
 .on_edge_conflict(|_stored, declared| Employment { weight: declared.weight + 1 })
 .on_cluster_conflict(|_stored, declared| {
     let mut row = declared.clone();
     row.insert("weight", …);
     row
 });
graph.incorporate(x)?;                                   // one landing: endpoints, edges, member rows, hooks
```

Each hook is handed the STORED payload and the DECLARED one and answers the row to write — `|old, _| old.clone()` keeps what is there, `|_, declared| declared.clone()` overwrites, `|old, declared| if declared.ts >= old.ts { declared.clone() } else { old.clone() }` is last-writer-wins; a rule the mutate-in-place form could not state, because the callback never saw the incoming value.

Deletion is the reverse verb, and its two rules differ: deleting a node deletes every edge of it (an edge cannot be missing an endpoint), deleting an edge keeps both endpoints (afterwards they may connect no edge at all — "a node exists only when it becomes an endpoint" is a write premise, not an invariant after deletion).

```rust
graph.retract(Fragment::new([]).delete_node(user_key).delete_edge(head))?;
```

Traversal is an ordered stream, one `next()` per hop: `out` and `inn` yield the same `EdgeStep` (direction is an argument, not two APIs), predicate filters push down to the `0x08`/`0x09` prefixes, `find`/`take_while` short-circuit naturally, and dropping the stream stops it. The streams implement `Seekable`, so `intersect`/`difference`/`union` share one implementation with set algebra and with cluster-member streams.

Clusters (hyperedges) land in three fixed faces: `0x0A` the cluster row (value = `parent_id` + dynamic payload), `0x0B` edge membership, `0x0C` the child-cluster index; plus `0x0D`, a derived edge→cluster index, so "which clusters does this edge belong to" is one point read. Membership records edges only; the participants are the two endpoints of the member edges, and the projection happens at the call site.

### 6. Cross-table atomic writes: one fragment, one commit

`Fragment` is the carrier of a cross-table write: the nodes, edges, member rows and conflict hooks of one batch commit together or not at all. There is no cross-table transaction language and no cascade mechanism — consistency rests on "one fragment = one commit", and on the two premises the write entry point enforces (a node must be an endpoint of some edge, a cluster needs at least two members).

```rust
let mut x = Fragment::<Employment>::new([Decl::edge(head)]);        // restating the same tuple = the same row
x.on_edge_conflict(|_stored, _declared| new_payload.clone());
graph.incorporate(x)?;                                              // row, faces and member rows stay put
```

Different carriers are separate commits: an edge's payload lives on the graph, its open attributes live in the document's dynamic segment (`0x01`), and the two belong to two write paths; when both must land together, the caller puts them in one higher-level flow rather than asking the engine to guess.

## Interface layers

Four tiers, each built on the one below it. The rule across all four: **a tier speaks in the currency of the one below it** — declarations emit bytes, assembly points call the engine contract, engines see nothing but bytes.

| Tier | The interface | What it commits to |
|---|---|---|
| Declaration | `KeyEncode` / `DocumentEncode` / `GraphEncode` (okm-derive) | pure `Vec<u8>` in/out — no storage, no I/O, no engine type; that is what keeps each derive a single-item pure function |
| Assembly | `Collection<S, K, R>` / `Graph<S, E>`, plus the reduce and subscribe hooks | binds **one** engine instance; reads and writes both take `&self` (a write needs no `&mut`: the collection is `!Sync`, so its single owner is fixed at compile time); `Graph` keeps `&mut self`, because its predicate vocabulary is a mutable cache field; the declaration IS the registry, so there is no schema object to keep in sync at run time |
| Engine contract | `VirtualStorage` / `SharedVirtualStorage` (`okm-core/src/engine/storage.rs`); `VirtualStorageAsync` is its mirror in `okm-async` (ADR-0059) | the seven methods over encoded bytes — the only language a backend must speak; the async mirror serves only engines and transports whose own API is async |
| Consumers | okm-query / okm-stream / okm-graph / okm-vector / okm-ngram / bindings | sit above the assembly tier and consume its outputs (ordered scan streams, subscribe events, documents) with zero core changes |

Both schema carriers — compiled declarations (okm-derive) and schema data (okm-dynamic) — reach the same two lower tiers, so "which mode" never changes the engine contract or the bytes.

### The engine tier in full

One contract, two worlds, one carrier, two adapters:

| Item | Kind | Contract |
|---|---|---|
| `VirtualStorage` | sync contract | `put` / `get` / `del` / `scan_suffix` / `scan_range` / `commit_batch`; writes take `&self` (ADR-0026); `scan_range` is the **only** ordered-read primitive (a lazy `ScanIter` of `(key, value)` pairs) — `scan_suffix` defaults onto it, and the eager form is the caller's `.collect()` |
| `VirtualStorageAsync` | async mirror | the same surface, awaited (`SlatedbStore` and `WireClient` implement it) |
| `SharedVirtualStorage` | capability | `shared_handle()` returns a handle to the **same physical keyspace**, never a copy; required by `NestStorage` and by fetch-back inside `scan_range` |
| `Batch` | carrier | an ordered op list (`ops: Vec<(key, Option<value>)>`: `Some` = put, `None` = del, **no dedupe** — replayed in order). `Default` + `put` / `del` is its whole API, and the assembly points are what fill it; one `commit_batch` flushes it as a single engine-level write |
| `ScanIter` | iterator | a closed enum over each backend's native owned iterator (fjall / redb / slatedb / remote / buffered); `DoubleEndedIterator` wherever the engine can walk backwards |
| `BatchStore<'a, S>` | adapter | a batch as a writer: point reads see pending ops newest-first then the store, writes only append, every scan shape panics (ADR-0031) |

Eight implementations of the sync contract — six backends and two adapters:

| Type | Where | Kind | Shared |
|---|---|---|---|
| `FjallStore` | `engine/fjall_backend.rs` | local LSM (feature `fjall`) | yes |
| `RedbStore` | `engine/redb_backend.rs` | COW B-tree (feature `redb`) | yes |
| `SlatedbSync` | `engine/slatedb_backend.rs` | sync view over slatedb; the async face is `SlatedbStore` | no |
| `RemoteStore` | `engine/nest.rs` | wire client; its server half `NestStorage` is a pump over one engine mutex, **not** a `VirtualStorage` | no |
| `TestStore` | `engine/test_engine.rs` | test matrix over the real engines (slatedb-mem / fjall / redb) | yes |
| `EngineBox` | `bindings/okm-entry` | **adapter**: dispatches to an in-process `TestStore` or a host-injected `Engine` (four byte methods) — what lets a host hold a different okm build | no |
| `BatchStore` | `engine/storage.rs` | **adapter**: decorates an `S`, redirects writes into a `Batch`, lands nothing itself | no |

#### `BatchStore` is a façade, not a backend

It implements the trait it decorates (`impl VirtualStorage for BatchStore<'_, S>`), which is how the reduce read-modify-write runs through a batch without knowing it is writing into one. What it adds is a redirection, not a capability:

- **writes do not pass through** — `put` / `del` only append to the `Batch`; **reads do** — `get` checks the batch's newest op for that key, then the store;
- it owns no data and only borrows the engine (`store: &'a S`): a one-off view bound to the borrowed batch, not something to hold or hand out;
- **the commit lives outside it** — `commit_batch` is the caller's call on the engine; called on the adapter instead, it only merges the inner ops into the outer batch;
- no `SharedVirtualStorage` (so `NestStorage` and `scan_range`'s fetch-back both refuse it), and every scan shape panics rather than answer a partial view.

Decision records live in [docs/adr/](docs/adr/); implementation status is [docs/PLAN.md](docs/PLAN.md).

## Design notes

- **Namespace stays in code** — the namespace vocabulary is compile-time constants, never stored in KV. The access pattern itself lives in code (binary keys, no separators, per-segment widths defined by code), so putting ns in code is the same act as putting the whole key layout in code. Macros run at compile time, when there is no KV to read — a vocabulary in KV would be a bootstrap deadlock. Numbers are assigned by hand, append-only, never reused; see [ADR-0002](docs/adr/0002-namespace-dictionary.md).
- **Two layout regimes** — primary keys are fixed-width (zero parsing, hot path); index entries are variable-length (discriminating text up front, so UTF-8 byte order = dictionary scan order; the primary-key ID rides at the key tail and the value is left empty). Width is a property of *structure*, not *data*; the test is access pattern: point lookups only may hash to fixed width, while anything needing prefix/range scan must keep raw text. Variable-length fields use a length prefix `[len: u16][bytes]` rather than NUL termination (no escaping burden); a fixed-width field *after* a variable-length one loses its compile-time offset and falls back to a run-time cursor — "fixed-width prefix + variable tail" keeps most of the zero-parsing benefit. Hex stability tests still apply to variable-length keys: what they lock is the **encoding scheme itself** (prefix layout, length byte order, limits), not specific bytes. The name→id index is the mainstream case and is almost always variable-length, since an index exists to answer prefix/range queries. The full argument is in [KV Storage Engine](https://github.com/orbsh/wiki/blob/main/kv-storage-engine-en.md).
- **Macro layer is deliberately storage-free** — encode/decode are pure `Vec<u8>` in/out functions; engine choice and lifecycle belong to the assembly site (`Collection::new(store)`). This is what keeps each derive a single-item pure function.
- **Portability**: the paradigm acts at the byte layer and is independent of the host language — a Python dataclass with the same `encode()` reproduces the layout (SlateDB's Python bindings via UniFFI provide the needed primitives: `get` / `scan_prefix` + `KeyRange` / `WriteBatch` / transactions). Portability has a structural cost, though: type errors move from compile time to run time (`assert` instead of the compiler), encoding is byte concatenation rather than memcpy-level pointer offsets (1–2 orders of magnitude slower on hot paths), and decorators/metaclasses register at run time instead of expanding at compile time. Same paradigm, guarantee level set by the host language's capability.

## Related docs

- [Modeling Guide](docs/MODELING.md) — the normative modeling method (four layers, three read paths, covering-index restraint)
- [Architecture](docs/ARCHITECTURE.md) — the two-layer declaration machine (derive expansion + constant tables)
- [Key Layout](docs/internals/key-layout.md) — key-space layout and fixed face allocation
- [Query Recipes](docs/query-recipes.md) — how merge / intersect / difference / union compose into SQL shapes