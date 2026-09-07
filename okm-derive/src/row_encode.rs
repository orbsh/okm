//! `RowEncode` — value/payload encoding + index declarations.
//!
//! One macro, three concerns (ADR-0006):
//!
//! 1. `#[kv_ref(KeyType)]` — the identity struct this row hangs off.
//! 2. Payload fields — encoded as TLV: `[tag u8][len u32 BE][value BE]`
//!    per field, `tag` = field declaration index (unique within the row,
//!    decoupled from field names). `len` is a redundant check for
//!    fixed-width fields today but keeps the same frame for the
//!    variable-length regime later.
//! 3. `#[kv_index(idx_name { fields(a, b), includes(c) })]` — one access
//!    method per declaration, slots start at 1 in attribute order
//!    (`0` is reserved for the primary table, ADR-0005). Generates a
//!    marker struct per index plus `Row::index_entries`, so `put`/
//!    `delete` cover every declared access method with no runtime
//!    registry — the declaration *is* the registry.

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, TokenStream as TS2, TokenTree};
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, Data, DeriveInput, Fields};

/// One parsed `#[kv_index(...)]` declaration. The attribute body uses
/// struct-ish syntax that `syn::Meta` does not cover, so it is parsed at
/// the token-stream level: `Ident` + brace group, with `(fields|includes)`
/// paren groups inside, comma-separated across multiple indexes.
struct IdxDecl {
    ident: syn::Ident,
    fields: Vec<String>,
    includes: Vec<String>,
}

fn parse_index_attr(attr: &syn::Attribute) -> Vec<IdxDecl> {
    let mut out = Vec::new();
    let ts: Vec<TokenTree> = attr.to_token_stream().into_iter().collect();
    // Shape: `#[kv_index(…)]` — take the top-level Bracket group, then the
    // inner Parenthesis group (the attribute arguments).
    let outer = ts
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => Some(g.stream()),
            _ => None,
        })
        .expect("kv_index: missing attribute brackets");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
            _ => None,
        })
        .expect("kv_index: missing argument parentheses");
    let ts: Vec<TokenTree> = body.into_iter().collect();

    let mut i = 0usize;
    while i < ts.len() {
        // Skip commas between index declarations.
        if matches!(&ts[i], TokenTree::Punct(p) if p.as_char() == ',') {
            i += 1;
            continue;
        }
        // Expect: index name (Ident).
        let ident = match &ts[i] {
            TokenTree::Ident(id) => id.clone(),
            t => panic!("kv_index: expected index name Ident, got {t}"),
        };
        i += 1;
        // Expect: { … } brace group.
        let body = match ts.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
            t => panic!("kv_index[{ident}]: expected {{ fields(…) }} block, got {t:?}"),
        };
        i += 1;

        // Inside: fields(a, b), includes(c) — Ident + paren group pairs,
        // comma-separated.
        let mut fields = Vec::new();
        let mut includes = Vec::new();
        let toks: Vec<TokenTree> = body.into_iter().collect();
        let mut j = 0usize;
        while j < toks.len() {
            let kw = match &toks[j] {
                TokenTree::Ident(id) => id.to_string(),
                t => panic!("kv_index[{ident}]: expected fields/includes, got {t}"),
            };
            let list: Vec<String> = match toks.get(j + 1) {
                Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => g
                    .stream()
                    .into_iter()
                    .filter_map(|t| match t {
                        TokenTree::Ident(id) => Some(id.to_string()),
                        TokenTree::Punct(_) => None,
                        t => panic!("kv_index[{ident}].{kw}: illegal token {t}"),
                    })
                    .collect(),
                t => panic!("kv_index[{ident}].{kw}: expected paren group, got {t:?}"),
            };
            match kw.as_str() {
                "fields" => fields = list,
                "includes" => includes = list,
                other => {
                    panic!("kv_index[{ident}]: unknown key {other} (supported: fields/includes)")
                }
            }
            j += 2;
            // Skip trailing comma.
            if matches!(toks.get(j), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
                j += 1;
            }
        }
        if fields.is_empty() {
            panic!("kv_index[{ident}]: fields must not be empty");
        }
        out.push(IdxDecl {
            ident,
            fields,
            includes,
        });
    }
    out
}

/// Payload-side field encoder triple: u8/u16/u32/u64 BE and `[u8; N]`.
/// Variable-length types (String etc.) go through the index variable-length
/// regime later; they are rejected here for now.
struct FieldEnc {
    ident: syn::Ident,
    enc: TS2,
    dec: TS2,
    width: TS2,
}

fn field_encoders(named: &syn::FieldsNamed, ctx: &str) -> Vec<FieldEnc> {
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
        fs.push(FieldEnc {
            ident: id,
            enc,
            dec,
            width,
        });
    }
    fs
}

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let row_name = &input.ident;

    // #[kv_ref(KeyType)] — the identity struct the row hangs off.
    let key_ty: syn::Type = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("kv_ref") {
                Some(a.parse_args::<syn::Type>().unwrap())
            } else {
                None
            }
        })
        .expect("missing #[kv_ref(KeyType)]");

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => panic!("RowEncode only supports structs with named fields"),
        },
        _ => panic!("RowEncode only supports structs"),
    };
    let fs = field_encoders(named, "RowEncode");
    let names: Vec<_> = fs.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();
    let widths: Vec<_> = fs.iter().map(|f| &f.width).collect();

    // Row impl: encode_payload emits per-field TLV; decode_payload reads it
    // back field by field.
    let mut tlv_out = quote! {};
    for (i, f) in fs.iter().enumerate() {
        let tag = proc_macro2::Literal::u8_unsuffixed(i as u8);
        let w = &f.width;
        let enc = &f.enc;
        tlv_out.extend(quote! {
            buf.push(#tag);
            buf.extend_from_slice(&(#w as u32).to_be_bytes());
            #enc
        });
    }
    let mut tlv_dec = quote! {};
    for (i, f) in fs.iter().enumerate() {
        let tag = proc_macro2::Literal::u8_unsuffixed(i as u8);
        let id = &f.ident;
        let dec = &f.dec;
        tlv_dec.extend(quote! {
            debug_assert_eq!(b[offset], #tag, "TLV tag mismatch at field {}", stringify!(#id));
            offset += 1;
            let len = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            let _ = len;
            #dec
        });
    }

    // #[kv_index(...)]: slots start at 1 (0 is the primary table) and
    // increment in attribute-declaration order.
    let idx_decls: Vec<IdxDecl> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("kv_index"))
        .flat_map(parse_index_attr)
        .collect();
    let mut index_out = quote! {};
    for (n, idx) in idx_decls.iter().enumerate() {
        let slot_lit = proc_macro2::Literal::u8_unsuffixed(n as u8 + 1);
        let iname = &idx.ident;
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, iname);
        let fields: Vec<&String> = idx.fields.iter().collect();
        let includes: Vec<&String> = idx.includes.iter().collect();
        let slot_doc = format!("{}", n + 1);
        index_out.extend(quote! {
            #[doc = concat!("Access method `", stringify!(#iname), "` over `", stringify!(#key_ty), "` (slot ", #slot_doc, ", ADR-0005/0006).")]
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy, Debug)]
            pub struct #struct_ident;

            impl ::okm::KvIndex for #struct_ident {
                type Key = #key_ty;
                const SLOT: u8 = #slot_lit;
                const FIELDS: &'static [&'static str] = &[#(#fields),*];
                const INCLUDES: &'static [&'static str] = &[#(#includes),*];
            }
        });
    }

    // index_entries: statically expands one encode_entry call per declared
    // #[kv_index] (slot order) — no runtime registry needed; the
    // declaration is the registry.
    let entry_calls = idx_decls.iter().map(|idx| {
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, idx.ident);
        quote! { out.push(<#struct_ident as ::okm::KvIndex>::encode_entry(ns, key)); }
    });
    let row_impl = quote! {
        impl ::okm::Row for #row_name {
            type Key = #key_ty;
            const PAYLOAD_FIELDS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];
            fn encode_payload(&self) -> Vec<u8> {
                let mut buf = Vec::new();
                #tlv_out
                buf
            }
            fn decode_payload(b: &[u8]) -> Self {
                let mut offset = 0usize;
                #tlv_dec
                Self { #(#names),* }
            }
            fn index_entries(key: &Self::Key, ns: u16) -> Vec<Vec<u8>> {
                let mut out = Vec::new();
                #(#entry_calls)*
                out
            }
        }
    };

    quote! {
        #row_impl
        #index_out
    }
    .into()
}
