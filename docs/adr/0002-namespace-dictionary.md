# ADR-0002: Namespace dictionary lives in code, never in KV

Date: 2026-09-06
Status: Accepted

## Context

String prefixes (`"user_sessions:"`, 14 bytes) have two structural weaknesses at scale (hundreds of millions of keys):

- **Space waste** — every key repeats the same prefix; at hundreds of millions of keys that is gigabytes of pure repetition, paid again as S3 transfer bandwidth.
- **Variable-length offsets** — key total length varies per record, so every decode pays variable-length offset arithmetic instead of a fixed offset.

The fix is a numeric namespace ID: `#[kv_ns(N)]` folds at compile time into a 2-byte big-endian prefix — an 85% compression with zero runtime lookup (an instruction immediate; faster than any L1-resident HashMap: no hash, no load).

The question is where the namespace dictionary itself lives.

## Rejected alternatives

- **Namespace registry in KV, read by the proc macro** — a bootstrap deadlock: macros run at compile time; the KV engine does not yet exist and has not been seeded by this very source.
- **Even if compile-time KV reads were possible** — it buys nothing: a separate program must maintain the metadata; two sources of truth (KV metadata + source) drift; compilation becomes opaque (depends on a running service); state must be synchronized. All burden, no benefit.
- **Runtime "open" registration** — meaningless: the access pattern already lives in code (binary keys, no separators, per-field widths); adding a namespace still requires a code change, so dynamic registration buys no dynamic access.

## Numbering discipline

- **The nsid is a discriminator, not a sort key** — it is the outermost distinguishing prefix; keys within one namespace already cluster for scans because they share the prefix bytes. Adjacent numbers carry no semantic value, so allocation is append-only (1, 2, 3, …), unique, never changed.
- **Not declaration-order auto-increment**: `#[derive]` processes items in source order; a global counter would renumber later insertions (inserting `C` between `A` and `B` shifts `B`). Any rank-based numbering (declaration order, alphabetical, any total order) is unstable — and the nsid is physically baked into on-disk keys; renumbering orphans all old data.
- **Not name hashing**: hash is order-independent, but it loses on the minimal-width goal — hash output spans the full value space (u32 = 4 bytes, double a fixed u16); truncating to 8 bits invites birthday collisions (≈ N²/512 expected pairs; N=50 → ~4.9 collisions) and requires a compile-time dedup+perturbation apparatus — which, once it collects all names, could just hand out a counter. Hash only wins when namespaces come from external sources (multi-tenant/plugins/cross-crate) with no central place to renumber; not our case.
- **Not runtime width derivation** (`fetch_max` / `OnceLock`): mechanically sound (fetch_max is commutative and order-independent) but binds key width to the *global* namespace set — adding the 256th namespace silently rewrites the prefix width of *all* pre-existing keys. During rolling upgrades, old and new binaries coexist with different widths and cannot read each other. This is the same disease as insertion-triggered renumbering, relapsed at runtime; ~2.4% byte savings cannot buy it back.
- **Width locked at u16 (2 bytes)**: supports 32768 namespaces after the direction-bit niche ([ADR-0001](0001-direction-bit-niche.md)); the actual namespace count is tens to low hundreds and grows slowly. 2B vs 1B costs ~1 byte per key (≈2.4%) and buys a structurally impossible-to-exhaust header — no machine, no maintenance, no cliff. Sub-byte widths (nibbles) would need bit operations and cross-byte offsets, shattering the "pure pointer slicing, zero parsing" fixed-width foundation.

### Why width is not configurable (features vs const generics)

A natural follow-up: make `ns` width selectable at dependency time — `features = [...]` or a const-generic parameter. Both are rejected; the reasons are mechanical, not stylistic.

**Cargo features cannot express a width.** Features are boolean toggles, not parameters — `ns_width = 1` is not a thing features can carry. Supporting three widths means three features (`ns-u8` / `ns-u16` / `ns-u32`), mutual-exclusion constraints between them, and `cfg` forking at every `ns: u16` site — type signatures, derive output, and hex tests all grow cfg arms. The build matrix triples for a knob nobody turns.

**Const generics pay at every call site.** `Table<S, K, R, const NS_W: usize>` threads a width parameter through the `Row` trait, the derive expansion, and every `Table<…>` type annotation. Rust's nominal typing turns "two widths" into "two distinct types": a `Table<S, K, R, 2>` and a `Table<S, K, R, 1>` share no impls, cannot be stored in the same collection, and double monomorphization for code paths that only ever run one width. The compile-time guarantee we'd buy (width mismatch caught in the type system) is already caught cheaper by the hex layout tests — a wrong width fails CI on day one, before any data exists.

**Width uniformity is a format promise, and that is a feature.** The physical key layout is OKM's core contract; the ns width is part of it. Under nominal typing, two crates in one dependency graph selecting different widths simply fail to unify their key types — cargo rejects the build. This looks like a limitation but is exactly the ADR-0002 invariant ("ns numbering globally unique") enforced by the compiler instead of by convention: one process, one width, one layout. A mechanism that *allows* per-dependency widths would weaken a guarantee the architecture depends on.

**And the u16 is not a bottleneck to parameterize around.** 32768 namespaces after the niche (see ADR-0001's trigger discipline) is structurally inexhaustible for this system's scale; the unification trigger there is explicitly not "ns capacity pressure". Adding configurability machinery for a dimension that cannot run out is negative expected value.

## Terminology and multi-tenancy

Secondary indexes do NOT consume namespace IDs — they are slots derived inside the table item (see [ADR-0005](0005-secondary-index-slots.md)); the manual numbering below applies to tables only.

- **namespace = table/collection (KV's native tongue)**: not "container/scope" — the standard KV/noSQL synonym for table/collection (Cassandra's keyspace likewise). It is a key-prefix discriminator only; there is no logical "table" with schema constraints or columns.
- **Multi-tenant key shape**: if tenants exist, the key is `[ns][tenant_id]...` — the namespace is outermost, followed by tenant_id. There is no `tenant_ns_id` layer (this system exposes no user-programmable query/schema surface; multi-tenancy is carried by the API gateway, tenants share tables), and no outermost tenant isolation layer — tenant_id is one discriminating field inside the key, not a partition/table boundary. Even for SaaS, per-tenant outer isolation is over-isolation. SQL inside a program is equally hard-coded; ad-hoc dynamism comes from exposing a query interface, not from the storage being dynamic.

- **No multi-level ns declaration**: `#[kv_ns]` takes exactly one number; a "hierarchical" form (`#[kv_ns(app, ns)]`, path-shaped ns) is rejected. One OKM instance IS one domain model — its ns dictionary is already a closed, append-only vocabulary, and a second level inside it would be a second dictionary to keep globally unique, halving the width budget for a distinction the domain layer should make (app boundaries are platform concerns, not key-layout concerns). Application-internal partitioning is a plain key field (tenant_id); cross-application isolation happens OUTSIDE the ns bytes — the receiver-prefix concatenation of [ADR-0010](0010-virtual-storage-remote-bytes.md), where each hosted instance brings its own complete single-level dictionary. Every layer that wanted "multi-level ns" gets its isolation elsewhere without growing this one.
