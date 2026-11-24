> **Languages:** [English](0081-the-declaration-emits-addresses-not-tables.md) (primary) · [中文](0081-the-declaration-emits-addresses-not-tables.zh-CN.md)

# ADR-0081 — The declaration emits addresses, not a table per type

**Status:** Decided 2026-11-24 (no code). Records the arrangement the tree already has: one physical table per store, the table's identity in the key.

## 1. The question

redb's idiom is a static typed handle — `const T: TableDefinition<&[u8], &[u8]> = TableDefinition::new("name")` — and its B-trees are per table. A store built on it naturally asks whether `okm-derive` should emit one per row type: the declaration is the only place that knows a table exists, so it could also be the place that names its table.

No, because in OKM a table is an address, not an object. `#[ok_ns(N)]` compiles into `NS` / `NS_PREFIX` (ADR-0002), one ns is one table (enforced at compile time by the per-ns static, ADR-0079), and every key any verb builds starts with that header (ADR-0011). What redb calls a table name is what ADR-0034 calls a name that means something only outside the store.

## 2. Measured: the per-type table is a wash, so this is not a performance decision

A throwaway probe (redb 4.2, 200 000 entries, 96-byte values, 200 namespaces × 1 000 rows, one write transaction per arm) compared one table whose keys carry `[ns 2B][id 8B]` against 200 tables whose keys carry `[id 8B]`:

| | one table | table per ns |
|---|---|---|
| file size | 33 689 600 B | 33 689 600 B |
| open + list tables | 0.20 ms | 0.18–0.19 ms |
| scan one ns (1 000 rows) | 48.0–48.6 µs | 42.3–43.4 µs |
| point read | 0.45–0.48 µs | 0.47–0.53 µs |
| bulk build | 104–113 ms | 77–79 ms |

The two-byte prefix does not reach the file at page granularity, and 200 tables cost what one costs to open. The only difference that survives repetition is the single-namespace scan, 48.0 against 43.4 µs on 1 000 rows — and a query that then fetches those 1 000 rows measured 943.8 µs in ADR-0080. A 5 µs difference on the scan side is a rounding error at the door a query actually pays.

So the read path pays nothing for the single table, and a benchmark cannot decide this question — or reopen it.

## 3. What decides it: the boundary

1. **One identity, not two.** The ns IS the table's name inside the store. A `TableDefinition` would give the same table a second name, a string, and only the string would be re-typed by hand when a declaration moves; the number is the one baked into every key on disk.
2. **The derive has no engine in it.** `okm-derive` carries no runtime dependency and none on `okm-core` — it emits `::okm_core::…` as tokens and nothing else. redb is an optional feature of `okm-core`, and ADR-0073 keeps the engine matrix behind features; `::redb::TableDefinition` in generated code would make every consumer link redb, and a fjall-only build would stop compiling.
3. **The engine boundary speaks bytes.** `VirtualStorage` is `put`/`get`/`del`/`scan_range` over `&[u8]`. fjall takes a keyspace name from its constructor, redb's adapter holds one table, slatedb has no tables at all, and the remote form moves frames byte-identical (ADR-0010). Engine handles in the declaration would turn that trait into per-engine vocabulary, which is the direction ADR-0080 refused.
4. **Several tables on one engine is a property of the bytes.** The test matrix and multi-tenant hosting run distinct logical tables on one store because their keys are byte-disjoint (ADR-0011 §3). Per-type tables replace that with a table-name registry, and a receiver executing a frame would have to know which table a key belongs to — the join ADR-0011 priced and refused.
5. **The address set is not closed at compile time.** `with_ns`/`rebased` root a table at another namespace (ADR-0062), the layout generation is a bit inside the header (ADR-0075), and a `DynamicCollection` opens a namespace the store describes (ADR-0025). A static handle per declaration cannot cover those, so a byte-prefix path exists regardless — and a second path beside it is how the two start to differ.

## 4. Decision

`okm-derive` emits addresses and nothing else: `NS`, `NS_PREFIX`, `PARTITION_ID`/`PARTITION_PREFIX`, `PLAN`, plus `LAYOUT_VERSION` and `TABLE_NAME` for the vocabulary that lives outside the store (ADR-0042, ADR-0034). No engine type appears in generated code.

Where a physical space boundary is real, it belongs to the store handle or to the key: `FjallStore::open(path, name)` takes its keyspace from the assembly point, redb's adapter uses one table named `okm`, the ephemeral engine has none, and `#[ok_partition(N)]` writes a structural escape segment into the address. None of these is a declaration attribute spelling another engine's vocabulary.

## 5. Refused alternatives

- **`TableDefinition` per row type.** Buys nothing measurable (§2); costs the feature matrix (§3.2), a second identity for the table (§3.1), the runtime namespaces (§3.5) and byte-identical frames (§3.4).
- **A typed key per table implementing `redb::Key`.** Moves the key codec's semantics into an engine trait impl. Composition is single-point today (ADR-0011's first consequence); a per-declaration `as_bytes`/`from_bytes` is that point twice.
- **`TABLE_NAME` as the table name.** Makes the outside-the-store name an in-store address — the second vocabulary ADR-0034 refused when it declined to spell a struct's own name as a table name.
- **A `space` argument on `VirtualStorage` now.** No backend needs one: redb's single table, fjall's name at construction and slatedb's absence of tables all fit the byte contract.

**Reopen gate:** an engine where a table is a first-class capability with no byte-prefix equivalent — dropping a whole table without a scan, a per-table encryption key, per-table compaction a shared tree cannot express — or a deployment that must hand one table its own file. Then the boundary grows one engine-agnostic parameter, each backend maps it, and the derive still emits addresses.

## 6. Where the argument lives

The module comments in `okm-core/src/engine/redb_backend.rs` and `fjall_backend.rs` state the same thing where a backend author will read it. They are the in-place form of this decision, not a second source for it.