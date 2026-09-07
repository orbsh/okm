# ADR-0002: Namespace dictionary lives in code, never in KV

Date: 2026-09-06
Status: Accepted

## Context

String prefixes (`"user_sessions:"`, 14 bytes) waste space at scale and force variable-length offset arithmetic on every decode. The fix is a numeric namespace ID: `#[kv_ns(N)]` folds at compile time into a 2-byte big-endian prefix — an 85% compression with zero runtime lookup (an instruction immediate; faster than any L1-resident HashMap: no hash, no load).

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

## Terminology and multi-tenancy

Secondary indexes do NOT consume namespace IDs — they are slots derived inside the table item (see [ADR-0005](0005-secondary-index-slots.md)); the manual numbering below applies to tables only.

- **namespace = table/collection (KV's native tongue)**: not "container/scope" — the standard KV/noSQL synonym for table/collection (Cassandra's keyspace likewise). It is a key-prefix discriminator only; there is no logical "table" with schema constraints or columns.
- **Multi-tenant key shape**: if tenants exist, the key is `[ns][tenant_id]...` — the namespace is outermost, followed by tenant_id. There is no `tenant_ns_id` layer (this system exposes no user-programmable query/schema surface; multi-tenancy is carried by the API gateway, tenants share tables), and no outermost tenant isolation layer — tenant_id is one discriminating field inside the key, not a partition/table boundary. Even for SaaS, per-tenant outer isolation is over-isolation. SQL inside a program is equally hard-coded; ad-hoc dynamism comes from exposing a query interface, not from the storage being dynamic.
