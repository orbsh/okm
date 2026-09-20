//! `EdgeEncode` — the fixed-ontology Graph Edge derive (ADR-0017).
//!
//! Declares a graph's edge collection: ns + participating-node registry
//! at the struct level, declared attribute fields on the fields. The
//! endpoints are NOT declared here (open endpoints — they travel as
//! runtime `NodeRef`s):
//!
//! ```ignore
//! #[derive(GraphEdgeEncode)]
//! #[ok_edge(ns = 100, nodes(User = 10, Org = 11))]
//! struct Follows {
//!     weight: u32,   // declared attribute field — one 0x1 face each
//!     since: u64,
//! }
//! ```
//!
//! Generates: the `KvGraph` impl (NS_PREFIX, REGISTRY constants +
//! attribute encoding via `__okm_encode_named`-style per-field arms),
//! and one access-method marker struct per declared attribute field
//! (`__OkmEdgeIndex_<edge>_<field>`), whose 0x1-segment face is
//! `[field value][edge_id]` — scanning it returns edge ids for the
//! `Graph::by_attr_face` walk. Attribute values are edge-own data,
//! fixed width by construction (variable-width attribute fields are
//! rejected: a variable-width face segment cannot delimit edge_id).

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, TokenTree};
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, Data, DeriveInput, Fields};

/// Parse `#[ok_edge(ns = N)]`. Endpoints need NO declaration — refs
/// are self-describing (`[ns 2B][len][pkey]`), so there is no node
/// registry to maintain.
fn parse_edge_attr(attr: &syn::Attribute) -> u16 {
    let ts: Vec<TokenTree> = attr.to_token_stream().into_iter().collect();
    let outer = ts
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => Some(g.stream()),
            _ => None,
        })
        .expect("ok_edge: missing attribute brackets");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
            _ => None,
        })
        .expect("ok_edge: missing argument parentheses");

    let mut ns: Option<u16> = None;
    let toks: Vec<TokenTree> = body.into_iter().collect();
    let mut i = 0;
    while i < toks.len() {
        if matches!(&toks[i], TokenTree::Punct(p) if p.as_char() == ',') {
            i += 1;
            continue;
        }
        let kw = match &toks[i] {
            TokenTree::Ident(id) => id.to_string(),
            t => panic!("ok_edge: expected `ns`, got {t}"),
        };
        if kw != "ns" {
            panic!("ok_edge: unknown key `{kw}` (supported: ns — endpoints are self-describing, no node declaration)");
        }
        assert!(
            matches!(&toks.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '='),
            "ok_edge: expected `ns = <u16 literal>`"
        );
        let lit = match &toks.get(i + 2) {
            Some(TokenTree::Literal(l)) => l.to_string(),
            t => panic!("ok_edge: expected ns literal, got {t:?}"),
        };
        ns = Some(
            lit.parse::<u16>()
                .unwrap_or_else(|e| panic!("ok_edge: bad ns literal `{lit}`: {e}")),
        );
        i += 3;
    }
    ns.expect("ok_edge: missing `ns = N`")
}

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let ns = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_edge"))
        .map(parse_edge_attr)
        .expect("missing #[ok_edge(ns = N)]");

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("GraphEdgeEncode only supports structs with named fields"),
        },
        _ => panic!("GraphEdgeEncode only supports structs"),
    };

    // Attribute fields: fixed-width primitives only (u8/u16/u32/u64,
    // [u8; N]) — the face segment must be fixed-width so edge_id is
    // locatable from the tail. Reuse the same encode arms as the
    // document macro (raw BE, no frames).
    let mut enc_arms = quote! {};
    let mut index_structs = quote! {};
    let mut attr_names: Vec<String> = Vec::new();
    let mut attr_tys: Vec<syn::Type> = Vec::new();
    let mut face_pairs = quote! {};
    for f in named.iter() {
        let id = f.ident.clone().unwrap();
        let fname = id.to_string();
        let ty = &f.ty;
        let ty_str = quote!(#ty).to_string().replace(' ', "");
        let (enc, width): (proc_macro2::TokenStream, proc_macro2::TokenStream) = match ty_str.as_str() {
            "u8" => (quote! { buf.push(self.#id); }, quote! { 1 }),
            "u16" => (quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); }, quote! { 2 }),
            "u32" => (quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); }, quote! { 4 }),
            "u64" => (quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); }, quote! { 8 }),
            _ if ty_str.starts_with("[u8;") => {
                let n: usize = ty_str
                    .trim_start_matches("[u8;")
                    .trim_end_matches(']')
                    .parse()
                    .expect("[u8; N]: N must be an integer literal");
                let nlit = proc_macro2::Literal::usize_unsuffixed(n);
                (quote! { buf.extend_from_slice(&self.#id); }, quote! { #nlit })
            }
            other => panic!(
                "GraphEdgeEncode[{name}].{fname}: unsupported attribute type `{other}` (fixed-width primitives only: u8/u16/u32/u64/[u8; N] — variable-width fields cannot delimit edge_id on the 0x1 face)"
            ),
        };
        enc_arms.extend(quote! {
            #fname => { #enc }
        });
        attr_names.push(fname.clone());
        attr_tys.push(ty.clone());

        // attr_faces entry: (slot, value BE bytes) for this field.
        let slot_lit_pair = 0x1001u16 + attr_names.len() as u16 - 1;
        face_pairs.extend(quote! {
            out.push((#slot_lit_pair, self.#id.to_be_bytes().to_vec()));
        });

        // One 0x1-segment access method per declared attribute field:
        // DECLARED base (0x1001) + declaration index, face
        // `[field value][edge_id u64 BE]`.
        let slot_lit = 0x1001u16 + attr_names.len() as u16 - 1;
        let struct_ident = format_ident!("__OkmEdgeIndex_{}_{}", name, fname);
        let slot_doc = attr_names.len().to_string();
        index_structs.extend(quote! {
            #[doc = concat!("Graph-edge declared-attribute access method `", stringify!(#name), ".", #fname, "` (0x1 segment, slot ", #slot_doc, "; face = [", #fname, "][edge_id], ADR-0017 §3).")]
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy, Debug)]
            pub struct #struct_ident;

            impl #struct_ident {
                /// The 0x1 segment slot this face occupies.
                pub const SLOT: u16 = #slot_lit;
                /// Entry key for one edge fact: `[ns 2B][slot][field BE][edge_id BE]`.
                pub fn entry_key(ns_prefix: &[u8], value: &#ty, edge_id: u64) -> Vec<u8> {
                    let mut k = ns_prefix.to_vec();
                    k.extend_from_slice(&Self::SLOT.to_be_bytes());
                    k.extend_from_slice(&value.to_be_bytes());
                    k.extend_from_slice(&edge_id.to_be_bytes());
                    k
                }
                /// Scan prefix: `[ns 2B][slot][field BE]` — everything
                /// after is edge ids of matching edges.
                pub fn entry_prefix(ns_prefix: &[u8], value: &#ty) -> Vec<u8> {
                    let mut k = ns_prefix.to_vec();
                    k.extend_from_slice(&Self::SLOT.to_be_bytes());
                    k.extend_from_slice(&value.to_be_bytes());
                    k
                }
            }
        });
        let _ = width; // width is implicit in the field's own BE encoding
    }

    let ns_hi = (ns >> 8) as u8;
    let ns_lo = (ns & 0xff) as u8;

    // ATTR_SLOTS constant: one 0x1 slot per declared field, declaration
    // order (mirrors the per-field marker structs' SLOT constants).
    let attr_slots: Vec<_> = (0..attr_names.len())
        .map(|i| {
            let lit = proc_macro2::Literal::u16_unsuffixed(0x1001u16 + i as u16);
            quote! { #lit }
        })
        .collect();

    quote! {
        impl ::okm_core::KvGraph for #name {
            const NS_PREFIX: &'static [u8] = &[#ns_hi, #ns_lo];
            const ATTR_SLOTS: &'static [::okm_core::index::Slot] = &[#(#attr_slots),*];
            fn attrs(&self) -> Vec<u8> {
                // The declared-attribute payload: one __okm_encode_named
                // walk over the fixed attribute encodings. The edge body
                // carries this after [src][dst][kind_id].
                let mut buf = Vec::new();
                let names: &[&str] = &[#(#attr_names),*];
                for n in names {
                    match *n {
                        #enc_arms
                        other => panic!("unknown graph attribute: {other}"),
                    }
                }
                buf
            }
            fn attr_faces(&self, _edge_id: u64) -> Vec<(::okm_core::index::Slot, Vec<u8>)> {
                // One face per declared field, declaration order; the
                // value bytes lead (scan prefix) and edge_id trails.
                let mut out = Vec::new();
                #face_pairs
                out
            }
        }

        #index_structs
    }
    .into()
}
