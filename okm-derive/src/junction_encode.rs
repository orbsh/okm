//! `JunctionEncode` — junctions (SQL many-to-many junction tables).
//!
//! The junction struct has exactly two fields, each a [`okm_core::Ref`]
//! whose document parameter carries the endpoint ns: the derive resolves
//! `<D as Document>::Key` for identity encoding and
//! `<D as Document>::NS_PREFIX` for the endpoint ns (ADR-0015/0016). ns is
//! declared once, on the document; the junction re-states nothing.
//! `#[ok_junction(n)]` sets the segment-0x3 discriminator.
//! `#[ok_head(field, ...)]` on each endpoint field declares that
//! endpoint's identity width (empty = full identity); generates the
//! `KvJunction` impl plus query traits hanging off the endpoint key types.

use proc_macro2::TokenStream as TS2;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields};

/// Parse + validate the declaration, then expand it. A malformed declaration
/// comes back as a span-carrying `syn::Error` — the caller turns it into
/// `compile_error!` on the offending token.
pub fn expand(input: DeriveInput) -> syn::Result<TS2> {
    let edge_name = &input.ident;

    let junction_attr = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_junction"));
    let junction_id: u16 = match junction_attr {
        Some(a) => a
            .parse_args::<syn::LitInt>()
            .map_err(|e| syn::Error::new_spanned(a, e))?
            .base10_parse::<u16>()
            .map_err(|e| syn::Error::new_spanned(a, e))?,
        None => {
            return Err(syn::Error::new(
                edge_name.span(),
                "missing #[ok_junction(n)]",
            ))
        }
    };
    // The discriminator rides the 12-bit slot counter as `id << 1 | dir`, so
    // 0x0000-0x07FF is the whole space. Masking it downstream would send an
    // out-of-range declaration onto another junction's slot — refuse the
    // declaration instead, where the mistake is visible.
    if junction_id > 0x07FF {
        return Err(syn::Error::new_spanned(
            junction_attr,
            format!(
                "#[ok_junction(0x{junction_id:04X})] is out of range: the discriminator is the 12-bit slot \
         counter's high bits (`id << 1 | dir`), so 0x0000-0x07FF is the whole space"
            ),
        ));
    }

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => {
                return Err(syn::Error::new(
                    edge_name.span(),
                    "JunctionEncode only supports structs with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new(
                edge_name.span(),
                "JunctionEncode only supports structs",
            ))
        }
    };
    let fields: Vec<_> = named.iter().collect();
    if fields.len() != 2 {
        return Err(syn::Error::new(
            edge_name.span(),
            "JunctionEncode needs exactly two fields (endpoint documents)",
        ));
    }
    let (fa, fb) = (&fields[0], &fields[1]);
    let (ta, tb) = (&fa.ty, &fb.ty);
    // Internal invariants: `Fields::Named` guarantees idents, and the length
    // check above guarantees two fields.
    #[allow(clippy::unwrap_used)]
    let (ia, ib) = (fa.ident.clone().unwrap(), fb.ident.clone().unwrap());

    // Endpoint document type: parse `Ref<User, UserKey>` → doc = User,
    // key = UserKey. The ns comes from the doc type (ADR-0015/0016).
    fn endpoint_parts(ty: &syn::Type, role: &str) -> syn::Result<(syn::Type, syn::Type)> {
        let s = quote!(#ty).to_string().replace(' ', "");
        let inner = s
            .trim_start_matches("Ref<")
            .trim_start_matches("Ref <")
            .trim_end_matches('>')
            .to_string();
        let (d, k) = inner
            .rsplit_once(',')
            .map(|(d, k)| (d.trim().to_string(), k.trim().to_string()))
            .ok_or_else(|| {
                syn::Error::new_spanned(ty, format!("{role}: endpoint must be Ref<Doc, Key>"))
            })?;
        let d_ty: syn::Type = syn::parse_str(&d).map_err(|e| {
            syn::Error::new_spanned(ty, format!("{role}: bad endpoint doc type `{d}`: {e}"))
        })?;
        let k_ty: syn::Type = syn::parse_str(&k).map_err(|e| {
            syn::Error::new_spanned(ty, format!("{role}: bad endpoint key type `{k}`: {e}"))
        })?;
        Ok((d_ty, k_ty))
    }
    let (doc_a, key_a) = endpoint_parts(ta, "first field")?;
    let (doc_b, _key_b) = endpoint_parts(tb, "second field")?;

    // #[ok_head(field, ...)] → &["a", "b"]; no attribute = full identity
    // (empty slice).
    fn head_names(field: &syn::Field) -> syn::Result<Vec<String>> {
        match field.attrs.iter().find(|a| a.path().is_ident("ok_head")) {
            Some(a) => {
                let list: syn::punctuated::Punctuated<syn::Ident, syn::Token![,]> = a
                    .parse_args_with(syn::punctuated::Punctuated::parse_terminated)
                    .map_err(|_| {
                        syn::Error::new_spanned(a, "ok_head format: #[ok_head(field, ...)]")
                    })?;
                Ok(list.iter().map(|i| i.to_string()).collect())
            }
            None => Ok(Vec::new()),
        }
    }
    let head_a = head_names(fa)?;
    let head_b = head_names(fb)?;

    // Method name: counterpart field role_id → get_role.
    let strip = |id: &syn::Ident| -> syn::Ident {
        let s = id.to_string();
        let base = s.strip_suffix("_id").unwrap_or(&s);
        format_ident!("get_{}", base)
    };
    let m_on_a = strip(&ib); // method on A's key type, named after the counterpart field
    let m_on_b = strip(&ia); // method on B's key type, named after the counterpart field

    let a_trait = format_ident!("{}_Ops", edge_name);
    let b_trait = format_ident!("{}ColR", edge_name);

    Ok(quote! {
        impl ::okm_core::KvJunction for #edge_name {
            type A = #doc_a;
            type B = #doc_b;
            const JUNCTION_ID: u16 = #junction_id;
            const A_HEAD: &'static [&'static str] = &[#(#head_a),*];
            const B_HEAD: &'static [&'static str] = &[#(#head_b),*];
            fn a(&self) -> &<Self::A as ::okm_core::Document>::Key { &self.#ia.key }
            fn b(&self) -> &<Self::B as ::okm_core::Document>::Key { &self.#ib.key }
            fn from_parts(
                a: <Self::A as ::okm_core::Document>::Key,
                b: <Self::B as ::okm_core::Document>::Key,
            ) -> Self {
                Self {
                    #ia: ::okm_core::Ref::ref_key(a),
                    #ib: ::okm_core::Ref::ref_key(b),
                }
            }
        }

        /// Query methods on the start endpoint.
        #[allow(non_camel_case_types)] // generated trait: <Edge>_Ops identity (ADR-0015)
        pub trait #a_trait {
            fn #m_on_a<S: ::okm_core::VirtualStorage>(
                &self,
                c: &::okm_core::Junction<S, #edge_name>,
            ) -> ::okm_core::error::Result<Vec<<#doc_b as ::okm_core::Document>::Key>>;
        }
        impl #a_trait for <#doc_a as ::okm_core::Document>::Key {
            fn #m_on_a<S: ::okm_core::VirtualStorage>(
                &self,
                c: &::okm_core::Junction<S, #edge_name>,
            ) -> ::okm_core::error::Result<Vec<<#doc_b as ::okm_core::Document>::Key>> {
                c.forward(self)
            }
        }

        /// Query methods on the end endpoint.
        #[allow(non_camel_case_types)] // generated trait: <Edge>ColR identity
        pub trait #b_trait {
            fn #m_on_b<S: ::okm_core::VirtualStorage>(
                &self,
                c: &::okm_core::Junction<S, #edge_name>,
            ) -> ::okm_core::error::Result<Vec<::okm_core::PrefixKey<#key_a>>>;
        }
        impl #b_trait for <#doc_b as ::okm_core::Document>::Key {
            fn #m_on_b<S: ::okm_core::VirtualStorage>(
                &self,
                c: &::okm_core::Junction<S, #edge_name>,
            ) -> ::okm_core::error::Result<Vec<::okm_core::PrefixKey<#key_a>>> {
                ::core::result::Result::Ok(c.reverse_raw(self)?
                    .into_iter()
                    .map(|suffix| {
                        ::okm_core::PrefixKey {
                            decoded: <#key_a as ::okm_core::KeyEncode>::decode(&suffix),
                            taken: <#key_a as ::okm_core::KeyEncode>::FIELD_WIDTHS
                                .iter()
                                .map(|(_, w)| *w)
                                .sum(),
                        }
                    })
                    .collect())
            }
        }
    })
}
