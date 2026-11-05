# ADR-0052: An index field comes from the payload

Date: 2026-11-05
Status: Accepted

## Context

An index entry is `[ns 2B][slot 2B][index fields…][key tail]`, and its **data segment** — the part
sorted right after the header — is generated from the document. `KvIndex::encode_named` receives
the key and the document, but the generated impls source every name from the payload; `fields(...)`
and `includes(...)` are validated against the payload's field list at expansion time.

That is what makes a relation table's reverse read possible at all (ADR-0051), and it also fixes
the price: a field the key already holds has to exist on the row as well, or the declaration cannot
be written. `memberships` pays it twice — once as a column in the row's payload, once as the tail
the entry appends after the data segment:

```text
[ns 2B][slot 2B][member_id 16B]   [channel_id 8B][member_id 16B]
                 ↑ from the row's column   ↑ the whole primary key
```

The alternative reading of the same bytes — let `fields(...)` name a key field, so the data segment
and the tail both come from the key — is not taken. It is worth recording why, because it looks like
free bytes and will be proposed again.

## Decision

**An index field is a payload field.** The declaration's names resolve against the document, the
entry's data segment is encoded from the document at write time, and the key contributes only the
tail. `key(...)` (ADR-0016) chooses how much of the key the tail holds; it does not change where the
data segment's values come from.

Consequences the read side depends on:

- a full tail (`KEY_PREFIX` empty) means `taken == Key::KEY_LEN`, so a scan decodes the whole
  primary key and fetches the row back in the same call;
- a narrowed tail means `taken < Key::KEY_LEN`, the fields past the boundary are zero-filled, and the
  scan returns keys with no rows — the caller assembles the primary key itself.

## Rejected: source the data segment from the key

**What it saves.** The duplicated column (16 bytes per row) and the second copy inside the entry
(16 bytes per entry): the entry goes from 44 to 28 bytes for a `Uuid7` identity in an 8-byte
container key, and from 28 to 20 for a u64 pair. Nothing about the query side gets slower.

**What it costs.**

1. **Compile-time validation of `fields(...)`, for every declaration.** The key struct is a separate
   derive and invisible at the document's expansion point, so a name that is not a payload field can
   no longer be told apart from a misspelling: the check moves from the compiler to the first write
   (`unknown group field: <name>`). This is the one cost that lands outside the changed declaration —
   an index field list stops being verified where it is written.
2. **The typed probe builder.** Its steps are generated from payload field types, which give both
   the width and the order. A key field has no payload type to name, only the runtime
   `KeyEncode::FIELDS` table, so a key-sourced index gets no builder and the query side passes raw
   bytes — correct only as long as the caller's encoding matches the declaration.
3. **One index form becomes two.** With the payload as the only source, every index has the same
   contract: fields are row fields, `taken == KEY_LEN`, a scan yields key and row, a builder exists.
   Sourcing from the key splits that into two shapes whose difference is invisible at the call site
   — the reader has to know where a given index's fields came from before knowing whether `decoded`
   is complete and whether a fetch-back will happen.

Sixteen bytes per row and per entry do not buy a second index form and a weaker check.

**When this flips.** Not on size — `KeyEncode` keys are fixed-width by contract, so the copy always
fits. It flips if the copy becomes *wrong* rather than merely redundant: an invariant that the index
entry provably equals the identity, where a divergence between column and key stops being a writer
bug and becomes a correctness hole. Reopening it means paying the two costs above first: a
compile-time check that survives (a const assertion over `Key::FIELDS`), and a `PrefixKey` that
carries how to rebuild the primary key from tail plus data-segment fields.

## Consequences

- A relation table's reverse read keeps its column. The entry carries the field twice and the whole
  primary key as tail; one scan yields key and row, and both decoded segments are true.
- `rebuild_faces` re-derives entries from key **and** row, which is why the copy has to be on the row
  — the key alone cannot produce the data segment.
- The byte layouts and the read-side arithmetic live in `docs/MODELING.*` (many-to-many section);
  this ADR owns the ruling, not the table.