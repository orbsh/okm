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
//! // pub enum EmploymentNode { User, Org }
//! // impl okm_core::NodeSet for EmploymentNode {
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
//! `<Name>Node`, its `NodeSet` impl, and `impl GraphEncode { type Nodes }`.
//! The suffix is SINGULAR because a type name names one value: a value of this
//! enum names one node collection (one ns), and the plural role — "the set of
//! collections this graph admits" — belongs to `NodeSet::NS_VALUES`, never to
//! the enum's name. `<Name>Nodes` would claim the type holds many nodes, which
//! no value ever does.
//! Identity is the mechanism's `EdgeKey` (the tuple — an edge has no number,
//! ADR-0067) — a graph declares no key struct. What the declaration DOES add
//! beyond the member list is each member's key width (`KEY_LENS`, from that
//! table's own `KEY_LEN`), which is what lets an edge key be written and parsed
//! with no length prefix, and one typed endpoint constructor per member
//! (`EmploymentNode::user(&key)`).

use proc_macro2::TokenStream as TS2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::Token;

use crate::document_encode::emit_row_impl;
use crate::schema::parse_schema_with_key;

/// `EmploymentNode`'s variant `WorksFor` → the constructor `works_for`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (at, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if at > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Parse + validate the declaration, then expand it. A malformed declaration
/// comes back as a span-carrying `syn::Error` — the caller turns it into
/// `compile_error!` on the offending token.
pub fn expand(input: syn::DeriveInput) -> syn::Result<TS2> {
    let name = input.ident.clone();
    let vis = input.vis.clone();

    if !input.attrs.iter().any(|a| a.path().is_ident("ok_ns")) {
        return Err(syn::Error::new(
            name.span(),
            format!(
                "GraphEncode[{name}]: missing #[ok_ns(N)] — the declaration is the only ns source (ADR-0017 §4)"
            ),
        ));
    }

    // #[ok_nodes(User, Org, …)] — the member set is a list of node TABLES.
    let nodes_attr = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_nodes"))
        .ok_or_else(|| {
            syn::Error::new(
                name.span(),
                format!(
                    "GraphEncode[{name}]: missing #[ok_nodes(NodeTable, …)] — the member set lists the node collections' document types, checked at write and decode"
                ),
            )
        })?;
    let tables: Punctuated<syn::Type, Token![,]> = nodes_attr
        .parse_args_with(Punctuated::parse_terminated)
        .map_err(|e| {
            syn::Error::new_spanned(
                nodes_attr,
                format!("ok_nodes format: #[ok_nodes(NodeTable, OtherTable)] — {e}"),
            )
        })?;
    if tables.is_empty() {
        return Err(syn::Error::new_spanned(
            nodes_attr,
            format!("GraphEncode[{name}]: #[ok_nodes()] must name at least one node table"),
        ));
    }

    let mut variants: Vec<syn::Ident> = Vec::with_capacity(tables.len());
    for ty in tables.iter() {
        let ident = match ty {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.clone())
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ty,
                        format!("ok_nodes: `{}` has no type name", quote!(#ty)),
                    )
                })?,
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    format!(
                        "ok_nodes: every member must be a document TYPE, got `{}`",
                        quote!(#other)
                    ),
                ))
            }
        };
        if variants.contains(&ident) {
            return Err(syn::Error::new_spanned(
                ty,
                format!(
                    "GraphEncode[{name}]: ok_nodes names `{ident}` twice — a table is one collection, listed once"
                ),
            ));
        }
        variants.push(ident);
    }
    let set_ident = quote::format_ident!("{}Node", name);

    // One ns per listed table, read off that table's own declaration. The
    // constant is not optional: a document without `#[ok_ns]` does not compile,
    // so a member always names a collection and this cannot fail.
    let ns_items = tables.iter().map(|ty| {
        quote! { <#ty as ::okm_core::Document>::NS }
    });

    // One pkey width per listed table, read off its key's own `KEY_LEN`. This is
    // the table an edge key parses by: no length prefix, because the declaration
    // already says how wide each endpoint is (ADR-0067).
    let len_items = tables.iter().map(|ty| {
        quote! { <<#ty as ::okm_core::Document>::Key as ::okm_core::KeyEncode>::KEY_LEN }
    });

    // The checked way to build an endpoint: one constructor per member, taking
    // THAT table's key type, so a key of another collection cannot be passed and
    // the ns can never drift from the table. `EmploymentNode::user(&key)`.
    let endpoint_fns = tables.iter().zip(variants.iter()).map(|(ty, variant)| {
        let fname = quote::format_ident!("{}", snake_case(&variant.to_string()));
        let doc = format!(
            "The endpoint `{variant}` names, from a key of its own collection — the \
             declared way to build a graph reference (ADR-0067)."
        );
        quote! {
            #[doc = #doc]
            pub fn #fname(key: &<#ty as ::okm_core::Document>::Key) -> ::okm_core::NodeRef {
                ::okm_core::NodeRef::new(
                    <#ty as ::okm_core::Document>::NS_PREFIX,
                    &::okm_core::KeyEncode::encode(key),
                )
            }
        }
    });

    let key_ty: syn::Type = syn::parse_quote!(::okm_core::EdgeKey);
    let schema = parse_schema_with_key(input, key_ty)?;
    let row = emit_row_impl(&schema)?;

    Ok(quote! {
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

        impl #set_ident {
            #(#endpoint_fns)*
        }

        impl ::okm_core::NodeSet for #set_ident {
            const NS_VALUES: &'static [u16] = &[#(#ns_items),*];

            /// Each member's pkey width, from its key's own `KEY_LEN` — the table
            /// an edge key parses by, with no length prefix (ADR-0067).
            const KEY_LENS: &'static [usize] = &[#(#len_items),*];

            /// The variant's position in `NS_VALUES` — the same order, so a
            /// write path handed a variant can name its collection. The enum is
            /// fieldless, so the discriminant IS the position.
            fn index(self) -> usize {
                self as usize
            }
        }

        impl ::okm_core::GraphEncode for #name {
            type Nodes = #set_ident;
        }
    })
}
