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
  became a boot error instead of a silent decode (ADR-0040). That is
  a per-product reimplementation of a fact okm itself knows — the shape its bytes
  were written in.
- The fixed-face band has one address at its top: 0x1F (0x0F when this ADR was
  written, before ADR-0033 widened the band to `0x00`–`0x1F`), with the headroom
  `0x10`–`0x1D` below it. Leaving the HIGHEST number free "for later" reserves
  the boundary for a face that may never exist, while the band below it is where
  new single faces actually land.

## Decision

**1. 0x1F is the table meta face.** Address `[partition?][ns][0x1F]`, no suffix —
a face-level singleton, the same shape as the vocabulary cursor at `[ns][0x02]`.
One entry per table, in the table's own ns.

**2. The highest face, deliberately.** `slot <= 0x1F` is the one-byte/two-byte
delimiter; the meta sits ON the boundary instead of inside the free band, so the
band stays contiguous (`0x10`–`0x1D` is the single headroom block) and no fixed
face added later can squeeze the meta out of its address.

**3. The value is a name → bytes map, not a struct.** Wire:
`[count u16 BE]` then, per entry, `[name_len varint][name UTF-8][value_len
varint][value bytes]`, entries in name order, lengths in the shared TLV varint
codec.

- `version` is the layout generation the writer speaks: one byte, the declaration's
  own number (`Document::LAYOUT_VERSION`, `#[ok_layout(version = N)]`, default 1).
  It is what lets an older store be REFUSED by number rather than half-read; a
  store that never recorded one cannot be refused for that reason, so absence is
  not a mismatch (see §4). `faces` (ADR-0048) is the fingerprint of the
  declaration's derived faces, and `table` (§7) is its `#[ok_name]`: those three
  are the runtime's names, and anything else is the caller's.
- Names are inline on purpose: stamping a table must not touch the field-name
  vocabulary (0x02/0x03). A probe would otherwise become a writer, and a store's
  self-description would depend on a dictionary that store may not have.
- A second name costs no layout decision — that is the whole point of the map.
  A value is opaque bytes here; the meaning belongs to whoever wrote the name.

**4. One verb writes the runtime's names: `ensure_layout`.** Absent row → seed
`version` (the number this build speaks) and the fingerprint of this declaration's
derived faces; present row → refuse a `version` this build does not speak
(`LayoutMismatch { stored, current }`, naming both), and otherwise answer the row
untouched (other names included), because rewriting the fingerprint would erase the
`Stale` signal `rebuild_faces` acts on. It is called at attach/boot, once per table.
`Collection::new` stays I/O-free and keeps returning `Self` — which is exactly why this
is a verb and not constructor work. A payload generation is not recorded here — it is the
segment header's (ADR-0042, amended) — but the STORE's number is, and that is the one this
row compares: a layout change is a cutover, so a store that records another number is
refused by number at attach instead of being decoded row by row under a declaration that
never wrote it. A row with NO `version` is older than the name, and old-plus-silent is not
a mismatch: it opens, exactly as a store written before this ADR opened.

**5. Callers record their own names with `set_meta`,** and the runtime re-stamps
`version` afterwards — and refuses to write its number over a row that records
another generation's, because that would relabel a store this build cannot read as
one it can. A caller cannot edit the shape stamp away, on purpose.

**6. It is a DETECTION device, never a compatibility promise (ADR-0042).** One
point read at attach turns "the store was written in another shape" into a refusal
that names both numbers, instead of a decode failure on the first row. Nothing is
read through the meta row, nothing is migrated, and no old shape becomes readable
because of it.

**7. The table's own name is recorded here, and one ns is one table.** The row
gains `table` → the declaration's `#[ok_name]`, written by the same first-touch
verb, and a row that already names a DIFFERENT table at this ns is a refusal
(`TableNameTaken`) rather than a second tenant: two tables in one ns would share
every face, the vocabulary cursors and this row itself. This is the store-side
half of the ns claim, and it exists because the compile-time half cannot leave a
module — the derive emits a name per ns (`E0428` on a duplicate), but it
sees one item, so tables declared in other modules or other crates meet nowhere
except here. Unnamed declarations write nothing and check nothing.

## Consequences

- **One point read per table at attach; nothing on the write path, nothing per
  row.** The verb is called where an application already says "I am attaching to
  this store".
- **It is another NON-ROW entry in the ns.** `scan_keys`, `scan_documents_raw` and
  the export path filter by slot, so 0x1F must be skipped the way 0x01/0x02/… are;
  `meta_test.rs` locks that the row scans never surface it.
- **It does not travel in export/snapshot.** The row describes the WRITER's shape;
  the importing build stamps its own. Export is rows, not self-description.
- **A stale store with no meta row and a fresh store are the same bytes.** Seeding
  an absent row blesses whatever is there, which is accepted (ADR-0042 already
  says old stores are discarded, not migrated); a store that DOES record a number is
  refused instead, and the segment header's `layout_version` stays the second line,
  checked where a log is opened.
- **ns ↔ shape is a declaration-time 1:1 that nothing enforces.** Two row types
  sharing one ns now also disagree on `version`, and `ensure_layout` refuses the
  second one — at attach. It is not a write-path guard: that would cost a point
  read per put, and a shared ns is a declaration bug, not a runtime condition.
- **The last fixed face is spent.** The next single-face kind takes 0x10, the
  bottom of the headroom, and 0x1F can never be reassigned without moving the
  width delimiter itself.

## Related records

ADR-0030 (fixed faces and counting families — the band this lands in), ADR-0041
(the compaction that freed 0x0B and put the vector snapshot at 0x0A — 0x1E in the
widened band), ADR-0042 (a
layout version is a stamp, not a compatibility promise), okm MODELING §"Payload
versions and field defaults" (the per-row side of the same fact).