//! `EdgeEncode` — bidirectional edges.
//!
//! The edge struct has exactly two fields (start, end). `#[kv_head(field,
//! ...)]` on each endpoint field declares that endpoint's identity width
//! (empty = full identity); generates the `KvEdge` impl plus query
//! traits hanging off the endpoint types.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields};

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let edge_name = &input.ident;

    let ns: u16 = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("kv_ns") {
                Some(
                    a.parse_args::<syn::LitInt>()
                        .unwrap()
                        .base10_parse()
                        .unwrap(),
                )
            } else {
                None
            }
        })
        .expect("missing #[kv_ns(N)]");

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("EdgeEncode only supports structs with named fields"),
        },
        _ => panic!("EdgeEncode only supports structs"),
    };
    let fields: Vec<_> = named.iter().collect();
    if fields.len() != 2 {
        panic!("EdgeEncode needs exactly two fields (start, end)");
    }
    let (fa, fb) = (&fields[0], &fields[1]);
    let (ta, tb) = (&fa.ty, &fb.ty);
    let ia = fa.ident.clone().unwrap();
    let ib = fb.ident.clone().unwrap();

    // #[kv_head(field, ...)] → &["a", "b"]; no attribute = full identity
    // (empty slice).
    fn head_names(field: &syn::Field) -> Vec<String> {
        field
            .attrs
            .iter()
            .find_map(|a| {
                if a.path().is_ident("kv_head") {
                    let list: syn::punctuated::Punctuated<syn::Ident, syn::Token![,]> = a
                        .parse_args_with(syn::punctuated::Punctuated::parse_terminated)
                        .expect("kv_head format: #[kv_head(field, ...)]");
                    Some(list.iter().map(|i| i.to_string()).collect())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
    let head_a = head_names(fa);
    let head_b = head_names(fb);

    // Method name: counterpart field role_id → get_role.
    let strip = |id: &syn::Ident| -> syn::Ident {
        let s = id.to_string();
        let base = s.strip_suffix("_id").unwrap_or(&s);
        format_ident!("get_{}", base)
    };
    let m_on_a = strip(&ib); // method on UserKey, named after the counterpart field
    let m_on_b = strip(&ia); // method on RoleKey, named after the counterpart field

    let a_trait = format_ident!("{}_Ops", edge_name);
    let b_trait = format_ident!("{}ColR", edge_name);

    quote! {
        impl ::okm_core::KvEdge for #edge_name {
            type A = #ta;
            type B = #tb;
            const NS: u16 = #ns;
            const A_HEAD: &'static [&'static str] = &[#(#head_a),*];
            const B_HEAD: &'static [&'static str] = &[#(#head_b),*];
            fn a(&self) -> &Self::A { &self.#ia }
            fn b(&self) -> &Self::B { &self.#ib }
            fn from_parts(a: Self::A, b: Self::B) -> Self {
                Self { #ia: a, #ib: b }
            }
        }

        /// Query methods on the start endpoint.
        pub trait #a_trait {
            fn #m_on_a<S: ::okm_core::KvEngine>(
                &self,
                c: &::okm_core::EdgeTable<S, #edge_name>,
            ) -> Vec<#tb>;
        }
        impl #a_trait for #ta {
            fn #m_on_a<S: ::okm_core::KvEngine>(
                &self,
                c: &::okm_core::EdgeTable<S, #edge_name>,
            ) -> Vec<#tb> {
                c.forward(self)
            }
        }

        /// Query methods on the end endpoint.
        pub trait #b_trait {
            fn #m_on_b<S: ::okm_core::KvEngine>(
                &self,
                c: &::okm_core::EdgeTable<S, #edge_name>,
            ) -> Vec<::okm_core::PrefixKey<#ta>>;
        }
        impl #b_trait for #tb {
            fn #m_on_b<S: ::okm_core::KvEngine>(
                &self,
                c: &::okm_core::EdgeTable<S, #edge_name>,
            ) -> Vec<::okm_core::PrefixKey<#ta>> {
                c.reverse_raw(self)
                    .into_iter()
                    .map(|suffix| {
                        ::okm_core::PrefixKey {
                            decoded: <#ta as ::okm_core::KeyEncode>::decode(&suffix),
                            taken: <#ta as ::okm_core::KeyEncode>::FIELD_WIDTHS
                                .iter()
                                .map(|(_, w)| *w)
                                .sum(),
                        }
                    })
                    .collect()
            }
        }
    }
    .into()
}
