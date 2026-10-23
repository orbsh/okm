# Concurrency and writers

OKM's semantic layer is **single-writer**. That sentence does a lot of work, so
this document makes it explicit: what the KV layer already guarantees, which
mechanisms depend on there being one writer, what a second one breaks, and the
four shapes that can carry more. The point is to make "multi-writer" a decision
with a bill attached instead of a wish.

ADR-0035 rule 10 is the type-level half (`&self` verbs, `!Sync` collection); this
is the analysis behind it.

## 1. Two layers, one guarantee

**The KV layer is concurrency-safe.** `VirtualStorage::put` / `del` / `get` /
`scan_*` all take `&self`, and every backend is interior-mutable behind that:
fjall's partition, redb's `&Database`, slatedb's `Arc<Db>`, the remote store's
`mpsc::Sender`, and `BatchStore`'s `RefCell`. `SharedVirtualStorage::shared_handle()`
is a cheap clone of the same physical engine. Thread-safe bytes are not an
accident; they are the layer's contract.

**The semantic layer is not.** `Collection`, `Graph` and `Junction` are where
merge's read-modify-write, reduce folds, vocabulary allocation, stale-face
cleanup and epoch ordering live. Every one of those assumes it is the only writer,
and none of them is protected by an engine-level lock. That — not the byte store —
is what "single-writer" names.

What enforces it today:

- the verbs take `&self` (ADR-0035 rule 10), so write access is not a borrow;
- `Collection` is `!Sync` — the epoch counter, the only mutable state a write
  touches, is a `Cell` — so a second thread cannot even hold a reference to it.

Cross-**process** exclusion is a separate axis and already closed by the engines:
fjall takes a file lock on open (a second process fails to open the directory),
slatedb fences a stale writer with an epoch in its manifest. Two processes cannot
share one directory; the question in this document is in-process threads only.

## 2. Five mechanisms that assume one writer

| mechanism | with a second writer | how it fails |
|---|---|---|
| `merge` / `replace` read-modify-write | two writers read the same stored row, both compute from it | **lost update**; and because the write also unfolds the old row from every reduce group, the ledger is double-adjusted |
| reduce exactly-once | the unfold(prev)+fold(new) pair is no longer a pair | accumulator drift — `Count`/`Sum` lose a write; `HighWater` can move backwards if the older write commits last |
| vocabulary allocation (`VocabCache::id_for`) | `name_key` probe misses, two writers both read the counter's next value and both claim it | **two names share one id**; id→name decodes to whichever landed last, silently renaming one of them |
| faces (path indexes) | the stale-face sweep derives its deletes from the STORED row, and both writers read the same one | the loser's new face survives, pointing at a document that no longer has that value — an index hit that resolves to nothing |
| epoch ordering | epoch is bumped per commit and stamped on subscribe events; consumers fold epochs into batch boundaries | bump order stops being commit order, so folded boundaries lie |

Every one of these is a *semantic* invariant. The commit underneath each write is
still atomic — a torn document is not what concurrency produces here; a
consistent-looking wrong one is.

## 3. Four shapes that carry more than one writer

**A. `Arc<Mutex<Collection>>`** — the cheapest, and what both consumers already do
one level up. `Mutex<T>: Sync` requires only `T: Send`, and `Collection` is `Send`
(`Cell<u64>` is `Send`), so this compiles and needs no change in okm. It
serialises everything, reads included, and changes no semantics: RMW, folds,
vocabulary and epoch all behave as they do today.

**B. A wrapper over `RwLock`** — buys concurrent reads, and only if the wrapper
never hands out `&Collection`. The verbs take `&self`, so a *read* guard is
enough to write with; `RwLock` protects the borrow, not the semantics. The wrapper
must own the verbs (`fn merge(&self, …) { self.rw.write().merge(…) }`) and keep
its field private. Otherwise the lock is decorative.

**C. One writer thread plus a command channel** — the actor shape, and the one
that matches the library's actual model. `Collection` is `Send`, so it can be
moved into a thread; commands arrive on a channel and RMW is atomic by
construction because exactly one thread executes it. Cost: a round trip per verb,
and a command surface that cannot be generic over `K`/`R` — each table needs its
own command enum, or a byte protocol.

**D. A channel as the *store*** — already shipped, twice: `RemoteStore`
(`engine/nest.rs`) implements `VirtualStorage` over `mpsc`, and `wire_client` is
its async twin. The receiving thread's `ExecCore::apply(&self)` locks
`engine: Mutex`, folds a frame's writes into ONE batch, commits, and answers the
frame's queries. It buys one engine owner for N threads and frame-level commit
atomicity. It does **not** buy RMW atomicity: a `get` and the `put` built from it
are two frames, and another writer fits between them.

## 4. What a frame buys — precisely

- **Writes in one frame = one commit.** All of them or none. This is ADR-0010's
  cross-assembly-point atomic path, and it survives remoteness.
- **Reads in one frame do NOT see that frame's writes.** The frame's ops are
  accumulated in a local batch that is committed after the loop; the mid-frame
  `commit_batch(Batch::default())` before a query is an empty batch, i.e. a no-op,
  so its comment ("reads see prior ops in the same frame") does not hold. No
  production caller builds a mixed frame today, so nothing breaks — but a mixed frame's
  read-your-own-writes would have to route the query through a `BatchStore` view
  instead. Recorded in `PLAN.md`.
- **No conditional op exists.** A frame can express "write this" but not "write
  this if the row is still what I read", which is the primitive a concurrent
  writer would actually need.

## 5. What a real multi-writer design would owe

Four debts, all at the semantic layer:

1. **A row version** — something in the entry a writer can compare against what it
   read, so a lost update is detected instead of committed. (Today: nothing; the
   entry is the payload.)
2. **Reduces that survive interleaving** — either provably commutative folds, or
   versioned accumulators checked like the row. `Count`/`Sum` are reversible but
   not order-free across an unfold/fold pair.
3. **Atomic vocabulary allocation** — claiming an id must be one operation, not
   read-cursor-then-write-cursor. Until then, even two writers touching *disjoint*
   documents can corrupt the field dictionary.
4. **Face bookkeeping** — a stale face is undetectable without either a
   per-document face list or a scan, so concurrent overwrite gives no way to know
   what to sweep.

Plus the clock: a per-process counter cannot order two writers' commits. Epoch
would have to come from the engine (a store-assigned sequence) or each writer
would need its own stream for consumers to merge.

Until those are paid, "concurrent writers" is a bytes-level feature with a
semantics-level hole. That is why `!Sync` is the last thing to lift in any
multi-writer design, not the first.

## 6. Where the line is today

- `!Sync` costs the current consumers nothing: aura keeps its realm in
  `Arc<tokio::sync::Mutex<Realm>>` (`crates/realm/src/lib.rs`), krystallizer's
  server keeps `Mutex<ChannelStore>` / `Mutex<GraphStore>`
  (`crates/mem-server/src/lib.rs`). Both had already serialised above okm.
- A caller who wants more threads has A, B or C available without a change in okm.
- `shared_handle()` gives thread-safe **bytes**, not thread-safe **semantics**:
  two threads each holding their own `Collection` over the same store is exactly
  the case the type cannot see — each collection is a separate `!Sync` value in
  its own thread, and nothing stops it. This is the single most likely misuse of
  the current API, which is why the rule is stated here rather than left implicit.