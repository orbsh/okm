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
//! #[ok_nodes(User, Org)]                         // the node TABLES, by type
//! #[ok_index(by_since { fields(since_year) })]   // a face is a declaration
//! struct Employment {
//!     since_year: u16,
//!     weight: u32,
//! }
//!
//! // generated, never written by hand:
//! // pub enum EmploymentNodes { User, Org }
//! // impl okm_core::NodeSet for EmploymentNodes {
//! //     const NS_VALUES: &'static [u16] = &[10, 11];   // the tables' own ns
//! // }
//! ```
//!
//! The member set is a set of TABLES, so `#[ok_nodes]` lists document types and
//! the derive writes the enum over them plus its `NodeSet` impl. The ns values
//! come from each table's own `#[ok_ns]`, never from a hand-copied number — a
//! new member is one type name in the edge's declaration, and the ns cannot
//! drift from the table. A variant is one COLLECTION (that is what membership
//! means: an endpoint's ns), so a variant is named after the table's type and
//! carries nothing.
//!
//! The member set is an associated type on the trait, named by the derive
//! attribute: `#[derive(GraphEncode)]` plus a hand-written
//! `impl GraphEncode for X { type Nodes = … }` cannot coexist (that would be
//! a second impl of the same trait), so the ONE declaration carries both.
//!
//! Generated: the full `Document` impl (payload codec, `#[ok_index]` marker
//! structs and entries, `NS_PREFIX`/`NS` from `#[ok_ns]`), the member-set enum
//! `<Name>Nodes`, its `NodeSet` impl, and `impl GraphEncode { type Nodes }`.
//! Identity is the mechanism's `EdgeKey` (the bare `edge_id`) — a graph
//! declares no key struct.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse_macro_input;
use syn::punctuated::Punctuated;
use syn::Token;

use crate::document_encode::emit_row_impl;
use crate::schema::parse_schema_with_key;

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    let name = input.ident.clone();
    let vis = input.vis.clone();

    if !input.attrs.iter().any(|a| a.path().is_ident("ok_ns")) {
        panic!(
            "GraphEncode[{name}]: missing #[ok_ns(N)] — the declaration is the only ns source (ADR-0017 §4)"
        );
    }

    // #[ok_nodes(User, Org, …)] — the member set is a list of node TABLES.
    let nodes_attr = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_nodes"))
        .unwrap_or_else(|| {
            panic!(
                "GraphEncode[{name}]: missing #[ok_nodes(NodeTable, …)] — the member set lists the node collections' document types, checked at write and decode"
            )
        });
    let tables: Punctuated<syn::Type, Token![,]> = nodes_attr
        .parse_args_with(Punctuated::parse_terminated)
        .unwrap_or_else(|e| panic!("ok_nodes format: #[ok_nodes(NodeTable, OtherTable)] — {e}"));
    if tables.is_empty() {
        panic!("GraphEncode[{name}]: #[ok_nodes()] must name at least one node table");
    }

    let mut variants: Vec<syn::Ident> = Vec::with_capacity(tables.len());
    for ty in tables.iter() {
        let ident = match ty {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.clone())
                .unwrap_or_else(|| panic!("ok_nodes: `{}` has no type name", quote!(#ty))),
            other => panic!(
                "ok_nodes: every member must be a document TYPE, got `{}`",
                quote!(#other)
            ),
        };
        if variants.contains(&ident) {
            panic!(
                "GraphEncode[{name}]: ok_nodes names `{ident}` twice — a table is one collection, listed once"
            );
        }
        variants.push(ident);
    }
    let set_ident = quote::format_ident!("{}Nodes", name);

    // One ns per listed table, read off that table's own declaration. `NS` is
    // `None` only for a layout-only document: a member must be a collection, so
    // that is a declaration error surfaced at compile time.
    let ns_items = tables.iter().map(|ty| {
        quote! {
            match <#ty as ::okm_core::Document>::NS {
                ::core::option::Option::Some(ns) => ns,
                ::core::option::Option::None => panic!(concat!(
                    "ok_nodes: ",
                    stringify!(#ty),
                    " declares no #[ok_ns] — a member must be a collection"
                )),
            }
        }
    });

    let key_ty: syn::Type = syn::parse_quote!(::okm_core::EdgeKey);
    let schema = parse_schema_with_key(input, key_ty);
    let row = emit_row_impl(&schema);

    quote! {
        #row

        /// The member set of this graph, generated from `#[ok_nodes(...)]`:
        /// one variant per node collection (the tables' own types) and one ns
        /// per variant, read off each table's `#[ok_ns]`. A declaration, not
        /// storage — no variant is ever constructed, so it carries no data.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[allow(dead_code)]
        #vis enum #set_ident {
            #(#variants),*
        }

        impl ::okm_core::NodeSet for #set_ident {
            const NS_VALUES: &'static [u16] = &[#(#ns_items),*];
        }

        impl ::okm_core::GraphEncode for #name {
            type Nodes = #set_ident;
        }
    }
    .into()
}
