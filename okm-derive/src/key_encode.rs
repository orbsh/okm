//! `KeyEncode` — fixed-width big-endian key encoding.
//!
//! Generates `encode`/`decode`/`KEY_LEN`/`FIELD_WIDTHS` plus the
//! `encode_prefix_named`/`prefix_width` pair used by index prefix scans.
//! The prefix functions are slice-pattern matches: one arm per declared
//! prefix combination of fields (in declaration order); the pattern match
//! itself validates the requested prefix, and each arm's width is a
//! compile-time constant sum.

use proc_macro2::TokenStream as TS2;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

/// Identity-side field encoder: same shape as the payload-side triple in
/// `document_encode` (u8/u16/u32/u64 BE, `[u8; N]`).
struct Field {
    ident: syn::Ident,
    enc: TS2,
    dec: TS2,
    width: TS2,
    /// `okm_core::FieldType` variant path, for the FieldDesc table (ADR-0007).
    kind: Option<TS2>,
}

/// FieldDesc table entries: `(name, FieldType, width)`, declaration order.
// reason: every supported key field kind carries a `kind` — an unsupported type
// already failed in `field_encoders`, so `kind` is Some here.
#[allow(clippy::expect_used)]
fn field_desc_entries(fs: &[Field]) -> TS2 {
    let documents = fs.iter().map(|f| {
        let name = f.ident.to_string();
        let kind = f.kind.as_ref().expect("field kind");
        let w = &f.width;
        quote! { (::okm_core::FieldDesc { name: #name, ty: #kind, width: #w }) }
    });
    quote! { &[ #(#documents),* ] }
}

/// Parse + validate the declaration, then expand it. A malformed declaration
/// comes back as a span-carrying `syn::Error` — the caller turns it into
/// `compile_error!` on the offending token.
pub fn expand(input: DeriveInput) -> syn::Result<TS2> {
    let name = &input.ident;

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => {
                return Err(syn::Error::new(
                    name.span(),
                    "KeyEncode only supports structs with named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new(
                name.span(),
                "KeyEncode only supports structs",
            ))
        }
    };

    let fs = field_encoders(named, "KeyEncode")?;
    let desc = field_desc_entries(&fs);

    let names: Vec<_> = fs.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();
    let encs: Vec<_> = fs.iter().map(|f| &f.enc).collect();
    let decs: Vec<_> = fs.iter().map(|f| &f.dec).collect();
    let widths: Vec<_> = fs.iter().map(|f| &f.width).collect();

    // encode_prefix_named / prefix_width: per-name lookup instead of
    // declaration-order-prefix arms. Index key(...) truncation may skip
    // fields (keep only the unique tail), so arbitrary named subsets must
    // work, encoded in request order. All key fields are fixed-width, so
    // prefix_width returns a plain usize.
    // 逐名匹配 arm：循环 extend 到单一 TokenStream，再单次插值
    // （嵌套 #(#vec)* 在此 quote 上下文会展开失败，探针已验证单次插值正常）。
    let mut enc_arms = quote! {};
    let mut width_arms = quote! {};
    for (f, name_lit) in fs.iter().zip(name_strs.iter()) {
        let enc = &f.enc;
        let w = &f.width;
        enc_arms.extend(quote! { #name_lit => { #enc } });
        width_arms.extend(quote! { #name_lit => { #w } });
    }

    Ok(quote! {
        impl ::okm_core::KeyEncode for #name {
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
            const FIELDS: &'static [::okm_core::FieldDesc] = #desc;
            /// Encode the requested named subset (request order) into
            /// `buf`; returns bytes written. Unknown names panic.
            #[allow(clippy::ptr_arg)] // &mut Vec is the encoder-API shape
            fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
                let before = buf.len();
                for n in names {
                    match *n {
                        #enc_arms
                        _ => panic!("invalid prefix name: {n}"),
                    }
                }
                buf.len() - before
            }
            /// Constant width of the requested named subset. Unknown names panic.
            fn prefix_width(names: &[&str]) -> usize {
                let mut sum = 0usize;
                for n in names {
                    sum += match *n {
                        #width_arms
                        _ => panic!("invalid prefix name: {n}"),
                    };
                }
                sum
            }
        }

        impl #name {
            /// Encode the first `n` fields in declaration order (numeric
            /// variant, handy for prefix scans).
            #[allow(clippy::ptr_arg)] // &mut Vec is the encoder-API shape
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
    })
}

fn field_encoders(named: &syn::FieldsNamed, ctx: &str) -> syn::Result<Vec<Field>> {
    let mut fs = Vec::new();
    for f in named.named.iter() {
        // Internal invariant: `syn::FieldsNamed` holds only named fields, so
        // every field carries an ident.
        #[allow(clippy::unwrap_used, clippy::unwrap_in_result)]
        let id = f.ident.clone().unwrap();
        let ty = &f.ty;
        let ty_str = quote!(#ty).to_string().replace(' ', "");
        let (enc, dec, width, kind) = match ty_str.as_str() {
            "u64" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u64::from_be_bytes(b[offset..offset+8].try_into().unwrap()); offset += 8; },
                quote! { 8 },
                Some(quote! { ::okm_core::FieldType::U64 }),
            ),
            "u32" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()); offset += 4; },
                quote! { 4 },
                Some(quote! { ::okm_core::FieldType::U32 }),
            ),
            "u16" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! { let #id = u16::from_be_bytes(b[offset..offset+2].try_into().unwrap()); offset += 2; },
                quote! { 2 },
                Some(quote! { ::okm_core::FieldType::U16 }),
            ),
            "u8" => (
                quote! { buf.push(self.#id); },
                quote! { let #id = b[offset]; offset += 1; },
                quote! { 1 },
                Some(quote! { ::okm_core::FieldType::U8 }),
            ),
            _ if ty_str.starts_with("Vocab<") || ty_str == "Vocab" => {
                // Vocab<N> (ADR-0032): an N-byte interned id in the index key,
                // same width as in the payload so key order stays id order.
                let id_ty = if ty_str == "Vocab" {
                    "u8".to_string()
                } else {
                    ty_str
                        .trim_start_matches("Vocab<")
                        .trim_end_matches('>')
                        .to_string()
                };
                let width: u16 = match id_ty.as_str() {
                    "u8" => 1,
                    "u16" => 2,
                    "u32" => 4,
                    "u64" => 8,
                    "u128" => 16,
                    other => {
                        return Err(syn::Error::new_spanned(
                            ty,
                            format!("{ctx}: Vocab<{other}> — the id width must be u8, u16, u32, u64 or u128"),
                        ))
                    }
                };
                let id_ty: syn::Type = syn::parse_str(&id_ty).map_err(|_| {
                    syn::Error::new_spanned(ty, format!("{ctx}: bad Vocab id width type"))
                })?;
                let w_lit = proc_macro2::Literal::u16_unsuffixed(width);
                let off_lit = proc_macro2::Literal::u16_unsuffixed(16 - width);
                (
                    quote! {{
                        let img = self.#id.wire16();
                        buf.extend_from_slice(&img[#off_lit..]);
                    }},
                    quote! { let #id = <::okm_core::Vocab<#id_ty>>::from_wire(&b[offset..offset+#w_lit]); offset += #w_lit; },
                    quote! { #w_lit },
                    Some(quote! { ::okm_core::FieldType::Vocab(0) }),
                )
            }
            _ if ty_str == "Uuid4" || ty_str == "Uuid7" => {
                // The derive already knows which one it is: emit the kind directly, because
                // `FIELDS` is a const array and a runtime `if` cannot live there.
                let kind_ty = if ty_str == "Uuid4" {
                    quote! { ::okm_core::FieldType::Uuid4 }
                } else {
                    quote! { ::okm_core::FieldType::Uuid7 }
                };
                (
                quote! { buf.extend_from_slice(self.#id.as_bytes()); },
                quote! {
                    let mut #id = [0u8; 16];
                    #id.copy_from_slice(&b[offset..offset + 16]);
                    offset += 16;
                    let #id = <#ty as ::core::convert::From<[u8; 16]>>::from(#id);
                },
                quote! { 16usize },
                Some(quote! { #kind_ty }),
                )
            }
            _ if ty_str.starts_with("[u8;") => {
                let n: usize = ty_str
                    .trim_start_matches("[u8;")
                    .trim_end_matches(']')
                    .parse()
                    .map_err(|_| {
                        syn::Error::new_spanned(ty, "[u8; N]: N must be an integer literal")
                    })?;
                let nlit = proc_macro2::Literal::usize_unsuffixed(n);
                (
                    quote! { buf.extend_from_slice(&self.#id); },
                    quote! {
                        let mut #id = [0u8; #nlit];
                        #id.copy_from_slice(&b[offset..offset+#nlit]);
                        offset += #nlit;
                    },
                    quote! { #nlit },
                    Some(quote! { ::okm_core::FieldType::FixedBytes }),
                )
            }
            _ if ty_str.starts_with("Reverse<") => {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "{ctx}: Reverse<T> on key field {id} — keys are fixed-width identity; \
                     put Reverse fields in the document payload instead"
                    ),
                ))
            }
            _ if ty_str.starts_with("VarInt<")
                | ty_str.starts_with("Quant<")
                | ty_str.starts_with("Enum<")
                | ty_str.starts_with("Offset<") =>
            {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "{ctx}: wrapper type {ty_str} on key field {id} — keys are fixed-width \
                     identity; wrapper codecs belong in the document payload"
                    ),
                ))
            }
            _ if ty_str.starts_with("String") => {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "{ctx}: String on key field {id} — the key encoding is fixed-width \
                     (pure pointer slicing); use [u8; N] or a hash"
                    ),
                ))
            }
            other => {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!("{ctx}: unsupported type {other} (field {id})"),
                ))
            }
        };
        fs.push(Field {
            ident: id,
            enc,
            dec,
            width,
            kind,
        });
    }
    Ok(fs)
}
