# ADR-0012: Object model — one encoding, declared fields plus a dynamic segment

Date: 2026-09-14
Status: Accepted (design; implementation pending — PLAN)

## Context

External data (MQ messages, JSON objects) has no fixed Rust struct at
write time. The current row model requires a declared struct before any
byte is written; storing such data today means inventing a wrapper row or
flattening into ad-hoc keys. Two encodings — a strict row and a loose
object — would double the surface: two derives, two layouts, two decode
paths, two sets of maintenance and teaching costs.

The observation that dissolves the split: a declared row **is** a object
whose dynamic segment is empty. One encoding serves both ends of the
spectrum, with the declared end keeping every capability it has today.

## Decision

**One model, named obj.** The public concept is the object; `ObjEncode` is
renamed `ObjEncode` in the full rename (see below), and a declared row
becomes simply "an obj with only declared fields".

Why not call it document: the concept is kin to document-oriented storage
(CouchDB, MongoDB), and that kinship is worth acknowledging — but the name
`obj` earns its place by a double meaning: object in the programming sense,
and object in the storage-format sense (object/doc/variant). The three terms
mark three distinct positions on the static/dynamic spectrum, and obj sits
deliberately apart from document:

- **document** — logically and physically all-dynamic; every field rides the
  dynamic path (dictionary number + value type per frame).
- **variant** — a dynamic blob nested as ONE declared static field (a
  `Vec<u8>`/`Bytes` payload field); the dynamics live inside a value, not in
  the key layout.
- **obj** — logically all-dynamic (any field may appear at run time), but
  declared static fields embed into the dynamic whole: hot/cold segments,
  indexing, and schema export apply to them exactly as today. A declared row
  is the degenerate obj with an empty dynamic path; a pure document is the
  degenerate obj with an empty declared path; both are the same encoding.

So an obj is NOT a document with a new name — the static-into-dynamic
embedding is the difference, and "obj" says object without surrendering the
document comparison.

Byte layout per obj payload (slot 0, primary):

```text
[version u8][hot_len u16 BE][hot segment][cold TLV]
```

— unchanged from today's row. Declared fields keep the current encoding:
fixed-width fields contiguous in the hot segment, variable-width fields as
`[tag][len u32][bytes]` frames in the cold segment, tag = declaration
index. Types come from the compile-time `FieldDesc` table (`Row::FIELDS`),
never from the bytes.

**Dynamic segment (slot 1).** An obj may additionally carry undeclared
fields, stored as ONE entry per row under `[ns][1][key payload]` — the
same skeleton as an index entry (`[ns][slot][...]`, slot right after ns),
with no field segment in between; the primary key rides in the tail.
Per-row data hangs off the row key: without it, every row's dynamic
segment would pile onto one physical key. The shared `[ns][1]` prefix
also makes the whole segment prefix-scannable (prune, rebuilds). Its
value is a list of frames:

```text
[slot 1 value] = ([field-id][value-type][len u32][value bytes])*
```

- `field-id`: the field NAME's dictionary number (see below).
- `value-type`: a small run-time value vocabulary — this is the one
  genuinely new element. Declared fields carry types in the compile-time
  FieldDesc table; dynamic fields must be self-describing, so their
  frames open with a value-type byte (integer / float / str / bytes /
  bool / null / array / obj-reserved). Nesting may reuse the obj frame
  recursively; the vocabulary ships with a reserved variant even if
  nesting support lands later. The framing is **CBOR-derived, not
  CBOR**: only the major-type pattern is taken — a type nibble opens
  each frame — and struct encoding rides the same scheme. CBOR's major
  types include lists and maps; with a field-name dictionary OKM needs
  only the list — a map IS an n-TLV list (id, type, len, value),
  structure information living in the values, not in a per-document
  schema.
- Ordering: frames are written in insertion order; readers address
  dynamic fields by id, not position.

**Only declared fields are indexed.** Indexes need stable wire order and
fixed-width comparison keys — properties a self-describing dynamic value
does not offer. Dynamic fields are retrievable but not indexable; if a
dynamic field becomes hot enough to query, it gets declared (a one-line
schema change plus a table rebuild), which is exactly the discipline the
row model already teaches.

## Field-name dictionary (slots 2/3)

Dynamic frames carry names by number. The dictionary is a **run-time,
per-table, bidirectional mapping** — the first mutable vocabulary OKM
stores in KV (ns stays compile-time; ADR-0002's "the dictionary lives in
code" is a statement about the ns layer, not this one):

```text
slot 2: [ns][2][field-id]      → name bytes   (number → name)
slot 3: [ns][3][name bytes]    → field-id     (name → number)
```

Discipline, copied from the ns dictionary:

- **Append-only.** Numbers are never reused, never renumbered. A name
  seen for the first time on the write path claims the next free number
  (single-writer allocation, serialized by the engine mutex).
- **Escaped ids, not renumbering.** u8 numbers would eventually run out;
  renumbering stored data is prohibitively expensive (this was the
  rejection reason). Instead id `0xFF` is an escape: `[0xFF][u16 id]`
  follows. Growth is append-only; stored data never migrates; objects
  with ≤254 fields pay one byte per frame.
- **Flat, not trie.** Both directions are plain point lookups on the
  engine's own byte-ordered structure. Trie sharing of name prefixes was
  evaluated and rejected: ordinary business names share few prefixes, the
  trie's multi-hop traversal and UTF-8 radix cost buy nothing, and the
  bidirectional flat mapping keeps number→name as cheap as name→number.
- Raw names in frames were considered (no dictionary at all) and
  rejected for the target shape: same-table objects reuse the same
  names heavily (MQ topics), so paying the full name per frame per
  object is the expensive end. A truly schemaless table (every row
  distinct names) degenerates to a 1:1 dictionary — no worse than raw
  names, and it still buys a single lookup structure.

**Why first-seen allocation beats inference and closed enums.** The
target workload is an interleaved stream with a bounded-but-open field
vocabulary — Aura events of finitely many types arriving in arbitrary
order, structured logs with fixed time/level/event plus unpredictable,
growing extras. Two standard answers fail here:

- **First-record inference** (Elasticsearch-style) assumes the first
  document reveals the schema; type B arriving after type A turns the
  guess into a mapping conflict and an operations incident. The
  dictionary has no first-record concept: a name claims its number
  whenever it first appears, in whatever order.
- **Closed declared enums** (a compile-time registry of every field
  name) would need a rebuild for every new category — exactly the
  unpredictability the workload rules out. The dictionary absorbs new
  categories at run time with no recompile and no estimate.

The type byte, by contrast, stays per-frame: the same stream interleaves
different value types at the same frame position, so type repetition
cannot be assumed and a type dictionary would buy nothing. (Fully
homogeneous collections — numeric columnar shapes — are not the object
model's target; they are served by nested declared objects, which
restore hot-segment O(1) offsets and indexing for the repeated
structure, while the outer object stays purely dynamic.)

## Slot allocation (revised)

```text
slot 0      primary table          [ns][0][key payload]
slot 1      obj dynamic segment    [ns][1][key payload]
slot 2      field-name dictionary  [ns][2][field-id]        → name
slot 3      field-name dictionary  [ns][3][name bytes]      → field-id
slot 4–13   reserved (two-ended growth buffer)
slot 14     edge forward           [ns][14][A·id][B·id]
slot 15     edge reverse           [ns][15][B·id][A·id]
slot 16+    indexes and reduces (declaration order), [ns][slot][...]
```

Fixed roles grow upward from 0; edges sit at the top (14/15) and grow
downward; 4–13 is an unpartitioned free buffer between the two fronts —
the heap/stack memory-layout shape. No internal zoning: a future fixed
role claims the next number from whichever front needs it; exhaustion =
the fronts meet. The fixed/declared boundary sits on a nibble edge
(`0x0?` fixed, `0x1?` declared) for hex-test legibility.
Edge slots 14/15 belong to the edge-via-slots decision, PLAN Phase 10 —
listed here so the table shows the complete final allocation; the
direction-bit niche of ADR-0001 is retired by it.)

Existing declared rows are byte-compatible: they simply use none of the
new slots. The reserved band (4–13) is cheap insurance (ten numbers);
indexes/reduces start at 16, leaving 240 per table — ample for any
single table.

## Renaming: kv_ → ok_

The attribute family renames `kv_` → `ok_` (`ok_ns`, `ok_index`, `ok_ref`,
`ok_subscribe`, `ok_default`, `ok_event_enum`), and the row concept
renames to obj (`ObjEncode`). The `o` is doubly loaded: object in the
programming sense, and the storage-format sense (object/doc/variant, orthogonal terms). The
rename lands with the doc work, before crates.io publishing — after
publishing it would be a breaking change. The derive macro names follow
the same root (`KeyEncode`, `ObjEncode`); `ObjEncode` disappears into the
obj concept.

## Consequences

- One encoding, one derive family, one decode path; declared rows are the
  zero-dynamic-segment special case and need no migration.
- OKM gains its first run-time vocabulary (field names) with ns-dictionary
  discipline: append-only, escaped growth, single-writer allocation.
- Value types for dynamic fields become part of the wire contract (a
  small closed enum in okm-core, mirrored in schema exports).
- Dynamic fields are readable everywhere but indexable only after being
  declared — schema evolution is the escape hatch, deliberately manual.
- Implementation is PLAN-gated: ObjEncode rename, slot-1 writer/reader,
  dictionary maintenance, value-type enum, schema export extension.


## Update 2026-09-19: frame length is a varint (P2)

The dynamic-segment frame `[field-id][value-type][len u32][bytes]`
became `[field-id][value-type][len varint][bytes]` — the length
prefix uses the prefix-monotonic wire codec introduced for `VarInt`
(P1), implemented once in `wrappers/wire.rs` and shared by the field
wrapper, the dynamic-segment frames, and Array/nested-Obj element
frames. Small frames cost 2-3 header bytes instead of 5. The
element-type vocabulary, dictionary, and ordering rules are
unchanged. Declared cold-segment frames (`[tag][len u32]`, ADR-0004)
keep the fixed length for now.
