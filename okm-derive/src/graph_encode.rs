//! `GraphEncode` — the graph declaration derive (ADR-0017 §4).
//!
//! One declaration IS the graph: the edge collection's ns, its own document
//! payload (both codec directions — the payload is an ordinary obj, ADR-0012),
//! the attribute faces it declares EXPLICITLY via `#[ok_index]`, and the node
//! collections it admits via `#[ok_nodes]`:
//!
//! ```ignore
//! #[derive(Clone, Default, GraphEncode)]
//! #[ok_ns(100)]
//! #[ok_nodes(EmploymentNodes)]
//! #[ok_index(by_since { fields(since_year) })]   // a face is a declaration
//! struct Employment {
//!     since_year: u16,
//!     weight: u32,
//! }
//!
//! enum EmploymentNodes { Users, Orgs }           // variants are COLLECTIONS
//! impl okm_core::NodeSet for EmploymentNodes {
//!     const NS_VALUES: &'static [u16] = &[10, 11];
//! }
//! ```
//!
//! The member set is an associated type on the trait, named by the derive
//! attribute: `#[derive(GraphEncode)]` plus a hand-written
//! `impl GraphEncode for X { type Nodes = … }` cannot coexist (that would be
//! a second impl of the same trait), so the ONE declaration carries both.
//!
//! Generated: the full `Document` impl (payload codec, `#[ok_index]` marker
//! structs and entries, `NS_PREFIX` from `#[ok_ns]`) plus
//! `impl GraphEncode { type Nodes }`. Identity is the mechanism's `EdgeKey`
//! (the bare `edge_id`) — a graph declares no key struct.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse_macro_input;

use crate::document_encode::emit_row_impl;
use crate::schema::parse_schema_with_key;

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    let name = input.ident.clone();

    if !input.attrs.iter().any(|a| a.path().is_ident("ok_ns")) {
        panic!(
            "GraphEncode[{name}]: missing #[ok_ns(N)] — the declaration is the only ns source (ADR-0017 §4)"
        );
    }

    let nodes: syn::Type = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_nodes"))
        .map(|a| {
            a.parse_args::<syn::Type>()
                .expect("ok_nodes format: #[ok_nodes(MemberEnum)]")
        })
        .unwrap_or_else(|| {
            panic!(
                "GraphEncode[{name}]: missing #[ok_nodes(MemberEnum)] — the member set is a compile-time enum over the node collections, checked at write and decode"
            )
        });

    let key_ty: syn::Type = syn::parse_quote!(::okm_core::EdgeKey);
    let schema = parse_schema_with_key(input, key_ty);
    let row = emit_row_impl(&schema);

    quote! {
        #row

        impl ::okm_core::GraphEncode for #name {
            type Nodes = #nodes;
        }
    }
    .into()
}