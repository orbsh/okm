# Idea: populating a hypergraph without an LLM in the write path

**Status**: idea, recorded 2026-10-24. Not a decision — no ADR, no PLAN item.
Companion to [hypergraph.md](hypergraph.md) (the carrier idea); this file is the
ingestion side, and the two are decidable independently. The proposal in
§1–§3 is an outside document (Chinese, 2026-10-24) reproduced in substance; §4
checks it against what okm actually provides, §5 lists the open questions.

## 1. The claim

Graph construction faces a cost triangle: an LLM reading every document is
accurate but slow and expensive; rule/statistical extraction (co-occurrence,
NER) is fast and free but misses multi-party relations and predicates. The
proposed middle: **local small models for structure, the store's key layout for
constraints** — never let a general LLM read everything; let a tiny local model
propose structure, and let the hypergraph's faces make the merge atomic.

## 2. Four methods as proposed

1. **IE-specific local models.** Not GPT-class: a 0.2B–1.5B model fine-tuned on
   triple/n-tuple extraction (REBEL, UIE), run locally (candle / onnxruntime),
   emitting short sequences instead of long text. Claimed: hundreds to thousands
   of documents per second on CPU or a consumer GPU, zero token cost.
2. **NER + embedding clustering + heuristic binding.** Fast BERT-class NER pulls
   entities; entities and their context become dense vectors; a sliding
   physical/time window plus a vector-distance threshold binds co-occurring
   entities into one hyperedge, written straight into the hypergraph slot. The
   argument: vectors plus a time window are two independent guards, so even
   without knowing the verb, the participants land in one "event cube".
3. **SLM skeleton, symbolic assembly.** Filter sentences that contain key
   entities; ask a 1B–3B model one narrow question ("A and B and C — what is the
   verb?") and take one word out; the Rust core assembles the hyperedge. The
   claimed anti-pattern is asking a large model for a JSON hypergraph.
4. **Feedback fine-tuning ("SONA").** Start from cheap extraction that is ~70%
   right; when an agent's retrieval is marked wrong (user downvote, agent
   self-detected contradiction), fine-tune path weights locally (Micro-LoRA) and
   prune bad links, so the graph converges over days of use.

The same source then splits the four by neural/symbolic share and gives figures
(recorded as claimed, verified in §4):

| method | neural role | symbolic role | hardware | latency claimed | trade-off claimed |
|---|---|---|---|---|---|
| 1. local extraction model (REBEL/UIE) | 100% — end-to-end Transformer emitting structured text | 0% — no graph constraint, trusts the model | mid: one consumer GPU or a strong CPU | 10–50 ms / sentence | most accurate on long-range semantics; can hallucinate unregistered entities |
| 2. embedding clustering + heuristic binding | 30% — small BERT embeds text | 70% — time/space/sliding window binds at the physical layer | very low: plain CPU, claimed 100k+ tokens/s | 1–5 ms | fastest, near-zero cost, claimed "no hallucination"; knows "related", not "how related" |
| 3. SLM skeleton | 80% — 1B–3B model classifies the predicate of one short context | 20% — rules/NER fix the entities first | mid-high: 4–8 GB VRAM to keep the SLM resident | 50–200 ms | precise predicates; highest latency of the four; small hallucination probability |
| 4. feedback fine-tuning ("SONA") | 50% — GNN computes attention path weights online (LoRA) | 50% — graph topology fixed rigidly in the KV layer | very low: Rust core, EWC on one CPU core, claimed sub-0.1 ms | < 0.1 ms at run time | improves with use, tolerates a bad cold start; needs interaction, weak before feedback arrives |

Routing the source recommends:

- **Semi-structured logs / flows / tables** (finance, auth): method 2 — the data
  already carries timestamps and device ids, so vector similarity plus a window
  is the cheapest deterministic binding.
- **Free-form text** (chat, reports, fiction): method 3 (or 1) as the front-end
  extractor, method 4 as the back-end corrector — language needs contextual
  understanding to find the participants and the verb, and the store keeps the
  result rigid.
- Overall: neural for the fuzzy semantic step, symbolic (typed KV, fixed-width
  keys, faces) for storage determinism and traversal speed.

## 3. Pipeline as proposed

```text
raw text / stream
  → local NER + embedding            (entities, context vectors)
  → time-window + semantic clustering (bind co-occurring members into a group)
  → local 1.5B SLM                   (one predicate per group)
  → write into OKM / "RuVector"      ([graph ns][face][NodeRef][edge id])
  → agent/user feedback              (online weight update, prune wrong links)
```

## 4. Checked against the code (2026-10-24)

What okm already offers the pipeline, and what it cannot own:

- **Embeddings have a home.** `okm-vector` is a real crate: `Vector<f32>` as a
  single-value function index, `ExactIndex` for exact, `Hnsw` over quantized
  codes built from the corpus or from a snapshot blob at fixed face `0x0B`
  (`okm-vector/src/lib.rs:1-40`, `okm-vector/src/index.rs:26-32`). It
  deliberately does no permission filtering, no per-row writes and no rerank —
  the caller drives `add`/`remove`. So "cluster by vector distance" is a caller
  loop over this index, not a new storage mechanism.
- **The predicate slot the SLM feeds already exists.** Graph predicates are
  runtime data allocated through the predicate vocabulary at faces `0x04`/`0x05`
  (`okm-core/src/model/graph.rs:26, 274-340`). "SLM emits one verb" maps onto
  `link`'s predicate resolution with no new surface — the verb becomes a
  fixed-width vocab id in every key, exactly as the layout requires.
- **The write shape in the diagram is today's edge face.** `[ns][face][NodeRef]
  [edge id]` is the 0x07/0x08 out/in face as implemented
  (`okm-core/src/model/graph.rs:28-29`), and the same shape as the membership
  face proposed in [hypergraph.md](hypergraph.md) §2.
- **Time/space windows are not a store primitive.** Nothing in the keyspace is
  time-ordered unless a payload field declares an index; a sliding window is a
  consumer computation. `okm-stream`'s subscribe/reduce path (ADR-0008) is the
  natural place for it, not a new slot family.
- **"The database gets smarter" is not a storage property.** okm holds bytes and
  encodes keys; there is no compute engine, no weights, no learner, and no
  mutable model state. Micro-LoRA training and GNN path weights would live
  outside the store (the agent runtime / effector side); what okm can hold is
  the *outcome* — a confidence field on the edge payload, an edge's existence
  or deletion, and the feedback event that drove it. The "100 µs" and "1 ms"
  figures are unverified claims from the source, not measurements.
- **Nothing exists for the claimed sub-0.1 ms GNN update to attach to.**
  `okm-vector`'s face `0x0B` holds a built HNSW blob that is reproducible from
  `(seed, insertion order)` — a cache, not trained weights
  (`okm-vector/src/lib.rs:1-40`). No okm API maps a feedback signal to a weight
  change, and no continual-learning method (EWC or otherwise) appears anywhere
  in the workspace.
- **The symbolic share is understated, and it is never zero.** Even method 1,
  described as "0% symbolic", writes through symbolic gates: `check_members`
  refuses any ref whose ns is outside the declared node set
  (`okm-core/src/model/graph.rs:322-333`), and a predicate must resolve to a
  vocabulary id. The split is a property of the *pipeline*; the store's own
  contribution is entirely symbolic.
- **"No hallucination" (method 2) means deterministic, not correct.** A
  threshold binding is a decision: a wrong entity resolution, or a coincidental
  window, writes a wrong hyperedge with no marker distinguishing it from a
  right one. Method 1's hallucination is at least visible as an unregistered
  entity; method 2's failure is silent.
- **Vocabulary growth is a structural risk for an automatic labeller.** The
  predicate vocabulary is append-only with first-seen-claims-next numbering and
  no removal API (`okm-core/src/model/obj_vocab.rs:23-28`; `graph.rs` exposes
  `predicates`, `predicate_vocab_name`, no delete). A model that invents a
  synonym per document grows the vocabulary permanently — an ingestion
  pipeline needs a canonicalization step before `link`, not after.
- **The reified-node alternative is unchanged.** Everything here also works
  against today's binary edge plus an event document; the pipeline does not
  itself require the hyperedge carrier.

## 5. Open questions

1. **Entity resolution comes before binding.** Two mentions of "Alice" must be
   one `NodeRef` before they can share a face; nothing in the proposal handles
   canonicalization, and a wrong resolution writes a wrong hyperedge with no
   marker that it is wrong.
2. **Who owns the thresholds.** Window width, vector distance, minimum
   co-occurrence: parameters of a consumer loop, so they live in the pipeline's
   config, not in the store — but then the "constraint" that keeps the graph
   clean is outside the thing the ADRs govern.
3. **Feedback: mutate or supersede?** A downvote could delete the edge (losing
   the record that it was once believed), lower a confidence field (needs a
   declared index to query), or write a contradicting edge (parallel edges
   already exist). The choice decides whether the graph keeps provenance.
4. **Vocabulary discipline under a noisy labeller.** Canonicalize to a closed
   predicate set, or accept unbounded append-only growth (see §4); `Vocab<u16>`
   as a payload field is the existing middle ground.
5. **Where does predicate classification belong in the batch?** Predicate
   allocation commits on its own, even inside a failed outer batch
   (`Graph::link_into` doc, `okm-core/src/model/graph.rs:382-395`); a pipeline
   retrying a failed write can therefore leave orphan predicate entries. That is
   documented as harmless for hand-written code, and worth re-checking at
   machine ingestion rates.
6. **Is a graph the right output at all for the log/stream case?** For
   timestamped, device-tagged system logs, the participants are usually already
   structured; the extraction stack may be answering a problem the data does not
   have.
7. **The resource choice the source poses last** (CPU-only throughput, method
   2 + 4, versus a few GB of VRAM for text precision, method 3 + 4) is a
   product decision, not a storage one: none of the four variants changes a key,
   a face, or a declaration. Method 4 is not available at any resource level
   inside okm (see §4), so the real pair of options is 2 or 3, with the
   feedback loop living in the consumer.