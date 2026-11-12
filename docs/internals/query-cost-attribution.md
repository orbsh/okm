# Where a query's time goes, measured

The question this answers: an outside proposal (`docs/ideas/schema-aware-index.md`) claims 3–100× from replacing the engine's index layer with a schema-aware block index or a custom ART. Such a claim can only hold if the engine's search is a large share of end-to-end query time. It is not. This document measures the split, so the proposal is judged by arithmetic rather than by the reputation of the papers behind it.

## Environment and corpus

AMD Ryzen AI 7 H 350, NixOS 26.11, kernel 6.18.60, XFS on NVMe, warm page cache. The store is real fjall with its memtable rotated and sealed (plus a compaction barrier), so every read below goes through SSTables and the block index rather than through the memtable — the configuration where an index-layer rewrite would actually matter.

200 000 rows over three indexes (`by_org`, `by_org_status`, `by_org_covered`), each with a 12-byte fixed key (4-byte org, 8-byte row). One tenant holds 180 000 rows (90 %); twenty tenants of 1 000 rows fill the rest. The scanned probe is one of the 1 000-row tenants: exactly 1 000 index entries, and 121 survivors when joined with a second index. SST footprint 15.1 MiB plus 1.4 MiB of index blocks.

Harness: `okm-core/benches/query_cost.rs`, criterion, `cargo bench -p okm-core --features fjall --bench query_cost`. Mean values, confidence interval at or below 0.5 %.

## Measured

| case | time |
|---|---|
| `store.get` (raw engine point read) | 590.4 ns |
| `Collection::get_row` (same read, document layer on top) | 622.8 ns |
| key encoding alone | 17.1 ns |
| payload decode alone | 11.2 ns |
| engine range only, 1 000 entries (no row reads) | 157.7 ns/entry → 157.7 µs |
| scan door split, `parts::<()>` | 177.8 µs |
| `parts::<(parts::Key,)>` | 185.0 µs |
| `parts::<(parts::Row,)>` drained | 925.7 µs |
| default `(Fields, Key, Row)` drained | 943.8 µs |
| the same, `.collect()` | 963.8 µs |
| covered index (`includes(amount)`) | 193.6 µs |
| two-stage join: key streams intersected only | 218.5 µs |
| two-stage join: same, plus one fetch per survivor | 317.8 µs |
| empty probe: tenant that exists nowhere | 2.764 µs |
| empty probe: tenant exists, no suffix matches | 2.785 µs |
| walk 10 / 100 / 1000 entries with `next()` | 4.29 / 20.9 / 183.6 µs |
| seek to the 10th / 100th / 1000th entry with `ScanIter::seek` | 5.14 / 5.21 / 5.29 µs |

## Attribution for one 1 000-entry query (943.8 µs)

| stage | per entry | share |
|---|---|---|
| index entry read (engine scan, block index included) | 157.7 ns | 16.7 % |
| scan door split + `Fields`/`Key` decode | 27.3 ns | 2.9 % |
| fetch stage: primary-table point read + decode | 758.7 ns | 80.4 % |
| — of which the engine's point read alone (`store.get`) | 590.4 ns | 62.6 % |
| materialisation into a `Vec` | 20.0 ns | 2.1 % |

Two lines follow from the table and decide the proposal.

First, the part an index rewrite would touch — the search itself, one descent through the block index — is the empty-probe cost: **2.76 µs per query, 0.3 % of the 943.8 µs**. The rest of the 16.7 % is reading the 1 000 entries the query legitimately asks for and splitting them; no tree shape makes those bytes unnecessary.

Second, the point read is not an index problem at all: `get_row` is 622.8 ns of which the document layer contributes 32 ns. A perfect index cannot improve a lookup whose cost is the row fetch.

## What this supports, contradicts, and cannot test

**Contradicted.** The 3–100× claim. The ceiling for an index-layer rewrite on this workload is 16.7 %, i.e. **1.20× end to end**, and the realistic figure is close to 1.0× because the search component is 0.3 %. The proposal's own headline case — an absent tenant answered by a prefix Bloom — is already 2.76 µs on the existing engine, roughly 110× cheaper than scanning that tenant's data; removing it entirely saves 0.3 % of a query.

**Supported.** A prefix filter does remove real work in the narrow sense that it skips a descent, and the covered index — 193.6 µs against 943.8 µs, **4.87×** — shows where the actual lever is: not reading the row. That lever already exists and needs no new engine.

**Cannot test.** Cold-cache I/O: every number here is warm page cache, so a claim about disk seeks is outside this harness's reach. Whether a packed-stride block beats fjall's compressed blocks for the same bytes is also untestable without writing that engine — which is the proposal's real cost, not a measurement.

## Bearing on the hypergraph discussion

Not applicable directly, but the same lesson transfers: at 1 000 entries the join's key streams cost 218.5 µs and the fetch loop over 121 survivors adds 99 µs, so the operator layer's cost is where algorithmic work pays — as the gallop result showed (42 µs against 1.49 ms, 35×, on skewed inputs). Encoding changes pay in bytes and in write amplification, not in query latency, and should be argued on that ground.