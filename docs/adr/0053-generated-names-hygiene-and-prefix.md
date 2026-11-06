# ADR-0053: Generated names — hygiene for locals, a prefix for items

Date: 2026-11-06
Status: Accepted

## Context

The derives emit code into the caller's crate: a `Document` impl, index marker types, prefix builders, and the bodies of the codecs. Those bodies need names of their own — a byte slice, a cursor, a length, a temporary — and the caller's declaration supplies names into the same scopes: one local per declared field, bound by `let <field> = …` and read back by the struct literal's shorthand (`Self { field }`).

Identifiers created by a proc macro carry `Span::call_site()` unless the macro asks otherwise — `quote!` documents exactly that — so they resolve as if the caller had typed them. A payload field named `b` and the decoder's byte-slice parameter `b` are therefore one name. Observed before this decision: `DocumentEncode` on `struct S { b: u64, … }` fails to compile with `E0599: no method named 'len' found for type 'u64'`, reported at the derive — the field's `let b = …` shadowed the slice the rest of the body reads. `offset` and `hot_end` fail the same way (type mismatch); a field named `ver` compiles and reads back correctly today only because nothing reads `ver` after the shadowing point, which a later edit would silently turn wrong.

The same class existed in `KeyEncode::decode`, which binds one local per key field in the scope that also holds a `b` parameter and an `offset` cursor.

## Decision

**Two kinds of generated name, two rules.**

**1. Local names inside generated bodies carry `Span::mixed_site()`** — the `macro_rules!` rule: local variables and labels resolve at the macro's definition site, while everything else (types, macros, paths) still resolves at the call site, so a generated body keeps naming the caller's types.

- The name does not change: the decoder's locals stay `b`, `offset`, `hot_end`, `v`, `len`. Readability and `cargo expand` output do not degrade.
- A name's identity is `(symbol, syntax context)`, so the generated `b` and a field's `b` are two names however they print. That is a structural guarantee: no probability, no reserved namespace, no prefix to remember.
- Interpolated tokens — a field's ident, its type, its declared default — keep their own spans and therefore stay call-site. That half is required, because the binding must still be readable by `Self { field }` and by `self.field`.

The rule that must be maintained: **a generated scope that binds an input-derived ident as a local must emit every one of its own locals at `mixed_site`.** Today those scopes are the three decoders — `Document::decode_payload`, `KvIndex::decode_covered`, `KeyEncode::decode`. The encoder walks, the map bridge and the probe builders bind no input-derived local, so they stay call-site. The mechanism lives in `okm-derive/src/hyg.rs` (`mixed()`, `ident()`, `mix()`); the per-type `dec` blocks are re-spanned once where the field schema is built, not at each of the ~30 declarations.

**2. Item names stay call-site and keep the `__Okm` prefix** — `__OkmIndex_{Row}_{index}`, `__OkmIndexPrefixBuilder_*`, `__OkmPrefixBuilder_*`, `__OkmCovered_*`. The caller has to be able to name them (`scan::<__OkmIndex_Row_by_tag>(&probe)`, a probe type in a signature, a covered struct's fields), so they must be derivable from the declaration, and the prefix is what keeps a generated type out of an ordinary name's way. Hygiene is not available for them in any case: `mixed_site` covers locals, labels and `$crate`, and `def_site` — the span that would make item names definition-site too — is unstable (`proc_macro_def_site`; semver-exempt in `proc-macro2`).

## Rejected: a prefix, or a random suffix, on the locals

- **`__okm_b`-style prefixes on the locals.** Same trade as any reserved namespace: it makes the collision unlikely instead of impossible, and it moves an internal name into the caller's visible surface where it can be matched by accident (`__okm_` is not syntactically protected — a caller may declare that field). It also has to be kept by convention at every new local, and it spends the decoder's readability for nothing. The prefix is the right tool for the item names, where it is load-bearing (the caller must be able to guess the name) and where the failure mode is a duplicate definition — loud, at compile time.
- **A random suffix per expansion.** Makes the collision astronomically unlikely and still a probability, and it costs determinism: the same source produces different token streams on every build, so `cargo expand` diffs become noise, incremental reuse of that module is invalidated, and diagnostics start naming `b_9f3a2c`. Deriving the suffix from the input (a hash of the struct name) recovers determinism but leaves it a bet that buys nothing over a fixed prefix — and it does not apply to item names, which the caller must be able to write.
- **`Span::def_site()` for the whole body.** Strictly more thorough — item names would be hygienic too — but unstable in `proc_macro` and therefore unreachable on stable.

## Consequences

- A field, key segment or `includes` entry may carry any name, including every name the derives use internally. `okm-core/tests/generated_name_hygiene_test.rs` locks it: the declaration there is named after the decoders' own locals, so losing the property is a compile error in that file rather than a latent bug in a user's struct.
- The property cannot be locked by an expansion snapshot: `proc-macro2`'s fallback defines `mixed_site()` as `call_site()` and its fallback `Span` carries no context, so the difference exists only inside rustc. The lock is an integration test that has to compile.
- Diagnostics stay pointed at the invocation (that is where a `mixed_site` span is located), and the names they print are the readable ones — the reason not to mangle.
- Adding a local to a decoder means emitting it through `hyg::mixed()` or inside a `quote_spanned!` body; `hyg.rs` states the rule and why the other scopes are exempt.