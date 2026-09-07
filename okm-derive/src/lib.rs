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
use quote::{format_ident, quote};
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
        fs.push(F { ident: id, enc, dec, width });
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
        let pat: Vec<_> = name_strs[..=i]
            .iter()
            .map(|s| quote! { #s })
            .collect();
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
                    let list: syn::punctuated::Punctuated<syn::Ident, syn::Token![,]> =
                        a.parse_args_with(
                            syn::punctuated::Punctuated::parse_terminated,
                        )
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
                c: &::okm::Collection<S, #edge_name>,
            ) -> Vec<#tb>;
        }
        impl #a_trait for #ta {
            fn #m_on_a<S: ::okm::KvEngine>(
                &self,
                c: &::okm::Collection<S, #edge_name>,
            ) -> Vec<#tb> {
                c.forward(self)
            }
        }

        /// 挂在终点端点上的查询方法
        pub trait #b_trait {
            fn #m_on_b<S: ::okm::KvEngine>(
                &self,
                c: &::okm::Collection<S, #edge_name>,
            ) -> Vec<::okm::PrefixKey<#ta>>;
        }
        impl #b_trait for #tb {
            fn #m_on_b<S: ::okm::KvEngine>(
                &self,
                c: &::okm::Collection<S, #edge_name>,
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
