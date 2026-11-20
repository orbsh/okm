# 0071 — The baseline is a pinned lake version, and the tail is still the segment

Status: decided 2026-11-20, implemented — the pointer, its verification, the lake-side compaction and the restore; the capture default and the compaction threshold are follow-ups in PLAN, because both only pay off once the baseline is pinned.

## Context

A capturing table keeps a second copy of its own payload. After compaction the log directory holds a **baseline**: a full scan of the truth half written back out as segments, measured at 125.4 B/row on the 1 M-row order arm, plus up to 1 GiB of uncovered deltas (`CompactPolicy` defaults: 1 GiB or 24 h, keep 0). The store itself is 128.5 B/row at 5 M rows with one secondary index. On the app host that is two copies of the table's bytes, and the second one exists only so a reader can start somewhere.

The lake already holds the same truth half. It is produced by a drainer from the same segments (ADR-0058, ADR-0061, ADR-0065), so the app re-derives by scanning what it could have read from an object store. ADR-0042 §7 already names the cold side of a move as a lakehouse export; this makes the *baseline* one.

The handover does not depend on the baseline's location. What it depends on is a position on the same medium as the segments: the owner file, the last sealed segment's name, three writes and one assertion (ADR-0056 §6). That does not move.

One measured defect has to be fixed with it. The lake's parquet writer sets no `WriterProperties`, so its data files are uncompressed: 120.2 B/row on the 1 M-row arm, against 33.0 B/row for the same rows with zstd(1) and 39.5 B/row for lance's default v2 encoding. Moving a baseline into the lake before turning compression on moves 120 B/row to 120 B/row and saves nothing.

## The decision

1. **A baseline may be a lake version.** When a lake export exists and is caught up, compaction records `(lake table, version, last covered segment)` in the log directory instead of writing a full-state segment. Replay is then lake@V restored, plus the segments after S. The app stops holding a second copy of the table and starts holding a pointer plus a bounded tail.
2. **Segments keep the tail and the fence.** The owner file, the position, the three writes and the one assertion are unchanged. The tail crosses as **sealed segment blobs**, not as a re-encoded window, so a cutover converts no formats on its critical path and does not wait for the drainer.
3. **The scanned baseline stays as the fallback.** No lake, lake behind, or a pointer that fails verification means writing the baseline by scanning, as today. The pointer is never the only way to start.
4. **The pointer is verified, not trusted.** The reader re-lists the lake, confirms the version exists and that its manifest covers S, and refuses the pointer on any mismatch. ADR-0065 refused `_last_checkpoint` for the same reason: a pointer is a second surface that can disagree with the listing. The listing it reads is the one **at V**, not the live one: a lake that has drained windows since must not answer for a version it no longer describes, so the log is read as of the version the pointer names.
5. **Lake data files are written with zstd.** The baseline's move is worth something only if the copy it lands in is smaller than the one it replaces.
6. **The window follows what the recovery term is.** `CompactPolicy::default()` stays at 1 GiB, because for a table whose baseline is a scan that scan is the only recoverable region the app host has. A table whose bulk is pinned in the lake takes `CompactPolicy::pinned()` — 256 MiB, where the smaller window costs nothing but a slightly longer replay tail. The two settings are one decision, which is why neither replaces the other.
7. **Capture is on by default, and `Off` is a statement about the deployment.** `Location::Off` stays, but its meaning narrows to "there is no local durable directory to put a log in" — `EphemeralStore` (ADR-0072: an in-memory engine whose data dies with the process), a read-only replica, a container's scratch disk. It is not "logging is expensive": with the baseline pinned in the lake, the steady cost is a bounded window, so the reason to switch it off has to be physical.
8. **Two levels, both explicit.** The store decides *where* (`ChangelogConfig::location`); the table decides *whether* (`Collection::captured` registers, `Collection::new` does not). Default-on turns the table level from opt-in to opt-out, so the constructor that opts a table out has to read like one.
9. **The lake's location is independent of the log's, and optional.** A lake may be another directory, an object store, or absent. Absent means compaction falls back to the scanned baseline. Because the writer verifies the pointer (item 4), a lake the app cannot read is a lake whose pointer is never claimed — that is the fallback working, not a failure.

## What follows from it

- **App disk**: the log directory becomes ≤ 1 GiB of deltas plus one small pointer. The permanent 1× table term is gone; the whole-host accounting moves from ~2× table to ~1× table plus a bounded window.
- **Cross-host cutover**: the bulk comes from object storage, the tail is one shipped blob. A shared POSIX mount stops being required for the bulk; it is still required for the fence, or replaced by an object-store lease — which is a separate decision, not taken here.
- **Restore**: lake@V plus segments, through the existing export/import path. Vocabulary ids re-intern (ADR-0032) and indexes rebuild, exactly as they do for an export today.
- **Backup**: for an exported table the lake becomes the archive of record, so its retention (`vacuum` by `keep_versions`) *is* the backup retention and must be stated as such. Tables nobody exports still have no archive but their own.
- **Not decided here**: putting the fence itself on object storage. Condition-put leases would reintroduce the per-provider argument ADR-0056 §6 rejected.

## Rejected

- **A lake version as the handover position.** The fence would lose its same-medium evidence and its position would advance in a third process (the drainer), turning a seconds-long assertion into a wait on someone else's progress.
- **Re-encoding the tail into a lake window at cutover.** It puts a format conversion and a drain dependency inside the window that must be short.
- **Lance as the lake format.** Measured at 39.5 B/row against parquet+zstd's 33.0, it pins arrow 58 against okm's 60 (two arrows in one binary), needs `protoc` to build, and pulls datafusion in. Its real strength — random access, indexes, vector search — belongs to a serving tier okm does not have; if that changes it is an additive export, not a replacement.
- **DuckLake.** The catalog is a SQL database, so the writer gains an engine and an atomic-commit problem across two media (catalog rows and parquet files) to replace one JSON append.
- **Iceberg.** Resolving the current metadata pointer needs a catalog, and `iceberg-rust` is still 0.x on the write side. It remains the format to translate *to* (XTable / UniForm) if an Iceberg-only reader ever matters.