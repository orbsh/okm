# ADR-0006: Row/Node model — one macro declares the row; ValueEncode absorbed

Date: 2026-09-07
Status: Accepted (design; implementation pending)

## Context

Secondary indexes (ADR-0005) were declared on the key struct, which forced row
attributes (e.g. `name`) into the key type. That is backwards: a key is pure
identity, `name` lives in the row, and hanging an index on the key struct only
so the macro can see the field pollutes the data model with tool constraints.
It also blocks variable-length fields — keys must stay fixed-width, but indexed
attributes have no such requirement.

At the same time the planned `ValueEncode` macro (ADR-0004) would have
re-declared the very fields a row macro already declares — two places for one
field list is redundant and a worse developer experience.

## Decision

**One macro per row.** `RowEncode` declares identity, payload, and access
methods in a single item; the three concerns share one field list:

```rust
#[derive(KeyEncode)]
#[kv_ns(1)]
pub struct UserKey {              // pure identity, fixed-width, unchanged
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(RowEncode)]
#[kv_row(key = UserKey)]
pub struct UserRow {
    #[kv_ref]
    pub id: UserKey,              // identity: encoded via KeyEncode
    pub name: String,             // payload: value-layout mechanisms (ADR-0004)
    #[kv_version(2)]              //   version byte, TLV ext section, wrappers —
    pub age: u8,                  //   all apply to the payload half
    #[kv_index(by_name { fields(name), includes(age) })]   // access methods
}
```

Consequences of the single declaration:

- **The `ValueEncode` macro is cancelled** (ADR-0004's mechanisms are not):
  versioned payload, hot/TLV sections, and field wrappers (`Enum<T>`,
  `VarInt<T>`, `Reverse<T>`, …) become the expansion rules for the row's
  payload half. A wrapper annotates the row field once and applies wherever
  that field is encoded (value, index entry).
- **Variable-length payload fields are now natural**: `String` lives in the
  value (TLV / length-prefixed) and in index entries (variable-length regime:
  discriminating text first, primary key ID at the tail). The key stays
  fixed-width and untouched.

## Node and Edge are peers

The type system resolves into two record kinds sharing one physical base
(ns header + BE field encoding):

| | identity | payload | access methods | scans |
|:--|:--|:--|:--|:--|
| **Node** (Row) | `#[kv_ref]` key | yes (versioned/TLV) | secondary indexes | by key / by index |
| **Edge** | both endpoints | none (empty value) | `#[kv_head]` truncation | forward / reverse |

This replaces the earlier conflation where `Collection` served both roles.

## Assembly points and storage binding

`Collection<S, E>` narrows to edge-only duty; node rows get a parallel
`Table<S, K, R>` — both plain generic structs, **no assembly macro** (the
ADR-0003 discipline is unchanged: binding is a type parameter, never code
generation; the row macro's motivation is index declaration plus the
single-source field list, not binding):

```rust
let store = FjallStore::open(path)?;                 // storage binds here
let users = Table::<_, UserKey, UserRow>::new(&store);   // node surface
let edges = EdgeTable::<_, UserToSessionEdge>::new(&store); // edge surface
```

Storage is bound the moment the engine instance is opened. Both assembly
points hold the **same store instance** — that is what makes cross-ns scans
and engine-level atomic writes (one WAL batch covering table + indexes + edge
keys) possible. Assembly points are operation surfaces, not storage binders.

`Table::put(row)` is the write orchestration point: primary key write + one
index entry per access method, in one engine batch.

### Multi-engine mixing

Because the binding unit is the **store instance**, not a global singleton,
different engines can coexist in one process — transactions on `fjall` (local,
low-latency), logs on `slatedb` (S3-backed, async batch). Each ns segment
follows its engine; the ns dictionary (ADR-0002) already lives in code, so the
"segment → engine" mapping is just one more explicit choice at the call site.

Two boundaries:

- **Atomicity stops at one engine.** The one-WAL-batch guarantee (table +
  indexes) holds within a single store instance only. Writes spanning two
  engines have no distributed transaction — eventual consistency with
  retry/reconciliation is the user's architectural decision; OKM stays a KV
  access layer and does not grow 2PC.
- **ns numbering stays globally unique across engines.** Two engines each
  holding "ns 1" are physically isolated and won't error, but segment numbers
  must remain unique database-wide — otherwise cross-engine migration/scan
  tooling would find the same segment meaning different things on each side.

## Snapshot export — columnar conversion, engine-independent

Rows can be converted to a columnar snapshot format (Parquet, or other
engines' equivalents). One mechanism, three uses:

- **Backup** — point-in-time copy outside the engine's own file format.
- **Data exchange** — Parquet is the lingua franca; anything downstream
  (Spark/DuckDB/Polars, another OKM instance, a non-OKM system) reads it.
- **Analysis / lakehouse** — the snapshot lands directly in object storage as
  a lakehouse table.

Design points:

- **Engine-independent by construction**: the conversion reads rows through
  the `KvEngine` trait's scan surface and writes the row's declared fields —
  it never touches engine-specific file formats. fjall-backed and slatedb-backed
  tables produce identical Parquet.
- **Self-describing output**: the binary ns header is reversed back into
  descriptive text (the ns dictionary lives in code, ADR-0002 — the exporter
  owns the reverse mapping). Column names are field names. A snapshot needs
  no sidecar schema file to interpret.
- **What indexes are NOT in the snapshot**: index entries are derived state,
  not data. Only rows (and edges as rows-with-empty-payload) are exported;
  rebuilding indexes on import is deterministic from the row declaration.
- **Round trip**: import restores rows; primary keys and payload decode back
  via the same field declarations. This is a bulk snapshot path, not
  replication — no incremental log shipping; consistency = the moment the
  scan was taken.

## Index entries and covering

Index key layout (per ADR-0005 slot mechanism, now fed from the row):

```
[table_ns 2B][slot 1B][indexed fields][includes fields][primary key ID]   value empty
```

- `includes(age)` appends payload fields into the index key, making the scan
  self-sufficient (no point-lookup back to the row). **Positioning: this is a
  materialized view for high-fanout queries, not a default optimization** — in
  KV, "回表" is one bloom-filtered point lookup, cheap; widening every index
  key and enlarging the update surface to save a couple of point lookups is a
  losing trade at low fanout. The mechanism is free (a longer field list);
  the posture is deliberate.
- Updating a covered field rewrites the index entry — cost belongs to the
  user's explicit `includes` choice.

## Consequences

- `RowEncode` expand-time work: identity codec via the referenced `KeyEncode`
  type; payload codec per ADR-0004 rules; per-index `AccessMethod` impls with
  item-local slots (unchanged from ADR-0005, relocated to the row item).
- Macro count stays at two derive families (key, row/edge) plus existing
  engine-agnostic traits; no assembly macro returns.
- ADR-0004 remains authoritative for value byte layout; its macro-level claims
  are superseded by this ADR. ADR-0005's slot/ns decisions are unchanged; its
  key-struct mounting is superseded.
