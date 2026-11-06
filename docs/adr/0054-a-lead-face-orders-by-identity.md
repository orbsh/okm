# ADR-0054: A lead face orders by identity, not by a copy

Date: 2026-11-06
Status: Accepted (implemented: declaration parsing, slot family, delegating `KvIndex` impl, typed probe builder with the const check, `rebuild_faces` / `prune_deprecated_slots` coverage, tests)

## Context

ADR-0052 keeps index fields payload-sourced, and names the cost it accepts: a field the key already holds must exist on the row as a column too, so that a reverse read is one index scan. That column is a **copy of identity**, and nothing in the model makes it agree — `split_names` treats a name that coincides with a key field as an ordinary value field, so `merge` / `replace` through the map bridge can write the column to something else while the key stays put. The reverse index then answers "rows whose column matched", not "rows whose identity matched". For a relation table (ADR-0051) that is the one invariant the pattern depends on, and it is not carried anywhere.

The second cost ADR-0052 charges against key-sourced segments is validation: a document derive cannot see another type's fields, so `fields(...)` could not be checked at expansion time, and the probe builder would lose its concrete parameter types. That argument is about where the error appears, not about whether the check can exist — `KeyEncode::FIELDS` is a `const` table (`name`, `ty`, `width`, all readable in a const context), so the check can run at type-check time instead.

So the question is not "cheaper bytes". It is whether a derived structure should be derived from identity or from a copy of identity.

## Decision

**1. A fourth declaration: `#[ok_lead(name { fields(…) [, key(…)] [, deprecated] })]`.** `fields` names **key** fields, not payload fields; the entry's sorted segment is those fields sliced out of the key's own encoding. `includes`, `func` and `where` are refused at expansion with a message that says why: a lead face has no payload half — there is nothing to cover, nothing to compute, nothing to predicate on.

```text
#[derive(KeyEncode)]
struct MembershipKey { channel: u64, member: Uuid7 }

#[derive(DocumentEncode)]
#[ok_ns(110)]
#[ok_lead(by_member { fields(member) })]
struct Membership { nickname: String, since_ms: u64 }
```

**2. The entry is the key, re-slotted.** Segment = `KeyEncode::encode_prefix_named(buf, FIELDS)`, width = `KeyEncode::prefix_width(FIELDS)`, tail = the full primary key (or `key(…)`'s truncation, exactly as in `ok_index`). The generated `KvIndex` impl is therefore three lines of delegation and never touches `Document`:

```text
[ns 2B][slot 2B][member 16B][channel 8B][member 16B]      44 B, tail = whole key
[ns 2B][slot 2B][member 16B][channel 8B]                 28 B, key(channel): keys only
```

**3. Same trait, same doors, own slot family.** A lead face implements `KvIndex`, so `scan`, `scan_exact`, `scan_covered`, the range iterators, `rebuild_faces` and the changelog's derived-face filter all apply unchanged; `Covered` is always `()`. Slots come from **`0x5nnn`** (`LEAD_SLOT_BASE = slot(0x5, 1)`), declaration order, append-only, `deprecated` keeps the position reserved — the same discipline as `ok_index`, in its own family so the two families can be numbered and diagnosed separately.

**4. Validation runs at type-check time, not at expansion time.** The derive cannot resolve `MembershipKey`'s fields, so the declaration is checked in two places, both of them const assertions against `<Key as KeyEncode>::FIELDS`:

- a module-level `const _: () = assert!(key_field_exists(…))` per declared field — so a name that is not a key field is an error **at the declaration** (`E0080`), not at the first probe. Without it a typo would write an empty segment: the entries would still exist (the tail distinguishes rows) but the face would sort by nothing;
- inside each probe step, `key_field_matches(…, T::KIND, T::WIDTH)` — the half that needs a value type, so it fires where the caller hands one over. Steps are generic over the sealed `KeyFieldValue` (`u8…u64`, `[u8; N]`, `Uuid4`, `Uuid7`, `Vocab<N>`).

`ok_index` keeps its expansion-time `syn::Error` on the `fields(...)` token; the two families validate on different clocks, and that is the price of the split, accepted deliberately. `okm-core/tests/compilefail/lead_not_a_key_field.rs` locks the declaration-side failure.

**5. Faces stay fingerprints.** Lead declarations enter `INDEX_DECLS` / `FACES` in the same `IndexDecl` shape (`fields` = key fields, `includes`/`func` empty); the slot family is the discriminator. A store written under an older declaration is therefore `Stale`, not silently empty (ADR-0048). The changelog needs no edit: its filter is a positive allow-list of truth-bearing faces, so a new derived family is excluded by construction.

## Rejected

- **A `source = key` marker inside `#[ok_index]`.** The index grammar is the payload's — `fields` / `includes` / `func` / `where`. A lead face has none of those optional halves, so one attribute would carry two mutually exclusive regimes, two validation clocks and two sort domains (payload value order vs identity byte order). Two attributes with one contract each is the smaller surface.
- **One fixed face plus a discriminator byte inside the key.** It buys nothing: a lead face is plural per collection, and the slot taxonomy's own criterion is "one face per collection → fixed number, many → counting family". It also costs a byte per entry and a second entry-key shape for every reader.
- **Leaving the column copy as the only option.** That keeps the drift hazard in the one pattern — the relation table — whose reverse direction ADR-0051 defines by identity. The copy also cannot be checked: two facts, no invariant.
- **Waiting for the derive to gain cross-type visibility.** It cannot; a proc macro sees only the annotated item. The const assertion is the reachable end state, not a workaround to be replaced later.

## Consequences

- A lead entry is a **pure function of the key**: it cannot go stale, `rebuild_faces` can regenerate it without decoding a payload (and therefore for a document that has no row at all, ADR-0035), and a payload update cannot invalidate it. The last point is an option rather than a saving today: `put` rewrites every face.
- The expression domain narrows, structurally: keys are ascending and fixed-width, `KeyEncode` refuses `Reverse` (descending), `String`/`VarInt` (variable width) and the wrapper codecs. A lead face can never be a covering index (`includes` does not exist for it), so a caller who needs `nickname` alongside pays one `get`.
- The entry duplicates the leading field unless the tail is truncated, and truncation changes the read contract (`taken < KEY_LEN`: keys, no rows, continue with a prefix scan). The two decisions — segment source and tail width — stay separate knobs, and the default is the full tail so that `scan` means the same thing on both families.
- Two declaration families now validate on different clocks: an `ok_index` typo is a compile error at the declaration, an `ok_lead` typo is a const-evaluation error at the generated impl. Both are compile-time; only the span differs.