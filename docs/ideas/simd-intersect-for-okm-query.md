# Idea: a SIMD intersection operator for okm-query — evaluated, not adopted

**Status**: evaluated 2026-11-10, **not adopted**. An outside proposal (AI-written, 2026-11-10) contributed a `simd_intersect_u64` function; it was built, checked against a brute-force oracle, and measured against the two-pointer it was meant to replace. It is correct, it does not compile on this repo's toolchain, and it is slower everywhere it was tried. What follows is the measurement, the three premises that do not hold, and the one optimization that did win on the shape where winning was possible (skewed sizes: **galloping**, 12.7× faster than the merge).

The proposal's *motivation* is not the problem. okm-query's operators are eager, clone per result pair, and have no lazily-composed form; those are real gaps, listed in §6, and they are where the work belongs.

## 1. What was proposed

A function taking two ascending `&[u64]` slices, loading four elements into a 256-bit register, and either skipping a whole block (when its maximum is below the other side's current minimum) or comparing one side's block against four broadcasts of the other's elements, then advancing one or both blocks by four. Rationale given: okm's secondary-index scans are ordered, so an intersection of two scans is a merge over sorted arrays, and portable SIMD should beat a branchy two-pointer.

## 2. Three premises that do not hold

**2.1 okm's streams carry bytes, not `&[u64]`.** Primary keys are encoded big-endian — `okm-derive/src/key_encode.rs` emits `to_be_bytes` for every integer width. Reinterpreting those bytes as `u64` on a little-endian host inverts the value, so ordering (and therefore a merge) is silently wrong. A real implementation has to byteswap; on x86 that is one instruction per element (`movbe` / `bswap`), which is also the moment the "zero-copy" claim dies.

**2.2 The tails are not contiguous.** A scan yields full keys: `<ns header><face slot><indexed field bytes><primary-key tail>`. Two consecutive tails are separated by a whole prefix plus the indexed fields, so there is no `&[u64]` in memory to point at. A lane-wise load needs a gather (slow, and not available in safe portable SIMD) or a copy to a side buffer.

**2.3 Worse: the copy already happened before the operator is reached.** `ScanIter::Item` is `Result<(Vec<u8>, Vec<u8>)>` — `okm-core/src/engine/storage.rs` builds each key with `k.to_vec()`. By the time okm-query sees a key, it is one allocation per entry, borrowed from nothing. "Straight from the page cache into a register" is not a description of this stack.

**2.4 `core::simd` is not in this toolchain.** On `rustc 1.99.1` (stable):

    error[E0554]: `#![feature]` may not be used on the stable release channel

The proposal's own import is stale as well: `SimdPartialEq` lives in `std::simd::cmp` now, not `std::simd`. Shipping it would mean nightly, or `std::arch` intrinsics behind runtime dispatch with a scalar fallback.

**2.5 The SIMD shape is inverted.** The inner step does four broadcasts, four compares and four mask reductions to replace four scalar compares — more work, not less. Proper vectorized intersection compares 4×4 in four permuted compares. "One clock cycle for four masks" is an order of magnitude off in either shape.

## 3. Correctness

400 random shapes (lengths 1–37 against 1–23, different strides, random gaps) compared against a brute-force two-pointer: **0 mismatches**. The algorithm is right. This document is not a bug report.

## 4. Speed

Machine: Ryzen AI 7 H 350 (Zen 5, AVX-512 available), release + thin LTO, `-C target-cpu=native`, pinned with `taskset -c 3`, five implementations interleaved over 21 rounds, **median** in milliseconds. `portable_simd` needed `RUSTC_BOOTSTRAP=1` to compile at all (see §2.4).

| case | two-pointer | proposal's SIMD | 4×4 rotate kernel | gallop |
|---|---|---|---|---|
| 1K vs 1K, 50 % overlap | 0.0013 | 0.0015 | 0.0021 | 0.0123 |
| 10K vs 10K | 0.0088 | 0.0147 | 0.0178 | 0.194 |
| 100K vs 100K | 0.0874 | 0.1478 | 0.1768 | 2.63 |
| 1M vs 1M, 50 % overlap | 0.881 | 1.515 | 1.784 | 33.4 |
| 1M vs 1M, disjoint | 1.193 | 1.205 | 1.179 | 22.1 |
| 1M vs 1K, skewed | 0.600 | 0.682 | 0.810 | **0.047** |

Read it as: the proposal's function is never faster and usually 15–70 % slower; the only tie is the disjoint case, where both sides simply stream their array once. A correctly shaped 4×4 kernel (four permuted compares per block, the shape the literature optimizes) does no better here — it ties at 1M disjoint and loses everywhere else.

Why: a merge over two sorted arrays is **memory-bandwidth bound with perfect branch prediction**. 1M + 1M dense in 0.88 ms is ~18 GB/s of key traffic on this machine — already at DRAM speed. SIMD set intersection wins in published work because the scalar comparator there branches unpredictably on 32-bit ints; it has nothing to recover here.

The one real win is skew, and it is not SIMD: galloping — binary-searching the large side once per element of the small one — is **12.7× faster than the merge** (0.047 ms vs 0.600 ms) and 14.5× faster than the proposal's block skip. The proposal advertises the skewed case; its skip is a fixed stride of four, not an exponential search, so it does not get it.

## 5. The `&[u64]` contract question

"Make the contract `&[u64]`" can mean three very different things, and only the weakest of them is cheap.

**Shape 1 — the operator's input is `&[u64]`.** The caller materializes two `Vec<u64>` from scan tails (8 bytes per element, one allocation per side, one byteswap per element) and passes slices. Cost: an extra pass and an extra copy that the byte-level merge never needed. Benefit: none, per §4 — that is exactly the configuration measured above, and it loses. This shape is architecturally harmless; it is simply not worth building.

**Shape 2 — the scan contract hands out numeric tails.** For this to be more than Shape 1 with extra steps, three things must be true at once, and two of them are not:

- the tail must be a fixed-width numeric field. okm's primary key may be composite, a string, a UUID or a `VarInt`; a `&[u64]` view is only definable for a subset of declarations, so the trait would need a second, optional method (`fn u64_tails(..) -> Option<..>`) — a capability the caller must negotiate and every backend must answer.
- the tails must be contiguous, and they are not (§2.2): the stride is the prefix plus the indexed fields. A gather is the only way to read them without a copy, and a gather is slower than the merge it would replace.
- the iterator must lend memory rather than own it. `ScanIter` currently allocates per key (§2.3), and two of its arms structurally cannot lend: `Remote` streams pages over a wire, `External` holds an engine from outside this crate (ADR-0059). A lending variant would work for two of five arms and need a fallback everywhere else.

**Shape 3 — change the storage layout so index entries hold host-order `u64` tails.** This is the only way to get a contiguous, castable array without a copy, and it breaks the invariant the whole model rests on: **big-endian encoding is what makes byte order equal numeric order** for every type in the same key. A host-order `u64` segment would sort wrongly against its neighbours, so scans would return wrong ranges — not slower, wrong. The variant that keeps the layout (store big-endian, byteswap four lanes with `vpshufb` on load) is Shape 1 again: the copy comes first.

So the honest answer to "what would it take to make it `&[u64]`" is: either one copy per side before the merge (Shape 1 — measured, loses), or an optional capability in the storage trait plus a lending iterator that the remote and external arms can never honour (Shape 2), or abandoning order-preserving bytes for the whole model (Shape 3). The `u64` specialization does not remove any work; it only removes a generic parameter from a loop that is already at memory speed.

## 6. What is actually wrong with okm-query today

Measured gaps, in the order they hurt, all inside the current sync contract:

1. ~~**Eager to the end.**~~ Fixed for the join path: `intersect`/`difference` are lazy adapters, and `scan_entries` feeds them without reading payloads (`set-algebra-join.md`). `merge_join` is still eager — deliberately, it is the multiplicity step now. `group_by` still folds into a `Vec`.
2. ~~**Clone per result pair.**~~ The key-level operators need no `Clone` at all (the key is moved out of the peeked stream). `merge_join` still clones, which is inherent to answering m×n pairs.
3. **No lazy adapters.** Still true for `group_by`: it folds into a `Vec`, so a filtered, grouped, limited pipeline allocates intermediates nobody reads.
4. ~~**No `intersect`/`union`.**~~ `intersect` and `difference` exist and are set-semantic, ordered, lazy (`set-algebra-join.md`). `union` still does not — `chain` plus a dedup pass is the shape, and nothing asks for it yet.

Galloping (§4) is still unbuilt: it belongs to `intersect` now, behind the ordered contract rather than a sort.

## 7. If this is revisited

All four, with the measurements in hand:

1. the query layer returns iterators, so the operator has streams to consume rather than slices it asks the caller to build;
2. a measured workload where 100K+ keys per side are intersected per query (okm's prefix scans are usually hundreds to tens of thousands — at 10K the merge takes 9 µs);
3. a stable implementation route (`std::arch` with runtime dispatch, not `portable_simd`);
4. the byte-to-`u64` crossing already paid somewhere else, so the operator is not the thing paying it.

## 8. Reproducing

`/home/master/.hermes/cache/scratch/simdjoin` (not in the repo — a scale, not a fixture):

    RUSTC_BOOTSTRAP=1 RUSTFLAGS='-C target-cpu=native' cargo build --release
    taskset -c 3 ./target/release/simdjoin

It holds the proposal's function verbatim (with the import corrected for this toolchain), a plain two-pointer, a scalar block merge, a 4×4 rotate kernel, and galloping search; it asserts all five against the two-pointer on 400 random shapes before timing them.