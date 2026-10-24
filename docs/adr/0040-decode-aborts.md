# ADR-0040: The codec's decode half aborts instead of answering `Result`

- **Status**: Accepted
- **Date**: 2026-10-24
- **Supersedes**: nothing. Amends the scope of ADR-0037 / ADR-0038 / ADR-0039, which
  gave the store-facing verbs an error face and left the codec untouched.

## Context

ADR-0037 gave the read verbs `Result` ("a store that cannot answer is an `Err`"),
ADR-0038 put a read fault on the scan item, and ADR-0039 did the same for the write
verbs. None of them touched the **codec**. Its decode half is infallible by
signature:

```text
DocumentEncode::decode_payload(&[u8]) -> Self      (derive-generated, per row type)
VectorElem::decode_le(&[u8])          -> Self
Vector::decode_payload(&[u8])         -> Self
ReduceCodec::decode_acc(&[u8])        -> Self
EdgeKey::decode(&[u8])                -> Self
rev_decode / offset_decode / varint_decode / dec_wire  -> Self
```

so bytes that do not fit abort the process from inside — `try_into().unwrap()`,
`expect("offset width")`, and slicing such as `payload[p..p + W]` that panics
before the conversion is even reached.

With the house rule "no `panic!`/`unwrap`; something that can fail returns
`Result`", this looked like ~40 lines of mechanical conversion. It is not: there
is no error position to put `?` in, and the only two answers that keep the
signature are *widen everything* or *fabricate a value*.

## Decision

**1. The decode half keeps its infallible signature and aborts.** `unwrap` /
`expect` with a message is the specified behaviour when the bytes do not fit.

**2. The reason.** The encoder and the decoder are one artifact, released
together. Anything this SDK cannot decode was therefore not produced by this SDK:
a version or build mismatch, or bytes written behind the API's back (a raw key
injected directly, a foreign backup restored, a store shared with another okm
rev). There is no interpretation to recover — if the decoder knew how to read
those bytes, the encoder would have written them that way.

**3. Aborting is also the safer half of the trade.** A half-true document does not
stay local: it is written back into derived state — index entries, reduce
accumulators, vocabulary ids — and the corruption is silent and durable. Failing
fast stops at the first bad byte instead of spreading it, and the process that
dies is the one holding a store it does not understand.

**4. The panic lints are allowed where the codec is, and only there.** Each
allow carries its reason in place:

| Location | Scope | Why |
| --- | --- | --- |
| `okm-core/src/model.rs` | module (`model` + children) | the codec itself — encode/decode pairs of one build |
| derive-emitted `decode_payload` | the generated fn | the codec, materialized in the consumer's crate |
| `bridge/arrow_bridge.rs` | file | the export side: our bytes, our column assembly |
| `bridge/iceberg.rs::stamp_batch` | fn | field-id stamping keeps our own columns in place |
| `SlatedbSync::open_mem` | fn | construction, once: no runtime/thread ⇒ no store |
| `TestStore::slatedb_mem` | fn | test-double construction |
| `Nest::serve`, `Nest::apply` | fn | `serve` twice; a scan frame that lost its begin |
| `okm-vector/src/codec.rs` | file | a stored code is this build's own bytes; a `Codec`/`Prepared` mismatch or a code shorter than its own norm prefix means the row came from somewhere else, and an invented distance would be folded straight back into the graph's edges |

**5. What stays `Result`.** Everything where the bytes are foreign, where the input
is the caller's own data, or where the store may be unable to answer: the storage verbs (ADR-0037 / 0039), scan items
(ADR-0038), and the **import** path — a Parquet file written by another producer is
input, so a column whose arrow type does not match the declaration answers
`Err` (`column_as`, `tooling.rs`), and the variant metadata frame is read with
`first_chunk` instead of a length check plus `expect`. The distinction is the
source of the bytes, not the direction of the call. The dynamic side is almost
entirely that class — its schema arrives as data (it round-trips through JSON) and
its field names arrive at runtime — so a cold field with no tag, a run of field ids
that runs out of width, and a write the engine refused are all `Err`s there, not
aborts.

## Scope: where a panic is still the right answer

The rule this ADR is an instance of is not "never panic". It is **"no panic where
someone could act on the failure"**, and `Result` is what the answerable residue
gets:

- **A program that cannot continue has nothing to buy with `Result`.** A one-shot
  run — a CLI invocation, a migration, a batch job — a proc-macro at compile
  time, a test: these stop either way, and a panic is the earlier stop, with a
  line number and a message. Routing it through `Result` moves the same abort a
  frame up the stack and taxes every line in between with `?`.
- **"The user has to fix their setup" is the same class, resident or not.** A
  missing config file can be created with defaults or refused so the user creates
  it; both are one action away from working, and the difference is how friendly
  the message is, not whether the failure is handleable.
- **The codec is the extreme case of it**: bytes from a build that cannot be read
  have no interpretation and no caller action, so the abort *is* the message.

What the gate is for is the opposite class: code that **must keep running** — a
request path, a serve loop, a background worker — turning a failure into silent
wrong data (ADR-0038's `None`-means-absent) or into a half-applied state. There a
`Result` is not a message, it is a decision point.

That scopes the lints: deny where the crate serves others. In a `bin/`, a build
script, a proc-macro crate or a batch tool the deny buys nothing — and where a
workspace-wide `[lints]` table is already in place, the right move is a crate-root
allow **carrying this reason**, not reshaping the tool into `Result` soup.

## Consequences

- **`okm-core`'s runtime half is clean** under `clippy::unwrap_used`,
  `clippy::expect_used` and `clippy::panic` — zero warnings. That is what the gate
  item in `docs/PLAN.md` was waiting on.
- **The compile-time surface is the allow class, not a rewrite queue.** `okm-derive`
  (130 warnings) and `okm-core/build.rs` (9) stop the build when they reject a
  declaration: the abort IS the error report, and no caller can act on it. Per the
  scope above they get a crate-root allow carrying that reason, not 139 `Result`
  signatures. Where a nicer diagnostic is worth the work — `syn::Error::to_compile_error`
  carries a span, the panic machinery does not — it is polish, taken site by site.
- **A store holding foreign bytes now aborts the reader.** That is the accepted
  trade. It is also why the protocol rev check belongs at the seam: the boundary
  should refuse a foreign rev before the codec has to.
- **Corrupt-byte detection is not a feature.** If a store must survive foreign
  bytes, the fix belongs at the boundary (rev check, explicit integrity report),
  never in the decoder.
- **The allow list above is the registry of permitted panics.** A fourth place
  requires amending this ADR — that is what keeps the gate from being widened by
  habit.

## Alternatives rejected

- **Widen every decode to `Result`.** ~40 direct sites plus the generated document
  decode reaching into every read path in every consumer crate. It buys nothing
  the abort does not: the caller's only sensible response to "these bytes are not
  mine" is to fail the read, which is what the abort does — one stack frame
  earlier, without a partial value in flight.
- **Decode to a default (`unwrap_or_default`, empty row, zeroed field).** Manufactures
  data, then stores it back on the next write. Strictly worse than either
  alternative.
- **Keep the panics with no allow list.** Then the lints cannot distinguish these
  panics from the ones this programme is removing, and the gate either never lands
  or lands with blanket exemptions — which is how "no unwrap" becomes decoration.