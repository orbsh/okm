# 0043 — Table Meta Lives at the Highest Fixed Face

> **Languages:** [English](0043-table-meta.md) (primary) · [中文](0043-table-meta.zh-CN.md)

**Status:** Accepted (2026-10-28). Landed with `okm-core/src/model/meta.rs`,
`Collection::{meta, ensure_layout, set_meta}`, and `okm-core/tests/meta_test.rs`.
The dynamic side followed through the same helpers (2026-10-30:
`DynamicCollection::{meta, ensure_layout, set_meta}`), and both bindings carry the
whole verb set (2026-10-31: Python `Collection.{ensure_layout, meta, set_meta}`,
Steel `(collection-{ensure-layout,meta,set-meta}! name …)`), so a table stamps,
reads and records its own names no matter which mode declared it.
**Date:** 2026-10-28

## Context

A payload carries a version byte (ADR-0042), and that byte answers exactly one
question: did MY shape write this row? It answers it one decode at a time, at the
first row a reader touches, and separately for every table in the store. A store
nobody reads yet is indistinguishable from a fresh one.

Two things were already true around it.

- Aura had to invent the same check on its own layer: a raw marker row in a
  reserved ns, seeded at first boot and verified at every boot, so a renumbering
  became a boot error instead of a silent decode (ADR-0040 Update there). That is
  a per-product reimplementation of a fact okm itself knows — the shape its bytes
  were written in.
- The fixed-face band had one address left at its top: 0x0F, with 0x0B–0x0E free
  below it. Leaving the HIGHEST number free "for later" reserves the boundary for
  a face that may never exist, while the band below it is where new single faces
  actually land.

## Decision

**1. 0x0F is the table meta face.** Address `[partition?][ns][0x0F]`, no suffix —
a face-level singleton, the same shape as the vocabulary cursor at `[ns][0x02]`.
One entry per table, in the table's own ns.

**2. The highest face, deliberately.** `slot <= 0x0F` is the one-byte/two-byte
delimiter; the meta sits ON the boundary instead of inside the free band, so the
band stays contiguous (0x0B–0x0E) and no fixed face added later can squeeze the
meta out of its address.

**3. The value is a name → bytes map, not a struct.** Wire:
`[count u16 BE]` then, per entry, `[name_len varint][name UTF-8][value_len
varint][value bytes]`, entries in name order, lengths in the shared TLV varint
codec.

- `version` is the one name the runtime writes; its value is a single byte, the
  same number the payload header carries (`Document::LAYOUT_VERSION`).
- Names are inline on purpose: stamping a table must not touch the field-name
  vocabulary (0x02/0x03). A probe would otherwise become a writer, and a store's
  self-description would depend on a dictionary that store may not have.
- A second name costs no layout decision — that is the whole point of the map.
  A value is opaque bytes here; the meaning belongs to whoever wrote the name.

**4. One verb writes the runtime's name: `ensure_layout`.** Absent → seed
`version = this build's`; matching → answer the row (other names included);
different, or a row that names no version → `Error::LayoutMismatch { stored,
current }`. It is called at attach/boot, once per table. `Collection::new` stays
I/O-free and keeps returning `Self` — which is exactly why this is a verb and not
constructor work.

**5. Callers record their own names with `set_meta`,** and the runtime re-stamps
`version` afterwards. A caller cannot edit the shape stamp away, on purpose.

**6. It is a DETECTION device, never a compatibility promise (ADR-0042).** One
point read at attach turns "the store was written in another shape" into a refusal
that names both numbers, instead of a decode failure on the first row. Nothing is
read through the meta row, nothing is migrated, and no old shape becomes readable
because of it.

## Consequences

- **One point read per table at attach; nothing on the write path, nothing per
  row.** The verb is called where an application already says "I am attaching to
  this store".
- **It is another NON-ROW entry in the ns.** `scan_keys`, `scan_documents_raw` and
  the export path filter by slot, so 0x0F must be skipped the way 0x01/0x02/… are;
  `meta_test.rs` locks that the row scans never surface it.
- **It does not travel in export/snapshot.** The row describes the WRITER's shape;
  the importing build stamps its own. Export is rows, not self-description.
- **A stale store with no meta row and a fresh store are the same bytes.** Seeding
  an absent row blesses whatever is there, which is accepted (ADR-0042 already
  says old stores are discarded, not migrated) and is why the per-payload version
  byte stays: it is the second line, checked on every decode.
- **ns ↔ shape is a declaration-time 1:1 that nothing enforces.** Two row types
  sharing one ns now also disagree on `version`, and `ensure_layout` refuses the
  second one — at attach. It is not a write-path guard: that would cost a point
  read per put, and a shared ns is a declaration bug, not a runtime condition.
- **The last fixed face is spent.** The next single-face kind takes 0x0B, and 0x0F
  can never be reassigned without moving the width delimiter itself.

## Related records

ADR-0030 (fixed faces and counting families — the band this lands in), ADR-0041
(the compaction that freed 0x0B and put the vector snapshot at 0x0A), ADR-0042 (a
layout version is a stamp, not a compatibility promise), okm MODELING §"Payload
versions and field defaults" (the per-row side of the same fact).