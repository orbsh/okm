# OKM — Implementation Plan

Design decisions live in `docs/adr/`. This plan tracks implementation status.

## Phase 1 — Core key/edge layer ✅ shipped

- [x] `KeyEncode` derive: fixed-width fields (`u32` / `u64` / `[u8; N]`), BE,
      compile-time `KEY_LEN` / `FIELD_WIDTHS`, `encode_prefix_named`.
- [x] `EdgeEncode` derive: `#[kv_head(field, …)]` per-endpoint identity width,
      2-byte direction-bit header (niche, ADR-0001), query methods on endpoint
      types.
- [x] `Collection<S, E>` assembly point, no `KvRecord` (ADR-0003).
- [x] Engines: `MockStore` (default), `fjall` (sync, feature), `slatedb`
      (async, feature).
- [x] Hex layout-stability tests (`tests/integration.rs`).

## Phase 2 — Value side (ADR-0004) — design locked, not implemented

- [ ] `ValueEncode` derive: `#[kv_version(n)]` versioned payload, lazy
      in-memory upgrade on decode.
- [ ] TLV extension section: fixed-width hot section + tagged extensions
      (`id: u8, len: u16, bytes`), old readers skip whole extension block.
- [ ] Field wrappers: `Enum<T>`, `Offset<T>`, `Delta<T>`, `VarInt<T>`,
      `Rle<T>`, `Quant<T>`, `Reverse<T>` (+ `Reversible` compile-time
      whitelist, floats excluded).
- [ ] Hot/cold promotion procedure (extension field → hot section tail,
      version bump, hex-test guarded).

## Phase 3 — Secondary indexes (ADR-0005) — design locked, not implemented

- [ ] `#[kv_index(name { fields(…) })]` attribute on table structs.
- [ ] Item-local slot counter; ns = `table_ns` + slot (mechanical derivation).
- [ ] 1-byte slot discriminator: header `[table_ns 2B][slot 1B]`.
- [ ] Per-index generated `IndexEncode` struct; index key = header + indexed
      fields BE + primary key ID; value empty.
- [ ] Leftmost-prefix scan API on index structs; whole-table segment scan.
- [ ] Slot holes never reused; hex tests lock index layouts.
- [ ] Variable-length field support follows the secondary-index regime
      (text-first, ID at tail).

### Header unification trigger (edge direction bit)

Current state: edge headers are 2B (direction bit niched into the ns field,
ADR-0001) while index headers are 3B (`[ns 2B][slot 1B]`). Merging dir into a
shared `[ns 2B full 16-bit][flags 1B]` header is deliberately NOT done now:
edges are the most numerous keys and the niche bit is free (ns is declared
u16, effectively 32768 = structurally inexhaustible), so unification would
cost every edge key 1 byte to buy unused ns capacity.

Trigger: when the edge side needs a second discriminator of its own (edge
grouping, edge versioning, …), the edge header grows to 3B anyway — at that
point unify all key types into `[ns 2B][flags 1B]` (dir at bit0, ns restored
to full 16 bits). The trigger is "edge needs a new discriminator", never
"ns capacity pressure".

## Phase 4 — Packaging

- [ ] Publish to crates.io (`okm`, `okm-derive`).
- [ ] Push to github.com/orbsh/okm (repo referenced by wiki cross-links).
