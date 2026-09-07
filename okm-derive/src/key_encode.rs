//! `KeyEncode` — fixed-width big-endian key encoding.
//!
//! Generates `encode`/`decode`/`KEY_LEN`/`FIELD_WIDTHS` plus the
//! `encode_prefix_named`/`prefix_width` pair used by index prefix scans.
//! The prefix functions are slice-pattern matches: one arm per declared
//! prefix combination of fields (in declaration order); the pattern match
//! itself validates the requested prefix, and each arm's width is a
//! compile-time constant sum.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TS2;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields};

/// Identity-side field encoder: same shape as the payload-side triple in
/// `row_encode` (u8/u16/u32/u64 BE, `[u8; N]`).
struct Field {
    ident: syn::Ident,
    enc: TS2,
    dec: TS2,
    width: TS2,
}

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => panic!("KeyEncode only supports structs with named fields"),
        },
        _ => panic!("KeyEncode only supports structs"),
    };

    let fs = field_encoders(named, "KeyEncode");

    let names: Vec<_> = fs.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();
    let encs: Vec<_> = fs.iter().map(|f| &f.enc).collect();
    let decs: Vec<_> = fs.iter().map(|f| &f.dec).collect();
    let widths: Vec<_> = fs.iter().map(|f| &f.width).collect();

    // encode_prefix_named: slice-pattern match — one arm per declared-order
    // prefix. Pattern match = validation (failure falls into the `_` arm);
    // each arm's width is a generated constant sum.
    let mut prefix_arms = quote! {};
    for i in 0..fs.len() {
        let pat: Vec<_> = name_strs[..=i].iter().map(|s| quote! { #s }).collect();
        let encs: Vec<_> = fs[..=i].iter().map(|f| &f.enc).collect();
        let width_sum: Vec<_> = widths[..=i].to_vec();
        prefix_arms.extend(quote! {
            &[ #(#pat),* ] => {
                #(#encs)*
                0 #(+ #width_sum)*
            }
        });
    }
    prefix_arms.extend(quote! { _ => panic!("invalid prefix: {names:?}") });

    // prefix_width: same arm structure, constant width only.
    let mut width_arms = quote! {};
    for i in 0..fs.len() {
        let pat: Vec<_> = name_strs[..=i].iter().map(|s| quote! { #s }).collect();
        let width_sum: Vec<_> = widths[..=i].to_vec();
        width_arms.extend(quote! {
            &[ #(#pat),* ] => 0 #(+ #width_sum)*,
        });
    }
    width_arms.extend(quote! { _ => panic!("invalid prefix: {names:?}") });

    quote! {
        impl ::okm::KeyEncode for #name {
            const KEY_LEN: usize = 0 #(+ #widths)*;
            const FIELD_WIDTHS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];

            fn encode(&self) -> Vec<u8> {
                let mut buf = Vec::with_capacity(Self::KEY_LEN);
                #(#encs)*
                buf
            }
            fn decode(b: &[u8]) -> Self {
                let mut offset = 0usize;
                #(#decs)*
                Self { #(#names),* }
            }
            fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
                match names {
                    #prefix_arms
                }
            }
            fn prefix_width(names: &[&str]) -> usize {
                match names {
                    #width_arms
                }
            }
        }

        impl #name {
            /// Encode the first `n` fields in declaration order (numeric
            /// variant, handy for prefix scans).
            pub fn encode_prefix_n(&self, buf: &mut Vec<u8>, n: usize) {
                let mut done = 0usize;
                #(
                    if done < n {
                        #encs
                        done += 1;
                    }
                )*
            }
        }
    }
    .into()
}

fn field_encoders(named: &syn::FieldsNamed, ctx: &str) -> Vec<Field> {
    let mut fs = Vec::new();
    for f in named.named.iter() {
        let id = f.ident.clone().unwrap();
        let ty = &f.ty;
        let ty_str = quote!(#ty).to_string().replace(' ', "");
        let (enc, dec, width) = match ty_str.as_str() {
            "u64" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u64::from_be_bytes(b[offset..offset+8].try_into().unwrap()); offset += 8; },
                quote! { 8 },
            ),
            "u32" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()); offset += 4; },
                quote! { 4 },
            ),
            "u16" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u16::from_be_bytes(b[offset..offset+2].try_into().unwrap()); offset += 2; },
                quote! { 2 },
            ),
            "u8" => (
                quote! { buf.push(self.#id); },
                quote! { let #id = b[offset]; offset += 1; },
                quote! { 1 },
            ),
            _ if ty_str.starts_with("[u8;") => {
                let n: usize = ty_str
                    .trim_start_matches("[u8;")
                    .trim_end_matches(']')
                    .parse()
                    .expect("[u8; N]: N must be an integer literal");
                let nlit = proc_macro2::Literal::usize_unsuffixed(n);
                (
                    quote! { buf.extend_from_slice(&self.#id); },
                    quote! {
                        let mut #id = [0u8; #nlit];
                        #id.copy_from_slice(&b[offset..offset+#nlit]);
                        offset += #nlit;
                    },
                    quote! { #nlit },
                )
            }
            other => panic!("{ctx}: unsupported type {other} (field {id})"),
        };
        fs.push(Field {
            ident: id,
            enc,
            dec,
            width,
        });
    }
    fs
}
