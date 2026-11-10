# 0047 — Two faces of one committed write: the event in memory, the record on disk

> **Languages:** [English](0047-two-faces-of-a-write.md) (primary) · [中文](0047-two-faces-of-a-write.zh-CN.md)

**Status:** Accepted (2026-10-31) — both halves decided, and both held down by a test. **Updates 2026-11-10:** the criterion is the interface, not the delivery contract — subscribe is the observer interface and the change log is a file; the five axes stay as description, and the delivery/latency difference is recorded as a property that does not decide the question. Same date: the point verbs now announce what they leave or take — `merge` and `replace` included — which the axes line states as the code's behaviour. **Date:** 2026-10-31

## Context

Two routes announce a committed write, and each has its own ADR: `subscribe` (ADR-0008) pushes a typed `Event<K, R>` to a sink in the same process, and the change log (ADR-0045) appends a byte frame to a segment file for another process to drain. Read separately, each makes its case. Read together they look like a de-duplication nobody finished, and the obvious reading of that is that one of them is a leftover.

They are not the same thing, on five axes.

- **Where it lives.** subscribe: process memory, a `SinkCell` behind a lock, nothing on disk and nothing to clean up. The change log: files under `<root>/<header hex>/` — `.partial` while being written, `.segment` when sealed, plus the retention that decides when a segment goes.
- **What it carries.** subscribe: `op`, `epoch`, the key, and the DECODED document `R` — the event knows the schema, because the type is its parameter. The change log: `OpFrame` op bytes, key and value, no schema at all, which is exactly why a reader that has none can still drain it.
- **Who consumes it.** subscribe: a sink in the same process — `okm-stream`'s combinators on top of it. The change log: another process — `okm-parquet` (a standalone binary, ADR-0056 §8) or anything that speaks `okm-wire`.
- **How it is delivered.** subscribe: synchronous `try_send` in the write path's own call stack, best-effort, dropped when nothing is listening, and no way to tell a consumer what it missed. The change log: staged before the commit and appended only after it returned `Ok` — never a phantom write — at most once, with a lost tail repaired by the reconciliation scan.
- **What it says, and what that costs.** subscribe: the key, the op and the decoded document, emitted once per semantic write on the point verbs — `put`, `upsert_with`, `merge`, `replace`, `purge`; a batch write announces nothing, a write that only touches the dynamic segment has no row to carry, and derived entries never produce an event; zero cost when no sink is registered. The change log: the truth half of a batch only — primary rows, the dynamic segment, the name and predicate faces, both directions of every vocabulary — with the derived half dropped and rebuilt, at one memcpy per write plus segment files and their retention.

## Decision

**Both stay, and the criterion is the interface, not the delivery contract.** subscribe is an interface you register at; the change log is files on disk. The five axes describe what each face is — they do not decide the question — and neither does the delivery contract: best-effort against at-most-once, microseconds against milliseconds, makes no difference in normal use, and losing part of the tail in a real disaster is acceptable. Both contracts stay written down; they are not the reason the pair exists.

**subscribe is decoupling: the reaction is written once.** The derive names the topic (`#[ok_subscribe]` → `SINK_<ENUM>`), the consumer registers one `EventSink` — any `Fn(E) -> bool` will do — and the reaction lives at that one registration point while the write path does not know who is listening. Without it, "when this data changes, do something" is spread over every place that changes the data (`put`, `upsert_with`, `merge`, `replace`, `purge` — one omission and it silently stops), or the reaction has to read the segment files and decode them before it can act. The name records the role: it was renamed from a channel to a subscription because it is an interface to register at, not a transport.

**The change log has no such point.** It is files on disk plus a reading convention: it says what happened, to any process that opens the directory, and there is nowhere to register "I care about these" — only programs read it.

A consumer chooses with two questions: does it live in the same process, and does it need the decoded document? Yes to both — subscribe. Otherwise — the change log. A consumer that needs both properties is building a drain, which is a separate crate's job (ADR-0045).

No third face is added, and no "unified event bus" is invented. A bus would be the union of both costs — durable, typed, per-table, always on — paid by consumers who each want one half of it.

## Rejected

**Tailing the segments to feed the in-process face.** The cheap-looking unification: one mechanism, one hook on the write path. It dies on placement — file IO and a schema decode sit between the write and the reaction, every subscriber pays for a file it did not ask for, and the reaction becomes a reader loop, which is the interface disappearing. It adds latency and infrastructure and removes the one thing worth keeping.

**Pushing the typed event through a file.** It needs a schema the writer may not have, and it turns a push into a pull with a delivery lag. That shape exists already; it is the change log, and its consumer is called a drain.

## Consequences

It is now a decision rather than an open question: a batch is the bulk path, its consumers are other processes, and the change log is the face another process reads — one frame per batch per table, staged before the commit and appended after it returned `Ok` (a capturing store, ADR-0056 §2).
- If an in-process consumer of batch writes ever appears, the shape it needs is the one the change log already uses: stage the events beside the writes, flush them after the commit returned `Ok`. That is a change to that caller's collection, not a new mechanism, and nothing builds it today because nothing asks for it.
- The typed face stays per-table and best-effort. A consumer that needs precision belongs inline — in the reduce fold, which runs inside the write — and not on a subscription; that distinction is ADR-0008's, and this one does not weaken it.