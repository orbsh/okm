# ADR-0056: The segment is the migration path

> **Languages:** [English](0056-the-segment-is-the-migration-path.md) (primary) · [中文](0056-the-segment-is-the-migration-path.zh-CN.md)

**Status:** Accepted (2026-11-10) — decision only; no part of the replay path exists yet. Landed since: capture at the STORE boundary (`ChangelogStore`, §2), the self-describing segment container (§7), `ensure_baseline` (§3), the decoder and the window join in core, and `okm-parquet` (segments → Parquet windows) as the standalone binary that replaced `okm-drain` (§8). Replay exists as `okm-parquet replay` (rows re-written through the target's write path, vocabulary ids re-interned), followed by the post-migration reconciliation scan (`--reconcile`: one range scan of the old store, which is the record); the sink turns the exported directory into a DELTA table (`okm_parquet::delta`, in the drain crate: one JSON commit per window, so appending is one line rather than a rebuilt snapshot; verified against DuckDB's `delta_scan`), and a window carries a document's open fields as a `variant` column in the canonical `struct<metadata, value>` shape, so the segment path is complete for append-only tables; the store-scan export is demoted to a state dump rather than retired; the cutover handshake is decided and NOT built (§6: a per-table `owner` file, a lease, and the equality assertion before a new writer may serve); still missing: delete files (or deletion vectors), partitioning, file-size targets — see the PLAN section of the same date. The Iceberg metadata writer this ADR originally named was replaced rather than extended: see ADR-0058.
**Date:** 2026-11-10
**Depends on:** ADR-0010 (wire bytes), ADR-0034 (a table has a name), ADR-0042 (a layout change is a version, and an upgrade is a deployment shape), ADR-0045 (the log: capture, segments, drain), ADR-0046 (`okm-series`, the other append-only container)

## Context

ADR-0045 left the log as an export feed: captured at the commit boundary, drained into Parquet, consumed by a lake. Two things it did not answer, and one it answered too narrowly.

1. **There is no way back in.** `okm-drain` produces Parquet; `parquet_io::import_parquet` restores a *typed* store through `Table::put`. No consumer turns segments into a store. The log is a one-way valve.
2. **The sink cannot express the truth it is fed.** A window containing a delete stops the drain (then `okm-drain/src/lib.rs:227`, now `okm-parquet/src/lib.rs`), because an append-only Parquet file has no way to say "this row is gone". The log can carry removals; the sink deliberately refuses them. *Answered 2026-11-11:* the export is a change table and a removal is a row (`okm_op`/`okm_slot`/`okm_seq`) — ADR-0058 § Amendment. The constraint below is what forced that design; it is no longer a live refusal.
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

Capture becomes a store decorator (`ChangelogStore`) above `commit_batch`, split per table by the key's 2-byte header, enabled once at startup by configuration, with a per-type opt-out (`#[ok_uncaptured]`) for tables nobody wants history for (an application's own audit trail, an activity log, a cache). Reasons:

- Every write path passes a `Batch` through `commit_batch`, including caller-owned cross-table batches — so the two documented ways to be invisible (an instance assembled without a log; a batch committed directly on the store) disappear, instead of being documented as discipline.
- `carried` needs no declaration, so a boundary with no schema knowledge can enforce the truth-half rule.
- A process-level switch matches the operational fact: the log is either the table's history or it is not, and that is not a per-call-site decision.

Two consequences are part of the decision, not side effects:

- **The sidecar moves out of `with_changelog` and is written unconditionally at assembly** from `CollectionSchema::of::<K, R>()`. A globally captured table whose declaration never registered would have segments no reader can decode.
- **The opt-out is declared on the type, not in the assembly.** Capture is whole-store by default and has to be: one table missing from the log is a restore that will not start, so the exception must be rare, explicit and visible where the table is defined. A list passed at startup is a second declaration site that nothing checks — it drifts from the code silently until someone restores a backup and finds a table absent. `#[ok_uncaptured]` becomes `const CAPTURED: bool = false` next to `NS_PREFIX` and `TABLE_NAME`, and `register` is the only place that reads it: no sidecar, no meta, so no writer is ever opened for that table. A hand-written `Document` impl sets the constant directly; the attribute is only its spelling. The boundary still splits tables by ns, because bytes are all it has — but that is a different question from where the exception is written down.

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

The new build runs as a second process on its own store. It replays baseline + segments until it is caught up, then asks the old writer to stop. The old instance drains its tail into the log and shuts down gracefully; the new instance applies that tail (seconds), claims, and only then is traffic moved to it — after which it serves, writing to its own store and appending to the **same per-table log**. The log belongs to the table, not to the store instance, so the history is one continuous stream across however many generations of stores it feeds.

The handshake is the part that must not stay vague, because what it protects is single-writer. **A hook that returns zero while the old writer can still commit is a split brain**, and process state cannot rule that out: `Changelog::drop` drains on a best-effort basis and cannot report a failure, and a killed process leaves a `.partial` that readers ignore by contract. The proof of the fence therefore has to be an artifact on the same medium as the segments, and the right to write has to be a fact that outlives any one process.

That artifact is one file per table, `<dir>/owner`, written the way a segment is sealed — temporary file, fsync, rename. It carries `holder`, `epoch`, `state` and the position: `writing`, `draining`, `handed-over`. A writer may commit only while the state is `writing`, the holder is itself, and its lease is fresh; it refreshes the lease on a timer and **stops writing when it cannot renew it**, because a directory and an object prefix cannot enforce exclusion on their own. Self-fencing is the only correct posture when the backend has no lock to give. Commits read a gate in memory, never the file: the poll is one `stat` (or one HEAD) every few hundred milliseconds, off the write path.

The file is text, one `key value` per line, unknown keys ignored — because its reader is a different BUILD by construction (the old version writes it, the new one reads it across an upgrade), so forward compatibility has to be free and `cat owner` has to be readable at 3 a.m. The state is one word (`writing`, `draining`, `handed-over`) and never a sentence; the position names a segment file rather than an ordinal, so "how a name is derived" stays in the code instead of being restated in the protocol. A claim this build cannot parse is an error and never a default: guessing is the split brain the file exists to prevent. A renewal is a read-verify-write, which is exactly atomic only on a backend with conditional puts; where it is not, the takeover side retries until it sees `handed-over` or gives up, and the loss is an extra round, never a second writer.

The cutover is then three writes and one assertion. The new instance, caught up, writes `draining` with `epoch + 1`; the old one sees it at its next refresh, refuses new work, lets the in-flight commits land, `seal_last()`s, and writes `handed-over` with the last segment and its position; the new instance sees `handed-over`, replays once more, and **may claim `writing` only if its own position equals the one it was handed**. Not equal, replay again; still not equal, abandon the cutover and stay read-only.

The position in that handover is the last sealed segment, and that is the whole of it — because the log lives on a POSIX filesystem: a disk, a bind mount, an NFS mount. That is the only place where a seal IS an atomic rename, a tail can be fsynced before a handover is announced, and two processes can both read the same file. An object store is not a log (no rename, no fsync), so S3 is not a location for the changelog; what leaves the machine leaves as a Delta table (ADR-0058), which serves backup, restore and analytics in one format and is readable by tools that have never heard of okm. A remote file protocol (FTP, WebDAV) is not a location either: both leave atomicity to the implementation, so the fence would have to be re-argued per server. A deployment that needs a shared log mounts a filesystem; one that needs a copy exports.

A log that only grows is a log that replays forever, so compaction is a first-class verb, not a chore left to the host. `ChangelogStore::compact(keep)` writes one baseline per table — close the writer, scan the truth half through the same filter capture uses, seal — and then deletes every segment numbered below the FIRST file of that baseline, keeping the newest `keep` of them as a window for a human. A baseline is a checkpoint, so what it covers is not lost: a replay applies the newest baseline and everything above it (ADR-0056 §3), and the segments below are exactly the history that snapshot already contains. `keep = 0` is the default shape of the mechanism's purpose — history lives in the Delta export, whose versions nothing here can prune.
Compaction is automatic, and the policy rides on the same parameter object as everything else (`ChangelogConfig.compact: Option<CompactPolicy>` — uncovered bytes, age of the covering point, and how many segments to keep). The trigger is a ROTATION and nothing else: that is where the writer is already at a boundary, already paying for an fsync, and already the process that may write; the check costs one directory read on a rare event, never a stat per commit. It is deliberately lazy about time — core has no clock and no scheduler (ADR-0045 §7), so a quiet table compacts at its next write, or when the host calls the verb itself. What a separate process CAN do is the maintenance side, and `okm-parquet` carries it: `status` reports what a directory holds and who holds it (segments, bytes the baseline does not cover, age of the covering point, and the claim) without opening anything; `compact` takes the STORE as a second argument — a baseline is a snapshot of state, and state lives in the store, not in the log — and refuses while a fresh claim says someone is writing. It cannot be a separate process: the baseline's scan has to be consistent with the writer's numbering, and a scanner running beside a live writer can miss a row that landed in a segment numbered below the new baseline — unrecoverable, and with no assertion that would notice. A second process may compact only what nothing is writing to, which makes it a repair tool, not a schedule.

Two guards, both about the claim. Compaction runs only for the holder that may write, and never while a handover is in flight: the position the holder named has to exist when the successor arrives, and a segment pruned under its feet is a cutover that can never finish. What compaction deletes is the LOG's history, never the store's state.

Nor is there a second copy of the segments to keep honest. The log on disk is the record of change; what leaves the machine leaves as a Delta table, and a copy of the internal format somewhere else would be a backup nobody can restore from and a second thing to keep consistent.

Readers need none of this. A sealed segment is immutable, so replay can run beside a live writer for as long as it takes; the file answers two questions only — may I write, and where did the last writer stop. Whatever triggers the drain — a signal, `kubectl delete`, an HTTP call to a drain route the application happens to expose — is a deployment-layer choice whose sole contract is the one above: non-zero aborts, and zero is honest only once the old writer is fenced. okm ships no control plane and no endpoint for this; it ships the file, the verbs that move it, and one program it will run when the claim changes hands — named by the deployment, never authored by okm.

Claiming the log makes an instance the writer; it does not move traffic, and it cannot, because where requests come from is never in the log. A gateway in front of two ports on one machine, a load balancer in front of two containers, a DNS weight — each is a fact about the deployment, and okm holds no control plane to publish it with. So the last step of a cutover is a PROGRAM the host names: `ChangelogConfig.handover: Option<HandoverHook>`, run once per take-over by the **successor** — the instance that just became the writer and is about to serve. Not the drainer: a holder on its way out has nothing to announce, and it may be gone before the switch is possible.

The order is claim first, announce second, and the reverse is a silent failure. An announcement that runs before the claim points traffic at an instance that can still fail to claim, and the gateway then sends requests to a process that may not write; with this order a hook failure leaves the store owned by the instance that could not announce, so single-writer holds, nothing is rolled back (a claim is never undone), and the operator sees a half-done cutover — owned, not announced — rather than a switch that quietly did nothing.

What the program is given is the environment, the one channel a script in a container shares with the process that spawned it: `OKM_CHANGELOG_ROOT`, `OKM_ROLE` (`successor` when it caught up from another instance, `resume` when the claim was already its own), `OKM_HOLDER`, `OKM_DATA` (this instance's store location, as configured) and `OKM_TABLES`. Nothing about the deployment is added. The shared-mount case needs no more than that: the script reads the owner files itself when it wants a position. The two-ports-on-one-machine case needs no more either: its own port is already in its environment, and the script's job is one write to the gateway's upstream. The alternative — okm publishing a routing API — would be a control plane, and a control plane is the thing this design keeps out (ADR-0045 §7).

A fresh log announces nothing: there is no predecessor, no traffic to move, and the deployment that just created the directory is the one that points at it. A `resume` does announce, because that is the retry path for a hook that failed after the claim: the instance is the writer either way, and its own restart is the cheapest place to try again.

Both sides of that are one call in `okm-core`, because the alternative is every host re-deriving the sequence: `ChangelogStore::drain_and_hand_over` seals every open table, waits for the copies it can wait for, and writes the position for each table whose claim asks it to; `ChangelogStore::take_over` decides the role per table, announces, waits, applies the log byte for byte up to the handed position, and claims — in that order, which is the safety argument. Capture itself is a parameter, never a config file okm reads: a location (`Local(path)`) or `Off`, where `Off` keeps the assembly identical and costs one branch per commit with nothing opened and nothing written.

A layout change rotates the segment and starts a new sidecar version (`schema-v<N>.json`, ADR-0042), so one log carries several epochs and a replay reads each op under the version it was written with. The DIRECTORY never changes, and nothing may refuse it on the way in: a generation stamps BYTES, not a path. So `register` opens a directory whose newest segment another build wrote, the declaration is chosen per SEGMENT by the number in its header, and a build that does not have that generation's sidecar stops rather than borrowing the neighbouring one — borrowing is how a payload gets read as the wrong row. Two consequences follow. **A window is one generation**: a sink decodes with a single declaration, so a segment of another generation ends the window and starts the next one. And **the cutover's apply is generation-aware**: a byte copy is honest only for segments this build's declaration produced, so a handover across a shape change replays the tail through the target's write path and takes the claim with that position (`take_over`'s `after`), instead of copying payloads its own store would read as another row. Two builds still never share a store: single-writer is the invariant the whole shape rests on, and it is what makes "old drains, new applies" well defined.

### 7. The container describes itself

A segment today is a bare record stream (`[u32 BE len][OpFrame]`); everything needed to read it lives in the directory name (header hex) and the sibling sidecar. That is exactly what stops a segment from being usable away from its producer, which is the point of a file-based migration path. Format version 2 adds:

```text
header  fmt u8 | kind u8 (delta=1, baseline=2) | layout_version u8 | hdr_len u8
        header | name_len u16 BE | name | schema fingerprint [16]
body    the existing record stream: [u32 BE len][OpFrame] …
footer  body_len u64 BE | records u64 BE | ordinal u64 BE | crc32 u32 BE
```

- **Header at open, footer at seal.** The rename is the commit, so a footer written just before it is atomic with the file.
- **`name` and `ns` make the table identity travel.** The name comes from `#[ok_name]` via `Document::TABLE_NAME` (ADR-0034) and becomes a field of `CollectionSchema`, because the directory name is the header in hex and a lake table has to be named.
- **The fingerprint ties bytes to their declaration**: the reader verifies the sidecar it found is the one this file was written under, instead of trusting the directory.
- **The ordinal (currently only in the file name) and the checksum make a copy trustworthy**: a gap becomes detectable instead of being silently skipped by `> watermark`, and a short transfer is refused instead of half-replayed.
- **No mark, no compatibility.** Neither end carries an ASCII magic: the file opens with its format byte and closes with a 28-byte footer, and a format-1 segment is not read. A marker had two jobs — telling v1 from v2, and confirming that what sits at the tail is a footer. The first is gone by decision; the second is `body_len` against the file's own length plus the checksum, which a marker cannot improve.
- **No schema body in the segment.** The sidecar carries the declaration, one file per layout version; embedding it per segment would duplicate the same declaration and make the file's atom the wrong one. The atomic unit is the **table directory**: segments + sidecars, which is what a copy ships and what `drain` already takes.

### 8. The cut inside the workspace

- **Core keeps the bytes and takes the drain.** Segment frame iteration, the truth-half filter (now derived from `SlotMap`'s boundaries instead of a hand-kept list — `declared_index_base` / `declared_reduce_base` / `declared_lead_base` / the vocabulary pairs already say which side a slot is on), window joining, the watermark and the local hand-off of a turned window all belong to the mechanism that produces the files. `okm-drain` stops being a crate of its own: the window join moves down into core, the format half moves out.
- **The document decoder moves down with it.** `okm_dynamic::decode_payload` is a pure function over `CollectionSchema` — already a core type — returning a map of core `DynamicValue`s. It sits above core for a historical reason (it was written beside the planner), not a structural one. Moving `decode.rs` into core is what makes "the drain is core functionality" true rather than nominal, and it removes the drain's only upward dependency; `okm-dynamic` keeps re-exporting the name so no existing call site changes.
- **Only the formats move out**: the schema-driven Arrow column builder (`okm_dynamic::arrow`), Parquet, the table-format log, the TYPED bridge (`to_record_batch`, the graph's edge table) and the snapshot export (`parquet_io`). A core that cannot depend upward cannot host them (ADR-0045 makes the same point about the Arrow batch builder), and each of them is a projection of a segment rather than part of producing one. **Replay does not move out**: applying a segment — the order, the row decode, the vocabulary re-interning, what a removal means — is mechanism, so it lives in core beside the window join (`okm_core::changelog::replay`), and the one thing core cannot name, the schema-driven target, comes in through the `ReplayTarget` trait that `okm-dynamic` implements for `DynamicCollection`. The binary that drives it (`okm-parquet replay`) is a CLI over that API, not its owner.
- **`okm-parquet` is a standalone CLI with no link to the application.** It reads table directories (segments + sidecars), writes Parquet + manifest, then commits the file to the table log. The "the application must hand the exporter an interface" requirement disappears together with the store-scan export that needed it: lake tables take their names from the segment header, and columns from the sidecar. The table format follows the same road, one projection further out.
- **Replay needs one file, not one interface.** The target declaration travels as a `CollectionSchema` JSON the new build exports once; baseline, segments, ordinals and sidecars are all on disk. So the tool is fully independent for `parquet`, and independent-with-one-file for `replay`.
- **The watermark becomes per `(consumer, table)`.** One file per consumer while segment numbers are per table means a multi-table drain silently skips every table that is behind the highest one (`okm-drain/src/lib.rs:182`).

### 9. Not decided here: S3, slatedb, multi-writer, other engines

Recorded because they were asked and they constrain what lands next:

- **S3 as a shipping path for the log.** Withdrawn: an object store has no atomic rename and no fsync, so it cannot be where a seal lands, and a copy of the internal format with no restore verb beside it is a backup nobody can use. Data leaves the machine as a Delta table (ADR-0058) — backup, restore and analytics in one format.
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
- The hot path is unchanged: one Option check, one memcpy, no fsync. Capture is either on or off for the whole store, and an exception is an attribute on the table's own type.
- Boot gains one bounded scan the first time a table is captured, and nothing after that.
- Two failure windows are now explicit and priced: a crash between commit and append (closed by the reconciliation pass at cutover), and a delete that a row scan cannot repair (closed by shipping the sealed segment that carried it).
- `okm-core` keeps the bytes, takes the drain (window join and the document decoder) and parts with the file formats; a standalone binary owns Parquet and replay.
- A table name stops being a compile-time secret of the export path: it travels in the segment.
- A cutover costs a lease: one poll loop and one self-fencing gate per writer, paid only by a deployment that runs two of them. A single-process deployment never opens the file.

## Related records

ADR-0045 (the log itself: capture point, at-most-once, segments, drain), ADR-0047 (the two faces of a commit — this ADR consumes the durable one), ADR-0042 (layout versions as stamps; the blue/green shape this ADR gives a carrier), ADR-0034 (table names, `#[ok_name]`, export shape), ADR-0032 (vocabulary ids are per store, labels are the fact), ADR-0046 (`okm-series`, the container this one is not).