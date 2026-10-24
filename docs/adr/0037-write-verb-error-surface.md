# ADR-0037: the write verbs return `Result`

Status: Accepted (2026-10-24). Refines ADR-0035; widened to every write verb
after the first draft — the map-facing half alone left the typed verbs
panicking, and a verb that cannot report a refused write has no error channel
at all.

## Context

`merge` and `replace` take a caller-built map — an HTTP body, a decoded Parquet
row, a host-language dict. Until now a name that matched a declared field but
carried the wrong `DynamicValue` kind was silently replaced by the field's
declared default. ADR-0035 recorded that as a defect ("reject a value whose kind
mismatches the declaration here") and queued it as its own change; the first fix
made it a panic.

A panic is the wrong channel for this class, because the library has exactly two
failure kinds and they differ in WHO can act on them:

- **Caller data** — the map does not fit the declaration. That is normal input, a
  400 rather than a fault, and the caller is the only party that can fix it.
- **Engine** — the store refused the commit (a fenced writer, an I/O failure, a
  remote frame error). Nothing the caller did; the engine's own text is what
  matters.

Only the first kind is recoverable by design, and it exists exactly where caller
data enters: the map-facing verbs.

## Decision

1. **`okm_core::Error`** (`thiserror`) is the crate's error type, in
   `okm-core/src/error.rs`, with `pub type Result<T> = std::result::Result<T,
   Error>`:
   - `KindMismatch { field, want, got }` — a present name, wrong kind.
   - `ValueOutOfRange { field, want, got }` — right kind, unrepresentable value.
   - `VocabularyRequired { name }` — a nested object reached an encoder that owns
     no vocabulary (it used to panic).
   - `VocabularyExhausted { width, next }` — a field's id width is full (it used
     to panic).
   - `EdgeAlreadyLive { edge_id }`, `EdgeNotFound { edge_id }`,
     `EdgeNotMember { side, ns, graph_ns }` — the graph's own refusals, which
     used to be `String`.
   - `Engine(String)` — the engine's refusal, carrying its own text.
2. **`Document::from_map` becomes `try_from_map -> Result<Self>`.** The rule is
   presence: an ABSENT name takes its declared default (the append-only
   evolution rule, same as the payload decoder), a PRESENT name must fit.
3. **Every write verb is fallible** — `put`, `upsert_with`, `purge`,
   `prune_deprecated_slots`, `merge`, `replace`, `save_with_prev_into`, the
   junction's `link` / `unlink`, and the graph's `link` / `link_into` /
   `unlink` / `unlink_into` / `replace_into`. No write verb panics: the engine's
   refusal comes back as `Error::Engine`, a full vocabulary as
   `VocabularyExhausted`, a caller's map that does not fit the declaration as
   `KindMismatch` / `ValueOutOfRange`, an edge mistake as `EdgeAlreadyLive` /
   `EdgeNotFound` / `EdgeNotMember`. The intern hooks are fallible end to end
   (`Document::__okm_intern`, `VocabCache::id_for`, `put_frame*`), because
   interning is the one allocation a write performs and it can fail. The map is
   validated before any op enters the batch, so a refused `merge` / `replace`
   writes nothing — dynamic half included.

## Alternatives rejected

- **Panic everywhere** (the previous state, and the first fix). The caller cannot
  handle a caller-data failure, so a bad request body becomes a dropped
  connection at best and a dead process at worst — and the type cannot tell "your
  data" apart from "the disk".
- **A `String` error**, the shape `okm-dynamic` uses. Matchability is lost: a
  caller that wants to answer `400` naming the field would have to parse text.
  The dynamic side keeps its own type; the typed side gets a real enum.
- **Half-Result: fallible `merge`, panicking `try_from_map` underneath.** A verb
  that returns `Result` while its own constructor panics cannot be used safely —
  the failure escapes the type, which is the situation this ADR exists to end.
- **Only the map-facing verbs.** The first draft drew the line at "the map is
  caller data"; the typed verbs were left panicking because their input is
  already a value. It reads well until the first engine refusal: a server that
  calls `put` dies where the same store error through `merge` is a value. The
  failure was never about input — it was about **an error with no channel**, and
  the single-writer contract is a concurrency argument, not an error-handling
  one. One rule for every write is worth the migration (~87 call sites outside
  this repo, mechanical `?`).

## Consequences

- A refused `merge` / `replace` leaves the store untouched and names the field,
  the declared kind, and the kind that arrived. `a_refused_merge_returns_the_error_and_writes_nothing`
  locks the "writes nothing" half.
- A refused write leaves the store untouched: validation and interning run before
  any op enters the batch, and `commit` is the last statement of each verb.
- No write verb panics. What still panics is the READ side on corrupt bytes
  (`decode_payload`, edge-body decode) and the raw per-op storage methods
  (`VirtualStorage::put` / `del`) — both are "the store is not what it claims"
  failures, not conditions a caller can act on. Widening either is a different
  change with a different subject (integrity reporting, storage trait).
- One write-path panic remains, and it is on the ENCODING surface rather than in
  a verb: `save_into` does not intern by design (it is the batch-alignment
  helper), so a hand-built document whose `Vocab` was never interned dies in
  `Vocab::wire16`. Making the payload encoder fallible means changing
  `encode_payload`, a trait method every index entry, batch writer and export
  path calls — its own change. `put` and `save_with_prev_into` cannot reach it:
  they intern first.
- Downstream migration is mechanical: `?` where the caller already returns a
  result, `.unwrap()` in tests. Every write call site outside this repo (~87)
  needs it, which is why the change ships as one API break, not a long tail of
  half-converted verbs.