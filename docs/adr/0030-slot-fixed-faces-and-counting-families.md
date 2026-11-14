# ADR-0030: Slot — one byte per fixed face, two for counting families

**Status**: Accepted (2026-10-17) — implemented, **revised 2026-11** (the band
widened to thirty-two fixed faces, `0x00`–`0x1F`, with counting families from
`0x2nnn` — see [The band widened](#the-band-widened-2026-11-revision)). The slot
is self-delimiting in `index::slot` / `write_slot` (a first byte below `0x20`
means one byte, anything else two), the caps are asserts rather than masks, and
the counting families in use are 0x2–0x6.
**Date**: 2026-10-17
**Chinese**: [0030-slot-fixed-faces-and-counting-families.zh-CN.md](0030-slot-fixed-faces-and-counting-families.zh-CN.md)
**Supersedes**: the segment table of ADR-0016 (the 4-byte head survives for
counting families; the slot's width now depends on the family)
**Depends on**: ADR-0005 (index slots), ADR-0012 (object model and field
vocabulary), ADR-0016 (head and segments), ADR-0017 (graph faces), ADR-0032
(declared vocabularies), ADR-0041 (graph faces carry both endpoints)

## Context

Every entry key today pays a 2-byte slot — `[ns 2B][slot 2B]`, a 4-bit
segment plus a 12-bit counter (ADR-0016). The counter was bought as
capacity headroom: 4096 per segment, declared-index capacity 240 → 4096.

Measured against its consumers, all twelve fixed faces in use are a
**single face**: the primary, the obj dynamic segment, both directions of the
field-name vocabulary, both directions of the predicate vocabulary, the
graph's four faces, the vector snapshot and the table meta. Their counter is
constant — 0/1/2/3 for the faces numbered inside segment 0x0, 1 for the
graph's — so it carries no information and costs one byte on every such key.

Those faces also encode the same kind of thing two different ways: the faces
below 0x06 are counters inside segment 0x0 (a fossil of the pre-0016 flat
role table, where roles 0–15 were one numbering space), while the graph's
faces each got a whole segment (`0x4001`…`0x8001`, counter pinned at 1). One
face, one number is the uniform rule.

The entries paying for it are the most numerous in the store: the primary
(one per document, one per edge) and the graph's traversal faces (thousands
per edge collection). ADR-0017 §2 priced one byte on a reference at ~5% and
took it seriously; here the byte sits on every primary key and every
traversal entry.

## Decision

### 1. The slot is self-delimiting — the family decides its width

The slot's first byte says whether a counter follows:

```text
fixed face        first byte < 0x20             slot = 1 byte    [ns 2B][0x1F]
counting family   first byte >= 0x20            slot = 2 bytes   [ns 2B][0x2n][nn]
                                                                 counter = (n << 8) | nn
```

So every fixed face — the primary, the dynamic segment, both field-name
vocabularies, the predicate vocabulary, the graph's faces, the handle registry,
the vector snapshot, the table meta — reads one byte, and the counting families
that carry instances keep their two.

In the implementation the width follows from the slot's **value**: a slot
≤ 0x1F is written as one byte, anything else as two big-endian bytes.
`0x0020`–`0x1FFF` is neither — a slot in that gap is refused, not squeezed into
a form: one byte would make it a face it is not, two bytes would make its high
byte the fixed face `0x00`–`0x1F`. `write_slot` asserts that rule, and
`slot_len` gives the width — both live in `okm-core::model::index` next to the
constants, so every site that assembles a key writes a slot the same way.

### 2. The fixed-face table (1 byte, 32 numbers)

```text
0x00  primary                      document collection or edge collection
0x01  obj dynamic segment          [ns][0x01][pkey]                            value = toc_len | blocks | values ([dynamic segment](../internals/dynamic-segment.md))
0x02  field-name vocabulary        [ns][0x02][field-id]                        → name
0x03  field-name vocabulary        [ns][0x03][name]                            → field-id
0x04  predicate vocabulary         [ns][0x04][predicate_id]                    → name
0x05  predicate vocabulary         [ns][0x05][name]                            → predicate_id
0x06  graph out (the edge)         [ns][0x06][src ref][pred][dst ref]          (value = the edge payload; ADR-0067)
0x07  graph in                     [ns][0x07][dst ref][pred][src ref]          (empty, derived)
0x08  graph predicate+out          [ns][0x08][pred][src ref][dst ref]          (empty, derived)
0x09  graph predicate+in           [ns][0x09][pred][dst ref][src ref]          (empty, derived)
0x0A  cluster                   [ns][0x0A][kind][hyper_id]           (value = parent_id + payload; ADR-0066)
0x0B  edge member               [ns][0x0B][kind][hyper_id][src][pred][dst]  (ADR-0066/0067)
0x0C  child cluster index       [ns][0x0C][kind][parent][child]    (derived, rebuildable)
0x0D  edge → cluster index      [ns][0x0D][src_h][pred][dst_h][kind][hyper_id]  (derived, rebuildable)
0x0E  handle → node             [ns][0x0E][handle 8]             → [node_ns 2B][pkey] (authority; ADR-0067)
0x0F  node → handle             [ns][0x0F][node_ns 2B][pkey]     → [handle 8 BE]      (derived)
0x10–0x1D  unassigned           ONE contiguous block — the band's only reserve
0x1E  Extension                [ns][0x1E][subkind]…      each subkind owns its contract (0x01 = vector snapshot)
0x1F  table meta                   [ns][0x1F]                   → name → bytes map (ADR-0043)
```

Rows are grouped by ROLE — a vocabulary precedes the faces that read its
ids; the NUMBERS are the mechanism. 0x1F is the highest face on purpose:
`slot <= 0x1F` is the one-byte/two-byte delimiter, so the meta sits on the
boundary and no face added later can squeeze it out. The band is packed from
`0x00`: core structure, then the handle pair, then the single reserve
(`0x10`–`0x1D`), then the two tail mnemonics — E for Extension, F for the final
face. Nothing is reserved at the low end for growth, so a new face takes `0x10`
and no existing number moves.

**The PREDICATE VOCABULARY holds its own pair (0x04/0x05).** A collection's
token vocabulary beyond field names — a graph's predicates, a node
collection's kind tokens — is not the payload's field-name vocabulary
(0x02/0x03): listing it is one prefix scan of 0x04, and two allocating
caches can no longer hand one id to two names (ADR-0017 §3). The graph's
declared attribute faces are ordinary declared indexes, family 0x3, in the
edge collection's own ns.

### 3. The counting families (2 bytes, 12-bit counter)

```text
0x2nnn  vocabularies       id → label; counter = the vocabulary's number (from 1)
                           (plus the allocation cursor at the bare prefix)
0x3nnn  declared indexes   counter = declaration order (from 1 → 4095 per collection)
0x4nnn  reduces            counter = declaration order, independent quota (4095)
0x5nnn  lead faces         counter = declaration order (from 1 → 4095 per collection, ADR-0054)
0x6nnn  vocabularies       label → id; same counter, other family — moved here from 0x1nnn
                           when the band widened (below)
segments 0x7–0xF           free (9 segments)
```

Family `0x1` is not a family number any more, and neither is `0x0`: `0x1nnn`
would encode as `0x10nn`–`0x1Fnn`, whose first byte is a fixed face, so the two
spaces would overlap on the delimiter byte itself. `index::slot` refuses both
ends rather than masking them.

The counter width does not move: a collection keeps 4095 declared indexes and
4095 reduces, exactly as before.

### 4. The delimiter is a fact about the byte, not a lookup

`b < 0x20` reads one slot byte, anything else reads two. The two spaces
cannot overlap on the first byte (0x00–0x1F versus ≥0x20), so a
family-prefix scan is exact without consulting the table: `[ns][0x07]`
cannot match a counting family, `[ns][0x2n][nn]` cannot match a fixed
face. Being a property of the byte rather than of a maintained list is the
same posture as the ns escape byte (`0xFF`, ADR-0012) and the self-
describing endpoint references (ADR-0017 §2).

### 5. Vocabulary

"Segment" now means the counting families' number only (the high nibble of
their first byte). A single-face family is a **fixed face number**: one
byte, no counter, nothing to enumerate.

## Alternatives considered

- **A flat 1-byte slot, families carved out as level-1 ranges** (fixed
  faces first, then one range each for indexes / reduces). Rejected: a
  level-1 range split presupposes how each family will grow. The pre-0016
  table's two-ended "heap/stack" band (fixed roles growing up from 0, edges
  pinned at 14/15 growing down, 4–13 as unassigned buffer) could only
  delimit two categories — a third has no boundary until the two ends meet.
  It also caps the families that genuinely count at whatever the split left
  them.
- **Promotion without the width change** (fixed faces get their own
  segments, everything keeps 2-byte slots). Rejected: it renumbers the
  counting families (indexes/reduces to 0x9/0xA, forcing a migration) and
  still pays the dead counter byte on the most numerous entries. Its only
  gain is vocabulary, bought with a harder migration.
- **Moving the delimiter to widen the fixed-face space** (e.g. first byte
  < 0x80 → 1 byte, giving 128 fixed faces and 8 counting families). Not
  taken when this ADR was written — 12 of 16 fixed faces and 4 of 15 counting
  families were in use — but **taken in 2026-11 at `k = 3`** (`< 0x20`:
  thirty-two faces, families `0x2nnn`–`0xFnnn`), because the sixteenth face was
  spent and the graph's registry had to fake a pair inside a family. `k` is the
  only knob: top `k` bits zero → `2^(8−k)` fixed faces, and the family nibble
  still names the family.
- **A 3-byte slot for counting families** (family byte + full u16 counter,
  e.g. 0x0100_01). Rejected: it makes index entries longer than today to
  buy counter headroom nothing consumes.

## Boundaries

- **A fixed face carries no counter.** If a family that is one face today
  ever needs instances, it becomes a counting family (take a family number from
  0x7–0xF) — it does not reuse its fixed-face number for sub-kinds. New
  single-face kinds take the next free number on their own side of the
  two-ended allocation described at the end of this ADR.
- **One counter width, 12 bits.** There is no per-family counter width;
  the family byte's low nibble belongs to the counter.
- **Declared indexes and reduces are 4095 per collection** (the counter's
  4096 values with 0 reserved), not 4096 — 4096 would collide with the next
  family's segment.
- **The caps are checked where the numbers are built.** `index::slot(family,
  counter)` asserts both halves instead of masking them: it is a `const fn`, so
  a bad declaration-time constant fails at compile time, and the derive's
  `declared_slot` runs its bound during expansion while a runtime caller gets
  the same refusal. A masked value would not be a wrong number but a
  legitimate slot of another kind — `0x3FFF + 1 = 0x4001` is the reduce family,
  so a 4096th index would be written into the reduce segment and read back as a
  reduce row, and a family above `0xF` wraps into the fixed-face space.
- The head is no longer uniform across families (3 or 4 bytes). Family is
  a compile-time constant at every site, so prefix assembly and the
  primary-key tail slicing stay static arithmetic — no runtime probing.

**A face-level singleton is not a band.** One metadata value per face may take
the bare face prefix as its key — `[ns][slot]` with no suffix. It is admissible
only when every real entry of that face carries at least one suffix byte; then a
zero-length suffix is unrepresentable as data, so the collision this ADR's "no
bands" rule exists to prevent cannot happen at all — it is excluded by LENGTH,
not by a reserved id or a reserved label. The rule generalises to any
one-value-per-face metadata (a version stamp, a watermark), and its two
counter-examples are part of it: the LABEL face (the empty label is legal) and a
no-group REDUCE face (its entry key is the bare prefix) admit no singleton.
Readers separate the two cases by suffix length, and every fixed-width decoder
already rejects a length mismatch. The table meta (0x0F) and each vocabulary's
allocation cursor are the singletons in use.

Every declared vocabulary pair takes one, and the key is DERIVED rather than
numbered: the cursor is `[ns][id face]`, so the exported slot map (`SlotMap`)
names both faces of each pair and no cursor at all. A host-side writer that
allocates ids itself therefore has to build the key from the id slot
(`obj_vocab::cursor_key`) and bump it in the same commit as the entries that
spend it. That is the one place the convention leaves the schema — stated here
because the alternative, giving the cursor a slot number, would make it a face
of its own, which is what this ADR's "no bands" rule exists to prevent.

## Consequences

- **Key-format break, pre-release.** Primary and every fixed face lose one
  byte; the dynamic segment and both dictionaries are renumbered
  (0x0001→0x01, 0x0002/0x0003→0x02/0x03); the graph's faces drop their
  counter and sit at 0x06–0x09, with the vector snapshot at 0x0A. Index and
  reduce entries keep their exact bytes — 0x3nnn and 0x4nnn, the slot values
  the bindings pass. No external consumer exists (PLAN), so a rebuild covers
  it: derived entries rebuild from documents, the dynamic segment and the
  dictionaries ride the same rebuild.
- `SlotMap` (schema export) keeps its `u16` fields — the values are
  0x00–0x1F for the fixed faces and 0x2nnn–0x6nnn for the
  counting families, with the width implied by the value. The bindings'
  slot arguments are unchanged.
- ADR-0016's segment table is superseded; its head-width decision survives
  for the counting families.
- docs/internals/key-layout and slot-mechanism carry the layout tables;
  the mechanism text there is the pair of this decision.

## The band widened (2026-11 revision)

The original band was sixteen faces, `0x00`–`0x0F`, and by 2026-11 every number
was spoken for: the graph's handle registry had to borrow a counting family
(`0xF`) and carry a direction byte inside it, because `0x10`/`0x11` were not
encodable at all. Thirty-two faces were one bit away the whole time, so the
band moved:

- **Fixed faces are `0x00`–`0x1F`**; the delimiter is `first byte < 0x20`.
- **Counting families start at `0x2nnn`** (nibbles 2..F, 57 344 family numbers).
  Family `0x1` is retired: its encoding's first byte is the fixed band.
- **The graph's handle registry became two fixed faces**, `0x0E` (handle → node,
  authority, cursor on the bare prefix) and `0x0F` (node → handle, derived).
  No family number, no discriminator byte, and the pair is adjacent in the band.
- **The tail moved up** so the mnemonics stay at the top: Extension `0x0E` →
  `0x1E`, table meta `0x0F` → `0x1F`. `0x1F` is again the highest face and the
  delimiter's edge, which is the same argument the old `0x0F` carried.
- **`0x10`–`0x1D` is one contiguous reserve** — the band's only one. It sits
  ABOVE the handles, so the next face added takes `0x10` and no number moves.
- **The vocabularies' label direction moved** `0x1nnn` → `0x6nnn`, the first
  number above the highest family in use. It moved rather than the whole table
  shifting by one: that would have rewritten every index, reduce and lead entry
  in every store to buy a prettier numbering.
- **Cost.** Index, reduce and lead slots keep their exact bytes (`0x3nnn`,
  `0x4nnn`, `0x5nnn`); the graph's traversal keys change (`0x0E`/`0x0F` registry,
  one byte shorter), and every store needs the same one-time rebuild this ADR
  already assumes.

## What the slot space actually is

A slot is a `u16`, and the encoding splits it, it is not a pool of 256 numbers. `0x0000`-`0x001F` are the fixed faces. A `0x2nnn`-`0xFnnn` value is a counting family, whose low twelve bits carry the counter. The gap in between — `0x0020` through `0x1FFF` — is rejected by the delimiter, so it is not available for a new fixed face even though it looks free in a byte dump.

Two consequences that the earlier version of this section got wrong:

- A new core row type cannot simply take a number that looks free in the gap. If it is a single row per key it needs one of the thirty-two fixed faces; if it is a growing family it takes a family number. There is no "grow from both ends" pool.
- A subsystem that needs a handful of its own faces either spends fixed-face numbers (eighteen of the thirty-two are in use, fourteen are the reserve) or takes one family number and keeps its discriminator inside the family's low bits.

