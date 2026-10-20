# ADR-0032: Categorical values are data — `Vocab<T>` over a fourth vocabulary family

**Status**: Accepted (2026-10-20) — implementation in progress; stage 1 (the
vocabulary class moving into counting family 4) has landed, `Vocab<T>` itself
follows
**Date**: 2026-10-20
**Chinese**: [0032-vocab-categorical-values.zh-CN.md](0032-vocab-categorical-values.zh-CN.md)
**Depends on**: ADR-0012 (field-name vocabulary), ADR-0017 §3 (token
vocabulary), ADR-0030 (fixed faces vs counting families)

## Context

`Enum<T>` carries a C-like enum field as a one-byte tag, and the tag table
(`EnumTag::TAGS`) is **code**: an explicit `&[(variant, u8)]` declared once and
kept stable. Two consequences, and the second one is a defect rather than a
trade-off:

- **A new category is a code change.** Data that arrives with a category the
  compiled enum does not know cannot be stored as that category.
- **The table does not travel with the data.** `FieldType::Enum` is exported to
  the schema as a bare unit variant (there is no payload for a tag table), so a
  dynamic reader — okm-dynamic, a Python/Steel booth, any host that assembles a
  collection at run time — decodes the field as a raw number
  (`okm-dynamic/src/decode.rs`: `FieldType::Enum => Value::U8(0)`, and
  `okm-dynamic/src/lib.rs` names the logical type `"U8 (tag)"`). The name exists
  only on the compiled side.

That breaks the promise that the two schema carriers produce the same bytes AND
the same semantics: a compiled writer stores `9`, and only compiled code can say
what `9` means. Field names, by contrast, are already data — interned in the
collection's own vocabulary faces — and every reader resolves them from the
store.

The vocabulary machinery needed for categorical values already exists:
`VocabFaces { id_slot, name_slot }` + `VocabCache` (`okm-core/src/model/obj_vocab.rs`),
allocating first-seen-claims-next, loading both directions lazily, and assuming
the single-writer discipline the rest of the engine assumes. Two vocabularies
use it today (field names 0x02/0x03, predicate/kind tokens 0x04/0x05).

## Decision

**Categorical values become one more vocabulary, and `Vocab<T>` replaces
`Enum<T>`.**

1. **The mapping is data.** `Vocab<T>` holds the NAME in memory and a one-byte
   id on the wire; the name↔id table lives in the collection's ns as ordinary
   vocabulary entries. `Enum<T>` / `EnumTag` retire: one way to model a
   categorical value, not two.

2. **The per-field vocabularies live in counting family 4.** Two `Vocab<T>`
   fields are two independent domains, so the table is per FIELD, and a
   vocabulary is a face PAIR — allocated in declaration order, the same
   discipline `#[ok_index]` uses for its slots:

   ```
   FIELD_VOCAB      = 0x02, 0x03    // field names — fixed faces, unchanged
   PREDICATE_VOCAB  = 0x04, 0x05    // predicates / kind tokens — unchanged
   DATA_VOCAB #0    = 4/0,  4/1     // first declared Vocab<T> field
   DATA_VOCAB #1    = 4/2,  4/3     // second …
   ```

   The two collection-wide name domains stay on their fixed faces: their NUMBER
   is fixed, so they need no allocation, and leaving them in place costs no
   data migration (moving them would mean copying live name tables — a silent
   failure mode, see Consequences). Family 4 carries the open-ended part:
   12-bit counters give 4095 faces, i.e. ~2047 categorical fields per document,
   and the fixed-face space stays reserved for single faces. Family 1 is NOT
   reused for this: its members are access methods, and mixing name tables in
   would end "scan family 1 to enumerate a table's indexes".

3. **One byte, 255 as a hard ceiling PER FIELD.** A table of its own keeps the
   id space to one field's categorical values (schema growth does not consume
   it), which is what makes one byte viable: a `Vocab<T>` field stays
   hot-segment eligible at exactly the cost `Enum<T>` had. Interning the 256th
   distinct value of one field is a loud error, not a wrap — the answer is a
   narrower domain or a `String` field.

4. **Interning is a write-path step, and the batch path carries it.**
   `put` interns before encoding (the document clone this costs is paid only by
   tables that declare a `Vocab` field); `save_with_prev_into` interns through
   `BatchStore`, so vocabulary growth, the row and its faces land in ONE batch
   (the discipline `VocabCache::id_for` already follows); `save_into` cannot
   intern — it is the encoding-only surface with no store by contract — so it
   requires documents whose `Vocab` fields were already interned through the
   collection. This is the concrete boundary the "encode is a pure function"
   rule bends at, and it is the same bend the dynamic segment already makes.

5. **Read fills the name back.** `decode_payload` yields the id (it is a pure
   function and has no store); a deref hook — the same shape as the embedded-ref
   hook `__okm_embed_deref` — resolves it to the name, so `to_map`, the document
   API and every reader see names, and a missing table entry degrades visibly
   instead of inventing a value.

6. **Categories are labels: no declared order.** A categorical string has no
   intrinsic ordering, and the vocabulary's ids are arrival order. A field that
   needs to sort or range-scan in a semantic order declares a separate rank
   column (an ordinary integer field). This is stated as a property of the type,
   not as a caveat: the old promise that tag numbers could encode an order was
   never exercised (no test or doc declared one) and was invisible to every
   dynamic reader anyway.

## Consequences

- A dynamic writer can introduce a category, and a dynamic reader can name it —
  the two-mode symmetry now covers the field's VALUE domain, not just its type.
- `okm-dynamic` loses its `Enum` arms: a `Vocab` field decodes to a name like a
  field name does, and its plan/exec path interns through the same cache the
  compiled path uses.
- The 255 ceiling is a modeling constraint with a stated remedy (narrow the
  domain, or model the value as `String`). It applies PER FIELD: two categorical
  fields are two independent domains, and a wide schema does not eat a field's
  headroom — the property a collection-wide table (or sharing `FIELD_VOCAB`)
  would have destroyed.
- The vocabulary class ends up in two homes BY ALLOCATION REGIME, and that is
  deliberate: the two collection-wide name domains are fixed in number (fixed
  faces, no allocation), the per-field data vocabularies are declaration-order
  and open-ended (family 4). The split is exactly what avoids migrating live
  name tables: a face move would have to COPY entries, and forgetting that
  makes `get_fields` drop fields silently (unknown ids drop the field).
- Migration is therefore about categories only, and it is NOT silent: old
  one-byte `Enum` tags decode without error into a DIFFERENT category, so the
  rewrite happens under a layout version bump — one layout change, no
  compatibility shim.
- Owed: `okm-python` / `okm-steel` exposure of `Vocab` (the bindings currently
  see the storage form); a snapshot/export path for a collection's vocabulary
  faces (today they are derived state outside the slot-0 export, exactly like
  index entries).