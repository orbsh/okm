//! Derive macros for OKM (object-keyspace mapping).
//!
//! Two macros, each a pure single-item function with zero I/O:
//!
//! - `KeyEncode`: fixed-width key encoding. Generates `encode`/`decode`/
//!   `KEY_LEN`/`FIELD_WIDTHS`/`encode_prefix_named`/`prefix_width`.
//! - `EdgeEncode`: bidirectional edges. `#[kv_head(field, ...)]` declares
//!   each endpoint's identity width; generates the `KvEdge` impl plus query
//!   methods on the endpoint types.
//!
//! Namespace IDs come from `#[kv_ns(N)]` and fold at compile time into a
//! 2-byte big-endian header; the dictionary itself lives in code, never in
//! KV (docs/adr/0002). Schema stability is locked by hex assertions in the
//! test suite.
//! 两个 derive 宏：
//!
//! KeyEncode：定宽 key 编码。生成 encode/decode/KEY_LEN/FIELD_WIDTHS/
//!            encode_prefix_named/prefix_width。
//! EdgeEncode：双向边。按 #[kv_head(字段名,...)] 声明各端点的"身份宽度"，
//!             生成 KvEdge impl + 挂在端点类型上的查询方法。
use proc_macro::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, DeriveInput, Fields};

// ==================== KeyEncode ====================

#[proc_macro_derive(KeyEncode, attributes(kv_ns))]
pub fn derive_key_encode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let named = match &input.data {
        syn::Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("KeyEncode 只支持命名字段结构体"),
        },
        _ => panic!("KeyEncode 只支持结构体"),
    };

    // 收集 (字段名, 类型)；支持 u64/u32（定宽大端）与 [u8; N]
    struct F {
        ident: syn::Ident,
        enc: proc_macro2::TokenStream,
        dec: proc_macro2::TokenStream,
        width: proc_macro2::TokenStream,
    }
    let mut fs = Vec::new();
    for f in named.iter() {
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
                    .expect("[u8; N] 的 N 须为整数字面量");
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
            other => panic!("KeyEncode 暂不支持类型 {other}（字段 {id}）"),
        };
        fs.push(F {
            ident: id,
            enc,
            dec,
            width,
        });
    }

    let names: Vec<_> = fs.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();
    let encs: Vec<_> = fs.iter().map(|f| &f.enc).collect();
    let decs: Vec<_> = fs.iter().map(|f| &f.dec).collect();
    let widths: Vec<_> = fs.iter().map(|f| &f.width).collect();

    // encode_prefix_named：切片模式 match —— 每个「声明序前缀组合」一个臂。
    // pattern 匹配即校验（失败落 _ 臂 panic），各臂宽度是生成期写死的常量和。
    let mut prefix_arms = quote! {};
    for (i, _) in fs.iter().enumerate() {
        // 本臂的 pattern：names 恰好等于前 i+1 个字段名（&str 字面量切片模式）
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

    // prefix_width：与 encode_prefix_named 相同的臂结构，只返回常量宽度
    let mut width_arms = quote! {};
    for (i, _) in fs.iter().enumerate() {
        let pat: Vec<_> = name_strs[..=i].iter().map(|s| quote! { #s }).collect();
        let width_sum: Vec<_> = widths[..=i].to_vec();
        width_arms.extend(quote! {
            &[ #(#pat),* ] => 0 #(+ #width_sum)*,
        });
    }
    width_arms.extend(quote! { _ => panic!("invalid prefix: {names:?}") });

    let key_impl = quote! {
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
            /// 按声明序前 n 个字段编码（数字版，前缀扫描常用）
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
    };

    key_impl.into()
}

// ==================== RowEncode ====================

/// `#[kv_index(idx_name { fields(a, b), includes(c) })]` — 类 struct 体语法，
/// syn::Meta 不覆盖，按 token 流解析：Ident + 大括号组；组内是
/// (fields|includes) + 圆括号组，逗号分隔多个索引。
struct IdxDecl {
    ident: syn::Ident,
    fields: Vec<String>,
    includes: Vec<String>,
}
fn parse_index_attr(attr: &syn::Attribute) -> Vec<IdxDecl> {
    let mut out = Vec::new();
    let ts: Vec<proc_macro2::TokenTree> = attr.to_token_stream().into_iter().collect();
    // ts 形如 `#[kv_index(…)]` → 顶层 Bracket 组；先拆开，再取内层
    // Parenthesis 组（属性参数）。
    let outer = ts
        .iter()
        .find_map(|t| match t {
            proc_macro2::TokenTree::Group(g)
                if g.delimiter() == proc_macro2::Delimiter::Bracket =>
            {
                Some(g.stream())
            }
            _ => None,
        })
        .expect("kv_index：缺少属性括号");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            proc_macro2::TokenTree::Group(g)
                if g.delimiter() == proc_macro2::Delimiter::Parenthesis =>
            {
                Some(g.stream())
            }
            _ => None,
        })
        .expect("kv_index：缺少参数括号");
    let ts: Vec<proc_macro2::TokenTree> = body.into_iter().collect();
    let mut i = 0usize;
    while i < ts.len() {
        // 跳过索引之间的逗号
        if matches!(&ts[i], proc_macro2::TokenTree::Punct(p) if p.as_char() == ',') {
            i += 1;
            continue;
        }
        // 期待：idx_name（Ident）
        let ident = match &ts[i] {
            proc_macro2::TokenTree::Ident(id) => id.clone(),
            t => panic!("kv_index：期待索引名 Ident，得到 {t}"),
        };
        i += 1;
        // 期待：{ … }（大括号组）
        let body = match ts.get(i) {
            Some(proc_macro2::TokenTree::Group(g))
                if g.delimiter() == proc_macro2::Delimiter::Brace =>
            {
                g.stream()
            }
            t => panic!("kv_index[{ident}]：期待 {{ fields(…) }} 块，得到 {t:?}"),
        };
        i += 1;
        // 组内：fields(a, b), includes(c) —— Ident + 圆括号组，逗号分隔
        let mut fields = Vec::new();
        let mut includes = Vec::new();
        let toks: Vec<proc_macro2::TokenTree> = body.into_iter().collect();
        let mut j = 0usize;
        while j < toks.len() {
            let kw = match &toks[j] {
                proc_macro2::TokenTree::Ident(id) => id.to_string(),
                t => panic!("kv_index[{ident}]：期待 fields/includes，得到 {t}"),
            };
            let list: Vec<String> = match toks.get(j + 1) {
                Some(proc_macro2::TokenTree::Group(g))
                    if g.delimiter() == proc_macro2::Delimiter::Parenthesis =>
                {
                    g.stream()
                        .into_iter()
                        .filter_map(|t| match t {
                            proc_macro2::TokenTree::Ident(id) => Some(id.to_string()),
                            proc_macro2::TokenTree::Punct(_) => None,
                            t => panic!("kv_index[{ident}].{kw}: 非法 token {t}"),
                        })
                        .collect()
                }
                t => panic!("kv_index[{ident}].{kw}: 期待圆括号组，得到 {t:?}"),
            };
            match kw.as_str() {
                "fields" => fields = list,
                "includes" => includes = list,
                other => panic!("kv_index[{ident}]：未知键 {other}（支持 fields/includes）"),
            }
            j += 2;
            // 跳过尾随逗号
            if matches!(toks.get(j), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == ',') {
                j += 1;
            }
        }
        if fields.is_empty() {
            panic!("kv_index[{ident}]：fields 不能为空");
        }
        out.push(IdxDecl {
            ident,
            fields,
            includes,
        });
    }
    out
}

/// Payload field encoder triple, shared by KeyEncode (identity side) and
/// RowEncode (payload side). Supported: u8/u16/u32/u64 BE, [u8; N].
/// Variable-length types (String etc.) go through the index variable-length
/// regime later; they are rejected here for now.
struct FieldEnc {
    ident: syn::Ident,
    enc: proc_macro2::TokenStream,
    dec: proc_macro2::TokenStream,
    width: proc_macro2::TokenStream,
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
                    .expect("[u8; N] 的 N 须为整数字面量");
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
            other => panic!("{ctx} 暂不支持类型 {other}（字段 {id}）"),
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

#[proc_macro_derive(RowEncode, attributes(kv_ref, kv_index))]
pub fn derive_row_encode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let row_name = &input.ident;

    // #[kv_ref(KeyType)] — 行挂靠的 key（身份）结构体
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
        .expect("必须标注 #[kv_ref(KeyType)]");

    let named = match &input.data {
        syn::Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => panic!("RowEncode 只支持命名字段结构体"),
        },
        _ => panic!("RowEncode 只支持结构体"),
    };
    let fs = field_encoders(named, "RowEncode");
    let names: Vec<_> = fs.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();
    let widths: Vec<_> = fs.iter().map(|f| &f.width).collect();

    // 生成 Row impl：encode_payload 逐字段 TLV；decode_payload 逐字段读回。
    // TLV 每字段 [tag u8][len u32 BE][value BE]，tag = 字段声明序号（行内唯一，
    // 解耦字段名）；len 对定宽字段是冗余校验，为后续变长字段留同一框架。
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

    // #[kv_index(...)]：slot 从 1 起（0 保留给主表），按属性出现顺序递增。
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

    // index_entries：静态展开每个 #[kv_index] 的 encode_entry 调用（slot 序），
    // put/delete 无需运行时注册表——声明即注册表。
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

// ==================== EdgeEncode ====================

#[proc_macro_derive(EdgeEncode, attributes(kv_ns, kv_head))]
pub fn derive_edge(input: TokenStream) -> TokenStream {
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
        .expect("必须标注 #[kv_ns(N)]");

    let named = match &input.data {
        syn::Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => panic!("只支持命名字段结构体"),
        },
        _ => panic!("只支持结构体"),
    };
    let fields: Vec<_> = named.iter().collect();
    if fields.len() != 2 {
        panic!("EdgeEncode 恰好需要两个字段（起点、终点）");
    }
    let (fa, fb) = (&fields[0], &fields[1]);
    let (ta, tb) = (&fa.ty, &fb.ty);
    let ia = fa.ident.clone().unwrap();
    let ib = fb.ident.clone().unwrap();

    // #[kv_head(字段名, ...)] → &["a", "b"]；无属性 = 全量身份（空 slice）
    fn head_names(field: &syn::Field) -> Vec<String> {
        field
            .attrs
            .iter()
            .find_map(|a| {
                if a.path().is_ident("kv_head") {
                    let list: syn::punctuated::Punctuated<syn::Ident, syn::Token![,]> = a
                        .parse_args_with(syn::punctuated::Punctuated::parse_terminated)
                        .expect("kv_head 格式：#[kv_head(字段名, ...)]");
                    Some(list.iter().map(|i| i.to_string()).collect())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
    let head_a = head_names(fa);
    let head_b = head_names(fb);
    let head_a_strs = &head_a;
    let head_b_strs = &head_b;

    // 方法名：对方字段 role_id → get_role
    let strip = |id: &syn::Ident| -> syn::Ident {
        let s = id.to_string();
        let base = s.strip_suffix("_id").unwrap_or(&s);
        format_ident!("get_{}", base)
    };
    let m_on_a = strip(&ib); // UserKey 上的方法，名字取对方字段
    let m_on_b = strip(&ia); // RoleKey 上的方法，名字取对方字段

    let a_trait = format_ident!("{}_Ops", edge_name);
    let b_trait = format_ident!("{}ColR", edge_name);

    quote! {
        impl ::okm::KvEdge for #edge_name {
            type A = #ta;
            type B = #tb;
            const NS: u16 = #ns;
            const A_HEAD: &'static [&'static str] = &[#(#head_a_strs),*];
            const B_HEAD: &'static [&'static str] = &[#(#head_b_strs),*];
            fn a(&self) -> &Self::A { &self.#ia }
            fn b(&self) -> &Self::B { &self.#ib }
            fn from_parts(a: Self::A, b: Self::B) -> Self {
                Self { #ia: a, #ib: b }
            }
        }

        /// 挂在起点端点上的查询方法
        pub trait #a_trait {
            fn #m_on_a<S: ::okm::KvEngine>(
                &self,
                c: &::okm::EdgeTable<S, #edge_name>,
            ) -> Vec<#tb>;
        }
        impl #a_trait for #ta {
            fn #m_on_a<S: ::okm::KvEngine>(
                &self,
                c: &::okm::EdgeTable<S, #edge_name>,
            ) -> Vec<#tb> {
                c.forward(self)
            }
        }

        /// 挂在终点端点上的查询方法
        pub trait #b_trait {
            fn #m_on_b<S: ::okm::KvEngine>(
                &self,
                c: &::okm::EdgeTable<S, #edge_name>,
            ) -> Vec<::okm::PrefixKey<#ta>>;
        }
        impl #b_trait for #tb {
            fn #m_on_b<S: ::okm::KvEngine>(
                &self,
                c: &::okm::EdgeTable<S, #edge_name>,
            ) -> Vec<::okm::PrefixKey<#ta>> {
                c.reverse_raw(self)
                    .into_iter()
                    .map(|suffix| {
                        ::okm::PrefixKey {
                            decoded: <#ta as ::okm::KeyEncode>::decode(&suffix),
                            taken: <#ta as ::okm::KeyEncode>::FIELD_WIDTHS
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
