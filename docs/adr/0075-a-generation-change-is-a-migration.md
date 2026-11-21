# 0075 — A generation change is a migration, one table at a time

Status: decided 2026-11-20, not yet implemented.

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

2. **Attach has two answers, and the second one is a state.** Same generation: open normally. Different generation: the table is *sealed* — the typed handle refuses reads and writes alike, and the error carries the table name, both generations and the verb that migrates it. Whether this build can move a given stored generation is the migrator's question; attach does not guess at it.

3. **Sealed is about the typed handle, not about the bytes.** A segment is read with the declaration it carries (`declaration_of`, `segment_labels`, `load_declaration`), never with this build's `R`, so the export leg works on a generation this build cannot otherwise touch. That is the one reader a migration needs, and it already exists.

4. **A migration is snapshot, swap, verify — and the swap has no readers.**

       snapshot   export the sealed table to the lake under its own declaration,
                  then verify it (row count, fingerprint of the stored declaration)
       swap       clear the ns and rewrite every document through a collection
                  assembled over the bare engine; reads are refused for the duration
       verify     counts and a key-by-key read on the new generation, then unseal
                  and vacuum the old generation's segments

   The snapshot is what serves reads while the swap runs. The clear pass is an unconditional prefix scan over `[partition][ns]`: when only the payload layout changed the backfill would overwrite every key, but an index entry for a field the new declaration dropped has nothing to overwrite it, so the pass does not depend on what changed.

5. **The backfill does not log.** The log records what changed; rewriting every row is not a change. A logged backfill would make the lake replay a full-table write and give the swap two sources of truth about the same table. What the old generation held is recorded by the snapshot, once, at the moment it stopped being writable.

6. **The driver is a lake subcommand, not a crate.** The legs exist — `baseline`, `read`, `restore`, `publish`, `delta`, `vacuum` — and what is missing is the verdict (compare generation, fingerprint, schema bytes), the swap, and the verify. A separate `okm-migrate` crate would copy the lake's bridge to do the same work in a second place.

## What follows from it

- `LayoutMismatch` stops being a terminal error and becomes a value a caller can act on: the table it names is the table to migrate, and nothing else stops.
- The lake gains a verb; `PLAN.md` carries it as the open item this ADR stands behind.
- A rewritten table keeps its old segments until the vacuum, so a failed verification costs a re-run rather than the data.
- `ns_override` stays what ADR-0062 made it — a restore's mapping — and does not become a general "which generation am I on" flag.

## Rejected

- **Whole-store migration.** It is the per-table one repeated, and it makes the unchanged tables pay.
- **True blue-green under a second ns** — building the new generation beside the serving one and flipping. It keeps reads available through the swap, but it needs a table → current-ns indirection: a catalog row consulted on every open, which is the lookup ADR-0074 §3 just took off the write path. Reopens when a migration has to run while readers are serving, which is a deployment fact and not a design one.
- **A read-only handle on the old generation.** Decoding a previous layout with this build's `R` is the misparse attach exists to refuse; one reader that is allowed to do it is one reader that decides by accident.
- **In-place key surgery without a snapshot.** The keyspace holds one copy of a row, and a key rewrite that fails halfway leaves neither layout.
- **A migration marker beside the store.** The generation in the segment header already says which declaration wrote the bytes; a second flag out of the file is a second truth about the same table.
- **Migrating inside attach.** An open that rewrites a table is an open that can fail for a reason the caller did not ask about.