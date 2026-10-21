# ADR-0033: The slot stays behind the ns, and a face's metadata stays on its face

**Status**: Accepted (2026-10-21) — the layout rule is as described; nothing in
the code changes because of this record.
**Date**: 2026-10-21
**Chinese**: [0033-slot-order-and-face-metadata.zh-CN.md](0033-slot-order-and-face-metadata.zh-CN.md)
**Depends on**: ADR-0002 (ns vocabulary), ADR-0012 (field-name vocabulary),
ADR-0030 (fixed faces vs counting families), ADR-0032 (the allocation cursor)

## Context

Every key is `[0xFF][part] (optional) [ns 2B][slot 1–2B][suffix]`, and the slot
carries its own width in its high nibble: `0x00–0x0F` is a fixed face (one
byte), anything else is a counting family (two bytes, 12-bit counter). ADR-0030
then admitted one metadata value per face at the BARE PREFIX — `[ns][slot]` with
no suffix — admissible exactly when every real entry of that face carries at
least one suffix byte. ADR-0032 used that for the vocabulary allocation cursor
(zero-length suffix on the id face).

Three questions follow, and none of them is answered by the existing records:

1. Can an iteration walk past the face it started in — from one slot into the
   next, and across the one-byte/two-byte seam?
2. If data and metadata share a face, what does the length judgement cost — per
   key, or per scan?
3. If that cost were real, should metadata stop living with data — and by the
   same argument, should the slot move in front of the ns?

## Decision

**1. Iteration is bounded by the prefix, not by per-key checks.**
`scan_suffix(p)` IS `scan_range(p, prefix_end(p))` (`engine/storage.rs`), and
`prefix_end` is the byte-order successor: increment the last byte below `0xFF`,
carry on `0xFF`, and `None` when every byte is `0xFF` (unbounded only for an
all-`0xFF` prefix, i.e. never for a real header). The end is exclusive AT PREFIX
LENGTH, so:

```text
[ns][0x02]          → end [ns][0x03]         the 0x03 face is outside
[ns][0x0F]          → end [ns][0x10]         [ns][0x10][nn] sorts AFTER [ns][0x10]
[ns][0x10][0x01]    → end [ns][0x10][0x02]   the next counter is outside
```

The second line is the one place where the two slot widths could collide, and it
does not: a key sorts after its own prefix, so the longer family key falls
outside a range that ends at the one-byte form. No per-key boundary validation
exists anywhere in the read path, and none is needed. The only way to cross a
face is `scan_range(begin, None)` — the engine's unbounded form, which OKM's own
reads never use for face traversal (the one `None` in the tree is
`NestStorage`'s host-key scan over its own prefix space).

**2. Suffix length is the separator, and its cost is one comparison.**
A face's real entries are either exactly `N` bytes (a vocabulary id face) or at
least one byte (a graph face's `edge_id` tail); the metadata entry is zero bytes.
The reader separates them by length, which is one integer compare against an
in-memory length: no value read, no allocation, no per-byte walk, and at most one
skipped entry per face. It shows up in three shapes:

- enumerating readers that must not decode the cursor: `Graph::predicates`
  (`suffix.len() == 2`), `VocabMirror::load` (same), the export's id → label scan
  (`suffix.len() != f.width`);
- partial-index matching (`index.rs`): `suffix.len() == taken` / `>= taken` plus
  one subtraction — a CORRECTNESS condition, not a boundary check: the probe may
  be satisfied by key tail bytes, and only the length says which;
- nowhere else. Fixed-width decoders reject a mismatched length on their own
  (`VocabId::from_be` accepts exactly the width), so a filter that misses yields
  a refusal rather than a wrong id.

No measurement is claimed here. What is established is the shape: O(1) per
enumerated entry, no I/O, fused into loops that already decode — against an
engine cost that is key comparison plus value copy.

**3. A face's metadata stays on that face.** The two ways of separating it were
both rejected:

- *Its own family* — the earlier `0x3nnn` cursor design. It renumbers indexes
  `0x4nnn`, reduces `0x5nnn`, junctions `0x6nnn` (an ordering reverted on
  2026-10-20), and it turns a face's watermark into a numbered instance: the
  thing describes one face, yet it lives as a peer of families.
- *Its own keyspace or prefix region* — the cursor stops being covered by the
  collection's ns. Drop, namespace teardown and partition isolation no longer
  reach it, so a missed cleanup hands stale ids to new data, silently; and
  "vocabulary growth + row + cursor in one batch" becomes a cross-region commit.

Colocation costs nothing because the discrimination is structural (0 bytes versus
`N` bytes is not expressible as data), not a runtime guess — ADR-0030's rule is
already the guarantee.

**4. The slot stays behind the ns.** Moving it in front (`[slot][ns][suffix]`)
would:

- make the ns offset non-constant — the slot is one or two bytes, so every
  reader would have to decode the slot width before it can find the collection's
  identity;
- displace the `0xFF` partition escape from the first byte. That escape is legal
  precisely because a legal ns never starts with `0xFF` (the ns vocabulary is
  `0x00–0xFE`); slot values `0x00–0x5F` would sit where it lives;
- stop a collection from being one contiguous region. Today "everything belonging
  to this collection" is the prefix `[part][ns]`; slot-first splits it into one
  region per face, so every whole-collection operation becomes an N-face loop or
  a keyspace walk with an ns filter.

It would buy one integer compare.

## Considered alternatives

- **Reserved sentinel value** (a magic id, a magic length) instead of a
  zero-length key: forbidden by ADR-0030 — the separation must be structural.
- **Per-key boundary validation** in scans (verify each key's suffix length
  against the face's declared width): redundant. The prefix already bounds the
  iteration, and the decoders reject mismatched widths. It would add a branch
  per key to re-derive a compile-time constant.
- **Metadata in the value of another entry** (e.g. parked in the ns dictionary):
  one more read per allocation, and the write no longer rides the same key family
  as the entries it governs.

## Consequences

- A face may host one metadata value at its bare prefix whenever every real entry
  carries at least one suffix byte — no slot, no family, no migration. That is
  the whole cost of the vocabulary cursor.
- The delimiter rule is load-bearing: a future fixed face above `0x0F`, or a
  family below `0x10`, breaks the seam silently. `FIXED_FACE_MAX`, `write_slot`'s
  assert and the hex-lock tests are what keep it honest.
- Readers that ENUMERATE a face must filter by width; readers that hold a point
  key never meet the cursor.
- Whole-collection operations stay expressible as one prefix per face. A future
  need to walk several faces is an explicit multi-prefix loop, never a keyspace
  walk with a filter.
- Layout mistakes are not self-detecting: a vocabulary pair whose id face points
  at its neighbour's slot passes every local check and only surfaces as ids
  restarting across a process boundary. Each family therefore gets its own test —
  `graph_edge_test::the_predicate_cursor_is_the_id_faces_bare_prefix` pins
  `[ns][0x04]`, the absence of a field-name cursor, and the resume-after-restart
  case.