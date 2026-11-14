# ADR-0012: Object model — one encoding, declared fields plus a dynamic segment

Date: 2026-09-14
Status: Accepted (2026-09-14; landed — the obj payload's two slots, the field-name vocabulary, `DocumentEncode` in both directions, `Ref`/`Refs` embedding). The dynamic segment's value layout is now ADR-0057's: a block table at the head of the value area, and the frame list below is no longer written or read.

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

**One model, named obj.** The public concept is the object; the derive is
`DocumentEncode`, and a declared row is "an obj with only declared fields".

Why not call it document: the concept is kin to document-oriented storage
(CouchDB, MongoDB), and that kinship is worth acknowledging — but the name
`obj` earns its place by a double meaning: object in the programming sense,
and object in the storage-format sense (object/doc/variant). The three terms
mark three distinct positions on the static/dynamic spectrum, and obj sits
deliberately apart from document:

- **document** — logically and physically all-dynamic; every field rides the
  dynamic path (vocabulary number + value type per frame).
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
[hot segment][cold TLV]
```

— unchanged from today's row. Declared fields keep the current encoding:
fixed-width fields contiguous in the hot segment, variable-width fields as
`[tag][len varint][bytes]` frames in the cold segment, tag = declaration
index. Types come from the compile-time `FieldDesc` table (`Row::FIELDS`),
never from the bytes.

**Dynamic segment (fixed face 0x01).** An obj may additionally carry undeclared
fields, stored as ONE entry per row under `[ns][0x01][key payload]` — the
same skeleton as an index entry (`[ns][slot][...]`, slot right after ns),
with no field segment in between; the primary key rides in the tail.
Per-row data hangs off the row key: without it, every row's dynamic
segment would pile onto one physical key. The shared `[ns][0x01]` prefix
also makes the whole segment prefix-scannable (prune, rebuilds). Its
value is a **block table over a value area** (ADR-0057 — the flat frame list
this ADR first specified is retired; the full entry grammar, the ordering
contract and the size envelope are in
[dynamic segment](../internals/dynamic-segment.md)):

```text
[dynamic-face value] = [toc_len u16 BE][blocks][values]
[block]              = [count varint]([field-id][type][len varint][value])*
```

- `field-id`: the field NAME's vocabulary number (see below).
- Block 0 holds the top-level entries; a composite entry owns the **next**
  block, so nesting is breadth-first: the *j*-th composite met while walking
  owns block *j+1*. The table stores no offsets — a block's value base is the
  sum of the lengths of the leaves before it, and a block's own entries the
  same, arithmetic over bytes already in hand.
- `len` is present on **leaf** entries only, and is the prefix-monotonic varint
  codec (`wrappers/wire.rs`) that `VarInt` uses, shared with the cold segment's
  frames and the Array/nested-Map element frames: a small entry costs 2–3
  header bytes instead of 5. A composite carries no length — its block's
  `count` delimits it.
- `type`: a small run-time value vocabulary — this is the one genuinely new
  element. Declared fields carry types in the compile-time FieldDesc table;
  dynamic fields must be self-describing, so their entries open with a
  value-type byte (integer / float / str / bytes / bool / null / array /
  obj-reserved), and **bit 7 of that byte marks an entry that owns the next
  block** — so every entry self-delimits without understanding the type, and
  an unknown type can still be skipped. Nesting may reuse the obj frame
  recursively; the vocabulary ships with a reserved variant even if
  nesting support lands later. The framing is **CBOR-derived, not
  CBOR**: only the major-type pattern is taken — a type nibble opens
  each frame — and struct encoding rides the same scheme. CBOR's major
  types include lists and maps; with a field-name vocabulary OKM needs
  only the list — a map IS an n-TLV list (id, type, len, value),
  structure information living in the values, not in a per-document
  schema.
- Ordering: entries carry NO ordering guarantee. Every writer in the tree
  takes the document map (`okm_value::Dict`, an opaque insertion-ordered
  name → value map), so the bytes come out in that map's INSERTION order —
  not id order, and not sorted by name. The map's container is not part of
  the API (it is a type now, not an alias: ADR-0035; its explicit method-only surface, and why it is not `Deref`, are ADR-0063), so no site can
  reach for sorted iteration; a document assembled in a different order
  encodes to different bytes, which is a LAYOUT difference: readers address
  dynamic fields by id, never by position, so nothing may assume ids ascend
  inside the blob (a binary search over ids is not available).

**Declared fields are indexed through their own faces; a dynamic field is
indexed through a declared path.** Indexes need stable wire order and
fixed-width comparison keys — properties a self-describing dynamic value
does not offer *as key material*. ADR-0029 closes the gap from the other
side: a path index over a declared path materialises one entry per resolved
value (the value types with a storage-form encoding), landed on the
schema-driven write path. What a dynamic name can never be is KEY material:
a name is runtime data, so a dynamic path is addressed by its declaration,
never by the key. A dynamic field that turns out to be hot in a key-shaped
way still gets declared (a one-line schema change plus a table rebuild) —
the row model's discipline, unchanged.

## Field-name vocabulary (fixed faces 0x02/0x03)

Dynamic frames carry names by number. The vocabulary is a **run-time,
per-table, bidirectional mapping** — the first mutable vocabulary OKM
stores in KV (ns stays compile-time; ADR-0002's "the vocabulary lives in
code" is a statement about the ns layer, not this one):

```text
fixed face 0x02: [ns][0x02][field-id]      → name bytes   (number → name)
fixed face 0x03: [ns][0x03][name bytes]    → field-id     (name → number)
```

Discipline, copied from the ns vocabulary:

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
- **The mirror is per handle, and an allocation is durable only when its
  commit lands.** One in-memory mirror lives for the whole `Collection`
  (rebuilt per operation it turned every dynamic field into a point probe,
  on the read door and the write door alike). Its name→number direction is
  trusted without a probe only after the commit that carried that
  vocabulary write returned `Ok`; a refused commit otherwise leaves the
  reader naming an id no entry defines. The allocation CURSOR is never
  cached: two handles on one engine allocate from the same counter, so a
  cached watermark would hand out an id the other just took. Two writers
  allocating concurrently still race on the cursor — the single-writer
  assumption, unchanged.
- **One write, both directions, so one face is the whole mapping.** An
  allocation writes name → id, id → name and the cursor in ONE batch, so
  the id face ALONE rebuilds the map in either direction: the
  schema-driven side loads a handle's vocabulary with a single scan and
  inverts it, then KEEPS it and resolves a miss with a point probe — a
  name another handle allocated is adopted before it can be given a second
  id, and an unknown id is looked up on the id face. The name face earns
  its bytes on point lookups, never on rebuilds. A writer that commits each
  allocation immediately (the graph's edge path takes the real store, not the
  edge batch) promotes them as soon as the write returns; one that stages
  growth into a caller's batch promotes only the ids that commit carried.
- Raw names in frames were considered (no vocabulary at all) and
  rejected for the target shape: same-table objects reuse the same
  names heavily (MQ topics), so paying the full name per frame per
  object is the expensive end. A truly schemaless table (every row
  distinct names) degenerates to a 1:1 vocabulary — no worse than raw
  names, and it still buys a single lookup structure.

**Why first-seen allocation beats inference and closed enums.** The
target workload is an interleaved stream with a bounded-but-open field
vocabulary — Aura events of finitely many types arriving in arbitrary
order, structured logs with fixed time/level/event plus unpredictable,
growing extras. Two standard answers fail here:

- **First-record inference** (Elasticsearch-style) assumes the first
  document reveals the schema; type B arriving after type A turns the
  guess into a mapping conflict and an operations incident. The
  vocabulary has no first-record concept: a name claims its number
  whenever it first appears, in whatever order.
- **Closed declared enums** (a compile-time registry of every field
  name) would need a rebuild for every new category — exactly the
  unpredictability the workload rules out. The vocabulary absorbs new
  categories at run time with no recompile and no estimate.

The type byte, by contrast, stays per-frame: the same stream interleaves
different value types at the same frame position, so type repetition
cannot be assumed and a type vocabulary would buy nothing. (Fully
homogeneous collections — numeric columnar shapes — are not the object
model's target; they are served by nested declared objects, which
restore hot-segment O(1) offsets and indexing for the repeated
structure, while the outer object stays purely dynamic.)

## Slot allocation

```text
0x00       primary table          [ns][0x00][key payload]
0x01       obj dynamic segment    [ns][0x01][key payload]
0x02       field-name vocabulary  [ns][0x02][field-id]        → name
0x03       field-name vocabulary  [ns][0x03][name bytes]      → field-id
0x04/0x05  predicate vocabulary   [ns][0x04][predicate_id]    (ADR-0017)
0x06–0x0D  graph faces            (ADR-0017, ADR-0041, ADR-0066)
0x0E/0x0F  handle registry        (graph handles, ADR-0033)
0x1E       Extension              (okm-vector cache — a fixed face, not 0x0A)
0x1F       table meta             (ADR-0043)
0x2nnn     vocabulary, label → id [ns][0x2nnn][...]
0x3nnn     vocabulary, id → label [ns][0x3nnn][...]
0x4nnn     declared indexes       [ns][0x4nnn][...]
0x5nnn     reduces                [ns][0x5nnn][...]
0x6nnn     lead faces             [ns][0x6nnn][...]
```

(The numbers above are the current band: ADR-0033 widened the fixed faces to
`0x00`–`0x1F` and put the mnemonics at the tail; the counting families run in
declaration order from `0x2`. This ADR's decision — every face the obj owns is
a one-byte fixed face — is unchanged.)

Every face the obj itself owns is a one-byte **fixed face**; declared
indexes and reduces are **counting families** (family nibble plus a 12-bit
counter, two bytes). The slot is self-delimiting — its first byte says
whether a counter follows — so a fixed face and a counting family can never
be confused. ADR-0030 owns the full assignment and the fixed-face table.

## Renaming: kv_ → ok_

The attribute family is `ok_` (`ok_ns`, `ok_index`, `ok_ref`,
`ok_subscribe`, `ok_default`, `ok_event_enum`) in place of the `kv_`
prefix, and the row concept is named obj. The `o` is doubly loaded:
object in the programming sense, and the storage-format sense
(object/doc/variant, orthogonal terms). The derive macro names follow
the same root (`KeyEncode`, `DocumentEncode`).

## Consequences

- One encoding, one derive family, one decode path; declared rows are the
  zero-dynamic-segment special case and need no migration.
- OKM gains its first run-time vocabulary (field names) with ns-vocabulary
  discipline: append-only, escaped growth, single-writer allocation.
- Value types for dynamic fields become part of the wire contract (a
  small closed enum in okm-core, mirrored in schema exports).
- Dynamic fields are readable everywhere; indexing one goes through a
  declared path (ADR-0029), which materialises one entry per resolved value
  on the schema-driven write path. A name is runtime data, so it is never
  KEY material — a dynamic field that needs key-shaped access still gets
  declared.
- The implementation covers the `DocumentEncode` derive, the slot-1
  writer/reader, vocabulary maintenance, the value-type enum, and the
  schema-export extension.
