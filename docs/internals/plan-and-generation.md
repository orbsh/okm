# The declaration, the plan, and the address it writes into

> **Languages:** [English](plan-and-generation.md) (primary) · [中文](plan-and-generation.zh-CN.md)

Mechanism/implementation document for what a declaration compiles to and where the compiled result lives: the plan (routing), the entry layout (producers), the capture filter, and the address the layout generation owns. The decisions are [ADR-0074 §3](../adr/0074-the-store-describes-its-tables.md) (a plan per table), [ADR-0075 §2](../adr/0075-a-generation-change-is-a-migration.md) (the generation is one bit of the address), [ADR-0076 §1](../adr/0076-the-mechanism-moves-under-the-declaration.md) (the entry layout is the only entry producer), [ADR-0056 §2](../adr/0056-the-segment-is-the-migration-path.md) (what the log carries), and [ADR-0042](../adr/0042-layout-versions-are-stamps.md)/[ADR-0043](../adr/0043-table-meta.md) (the stamp). The numbers themselves are [key layout](key-layout.md); the entries are [index mechanism](index-mechanism.md); the segment's bytes are [dynamic segment](dynamic-segment.md). 中文版：[plan-and-generation.zh-CN.md](plan-and-generation.zh-CN.md).

Four things come out of one declaration and they are easy to confuse. The ADDRESS is where the table's rows live — its ns, with one bit that belongs to the layout generation. The PLAN is what routing needs: the key prefix, which slots the log carries, what numbers the declaration gave them. The ENTRY LAYOUT is what a row has to produce. The STAMP is what the store remembers about the first two, written once into the table's meta row. They are built at different times, held by different owners, and they move differently when a declaration changes.

## The address: identity, address, and the bit between them

A key is `[partition?][ns 2B][slot][payload…]`. The ns is two bytes and only fifteen of its bits name the table:

```rust
/// The bits of an ns that name the TABLE: the rest belong to the layout generation
/// (ADR-0075 §2).
pub const IDENTITY_MASK: u16 = 0x7FFF;
```

The high bit of the first byte is the generation's parity — `LAYOUT_VERSION & 1` — so `ns < 0x8000` is a checked precondition, not a comment (`document.rs::ns_bytes` asserts it with the ADR number in the message). Identity and address are two different questions and the mask is where a value crosses between them: a name, a membership list, a `#[ok_nodes]` entry and `Collection::ns` speak the identity; a key speaks the address.

Three builders set that bit, and they have to agree to the byte:

- **The derive**, at compile time: `NS_PREFIX` is `&[ns_hi | parity, ns_lo]`, with `parity = (layout_version & 1) << 7` (`okm-derive/src/document_encode.rs:1550`). A typed key starts from the const, so the bit is in it before any runtime code runs.
- **`ns_bytes::<R>(ns)`**, for the two constructors that take an ns at runtime (`with_ns`, `rebased`): the same OR, so a mapped table still carries THIS declaration's generation.
- **`TablePlan::compile`**, for a declaration read back from a store: it masks the stored ns with `0x7F` first and ORs its own parity, so a schema that arrived as bytes names the same address the compiled declaration does, byte for byte.

What a flip does NOT move: the slot numbers, the payload widths, the face fingerprint over them. That is the whole reason the generation is a bit of the address instead of a version on every row — a new generation is the same table at another address, so the migration is a copy plus a replay, not a rewrite of the bytes the previous generation already wrote (ADR-0075).

## The plan: routing, and only routing

`TablePlan` holds three fields and answers three questions (`okm-core/src/model/table_plan.rs`):

```rust
pub struct TablePlan {
    header: &'static [u8],            // [partition?][ns 2B], the address above
    sides: [u64; BITMAP_SLOTS / 64],  // one bit per slot: set = the log carries it
    slots: SlotMap,                   // the declaration's numbers, for what the bitmap cannot hold
}
```

`header()` is what a write prepends. `carries(slot)` answers the capture question: one test and one shift below `0x0100`, and the rule again above it, because the bitmap only covers the fixed band and the standard families (`BITMAP_SLOTS = 256`). `carries_key(key)` is the same question asked of bytes: strip the header (a key of another table in a cross-collection batch is not ours to write), read the slot positionally (one byte up to `FIXED_FACE_MAX = 0x1F`, two above it), refuse a bare prefix (a vocabulary cursor and the meta row are singletons — no row has that shape), then `carries`.

Two constructors, one shape:

- `TablePlan::of(header, slots)` is `const`: nothing to build, nothing to lock, no second copy to drift. The derive emits one plan per ns into `static __OKM_NS_{ns}_ENTRY: OnceLock<TablePlan>`, and attach fills it once with `get_or_init(compile(declaration))` (ADR-0079 §2). A second row claiming the same ns in the same module is a build error naming the ns.
- `TablePlan::compile(&schema)` is the dynamic path: no type to ask, so the same value is built at runtime from the declaration the store's meta row holds. The header is leaked — two bytes per distinct declaration — because a plan is `'static` by construction and the alternative is a lifetime parameter on every table.

The plan **must not fail**: `compile` answers `Self`, not a `Result`, because the log holds one copy per table and asks it on every staged op (`VirtualStorage::note_table` takes `&'static TablePlan`; the log keeps `Known { meta, plan }`). A structure shared that widely cannot carry a per-handle refusal — which is the first of the reasons the entry half lives somewhere else.

## The entry layout: the half that can be refused

`EntryLayout` is the other half of the same compilation, and it rides the COLLECTION, beside the reused `Entries` buffer (`okm-core/src/model/entry_layout.rs`, ADR-0076 §1):

- It is **fallible**. `EntryLayout::compile(schema, faces)` answers `Result<Self, Unsupported>`: a face may name a field the schema does not have, put a variable-width field anywhere but last, or take its segment from a `func`/`where`. The refusal surfaces where the declaration arrives — `declare_index` for a dynamic table, assembly for a typed one.
- It is **per handle**, not per type. A dynamic table recompiles it whenever a declaration arrives, while its plan's header, bitmap and slots never move.
- It has **one reader**, the write path. The log never asks it anything.

The typed side holds it as an `Option`, and `None` is a decision rather than an absence: a table with a `func` face or a `where(...)` predicate keeps the generated producer for ALL of its faces, because one face has one producer and the walk cannot produce that one.

What the layout contains — a key field's static offset, a hot field's offset, a cold field's tag, plus each face's includes and its tail — and how the walk turns those into entries is [index mechanism](index-mechanism.md).

## Capture: which slots the log carries

The split is written once, in `changelog::side_of(slots, slot)`, and both the capture filter and the drainer read that answer instead of re-typing numbers (`okm-core/src/changelog/mod.rs:93`):

- **Truth** — what a replay cannot re-derive: the primary row, the dynamic segment, the four vocabulary directions (`field_vocab_id/name`, `predicate_vocab_id/name`), the cluster-kind vocabulary (`0x0A`/`0x0B`, a dictionary rather than an inference), the cluster row (`0x0C`) and its edge members (`0x0D`) — a grouping is an assertion, not a derivation.
- **Derived** — everything a reader can rebuild by replaying the truth through a write path: declared indexes, reduce accumulators, lead faces, the graph's traversal faces, the cluster's child index (`0x0E`) and edge → cluster index (`0x0F`), the vector snapshot, the table meta row.
- **Family fallback** — above the named numbers the answer comes from the family in `slot >> 12`: `VOCAB_LABEL`/`VOCAB_ID` are truth, everything else is derived. That is why the classification can be total: a new slot number inside a known family is answered, not silently dropped through a whitelist.

The plan precomputes the answer for the fixed band as a 256-bit bitmap, built in `TablePlan::of` from the ten named truth slots the declaration carries. Above the bitmap `carries` asks `side_of` again, so the fast answer and the general rule cannot disagree — within the band the rule's answer is a named slot every time, and above it the rule costs one call.

## The stamp, and the second parity

The store remembers two facts about a declaration, in the table's meta row at `[header][0x1F]` (ADR-0043, ADR-0042): `version` — the layout generation the payloads were written under — and `faces` — the fingerprint of the declared faces (ADR-0048).

`meta::stamp_layout(store, header, layout_version, faces)` is the only writer: read the row (a fresh store has none), `check_layout`, set both names, put. And `check_layout` is where a foreign generation is refused by name rather than by a wrong read:

```rust
Some(stored) if stored != layout_version => Err(Error::LayoutMismatch { stored, current }),
Some(_)                                  => Ok(()),
None                                     => Err(Error::LayoutUnstamped { current }),
```

First touch never REVISES `faces`: a row that exists without a fingerprint is either older than faces or a rebuild that never finished, and both must keep answering `Stale`/`Unknown` so the repair verb has something to act on (ADR-0048).

Because the parity is a bit of the address, a flip leaves two meta rows one bit apart. `meta::current(store, header, this_build)` probes both — `other_parity(header)` flips `0x80` on the header's first byte — and takes the larger version as current. Two rows are therefore not a conflict but a fact: a flip happened, the newer row is live, the older one waits for its vacuum.

The log follows the same address. Its directory is the header in hex (`table_dir(root, header)`), so a generation flip puts the new generation's log in a directory of its own. A segment's header carries `layout_version` too, and a window is ONE generation: a segment stamped with another generation ends the window and starts the next one — after an upgrade that is the normal case, not a corruption (ADR-0056 §5/§6).

## What moves when a declaration changes

| change | what moves |
| --- | --- |
| payload shape (`LAYOUT_VERSION` +1) | parity bit, address, meta row, log directory; slot numbers and widths stay |
| a face added / removed / renumbered | the `faces` fingerprint: `faces_status` answers `Stale`, `rebuild_faces` re-derives |
| a face's callable changed | `AccessMethod.version` (dynamic) rides the fingerprint — nobody has to remember a bump |
| nothing declared | nothing moves; `ensure` writes the stamp once and the log's `Known` record is shared |

## Where the numbers live

`okm-core/src/model/layout.rs` holds every width and every fixed face in one file, with the band documented as a contiguous table: `0x00`–`0x1F` fixed faces, `FIXED_FACE_MAX = 0x1F`, so `slot <= 0x1F` is one byte and anything above is two; `SLOT_COUNTER_BITS = 12`, families `0x2..=0xF`. Changing a width is a change to that file — everything else is expressed in terms of its constants.

## Changing generation

The verdict, the copy and the flip are one function, `okm_core::changelog::migrate::migrate_table`, and one verb: `okm-log migrate --store <dir> --table <hex> --to <schema.json> [--root <dir>]`. `--table` is the header a table's log directory is named by — the address, bit and all — and `--to` is the next declaration as a build exports it.

The verdict asks the store, not the caller: `meta::current` says which generation is live, a target that does not move that table by one is refused (it would land on the same parity), and a target naming another ns is refused. Comparing the two declarations' fingerprints is not part of it — a migration's premise is that they differ, so that comparison could only answer yes. It happens at attach (ADR-0074 §5), which is also where a migration onto the wrong target is caught, before a row is served.

The copy decodes every row of the old range through the declaration the store records and writes it through the new one, so the offsets that moved are the only thing the two generations disagree about. Then the tail: `--root` given means segments sealed after the copy's mark are replayed on top, because the writer kept running; absent means the caller quiesced the table, and the verb prints that it assumed so. The flip is the one record the new handle writes, and nothing is deleted.

The copy is verified twice, and both times before anything irreversible happens. Every row of the old range must exist in the new one and decode to the same values through its own declaration: offsets differ by construction, so the comparison is on decoded rows, never on bytes. A row missing on the new side, a field whose value differs, and a field the new declaration dropped are each refused by name. `migrate` runs it before writing the record, `vacuum` runs it before deleting the first key — so the flip and the delete, the only two steps that cannot be taken back, both sit behind a comparison that has already succeeded.

`okm-log vacuum --store <dir> --table <hex> [--root <dir>]` is the second decision and the only place that deletes. The older of the two records decides which range goes; the range's keys and its record go together; with `--root` the old generation's log directory goes too, because a segment left behind would be replayed into a range with no record — a table nobody can open. A second run answers the same way as a finished first one. This vacuum is the store's; `okm-lake vacuum` reclaims lake files behind retired versions, which is a different question.

## What this document leaves out

The key's payload segments and their order ([key layout](key-layout.md)); how entries are built from them ([index mechanism](index-mechanism.md)); the dynamic segment's block table ([dynamic segment](dynamic-segment.md)); accumulators ([reduce mechanism](reduce-mechanism.md)); the slot band's full table ([slot mechanism](slot-mechanism.md)).