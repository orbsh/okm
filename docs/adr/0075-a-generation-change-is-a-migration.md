# 0075 — A generation change is a migration, one table at a time

Status: decided 2026-11-21, not yet implemented.

## Context

ADR-0074 §2 and §4 made the store describe its tables: the meta row and every segment header carry the declaration as bytes, and attach compares that copy against the one this build compiles from its types. Until then a declaration existed only in the binary — a store could not disagree with a build, because it carried no opinion of its own. Now it can, and the answer it gets is a single `LayoutMismatch`: a door with nothing behind it. A store of the previous generation is a store this build cannot use, and the only migration anyone can write today is "export everything, wipe everything, import everything".

That answer is wrong twice over. It makes a table that never changed pay for the one that did, and it throws away the bytes before anything has said what they were.

Four facts decide the shape this decision can take.

- **The key carries the ns, not the generation.** A key is `[partition prefix][ns: 2 bytes][encoded primary key]`; `layout_version` lives in the segment header and in the meta row, never in the key. Two generations therefore cannot share a keyspace: the old row and the new row are the same key, so a backfill lands on top of the only copy there is.
- **Generations already coexist per table.** The log is one directory per table, a window is one generation and ends where the next begins (`window.rs`: "the changelog directory never changes, so this is the normal case after an upgrade"), and `load_declaration` answers with the newest one. What the log cannot do is read a segment without the declaration that segment carries — which is exactly why it survives a generation bump at all.
- **The log is a wrapper, not a store.** `ChangelogStore<S>` is a `VirtualStorage` and staging lives in its `commit_batch` alone. A `Collection` assembled over the bare engine writes and records nothing. ADR-0062's restore already relies on the assembly point being where this is decided, through `ns_override`; this decision uses the same seam for the same reason.
- **The ns is compile-time** (ADR-0002), and `ns_override` is the only bypass. Nothing in the store says which ns a table is currently on, so "point this table at another ns" is a caller's fact, not a stored one.

## The decision

1. **The unit of migration is the table.** A declaration describes one table, a failed comparison names one table, and the rest of the store keeps serving while it moves. A store-wide migration is what a per-table one does N times, with N−1 of them unnecessary.

2. **The generation is a bit of the address.** `#[ok_ns]` stays the table's identity and never moves. The parity of `LAYOUT_VERSION` rides in the high bit of the ns's first byte — `ns < 0x8000`, asserted at the declaration — so every data key and every index entry carries it, and the version being compile-time keeps the header a const: `[partition][ns_hi | parity << 7][ns_lo]`. Two consecutive generations therefore occupy two disjoint ranges of one table that a store can hold at once. The price is the ns range (32768 tables per store) and nothing else: offsets, slot positions and key widths are untouched, which is the whole reason it is a bit and not a byte. A store that outgrows 32768 tables is an application that wants splitting, not a wider address.

3. **Each generation records itself, and the larger version is the current one.** The table's record row is per generation, at `[partition][ns|parity][0x1F]`, carrying that generation's schema, version and capture flag. Attach probes both parities and takes the larger version: one row is a quiet table, two rows are a flip that happened with the older one waiting for its vacuum. There is no pointer row, no seal and no phase byte — the newer record's version *is* the pointer, the flip is one put, and a crash leaves a state the next attach reads without guessing: before the flip the new range has no record (the next attempt sweeps it and starts over), after it the max rule names the new generation and the vacuum resumes. Because the record is per generation, the old layout stays decodable out of the store itself, which is what makes a migration resumable by a binary that has never held the old declaration.

4. **A handle speaks one layout, and the log is the catch-up channel.** Attach refuses a store whose current generation is not this declaration's, so every open handle decodes one layout: the old generation serves every read for the whole window — no second probe, no tombstone, no dual decoder in a read path. The migrator copies the old range into the new one under the new declaration, remembers where the copy began, then replays the old generation's log from that point through the same declaration until it reaches the head of a quiet window. The log already holds every change since — its segments carry the declaration that wrote them — so nothing is written twice and there is no per-write mirror to get wrong.

5. **The flip is a quiesce, not a stop.** Stop taking writes, replay the last slice, write the new record, resume on the new generation: one window of microseconds, unrelated to the size of the table. The copy is what takes time, and it never holds the writer.

6. **The copy is not a change; the replayed tail is.** Copied rows are the new generation's baseline — the lake pins or exports them, and the log is not the record of what moved. The replayed changes are changes, and they enter the log under the new generation's header. This is why the old generation's log must outlive the flip: it is the only record of what happened between the snapshot and the switch.

7. **The driver is the log's subcommand, not a crate.** Nothing on this path needs the lake: the copy reads the old range through the stored declaration, the catch-up reads the log's own segments, and the flip writes one record row — all of which `okm-log` already holds handles for. What is missing is the verdict (compare generation, fingerprint, schema bytes), the copy and the flip; a separate `okm-migrate` crate would carry the same three legs in a second place. The lake stays what ADR-0056 §5 made it — the analytics consumer and the disaster path — and a generation change does not pass through it.

## What follows from it

- `#[ok_ns]` is bounded below `0x8000`, asserted where it is declared, because the top bit of the first byte belongs to the generation.
- `TablePlan::compile` derives the parity bit from `schema.layout_version`, so a plan compiled from stored bytes carries the same header the declaration does — one plan per generation, exact, no masked comparison anywhere.
- `LayoutMismatch` stops being a terminal error and becomes a value a caller can act on: the table it names is the table to migrate, and nothing else stops. A process that finds a *newer* generation is a stale process, and it says so instead of running against bytes it cannot read.
- The log gains a verb; `PLAN.md` carries it as the open item this ADR stands behind.
- A migrated table keeps its old range and record until the vacuum, so a failed verification costs a re-run rather than the data.
- `ns_override` stays what ADR-0062 made it — a restore's mapping — and does not become a general "which generation am I on" flag: the generation is in the key, not in the handle's options.

## Rejected

- **Whole-store migration.** It is the per-table one repeated, and it makes the unchanged tables pay.
- **A movable ns** — build the new generation under `ns + 1`, copy, delete the old. It needs a name → ns lookup on every open, and the derive cannot express it: `#[ok_ns]` is a const, and the plan statics, the lake table name and the log's table identity are all keyed on it. The bit buys the same coexistence with the address doing the work instead of a catalog row.
- **A version byte in the header.** The same idea at the wrong size: it shifts every slot and key-field offset and costs a byte on every key and every index entry, for a distinction one bit already carries.
- **Merged reads over both generations.** Reading the new range and falling back to the old needs a second decoder and a tombstone in every read path, and keeps them there after the migration too — the shadow scheme's cost paid by every reader forever, instead of by one replay.
- **Decoding the old layout with this build's `R`.** That is the misparse attach exists to refuse. The migrator decodes with the *stored* declaration, which is a different thing: the declaration it reads names the layout, and the plan it compiles comes from those bytes.
- **In-place key surgery without a snapshot.** The keyspace holds one copy of a row, and a key rewrite that fails halfway leaves neither layout.
- **A migration marker beside the store.** The generation is in the key and the record names the layout; a second flag out of the file is a second truth about the same table.
- **Migrating inside attach.** An open that rewrites a table is an open that can fail for a reason the caller did not ask about.
