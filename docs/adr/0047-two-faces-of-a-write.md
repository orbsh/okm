# 0047 — Two faces of one committed write: the event in memory, the record on disk

> **Languages:** [English](0047-two-faces-of-a-write.md) (primary) · [中文](0047-two-faces-of-a-write.zh-CN.md)

**Status:** Accepted (2026-10-31) — the pair stands; the batch half is decided, not built. **Date:** 2026-10-31

## Context

Two mechanisms announce a committed write, and each has its own ADR: `subscribe` (ADR-0008) pushes a typed `Event<K, R>` to a sink in the same process, and the change log (ADR-0045) appends a byte frame to a segment file for another process to drain. Read separately, each makes its case. Read together they look like a de-duplication nobody finished, and the obvious reading of that is that one of them is a leftover.

They are not the same thing, on five axes.

- **Where it lives.** subscribe: process memory, a `SinkCell` behind a lock, nothing on disk and nothing to clean up. The change log: files under `<root>/<header hex>/` — `.partial` while being written, `.segment` when sealed, plus the retention that decides when a segment goes.
- **What it carries.** subscribe: `op`, `epoch`, the key, and the DECODED document `R` — the event knows the schema, because the type is its parameter. The change log: `OpFrame` op bytes, key and value, no schema at all, which is exactly why a reader that has none can still drain it.
- **Who consumes it.** subscribe: a sink in the same process — `okm-stream`'s combinators on top of it. The change log: another process, `okm-drain` or anything that speaks `okm-wire`.
- **How it is delivered.** subscribe: synchronous `try_send` in the write path's own call stack, best-effort, dropped when nothing is listening, and no way to tell a consumer what it missed. The change log: staged before the commit and appended only after it returned `Ok` — never a phantom write — at most once, with a lost tail repaired by the reconciliation scan.
- **What it says, and what that costs.** subscribe: everything the write path emits, derived state included, at zero cost when no sink is registered. The change log: the truth half of a batch only — primary rows, the dynamic segment, the name and predicate faces, both directions of every vocabulary — with the derived half dropped and rebuilt, at one memcpy per write plus segment files and their retention.

## Decision

Both stay. They are two faces of one event, and the axes above do not intersect: no consumer ever needs "durable but untyped in this process" or "typed but eventually, later".

Neither face can be built out of the other without moving work into the wrong layer. Draining bytes into an in-process callback puts file IO and a schema decode inside the write path's own stack, and it makes every subscriber pay for a file it did not ask for. Pushing typed events through a file needs a schema the writer may not have, and turns a synchronous push into a pull with a delivery lag. That is the whole argument for keeping two.

A consumer chooses with two questions: does it live in the same process, and does it need the decoded document? Yes to both — subscribe. Otherwise — the change log. A consumer that needs both properties is building a drain, which is a separate crate's job (ADR-0045).

No third face is added, and no "unified event bus" is invented. A bus would be the union of both costs — durable, typed, per-table, always on — paid by consumers who each want one half of it.

## Consequences

- A batch writer (ADR-0031) emits no subscribe event. That is now a decision rather than an open question: a batch is the bulk path, its consumers are other processes, and the durable face already carries it with the guarantee the in-process face cannot give — staged before the commit, appended after it returned `Ok` (`stage_capture` / `flush_capture`). A caller that wants events for a batch turns the change log on.
- If an in-process consumer of batch writes ever appears, the shape it needs is the one the change log already uses: stage the events beside the writes, flush them after the commit returned `Ok`. That is a change to that caller's collection, not a new mechanism, and nothing builds it today because nothing asks for it.
- The typed face stays per-table and best-effort. A consumer that needs precision belongs inline — in the reduce fold, which runs inside the write — and not on a subscription; that distinction is ADR-0008's, and this one does not weaken it.