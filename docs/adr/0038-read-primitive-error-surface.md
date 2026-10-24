# ADR-0038: a failed read is not an absent row

Status: Accepted (2026-10-24). Companion to ADR-0037 — that one gave the write
verbs an error channel; this one gives the read verbs theirs.

## Context

Every read verb answered in a type that had room for exactly one bad news:
`Option`. Absent and broken were the same answer, all the way down:

- `WireClient::get` was `.ok().and_then(|r| r.value)`, `scan_suffix` was
  `.ok()`;
- the fjall backend's `scan_suffix` was `filter_map(|k| k.ok())`, dropping a
  mid-walk failure and returning a SHORTER answer;
- `get` on fjall / slatedb were `expect(...)`, i.e. a panic inside a library;
- the wire answer had no error field at all, so the sending side could not be
  told even if the receiver knew.

The consequence is the one a KV store must never produce: a consumer that reads
before writing (`store-exec` in prism and aura is exactly this shape) sees
`None`, concludes "no such row", and overwrites. A network blip becomes silent
data loss.

## Decision

1. **Absence stays `Option`; failure becomes `Result`.** They are different
   answers and they keep different types: `Result<Option<R>>` at the semantic
   verbs (`get`, `fetch`, `edge`, `get_edge`, `attrs`, `reduce_get`,
   `predicate_vocab_name`), `Result<Vec<…>>` at the collection-shaped ones
   (`scan`, `scan_exact`, `scan_range`, `scan_covered`, `scan_keys`,
   `scan_document_keys`, `scan_documents_raw`, `slot1_map`, `edge_ids`,
   `predicates`, junction `forward` / `reverse*`, the graph traversal family).
   Merging the two into one three-state enum was rejected: it makes every
   call site pay for a distinction most callers do not need.
2. **The storage boundary speaks `Result`.** `VirtualStorage` /
   `VirtualStorageAsync` `get`, `scan_range`, `scan_suffix`, `commit_batch` all
   return `crate::error::Result`, and `ScanIter::Item` becomes
   `Result<(Vec<u8>, Vec<u8>)>` — a walk that dies mid-way has to say so at the
   point it dies, not by ending early. The six backends (fjall, redb, slatedb,
   localstorage, test engine, remote) follow; none of them `expect`s on a read
   any more. redb gets `fn eng<E: Display>(e: E) -> Error` because each of its
   operations raises a different error type.
3. **The wire answer can be a fault.** `OpResponse` gains `fault:
   Option<String>`, encoded as its own `[2][len][utf8]` frame. `0` (absent) and
   `1` (present) are byte-for-byte unchanged, so no reader of the old grammar
   changes behaviour; `2` was previously malformed and decoded as `None`, which
   no consumer could mistake for data. A fault answer carries no value, no
   suffixes and no page — an answer that is a failure has none of those things.
   The receiver sets it (`OP_GET`, `OP_SCAN`, and `OP_SCAN_STREAM` per page,
   where a failed page breaks the walk instead of silently truncating it); the
   sender folds it back into `Err`.
4. **Three read hooks are fallible**, because they read: `Document::
   __okm_embed_deref`, `Document::__okm_deref_vocab`, and — the one that hid
   behind a write — `Document::__okm_apply_reduces`, which reads the group's
   accumulator before folding. The junction's generated query methods
   (`get_session`, `get_user`) follow: they call `forward` / `reverse_raw`.
   The dynamic side follows too, and its shape is the one that shows why the
   rule is not about verbs: `AccOf`, the caller-supplied "what is this group's
   accumulator right now" closure, returned `Option<Vec<u8>>`, so every
   implementation was written `&|ek| store.get(ek).unwrap()` — a store failure
   panicking inside a closure the plan calls. It answers
   `Result<Option<Vec<u8>>, String>` now, `okm_dynamic::reduce_get` with it.
5. **Two store-contract violations stop aborting.** A junction `forward` whose
   endpoint set is truncated cannot decode the far side back into its key type
   (`Error::Corrupt`), and a `BatchStore` cannot be scanned at all
   (`Error::ScanThroughBatch`). Both are caller errors, reported as such.
6. `Error::Engine` no longer renders as "engine commit failed": reads reach it.

## Deliberately not done

`decode_payload` still panics on bytes that do not decode. Corrupt bytes mean
the store returned something it promised not to, and no caller has an action
for that; making it `Err` would thread a `Result` through every derive
decoder, every index decode, the covered-scan decode and the Parquet export
paths — a change larger than this whole refactor, for an error nobody can
handle. It stays a loud abort, documented where it lives — ADR-0040 states the
general rule this is one case of: a panic is wrong where someone could act on the
failure, not everywhere.

## Alternatives rejected

- **`Option` plus a side channel** (a thread-local last-error, a store flag).
  Invisible at the type level: the call site that needs the distinction is
  exactly the one that will forget to ask.
- **Failure folded into absence** (the previous state). This is the defect.
- **`Result<Option<T>>` for `decode_payload` too.** See above: cost without a
  consumer.
- **A fault answer that still carries `suffixes` / `hits`.** It would make a
  partially-failed page look like a complete one; a failure is not a page.

## Consequences

- Every read call site says what it wants: `?` where a failure matters,
  `.unwrap()` in tests and benches where the store is known-good.
- The wire codec gained its first tests of its own (round trip, absence,
  fault in both grammars, a fault frame refusing trailing bytes).
- One cleanup fell out of the same audit: the alert is worth nothing if the
  wrapper a caller hands us may panic, which is how `AccOf` was shaped.
- `kv_nest_test::nest_derive_end_to_end` caught a real regression on the way:
  the receive side must NOT strip the hosted prefix from a prefix-scan answer
  (those suffixes are already relative to the hosted key); stripping the range
  answers is what the wire contract needs.