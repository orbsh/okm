# ADR-0065: A checkpoint states the log, not the table

Date: 2026-11-12
Status: RULED & LANDED 2026-11-12

**Depends on:** ADR-0058 (the exported table is a Delta table), ADR-0061 (the lake is a key space — the checkpoint is one more key, written through the same store)

## 1. Context

A version carries only what changed, which is what makes an append cheap: a window is one `add`, a publish is one `add` plus the `remove`s of the files it replaced. The price is paid by the READER, and it grows with time: "which files are live now" is the sum of every version's actions since version 0, so a publisher that lands a version per interval hands its readers a log that is replayed in full to learn that one file is live.

The drain amortises that with a fold (`condense`), but a fold rewrites FILES; it does not shorten the log, and after the fold the log still holds every `add` and `remove` it took to get there. `publish` is where the shape is unavoidable: it replaces its whole state every version, keeps the table at one file, and still grows the log by a version per publish forever.

Delta's answer is the checkpoint: every N versions the writer states the log's current membership in one artifact, and readers start there. This writer made none — `read_log` said so in a comment — and `publish` was written to be the first caller that needs one.

## 2. Decision: an artifact of the log's own actions, at `<V>.checkpoint.json`

A checkpoint is `_delta_log/<V>.checkpoint.json`: JSON lines carrying the SAME action vocabulary a version carries — the `protocol` action as last written, the table's `metaData` verbatim (id, schema, partition columns, created time), and one `add` per live file, in log order. Removals are resolved away: a checkpoint states membership, so a file that left is simply not named. It is claimed with a create-only write like a version, so two writers cannot disagree about what one version's state was.

The reader starts at the NEWEST checkpoint and replays only the versions after it; `absorb` is one function for both artifacts, which is the property being bought — a checkpoint is not a second format, it is the log with its history already folded. If the versions behind it are missing, that is not an error: the checkpoint is where reading starts, and it states `max_version` too.

`N` is the writer's knob, not the format's: `PublishOptions::checkpoint_every` (`0`, the default, writes none), and `delta::write_checkpoint` is public so any other writer — the drain's fold being the obvious next caller — can state its log on its own schedule. `publish` writes one after the commit and after the deletes, never between them: a checkpoint that names a file the store no longer has would turn a saving into a failure.

## 3. Why JSON, and not Delta's Parquet checkpoint

Delta's own checkpoints are Parquet files with a fixed `checkpointSchema` (`add` with its `stats` struct, `partitionValues` map, timestamps, nullability) plus a `_last_checkpoint` pointer file. Writing that faithfully is a second schema to keep in step with the log for the benefit of readers that are not ours: this crate's reader already parses actions as JSON, and the checkpoint's job here is to save it work, not to serve an external engine.

So the form is honest about its audience: it is read by this crate and by nothing else, and the day an engine must skip the log, the Parquet artifact is ADDED beside this one — the action list is the same, and a reader that understands the JSON form needs no second parse. Recording the limit is cheaper than a Parquet writer whose columns no consumer reads.

Two related non-decisions, for the same reason. First, no `_last_checkpoint` pointer: the reader lists `_delta_log/` anyway to find versions, and a pointer is a second surface that can disagree with the listing. Second, a checkpoint is not a data file and not `condense`: `condense` materialises current state as ROWS and commits a replacement, while a checkpoint states WHICH FILES hold that state and commits nothing. They look alike exactly where a table's live set is one file — a publisher's — and there the checkpoint names the same file a `condense` would have written, which is why the two are easy to confuse and worth stating apart.

## 4. What this makes legal, and what is not built

Versions at or below the newest checkpoint are no longer read by anything. Reclaiming them is therefore legal, and it is NOT implemented: deleting a table's history is a retention decision with its own verb and its own default, and the log's versions remain the format's authority for everything a checkpoint does not state. A vacuum that also swept `NNN.json` files is the next step, not a silent part of this one.

An unparseable checkpoint is an error rather than a fallback to version 0. The checkpoint is an optimisation, but it is also the only artifact stating the versions it replaced; a reader that silently replayed from 0 would answer correctly on a small table and hide a broken artifact until the day the old versions were gone.

## 5. Verification

`okm-lake/tests/checkpoint_test.rs` makes the claim in both shapes. A publisher's table with `checkpoint_every = 3` over seven versions gets checkpoints at versions 2 and 5 and nowhere else; its live file and row count equal a table published without checkpoints. Then versions 0..=5 are deleted from the store, and the reader — checkpoint plus version 6 only — answers the same live file and the same rows, which is what "start at the checkpoint" has to mean. The second test drains a change table over three windows, checkpoints it, deletes the first two versions, and asserts `read::current_state` folds to a batch EQUAL to the full replay's, key columns included.

## 6. Consequences

- A publish costs one extra write every N versions: the checkpoint's size is the live file count, not the table's.
- `read_log` now lists for checkpoints as well as versions, and prefers the newest checkpoint; a table with none behaves exactly as before.
- The unit of a checkpoint is the whole table, because that is what the log states. A table with ten thousand live files gets a ten-thousand-line artifact; splitting it into parts is Delta's multi-part checkpoint and would be a second decision.
- A checkpoint never lands on a version that already has one: asking again answers with the existing artifact, so an idempotent publish re-run does not grow the log's side artifacts.