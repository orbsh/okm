# ADR-0056: The segment is the migration path

> **Languages:** [English](0056-the-segment-is-the-migration-path.md) (primary) · [中文](0056-the-segment-is-the-migration-path.zh-CN.md)

**Status:** Accepted (2026-11-10) — decision only; no part of the replay path exists yet. Landed since: capture at the STORE boundary (`ChangelogStore`, §2), the self-describing segment container (§7), `ensure_baseline` (§3), the decoder and the window join in core, and `okm-parquet` (segments → Parquet windows) as the standalone binary that replaced `okm-drain` (§8). Replay exists as `okm-parquet replay` (rows re-written through the target's write path, vocabulary ids re-interned), followed by the post-migration reconciliation scan (`--reconcile`: one range scan of the old store, which is the record); still missing: Iceberg metadata — see the PLAN section of the same date.
**Date:** 2026-11-10
**Depends on:** ADR-0010 (wire bytes), ADR-0034 (a table has a name), ADR-0042 (a layout change is a version, and an upgrade is a deployment shape), ADR-0045 (the log: capture, segments, drain), ADR-0046 (`okm-series`, the other append-only container)

## Context

ADR-0045 left the log as an export feed: captured at the commit boundary, drained into Parquet, consumed by a lake. Two things it did not answer, and one it answered too narrowly.

1. **There is no way back in.** `okm-drain` produces Parquet; `parquet_io::import_parquet` restores a *typed* store through `Table::put`. No consumer turns segments into a store. The log is a one-way valve.
2. **The sink cannot express the truth it is fed.** A window containing a delete stops the drain (`okm-drain/src/lib.rs:227`), because an append-only Parquet file has no way to say "this row is gone". The log can carry removals; the sink deliberately refuses them.
3. **ADR-0042 §7 named the deployment shape (blue/green, short cutover, incremental catch-up) but not its carrier.** The catch-up was left as "import", which in practice means Parquet — which loses the deletes and collapses history into an append.

Meanwhile three structural facts about the current capture decide what is possible:

- **The captured half is already the truth half.** `carried()` (`okm-core/src/changelog.rs:476`) drops declared indexes, reduces, lead faces, the graph's traversal faces, the vector snapshot and the table meta row, and keeps primary rows, the dynamic segment, the four vocabulary directions and the two predicate faces. What the log carries is documents, not entries — which is exactly what a write path needs to rebuild the derived half.
- **The filter is bytes, not declaration.** `carried` reads the key's header prefix and the slot number and nothing else. It therefore does not need a schema, a type, or an assembly point — only the key space.
- **Capture is wired per collection instance.** `with_changelog` is a constructor on `Collection` (`okm-core/src/model/document.rs`): it opens that table's directory, writes that table's sidecar, and stages only through that instance's commits. An instance assembled without it writes rows no log will ever mention, and a caller-owned batch must remember `Collection::commit_batch` or the `stage_capture` / `flush_capture` pair to be captured at all.

## Decision

### 1. Vocabulary: changelog, segment, baseline, drain, replay

| term | what it is |
|---|---|
| **changelog** | the mechanism: one change stream per table, appended at the commit boundary, drained out of process (ADR-0045) |
| **segment** | the stream's sealed file unit (`.segment`), immutable, ordered by name |
| **baseline** | a segment whose kind is *state* instead of *delta*: the table's whole truth half at one moment |
| **drain** | a consumer that turns segments into another format (Parquet today) |
| **replay** | a consumer that turns segments back into writes on a store |

`okm-series` is **not** a second segment. Its sealed parts are `.log` files with a different body (bucketed directories, time deltas, compressed blocks with an index, nTLV bodies, ADR-0046). The two containers share conventions — directory of parts, seal by rename, a torn tail is truncation — and nothing else. So each format carries its own magic and a reader refuses the other one; there is no cross-format kind byte. Inside the segment format one kind byte remains, because delta and baseline have the same record shape and the same file extension but opposite replay meaning.

### 2. Capture moves to the store boundary, enabled globally

Capture becomes a store decorator (`ChangelogStore`) above `commit_batch`, split per table by the key's 2-byte header, enabled once at startup by configuration, with an exclusion list by ns for tables nobody wants history for (an application's own append-only log tables, caches). Reasons:

- Every write path passes a `Batch` through `commit_batch`, including caller-owned cross-table batches — so the two documented ways to be invisible (an instance assembled without a log; a batch committed directly on the store) disappear, instead of being documented as discipline.
- `carried` needs no declaration, so a boundary with no schema knowledge can enforce the truth-half rule.
- A process-level switch matches the operational fact: the log is either the table's history or it is not, and that is not a per-call-site decision.

Two consequences are part of the decision, not side effects:

- **The sidecar moves out of `with_changelog` and is written unconditionally at assembly** from `CollectionSchema::of::<K, R>()`. A globally captured table whose declaration never registered would have segments no reader can decode.
- **The exclusion list is by ns**, because a name is not knowable at the boundary. Excluding by table name would require a runtime name↔ns registry, and the store contract deliberately has none.

### 3. A baseline exists before the first delta is trusted

At startup, with capture on: if a table has no baseline segment, one is produced — scan the table's **truth half** and write the frames into a segment whose kind is baseline. The prototype already exists as `Collection::reconcile` (`document.rs:1443`, rows → the log's own PUT frames); the extension is that it must cover the whole truth set, not slot 0 alone: dynamic segment `0x01`, field-name and predicate vocabulary `0x02`–`0x05`, and both directions of every categorical vocabulary (`0x1nnn`/`0x2nnn`).

Two properties of that step are structural:

- **It can only run inside the process that holds the store.** A KV store has one writer, so a second process cannot read the engine's directory. The baseline is therefore a boot cost — one bounded full scan per table, once, not a standing tax — and it is the reason this ADR does not make export depend on an external tool.
- **A log enabled mid-life is still complete.** Without a baseline, everything written before capture is invisible to every consumer, forever. With one, the log becomes a migration source regardless of when it was turned on, which is what makes a global switch safe to flip on an existing deployment.

### 4. The log stays a hint; completeness is verified against the store

This ADR does **not** promote the log to a write-ahead record. Nothing is appended before the engine commit returns `Ok` (ADR-0045 §7, unchanged), there is no fsync on the hot path, and a crash between the commit and the append still loses the tail. Consequence for migration, stated once so nobody discovers it during a cutover:

**A migration ends with a reconciliation pass against the still-live old store.** The replay covers the bulk; the old store is the authority for whatever the log missed, including removals, which a row scan cannot see (`reconcile` cannot repair a delete and says so). The cutover order keeps that window short: the old instance stops accepting writes, its sealed segments are shipped, one reconciliation range scan closes the gap, then traffic moves.

Why not the other way round — log first, then store, one fsync per write, the log as the recovery authority? Because every engine here already has a journal of its own (fjall, redb, slatedb), and a second one above it is double bookkeeping bought with per-write latency; and because at-most-once with a store-side check is the cheaper of the two ways to be honest about a file that can be torn. The name "WAL" would promise both durability-before-ack and recovery, and this file has neither.

### 5. Replay is the migration consumer; Parquet is the analytics consumer

Replay reads baseline + segments in order and applies them **through the new build's write path**: decode with the sidecar of the version the op was written under, re-encode under the new declaration, commit. Derived state (indexes, reduces, lead faces, graph faces, vector rows) is rebuilt by the write path and never copied. Vocabulary ids are **re-resolved through the label faces** (`id → label → new id`), never copied, because an id is per-store first-write order (ADR-0032) while the label is the fact.

The split of duties is total: **replay is the only consumer that can express a removal**, so migration goes through replay, and Parquet keeps its job of feeding a lake where history is an append and compaction belongs to the sink. A shape change therefore does not pass through Parquet at all.

### 6. The upgrade flow, in full

The new build runs as a second process on its own store. It replays baseline + segments until it is caught up, then signals. The coordinator switches traffic; the old instance drains its tail into the log and shuts down gracefully; the new instance applies that tail (seconds), then serves — writing to its own store and appending to the **same per-table log**. The log belongs to the table, not to the store instance, so the history is one continuous stream across however many generations of stores it feeds.

A layout change rotates the segment and starts a new sidecar version (`schema-v<N>.json`, ADR-0042), so one log carries several epochs and a replay reads each op under the version it was written with. Two builds still never share a store: single-writer is the invariant the whole shape rests on, and it is what makes "old drains, new applies" well defined.

### 7. The container describes itself

A segment today is a bare record stream (`[u32 BE len][OpFrame]`); everything needed to read it lives in the directory name (header hex) and the sibling sidecar. That is exactly what stops a segment from being usable away from its producer, which is the point of a file-based migration path. Format version 2 adds:

```text
header  "OKMS" | fmt u8 | kind u8 (delta=1, baseline=2) | layout_version u8
        ns u16 BE | name_len u16 BE | name | schema fingerprint [16]
body    the existing record stream: [u32 BE len][OpFrame] …
footer  ordinal u64 BE | records u64 BE | body_len u64 BE | crc32 u32 BE | "OKME"
```

- **Header at open, footer at seal.** The rename is the commit, so a footer written just before it is atomic with the file.
- **`name` and `ns` make the table identity travel.** The name comes from `#[ok_name]` via `Document::TABLE_NAME` (ADR-0034) and becomes a field of `CollectionSchema`, because the directory name is the header in hex and a lake table has to be named.
- **The fingerprint ties bytes to their declaration**: the reader verifies the sidecar it found is the one this file was written under, instead of trusting the directory.
- **The ordinal (currently only in the file name) and the checksum make a copy trustworthy**: a gap becomes detectable instead of being silently skipped by `> watermark`, and a short transfer is refused instead of half-replayed.
- **Format 1 stays readable.** A reader accepts the bare stream (no magic) as version 1 — the shipped logs do not need a migration to be read by the new tool.
- **No schema body in the segment.** The sidecar carries the declaration, one file per layout version; embedding it per segment would duplicate the same declaration and make the file's atom the wrong one. The atomic unit is the **table directory**: segments + sidecars, which is what a copy ships and what `drain` already takes.

### 8. The cut inside the workspace

- **Core keeps the bytes and takes the drain.** Segment frame iteration, the truth-half filter (now derived from `SlotMap`'s boundaries instead of a hand-kept list — `declared_index_base` / `declared_reduce_base` / `declared_lead_base` / the vocabulary pairs already say which side a slot is on), window joining, the watermark and the local hand-off of a turned window all belong to the mechanism that produces the files. `okm-drain` stops being a crate of its own: the window join moves down into core, the format half moves out.
- **The document decoder moves down with it.** `okm_dynamic::decode_payload` is a pure function over `CollectionSchema` — already a core type — returning a map of core `DynamicValue`s. It sits above core for a historical reason (it was written beside the planner), not a structural one. Moving `decode.rs` into core is what makes "the drain is core functionality" true rather than nominal, and it removes the drain's only upward dependency; `okm-dynamic` keeps re-exporting the name so no existing call site changes.
- **Only the formats move out**: the schema-driven Arrow column builder (`okm_dynamic::arrow`), Parquet, Iceberg metadata, and the replay driver. A core that cannot depend upward cannot host them (ADR-0045 makes the same point about the Arrow batch builder), and none of them is mechanism — each is a projection of a segment.
- **`okm-parquet` is a standalone CLI with no link to the application.** It reads table directories (segments + sidecars), writes Parquet + manifest, then Iceberg metadata. The "the application must hand the exporter an interface" requirement disappears together with the store-scan export that needed it: lake tables take their names from the segment header, and columns from the sidecar. Iceberg follows the same road, one format projection further out.
- **Replay needs one file, not one interface.** The target declaration travels as a `CollectionSchema` JSON the new build exports once; baseline, segments, ordinals and sidecars are all on disk. So the tool is fully independent for `parquet`, and independent-with-one-file for `replay`.
- **The watermark becomes per `(consumer, table)`.** One file per consumer while segment numbers are per table means a multi-table drain silently skips every table that is behind the highest one (`okm-drain/src/lib.rs:182`).

### 9. Not decided here: S3, slatedb, multi-writer, other engines

Recorded because they were asked and they constrain what lands next:

- **S3 may live in core, behind a feature.** The dependency objection does not apply: `object_store` and `tokio` are already optional dependencies of `okm-core` (`Cargo.toml`, for the slatedb backend), so an S3 sink adds no crate to the tree — it adds a feature and a shipping path. What must stay true: the upload never sits inside a commit (a sealed segment is handed to a worker queue, so the write path is still one memcpy), the store contract stays sync, and the feature is off by default. The unit to ship is the table directory (sealed segments + sidecars + baseline); the client is `object_store` — S3 / GCS / Azure / local — not `aws-sdk-s3`.
- **`slatedb` stays an engine option.** Rebuilding a store from a log is a boot-time cost; a store that lives on object storage is a different capability (no local disk, low-latency writes after boot). Log + baseline in S3 makes the *system* recoverable; it does not make any engine redundant.
- **Multi-writer is declined, not missing.** Two builds never share a store (ADR-0042 §7); implementing concurrent writers would add coordination (leases, CAS, conflict resolution) to a contract whose simplicity is the product. If a deployment needs many writers, that is a different system.
- **Any engine that satisfies the store contract qualifies** — ordered `scan_range` plus atomic batch. `surrealkv` is a candidate on capability; rocksdb is declined on build cost, not capability.

## Alternatives

- **A Parquet round trip as the migration path.** Rejected: the sink refuses deletes, so a migration through it silently loses removals; and a round trip through a columnar file is a lossy re-declaration of a shape we already have as bytes plus sidecar.
- **Raw byte replay of the segment's ops** (no decode/re-encode). Rejected: it re-writes derived entries into the derived slots, so a shape change keeps the old faces and the old reduce totals, and vocabulary ids carry across stores where they mean nothing.
- **A true WAL: append before the commit, fsync, rebuild the store from the log.** Rejected as above — double journaling above engines that already journal, plus a per-write latency tax, for a recovery authority we do not need.
- **Per-collection capture, kept as is.** Rejected: it is opt-in per assembly site, so completeness depends on the application remembering; and it costs a documented discipline around caller-owned batches.
- **A per-segment embedded schema.** Rejected: a second source of truth for one declaration, and a bigger file for no reader.

## Consequences

- The log becomes the thing that survives: stores are replaceable, the directory is the history. Export, migration and lake loading all read one format.
- The hot path is unchanged: one Option check, one memcpy, no fsync. Capture is either on or off for the whole store, and the exclusion list is a config line.
- Boot gains one bounded scan the first time a table is captured, and nothing after that.
- Two failure windows are now explicit and priced: a crash between commit and append (closed by the reconciliation pass at cutover), and a delete that a row scan cannot repair (closed by shipping the sealed segment that carried it).
- `okm-core` keeps the bytes, takes the drain (window join and the document decoder) and parts with the file formats; a standalone binary owns Parquet and replay.
- A table name stops being a compile-time secret of the export path: it travels in the segment.

## Related records

ADR-0045 (the log itself: capture point, at-most-once, segments, drain), ADR-0047 (the two faces of a commit — this ADR consumes the durable one), ADR-0042 (layout versions as stamps; the blue/green shape this ADR gives a carrier), ADR-0034 (table names, `#[ok_name]`, export shape), ADR-0032 (vocabulary ids are per store, labels are the fact), ADR-0046 (`okm-series`, the container this one is not).