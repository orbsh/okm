# ADR-0077: The graph faces stay in core, because the boundary is worth 3 KB

**Status**: decided 2026-11-21. No code follows except one repair of where two modules read the slot band from: the plan and the change log asked `model::hyper`, which only re-exports the band that `model::layout` defines, and now ask `model::layout`. No PLAN phase — there is nothing to implement.

Edges are one of okm's three primitives, and most applications declare none. That asymmetry is real: the graph half of `model/` is 8,456 of 21,235 lines, and a store that never writes an edge never touches a line of it. The question this ADR answers is therefore not "is the graph code big" — it is "does an application that ignores it pay for it, and if so, how much".

## Context

What the half is: the typed graph (`model/graph.rs`, 2,304), the hyperedge/cluster layer (`model/hyper.rs`, 3,074), and the two schema-free forms (`model/dynamic_graph.rs` 1,094, `model/dynamic_cluster.rs` 1,984).

Who references it, outside those five files: `okm-lake` (8 references — its `EdgeTable`/`EdgeRestore` implementations for snapshot and restore), and each binding (a full graph surface in Python and in steel). `okm-query`, `okm-ngram`, `okm-vector`, `okm-stream`, `okm-series`, `okm-log`, `okm-async` and `okm-wire` reference it zero times.

Three levers were on the table: extract the faces into a crate of their own, gate them behind a feature, or leave them where they are.

## The measurement

Clean target directories, two runs each, same commit; the consumer declares one document type, writes one row and reads it back, and declares no edges.

| | compile, release | rlib | linked app, ephemeral | linked app, fjall |
|---|---|---|---|---|
| faces in core | 5.42 s | 7.82 MB | 722,152 B | 2,414,144 B |
| faces gated out | 4.89 s | 5.17 MB | 719,416 B | 2,411,168 B |
| delta | −0.53 s (−10 %) | −2.65 MB (−34 %) | −2,736 B (−0.38 %) | −2,976 B (−0.12 %) |

Debug builds move the same way: 4.23 s → 3.89 s, 11.43 MB → 9.50 MB.

## Decision

**1. The faces stay in core. No crate, no feature.**

**2. The rlib number is the one that misleads.** An rlib holds every compiled item; the linker takes only what is referenced. The 2.65 MB never reaches a binary — what an application actually pays is 3 KB, and with a real engine linked, one tenth of one percent. The cost that felt large was the cost of compiling, not of carrying.

**3. A crate boundary would charge the users who declare edges.** The derive emits `::okm_core::NodeRef` and `::okm_core::EdgeKey` inside every `#[ok_nodes]`/`#[ok_edges]` expansion, so moving the faces out needs a serde-style `crate = "::okm_graph"` path on every graph declaration — a per-declaration tax on the people who use the feature, to save 3 KB for the people who do not. The same move also separates the band table (`model::layout`, where the slots are defined) from the key construction that reads it, which is the seam ADR-0076 spent a round closing.

**4. A feature gate buys the same 3 KB and costs cfg points.** It needs nine `#[cfg]` attributes (five modules, three re-export blocks, `CollectionSchema::of_graph`), `required-features` on four test targets, and two modules rerouted to read the band from `layout` — and cargo unifies features, so any binary that links the lake or a binding gets the code anyway. It is a real lever only for a consumer that depends on `okm-core` alone, and for that consumer the saving is 0.53 s of one compile.

**5. The bindings are not split either.** They are divided by language, not by capability. A graph/non-graph pair of bindings doubles the packaging matrix to save the same 3 KB, and the graph surface is the smaller half of each binding.

**6. The scale argument is decisive.** An application that links fjall is 2.4 MB; the graph faces are three kilobytes of it. Optimizing noise beside a storage engine is not optimization.

## Rejected

- **An `okm-graph` crate holding the faces.** `okm-graph` already exists and is something else: 149 lines of algorithms (label propagation, PageRank) over neighbor scans, with no callers inside the workspace. Moving storage into a crate named after algorithms would merge two different things, and the derive-path cost above applies either way.
- **A `graph` feature in core.** Buys 3 KB and 0.5 s for twelve cfg points and a unification caveat.
- **Splitting each binding into graph and non-graph builds.** Doubles the packaging matrix for no artifact gain.

## What would reopen this

- The graph growing an algorithm surface that is not traversal. Algorithms have a home already (`okm-graph`); the faces are storage, and today they carry no computation worth extracting.
- A consumer that cannot name `::okm_core::…` at all — a no-derive consumer generating declarations by hand would make the derive path a real constraint rather than a tax, and that is the one premise here that is structural rather than priced.
- A deployment class where 0.5 s of core compile or 3 KB of binary is measurable next to the engine. None exists today; the numbers above are the floor any future proposal has to beat.

## Consequences

- Core keeps 8,456 lines it alone compiles, and a store that declares no edges cannot tell they are there.
- One home for the band table (`model::layout`) and for key construction (`model::graph`, `model::hyper`), which is what ADR-0076 established; the plan and the log now read the band from the former instead of through the latter.
- The measurement is recorded, so this question is not re-opened from intuition: the next proposal must bring a number larger than 3 KB.