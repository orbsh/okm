//! Two kinds of generated name, two rules (ADR-0053).
//!
//! **Item names stay call-site and carry the `__Okm` prefix.** The caller has
//! to be able to write them — `scan::<__OkmIndex_Row_by_tag>`, or a probe
//! builder's type in a signature — so they must be guessable from the
//! declaration, and the prefix is what keeps a generated type out of an
//! ordinary name's way. Hygiene does not reach item names anyway (`mixed_site`
//! covers locals, labels and `$crate`; `def_site`, which would cover items too,
//! is unstable). A collision here is a duplicate definition — loud, at compile
//! time, never a silent wrong value — which is why a prefix is the right tool
//! and not a weaker one.
//!
//! **Local names inside generated bodies carry `Span::mixed_site()`.** That is
//! the `macro_rules!` rule: the name resolves at the macro's definition site,
//! while every identifier interpolated from the input keeps its own call-site
//! span. So a payload field called `b` (whose ident comes from the user's
//! source) can no longer shadow the decoder's `b`, and the decoder keeps a
//! readable name instead of a prefix. This is a structural guarantee, not a
//! small probability: the two `b`s are different names, whatever they print as.
//!
//! The rule to maintain: **a generated scope that binds an input-derived ident
//! as a LOCAL must emit every one of its own locals at `mixed_site`.** Today
//! those scopes are the decoders — `Document::decode_payload`,
//! `KvIndex::decode_covered`, `KeyEncode::decode` — because each binds one
//! local per declared field. The encoder walks and the map bridge bind no
//! input-derived local, so they stay call-site; widen this if a future verb
//! starts binding one.

use proc_macro2::{Span, TokenStream, TokenTree};

/// The span for a generated local: definition-site for locals, call-site for
/// everything else (types, macros, paths) — `macro_rules!` hygiene.
pub(crate) fn mixed() -> Span {
    Span::mixed_site()
}

/// `mixed()` as an identifier, for a generated local that must also be
/// *written* by name (a function parameter, say).
pub(crate) fn ident(name: &str) -> proc_macro2::Ident {
    proc_macro2::Ident::new(name, mixed())
}

/// Re-span a fragment so every token in it is a definition-site name.
///
/// For fragments that interpolate nothing from the input (the per-type `dec`
/// blocks), this beats wrapping each of the ~30 declarations in
/// `quote_spanned!`. Do NOT apply it to a template that interpolates a field
/// ident: the walk cannot tell an interpolated token from a generated one, and
/// re-spanning a field ident would split it from the call-site binding
/// `Self { field }` reads.
pub(crate) fn mix(tokens: TokenStream) -> TokenStream {
    let mut out = TokenStream::new();
    for tt in tokens {
        let moved = match tt {
            TokenTree::Group(g) => {
                let mut ng = proc_macro2::Group::new(g.delimiter(), mix(g.stream()));
                ng.set_span(mixed());
                TokenTree::Group(ng)
            }
            TokenTree::Ident(mut i) => {
                i.set_span(mixed());
                TokenTree::Ident(i)
            }
            TokenTree::Punct(mut p) => {
                p.set_span(mixed());
                TokenTree::Punct(p)
            }
            TokenTree::Literal(mut l) => {
                l.set_span(mixed());
                TokenTree::Literal(l)
            }
        };
        out.extend(std::iter::once(moved));
    }
    out
}
