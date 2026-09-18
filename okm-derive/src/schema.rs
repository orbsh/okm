//! Schema IR for `DocumentEncode` — parse once, emit many.
//!
//! `parse_schema` turns the derive input into a validated `DocumentSchema`
//! (attributes, payload field encoders, index declarations). All attribute
//! parsing and all compile-time validation happens here; the emit functions
//! in `document_encode.rs` consume the schema and only generate code.
//!
//! The IR holds pre-compiled `TokenStream` fragments (enc/dec/width): the
//! macro's discipline is mechanical expansion of declaration info, and the
//! per-kind encoding snippets ARE that expansion — re-deriving them as a
//! structured enum would only move `field_encoders`'s match without
//! changing its nature.

use proc_macro2::{Delimiter, TokenStream as TS2, TokenTree};
use quote::{quote, ToTokens};
use syn::{Data, DeriveInput, Fields};
/// One parsed `#[ok_index(...)]` declaration. The attribute body uses
/// struct-ish syntax that `syn::Meta` does not cover, so it is parsed at
/// the token-stream level: `Ident` + brace group, with `(fields|includes|
/// key)` paren groups inside, comma-separated across multiple indexes.
pub(crate) struct IdxDecl {
    pub ident: syn::Ident,
    pub fields: Vec<String>,
    pub includes: Vec<String>,
    pub key: Vec<String>,
    /// Function-index function path (empty = plain field index): the
    /// single token inside `func(...)` is spliced verbatim into the
    /// generated `KvIndex` impl, which calls it as `#path(&document)`.
    pub func: String,
    /// `deprecated` flag on the declaration: the slot stays reserved
    /// (declaration order is a persistent contract — removing the entry
    /// would shift every later slot onto stale data), but no write path,
    /// scan surface, or marker struct is generated. Stale entries from
    /// before the deprecation are cleared by `Collection::prune_deprecated_slots`.
    pub deprecated: bool,
}

pub(crate) fn parse_index_attr(attr: &syn::Attribute) -> Vec<IdxDecl> {
    let mut out = Vec::new();
    let ts: Vec<TokenTree> = attr.to_token_stream().into_iter().collect();
    // Shape: `#[ok_index(…)]` — take the top-level Bracket group, then the
    // inner Parenthesis group (the attribute arguments).
    let outer = ts
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => Some(g.stream()),
            _ => None,
        })
        .expect("ok_index: missing attribute brackets");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
            _ => None,
        })
        .expect("ok_index: missing argument parentheses");
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
            t => panic!("ok_index: expected index name Ident, got {t}"),
        };
        i += 1;
        // Optional `deprecated` marker BEFORE the brace group.
        let mut deprecated = matches!(&ts.get(i), Some(TokenTree::Ident(id)) if id == "deprecated");
        if deprecated {
            i += 1;
        }
        // Expect: { … } brace group.
        let body = match ts.get(i) {
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
            t => panic!("ok_index[{ident}]: expected {{ fields(…) }} block, got {t:?}"),
        };
        i += 1;
        // Optional `deprecated` marker AFTER the brace group (trailing) —
        // skip the separating comma first.
        if matches!(&ts.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
            i += 1;
        }
        if !deprecated && matches!(&ts.get(i), Some(TokenTree::Ident(id)) if id == "deprecated") {
            deprecated = true;
            i += 1;
        }

        // Inside: fields(a, b), includes(c) — Ident + paren group pairs,
        // comma-separated.
        let mut fields = Vec::new();
        let mut includes = Vec::new();
        let mut key = Vec::new();
        let mut func = String::new();
        let toks: Vec<TokenTree> = body.into_iter().collect();
        let mut j = 0usize;
        while j < toks.len() {
            let kw = match &toks[j] {
                TokenTree::Ident(id) => id.to_string(),
                t => panic!("ok_index[{ident}]: expected fields/includes/key, got {t}"),
            };
            let list: Vec<String> = match toks.get(j + 1) {
                Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => g
                    .stream()
                    .into_iter()
                    .filter_map(|t| match t {
                        TokenTree::Ident(id) => Some(id.to_string()),
                        TokenTree::Punct(_) => None,
                        t => panic!("ok_index[{ident}].{kw}: illegal token {t}"),
                    })
                    .collect(),
                t => panic!("ok_index[{ident}].{kw}: expected paren group, got {t:?}"),
            };
            match kw.as_str() {
                "fields" => fields = list,
                "includes" => includes = list,
                "key" => key = list,
                "func" => {
                    // func(path) — the function path spliced verbatim into
                    // the generated impl (called as `path(&document)`).
                    if list.len() != 1 {
                        panic!("ok_index[{ident}].func: expected exactly one function path");
                    }
                    func = list[0].clone();
                }
                other => {
                    panic!(
                        "ok_index[{ident}]: unknown key {other} (supported: fields/includes/key/func)"
                    )
                }
            }
            j += 2;
            // Skip trailing comma.
            if matches!(toks.get(j), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
                j += 1;
            }
        }
        // Function-index regime: the entry's sort segment is the function
        // result, not payload fields — fields/includes would have no place
        // in the wire. `fields` stays empty there, so the emptiness check
        // applies only to plain field indexes.
        if !func.is_empty() {
            if !fields.is_empty() || !includes.is_empty() {
                panic!(
                    "ok_index[{ident}]: func(...) and fields/includes are exclusive — the function result IS the sort segment"
                );
            }
        } else if fields.is_empty() && !deprecated {
            panic!("ok_index[{ident}]: fields must not be empty");
        }
        // key(...) prefix validation happens at encode time (the generated
        // encode_prefix_named match panics on non-prefix names), same
        // discipline as the edge macro's ok_head.
        out.push(IdxDecl {
            ident,
            fields,
            includes,
            key,
            func,
            deprecated,
        });
    }
    out
}

/// Parse `#[ok_reduce(name { group(a, b) })]` — same Ident + brace
/// shape as one `ok_index` declaration, only the keyword set differs.
pub(crate) fn parse_reduce_attr(attr: &syn::Attribute) -> ReduceDecl {
    let ts: Vec<TokenTree> = attr.to_token_stream().into_iter().collect();
    let outer = ts
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => Some(g.stream()),
            _ => None,
        })
        .expect("ok_reduce: missing attribute brackets");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
            _ => None,
        })
        .expect("ok_reduce: missing argument parentheses");
    let toks: Vec<TokenTree> = body.into_iter().collect();

    let ident = match toks.first() {
        Some(TokenTree::Ident(id)) => id.clone(),
        t => panic!("ok_reduce: expected reduce name Ident, got {t:?}"),
    };
    let group_body = match toks.get(1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
        t => panic!("ok_reduce[{ident}]: expected {{ group(…) }} block, got {t:?}"),
    };
    let gtoks: Vec<TokenTree> = group_body.into_iter().collect();
    let kw = match gtoks.first() {
        Some(TokenTree::Ident(id)) => id.to_string(),
        t => panic!("ok_reduce[{ident}]: expected group(...), got {t:?}"),
    };
    if kw != "group" {
        panic!("ok_reduce[{ident}]: unknown key {kw} (supported: group)");
    }
    let group: Vec<String> = match gtoks.get(1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => g
            .stream()
            .into_iter()
            .filter_map(|t| match t {
                TokenTree::Ident(id) => Some(id.to_string()),
                TokenTree::Punct(_) => None,
                t => panic!("ok_reduce[{ident}].group: illegal token {t}"),
            })
            .collect(),
        t => panic!("ok_reduce[{ident}]: expected paren group, got {t:?}"),
    };
    if group.is_empty() {
        panic!("ok_reduce[{ident}]: group must not be empty — a global single-group reduce has no group key to scan by");
    }
    ReduceDecl {
        logic: ident.to_string(),
        ident,
        group,
    }
}

/// Is this payload field variable-width (no static width)? Width-0 kinds:
/// `String` (raw UTF-8 in the index segment) and `VarInt<T>` (LEB128). Both
/// are cold-segment TLV fields in the payload; in an index segment they are
/// naked bytes with no frame, hence the last-position rule.
fn variable_width(fs: &[FieldSchema], name_strs: &[String], n: &str) -> bool {
    fs.iter()
        .zip(name_strs)
        .find(|(_, s)| s.as_str() == n)
        .map(|(f, _)| f.width.to_string() == "0")
        .unwrap_or(false)
}

/// Payload-side field encoder record: u8/u16/u32/u64 BE and `[u8; N]`.
/// `String` is the variable-length kind (TLV `len` is the prefix);
/// `Reverse<T>` applies the descending-order bit-flip of `T`.
///
/// `dec_val` is a BLOCK EXPRESSION that reads the field's value from
/// `b[offset..]`, advances `offset` past it, and yields the value — the
/// uniform shape decode needs for the default-filling `if` branches.
pub(crate) struct FieldSchema {
    pub ident: syn::Ident,
    /// Rust type as written (normalized string), for the document-map bridge
    /// to emit exact casts (`DynamicValue::UInt` -> `u32 as u32` etc.).
    pub ty_str: String,
    pub enc: TS2,
    pub dec: TS2,
    pub width: TS2,
    /// TLV frame `len` expression: the declared width for fixed-width kinds,
    /// the actual value byte length for variable-length kinds (`String`).
    pub len_expr: TS2,
    /// `okm_core::FieldType` variant path, for the FieldDesc table (None = unsupported).
    pub kind: Option<TS2>,
    /// Hot/cold split: `true` = fixed-width hot segment (contiguous region
    /// after the document header, O(1) offsets); `false` = variable-width cold
    /// segment (TLV frames, tag = declaration index). Width 0 == cold.
    pub hot: bool,
    /// Expression producing the field's default value — used when a
    /// payload written by an older layout version lacks this field
    /// (append-only evolution fills the tail with defaults). From
    /// `#[ok_default(expr)]`, else `<T as Default>::default()`.
    pub default_expr: TS2,
    /// Exportable const literal (Some only when #[ok_default] is a plain
    /// literal or `"x".to_string()`); feeds the Document::DEFAULTS const for
    /// the dynamic reader's version migration.
    pub default_lit: Option<TS2>,
    /// `#[ok_len(N)]` — expected element count of a `Vector<T>` field
    /// (embedding dims known to the app). A decode-time check, not a wire
    /// constraint: the frame carries its own count. None = unrestricted.
    pub ok_len: Option<usize>,
}

/// Encoded width of a plain primitive type name (for `Reverse<T>` fields —
/// same width as the unwrapped encoding).
fn inner_w(ty: &str) -> TS2 {
    match ty {
        "u8" | "i8" => quote! { 1 },
        "u16" | "i16" => quote! { 2 },
        "u32" | "i32" => quote! { 4 },
        "u64" | "i64" => quote! { 8 },
        other => panic!("Reverse<{other}>: inner type not on the Reversible whitelist"),
    }
}

/// FieldType of a plain primitive type name (Reverse keeps the inner kind —
/// the wire is still a fixed-width integer, just bit-flipped).
fn inner_kind(ty: &str) -> TS2 {
    match ty {
        "u8" | "i8" => quote! { ::okm_core::FieldType::U8 },
        "u16" | "i16" => quote! { ::okm_core::FieldType::U16 },
        "u32" | "i32" => quote! { ::okm_core::FieldType::U32 },
        "u64" | "i64" => quote! { ::okm_core::FieldType::U64 },
        other => panic!("Reverse<{other}>: inner type not on the Reversible whitelist"),
    }
}

fn field_encoders(named: &syn::FieldsNamed, ctx: &str) -> Vec<FieldSchema> {
    let mut fs = Vec::new();
    for f in named.named.iter() {
        let id = f.ident.clone().unwrap();
        let ty = &f.ty;
        let ty_str = quote!(#ty).to_string().replace(' ', "");
        // #[ok_default(expr)] or #[ok_default = expr] — value used when an
        // older-layout payload lacks this field (append-only schema
        // evolution). Optional; fallback is `<T as Default>::default()`.
        let ok_default: Option<TS2> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("ok_default"))
            .map(|a| {
                let mut e = None;
                let _ = a.parse_nested_meta(|meta| {
                    if meta.path.is_ident("expr") {
                        e = Some(meta.value()?.parse::<syn::Expr>()?);
                    }
                    Ok(())
                });
                e.map(|x| quote! { #x })
                    .or_else(|| a.parse_args::<syn::Expr>().ok().map(|x| quote! { #x }))
                    .expect("ok_default: expected `#[ok_default(expr)]` or `#[ok_default = expr]`")
            });
        // #[ok_len(N)] — Vector element-count expectation (decode check).
        let ok_len: Option<usize> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("ok_len"))
            .map(|a| {
                a.parse_args::<syn::LitInt>()
                    .expect("ok_len: expected `#[ok_len(N)]`")
                    .base10_parse()
                    .expect("ok_len: N must be a usize literal")
            });
        // Literal detection: `#[ok_default(3)]` etc. exports as data for
        // the dynamic reader; non-literal exprs stay Rust-only.
        let ok_default_lit: Option<TS2> = ok_default.as_ref().and_then(|e| {
            syn::parse2::<syn::Expr>(e.clone())
                .ok()
                .and_then(|x| match x {
                    syn::Expr::Lit(l) => match l.lit {
                        syn::Lit::Int(i) => Some(quote! { ::okm_core::field::DefaultValueConst::I64(#i) }),
                        syn::Lit::Float(fl) => {
                            Some(quote! { ::okm_core::field::DefaultValueConst::F64(#fl) })
                        }
                        syn::Lit::Bool(b) => {
                            Some(quote! { ::okm_core::field::DefaultValueConst::Bool(#b) })
                        }
                        syn::Lit::Str(st) => Some(quote! {
                            ::okm_core::field::DefaultValueConst::Str(#st)
                        }),
                        _ => None,
                    },
                    // `"eu".to_string()` on a str literal: the idiomatic
                    // String default — export the inner literal.
                    syn::Expr::MethodCall(mc)
                        if mc.method == "to_string"
                            && matches!(*mc.receiver, syn::Expr::Lit(ref l) if matches!(l.lit, syn::Lit::Str(_))) =>
                    {
                        match *mc.receiver {
                            syn::Expr::Lit(l) => match l.lit {
                                syn::Lit::Str(st) => Some(quote! {
                                    ::okm_core::field::DefaultValueConst::Str(#st)
                                }),
                                _ => None,
                            },
                            _ => None,
                        }
                    }
                    _ => None,
                })
        });
        // #[ok_offset(base = <i64 literal>)] — the Offset wrapper's static
        // base, required exactly when the type is Offset-shaped.
        let offset_base: Option<i64> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("ok_offset"))
            .map(|a| {
                let mut b = None;
                let _ = a.parse_nested_meta(|meta| {
                    if meta.path.is_ident("base") {
                        let v: syn::LitInt = meta.value()?.parse()?;
                        b = Some(v.base10_parse::<i64>().expect("ok_offset: base must be an i64 literal"));
                    }
                    Ok(())
                });
                b.expect("ok_offset: missing `base = <i64>`")
            });
        // #[ok_len(N)] — Vector element-count expectation (decode check).
        let ok_len: Option<usize> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("ok_len"))
            .map(|a| {
                a.parse_args::<syn::LitInt>()
                    .expect("ok_len: expected `#[ok_len(N)]`")
                    .base10_parse()
                    .expect("ok_len: N must be a usize literal")
            });
        let (enc, dec, width, len_expr, kind) = match ty_str.as_str() {
            "u64" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! {{ let v = u64::from_be_bytes(b[offset..offset+8].try_into().unwrap()); offset += 8; v }},
                quote! { 8 },
                quote! { 8 },
                Some(quote! { ::okm_core::FieldType::U64 }),
            ),
            "u32" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! {{ let v = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()); offset += 4; v }},
                quote! { 4 },
                quote! { 4 },
                Some(quote! { ::okm_core::FieldType::U32 }),
            ),
            "u16" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! {{ let v = u16::from_be_bytes(b[offset..offset+2].try_into().unwrap()); offset += 2; v }},
                quote! { 2 },
                quote! { 2 },
                Some(quote! { ::okm_core::FieldType::U16 }),
            ),
            "u8" => (
                quote! { buf.push(self.#id); },
                quote! {{ let v = b[offset]; offset += 1; v }},
                quote! { 1 },
                quote! { 1 },
                Some(quote! { ::okm_core::FieldType::U8 }),
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
                    quote! {{
                        let mut v = [0u8; #nlit];
                        v.copy_from_slice(&b[offset..offset+#nlit]);
                        offset += #nlit;
                        v
                    }},
                    quote! { #nlit },
                    quote! { #nlit },
                    Some(quote! { ::okm_core::FieldType::FixedBytes }),
                )
            }
            _ if ty_str.starts_with("Reverse<") => {
                // Reverse<T> — bit-flipped descending-order encoding applied
                // at every destination of this field (payload value here,
                // key/index positions rejected in the key macro). Inner type
                // must be on the Reversible whitelist (compile-time check:
                // the generated code calls ::okm_core::Reversible::rev_encode).
                let inner = ty_str
                    .trim_start_matches("Reverse<")
                    .trim_end_matches('>')
                    .to_string();
                let inner_ty: syn::Type = syn::parse_str(&inner)
                    .unwrap_or_else(|_| panic!("{ctx}: bad Reverse inner type {inner}"));
                let w = inner_w(&inner);
                let kind = inner_kind(&inner);
                (
                    quote! { buf.extend_from_slice(&self.#id.encode()); },
                    quote! {{ let v = ::okm_core::Reverse(#inner_ty::rev_decode(&b[offset..offset+#w])); offset += #w; v }},
                    quote! { #w },
                    quote! { #w },
                    Some(kind),
                )
            }
            _ if ty_str.starts_with("VarInt<") => {
                // VarInt<T> — LEB128 variable-length unsigned integer.
                // Same regime as String: frame len is authoritative,
                // FieldDesc width is 0. T ∈ {u16, u32, u64} (u8 is already
                // minimal-width).
                let inner = ty_str
                    .trim_start_matches("VarInt<")
                    .trim_end_matches('>')
                    .to_string();
                if !matches!(inner.as_str(), "u16" | "u32" | "u64") {
                    panic!("{ctx}: VarInt<{inner}> unsupported (u16/u32/u64 only)");
                }
                let inner_ty: syn::Type = syn::parse_str(&inner)
                    .unwrap_or_else(|_| panic!("{ctx}: bad VarInt inner type {inner}"));
                (
                    quote! { buf.extend_from_slice(&self.#id.encode()); },
                    quote! {{
                        let (raw, n) = <#inner_ty as ::okm_core::VarIntEnc>::varint_decode(&b[offset..]);
                        offset += n;
                        ::okm_core::VarInt(raw)
                    }},
                    quote! { 0 },
                    // Variable-length frame: len = actual byte length.
                    quote! { self.#id.encode().len() },
                    Some(quote! { ::okm_core::FieldType::VarInt }),
                )
            }
            _ if ty_str.starts_with("Quant<") => {
                // Quant<f64, P> — fixed-point i64 wire (8 bytes, stable
                // width across P). Composes with Reverse for descending
                // float order.
                // Accepted spellings: Quant<P> (inner is always f64) and
                // the explicit Quant<f64, P>. The wire is i64 BE either way.
                let args = ty_str
                    .trim_start_matches("Quant<")
                    .trim_end_matches('>')
                    .to_string();
                let (inner, p_str) = match args.split_once(',') {
                    Some((i, p)) => (i.trim().to_string(), p.trim().to_string()),
                    None => ("f64".to_string(), args),
                };
                let p: u32 = p_str
                    .parse()
                    .unwrap_or_else(|_| panic!("{ctx}: Quant precision P must be an integer literal, got {p_str}"));
                if inner != "f64" {
                    panic!("{ctx}: Quant<{inner}> unsupported (f64 only)");
                }
                let plit = proc_macro2::Literal::u32_unsuffixed(p);
                (
                    quote! { buf.extend_from_slice(&self.#id.encode()); },
                    quote! {{
                        let v = ::okm_core::Quant::<#p>::decode(&b[offset..offset+8]);
                        offset += 8;
                        v
                    }},
                    quote! { 8 },
                    quote! { 8 },
                    Some(quote! { ::okm_core::FieldType::Quant(#plit) }),
                )
            }
            _ if ty_str.starts_with("Vector<") || ty_str.starts_with("Vector <") => {
                // Vector<T> — typed homogeneous list (ADR-0015 §4): a
                // variable-length cold TLV frame, payload = [count u32 BE]
                // + count × element encoding. Element encoding by T:
                // fixed-width scalars are bare V (zero per-element
                // overhead); Str elements carry their own len (LV).
                // #[ok_len(N)] validates the count at decode (a check, not
                // a wire constraint — the frame carries its own count).
                let t_str = ty_str
                    .trim_start_matches("Vector <")
                    .trim_start_matches("Vector<")
                    .trim_end_matches('>')
                    .to_string();
                let t_lit = &t_str;
                let t_ty: syn::Type = syn::parse_str(&t_str)
                    .unwrap_or_else(|e| panic!("{ctx}: bad Vector element type `{t_str}`: {e}"));
                let (elem_enc, elem_dec, elem_fixed): (proc_macro2::TokenStream, proc_macro2::TokenStream, usize) = match t_str.as_str() {
                    "f32" | "u32" | "i32" => (
                        quote! {
                            // works for both an owned buf and a &mut alias:
                            // extend needs no re-borrow of `buf` itself
                            let mut tmp = Vec::with_capacity(4);
                            e.encode_le(&mut tmp);
                            buf.extend_from_slice(&tmp);
                        },
                        quote! {{
                            elems.push(<#t_ty as ::okm_core::VectorElem>::decode_le(&payload[p..p+4]));
                            p += 4;
                        }},
                        4usize,
                    ),
                    "u64" | "i64" => (
                        quote! {
                            let mut tmp = Vec::with_capacity(8);
                            e.encode_le(&mut tmp);
                            buf.extend_from_slice(&tmp);
                        },
                        quote! {{
                            elems.push(<#t_ty as ::okm_core::VectorElem>::decode_le(&payload[p..p+8]));
                            p += 8;
                        }},
                        8usize,
                    ),
                    "String" => (
                        quote! {{
                            let eb = e.as_bytes();
                            ::okm_core::put_len(&mut buf, eb.len());
                            buf.extend_from_slice(eb);
                        }},
                        quote! {{
                            let (l, ln) = ::okm_core::take_len(&payload[p..])
                                .expect("Vector<String> element: truncated length");
                            p += ln;
                            elems.push(String::from_utf8(payload[p..p+l].to_vec())
                                .expect("Vector<String> element is valid UTF-8"));
                            p += l;
                        }},
                        0usize,
                    ),
                    other => panic!("{ctx}: unsupported Vector element type `{other}` (scalars: f32/u32/i32/u64/i64; dynamic-width: String)"),
                };
                // #[ok_len] is an ENCODE-time contract check (the write
                // boundary is where the application's dimension promise is
                // enforced). Decode never checks: bypassing the decoder is
                // the reader's own problem — raw frame bytes are as opaque
                // as an unrendered image. The contract still travels in
                // FieldSchema::expect_len for dynamic readers to enforce.
                let ok_len_check = match ok_len {
                    Some(n) => quote! {
                        if self.#id.elems.len() != #n {
                            panic!(concat!(
                                "Vector count mismatch (#[ok_len] contract): field `",
                                stringify!(#id), "`: expected ", #n, ", found {}"
                            ), self.#id.elems.len());
                        }
                    },
                    None => quote! {},
                };
                // len_expr: frame payload bytes = count prefix + elements
                let len_expr = if elem_fixed > 0 {
                    let fixed_lit = proc_macro2::Literal::usize_unsuffixed(elem_fixed);
                    // fully parenthesized: the cold-loop template casts
                    // `#len as u32`, and `as` binds tighter than `*`.
                    quote! { (4 + self.#id.elems.len() * (#fixed_lit)) }
                } else {
                    quote! { 4 + self.#id.elems.iter().map(|e| 4 + e.len()).sum::<usize>() }
                };
                (
                    // enc: contract check + count prefix + elements (cold
                    // TLV loop adds the frame header; this writes the payload)
                    quote! {{
                        #ok_len_check
                        buf.extend_from_slice(&(self.#id.elems.len() as u32).to_be_bytes());
                        for e in &self.#id.elems { #elem_enc }
                    }},
                    // dec: count, optional ok_len check, per-element decode
                    quote! {{
                        let mut count_bytes = [0u8; 4];
                        count_bytes.copy_from_slice(&b[offset..offset+4]);
                        let count = u32::from_be_bytes(count_bytes) as usize;
                        offset += 4;
                        let payload = &b[offset..offset + len - 4];
                        let mut elems = Vec::with_capacity(count);
                        let mut p = 0usize;
                        for _ in 0..count { #elem_dec }
                        offset += len - 4;
                        ::okm_core::Vector { elems }
                    }},
                    // width 0 = cold (variable length)
                    quote! { 0 },
                    len_expr,
                    Some(quote! { ::okm_core::FieldType::Vector { elem: #t_lit } }),
                )
            }
            _ if ty_str.starts_with("Ref<") => {
                // Ref<D, K> — child-document by key reference. Wire =
                // K::encode() (fixed width, hot segment); the child payload
                // lives at its own key (written by Collection::put's embed
                // pass). The in-memory `value` never touches this wire.
                let generics = ty_str
                    .trim_start_matches("Ref<")
                    .trim_end_matches('>')
                    .to_string();
                // Split "D, K" on the top-level comma (D/K are type names;
                // nesting deeper generics inside Ref is not supported
                // — the child must be a plain document type).
                let (_d_ty, k_ty) = match generics.rsplit_once(',') {
                    Some((d, k)) => (d.trim().to_string(), k.trim().to_string()),
                    None => panic!("{ctx}: Ref requires <D, K> (field {id})"),
                };
                let k_ty_parsed: syn::Type = syn::parse_str(&k_ty)
                    .unwrap_or_else(|e| panic!("{ctx}: bad Ref key type `{k_ty}`: {e}"));
                (
                    // enc: the child key bytes only
                    quote! { buf.extend_from_slice(&self.#id.key.encode()); },
                    // dec: key back; value starts None (deref fills it)
                    quote! {{
                        let key = <#k_ty_parsed as ::okm_core::KeyEncode>::decode(&b[offset..offset+<#k_ty_parsed as ::okm_core::KeyEncode>::KEY_LEN]);
                        offset += <#k_ty_parsed as ::okm_core::KeyEncode>::KEY_LEN;
                        ::okm_core::Ref { key, value: None }
                    }},
                    // width expr: static KEY_LEN
                    quote! { <#k_ty_parsed as ::okm_core::KeyEncode>::KEY_LEN },
                    quote! { <#k_ty_parsed as ::okm_core::KeyEncode>::KEY_LEN },
                    Some(quote! { ::okm_core::FieldType::FixedBytes }),
                )
            }
            _ if ty_str.starts_with("Refs<") => {
                // Refs<D, K> — many-form of Ref: wire = variable-length
                // cold frame [count u32 BE][K bytes × count]. Values never
                // touch this wire; Collection::put's embed pass writes
                // children, get's deref pass backfills them.
                let generics = ty_str
                    .trim_start_matches("Refs<")
                    .trim_end_matches('>')
                    .to_string();
                let (_d_ty, k_ty) = match generics.rsplit_once(',') {
                    Some((d, k)) => (d.trim().to_string(), k.trim().to_string()),
                    None => panic!("{ctx}: Refs requires <D, K> (field {id})"),
                };
                let k_parsed: syn::Type = syn::parse_str(&k_ty)
                    .unwrap_or_else(|e| panic!("{ctx}: bad Refs key type `{k_ty}`: {e}"));
                (
                    // enc: [count u32 BE][key bytes × n] — child keys share
                    // one static KEY_LEN (the K type is fixed-width).
                    quote! {{
                        buf.extend_from_slice(&(self.#id.keys.len() as u32).to_be_bytes());
                        for k in &self.#id.keys {
                            buf.extend_from_slice(&k.encode());
                        }
                    }},
                    // dec: count, then keys; values all None (deref fills).
                    quote! {{
                        let n = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()) as usize;
                        offset += 4;
                        let kl = <#k_parsed as ::okm_core::KeyEncode>::KEY_LEN;
                        let mut keys = ::std::vec::Vec::with_capacity(n);
                        for _ in 0..n {
                            keys.push(<#k_parsed as ::okm_core::KeyEncode>::decode(&b[offset..offset+kl]));
                            offset += kl;
                        }
                        ::okm_core::Refs {
                            keys,
                            values: (0..n).map(|_| ::core::option::Option::None).collect(),
                        }
                    }},
                    // width 0 = variable-length → cold TLV.
                    quote! { 0 },
                    // frame length expr: 4 + n * KEY_LEN
                    quote! { (4usize + self.#id.keys.len() * <#k_parsed as ::okm_core::KeyEncode>::KEY_LEN) },
                    Some(quote! { ::okm_core::FieldType::Bytes }),
                )
            }
            _ if ty_str.starts_with("Enum<") => {
                // Enum<T> — one-byte explicit tag via the user's EnumTag
                // impl (tags are a wire contract, never positional).
                let inner = ty_str
                    .trim_start_matches("Enum<")
                    .trim_end_matches('>')
                    .to_string();
                let inner_ty: syn::Type = syn::parse_str(&inner)
                    .unwrap_or_else(|_| panic!("{ctx}: bad Enum inner type {inner}"));
                (
                    quote! { buf.extend_from_slice(&self.#id.encode()); },
                    quote! {{
                        let v = ::okm_core::Enum::<#inner_ty>::decode(&b[offset..offset+1]);
                        offset += 1;
                        v
                    }},
                    quote! { 1 },
                    quote! { 1 },
                    Some(quote! { ::okm_core::FieldType::Enum }),
                )
            }
            _ if ty_str == "Offset" || offset_base.is_some() => {
                // Offset — #[ok_offset(base = N)] i64 fields stored as a u32
                // displacement from the static base. Base and shape must
                // agree: missing either half is a declaration error.
                let base = offset_base
                    .unwrap_or_else(|| panic!("{ctx}: {id} is Offset but lacks #[ok_offset(base = <i64>)]"));
                if ty_str != "Offset" {
                    panic!("{ctx}: {id} has #[ok_offset] but is not an Offset field");
                }
                let blit = proc_macro2::Literal::i64_unsuffixed(base);
                (
                    // Field value is the Offset newtype; the wire is the
                    // u32 displacement. .0 is the absolute i64 value.
                    quote! { buf.extend_from_slice(&::okm_core::offset_encode(self.#id.0, #blit)); },
                    quote! {{ let v = ::okm_core::Offset(::okm_core::offset_decode(&b[offset..offset+4], #blit)); offset += 4; v }},
                    quote! { 4 },
                    quote! { 4 },
                    Some(quote! { ::okm_core::FieldType::Offset(#blit) }),
                )
            }
            _ if ty_str.starts_with("String") => {
                // Variable-length regime: the TLV frame's `len u32` IS the
                // length prefix — no second prefix on the wire. The frame
                // header is emitted by the shared TLV loop, so here we only
                // write/read the raw UTF-8 bytes.
                (
                    quote! { buf.extend_from_slice(self.#id.as_bytes()); },
                    quote! {{
                        let v = String::from_utf8(b[offset..offset+len].to_vec())
                            .expect("TLV String field is valid UTF-8");
                        offset += len;
                        v
                    }},
                    // FieldDesc width is a static concept; the dynamic length
                    // lives in the frame. 0 marks variable length.
                    quote! { 0 },
                    // Variable-length frame: len = actual byte length.
                    quote! { self.#id.as_bytes().len() },
                    Some(quote! { ::okm_core::FieldType::Str }),
                )
            }
            _ if ty_str.starts_with("Vec < u8 >") || ty_str.starts_with("Vec<u8>") => {
                // Bytes — same variable-length TLV frame as String, minus
                // the UTF-8 constraint: raw binary payloads (CBOR, etc.).
                (
                    quote! { buf.extend_from_slice(&self.#id); },
                    quote! {{
                        let v = b[offset..offset+len].to_vec();
                        offset += len;
                        v
                    }},
                    quote! { 0 },
                    quote! { self.#id.len() },
                    Some(quote! { ::okm_core::FieldType::Bytes }),
                )
            }
            other => panic!("{ctx}: unsupported type {other} (field {id})"),
        };
        fs.push(FieldSchema {
            ty_str: ty_str.clone(),
            ident: id,
            enc,
            dec,
            width: width.clone(),
            len_expr,
            kind,
            // Fixed width → hot segment; width 0 (Str/VarInt) → cold.
            hot: width.to_string() != "0",
            default_expr: if ty_str.starts_with("Ref<") || ty_str.starts_with("Ref <") {
                // No Default for Embedded (D is a document): the fallback is
                // a key-default reference — K must implement Default, which
                // every generated key type does.
                let k_ty_str = ty_str
                    .trim_start_matches("Ref <")
                    .trim_start_matches("Ref<")
                    .trim_end_matches('>')
                    .rsplit_once(',')
                    .map(|(_, k)| k.trim().to_string())
                    .expect("embedded: <D, K>");
                let k_ty: syn::Type = syn::parse_str(&k_ty_str)
                    .unwrap_or_else(|e| panic!("{ctx}: bad Ref key type `{k_ty_str}`: {e}"));
                quote! { ::okm_core::Ref { key: <#k_ty as ::core::default::Default>::default(), value: None } }
            } else if ty_str.starts_with("Refs<") {
                quote! { ::okm_core::Refs { keys: ::std::vec::Vec::new(), values: ::std::vec::Vec::new() } }
            } else {
                ok_default
                    .unwrap_or_else(|| quote! { <#ty as ::core::default::Default>::default() })
            },
            default_lit: ok_default_lit,
            ok_len: if ty_str.starts_with("Vector<") || ty_str.starts_with("Vector <") { ok_len } else { None },
        });
    }
    fs
}

/// One `#[ok_reduce(name { group(a, b) })]` declaration. The fold /
/// unfold callbacks and the accumulator type come from a user-implemented
/// `Reduce` impl on a marker struct named `__OkmReduce_{document}_{name}`;
/// this IR only carries the declaration (slot allocation + group fields).
pub(crate) struct ReduceDecl {
    pub ident: syn::Ident,
    /// The user's `ReduceLogic` impl type (the attribute's name token).
    pub logic: String,
    pub group: Vec<String>,
}

/// One `#[ok_subscribe]` declaration: bare only. The event enum name comes
/// from `#[ok_event_enum(Alias)]` (default `RowEvent`); the variant IS the
/// document type name (build.rs derives it — no hand-written mapping, no
/// per-document-type fallback channel; the bare-channel degraded shape is
/// explicitly unsupported).
pub(crate) struct SubDecl {
    /// Generated event enum name for this document's events (`RowEvent` unless
    /// overridden by `#[ok_event_enum]`).
    pub enum_name: String,
}

/// Parse `#[ok_subscribe]` (bare). Any parenthesized argument is rejected:
/// the enum name lives in `#[ok_event_enum]`, not here.
pub(crate) fn parse_subscribe_attr(attr: &syn::Attribute) -> SubDecl {
    if attr.parse_args::<syn::ExprPath>().is_ok() {
        panic!(
            "ok_subscribe: variant paths are not supported — declare `#[ok_subscribe]` \
             (bare); the event enum is `{}` and the variant is the document type name \
             (rename the enum with `#[ok_event_enum(...)]`)",
            DEFAULT_EVENT_ENUM
        );
    }
    SubDecl { enum_name: DEFAULT_EVENT_ENUM.to_string() }
}

pub(crate) const DEFAULT_EVENT_ENUM: &str = "RowEvent";

/// Optional `#[ok_event_enum(Alias)]`: renames the generated event enum
/// this document's events dispatch through. Only read on documents that also carry
/// `#[ok_subscribe]`.
pub(crate) fn parse_event_enum_attr(attr: &syn::Attribute) -> String {
    let ts = match attr.parse_args::<syn::ExprPath>() {
        Ok(p) => p.to_token_stream().to_string().replace(' ', ""),
        Err(_) => panic!("ok_event_enum: expected an enum name, e.g. `#[ok_event_enum(MyEvents)]`"),
    };
    if ts.split("::").count() != 1 {
        panic!("ok_event_enum: expected a bare enum name, got `{ts}`");
    }
    ts
}

/// Whole-macro IR: parse once, consumed by every emit function.
pub(crate) struct DocumentSchema {
    pub row_name: syn::Ident,
    pub key_ty: syn::Type,
    /// `#[ok_ns(N)]` — the document's table namespace. None = not declared
    /// (layout-only document; table-less usage keeps an empty prefix).
    pub ns: Option<u16>,
    /// `#[ok_partition]` / `#[ok_partition(N)]` — the document's table
    /// partition id. None = no partition segment in the key (default;
    /// zero cost for tables without partition needs). Some(id) prepends
    /// a 1-byte segment `[part id]` before the ns header — physical
    /// partition routing (Fjall) and workload isolation in the key
    /// space; engines without partition semantics ignore the physical
    /// split but the key encoding (and thus byte layout) is identical
    /// everywhere. Declared on the document like ok_ns.
    pub partition: Option<u8>,
    pub layout_version: u8,
    /// Payload fields, declaration order. The TLV tag = vec index, so
    /// order is a wire contract here.
    pub fields: Vec<FieldSchema>,
    /// Index declarations, attribute order (slot = position + 1).
    pub indexes: Vec<IdxDecl>,
    /// Reduce declarations, attribute order (slots continue after
    /// the last index — same append-only counter, never reused).
    pub reduces: Vec<ReduceDecl>,
    /// Subscribe declaration — at most one per document type.
    pub subscribe: Option<SubDecl>,
}

/// Parse + validate the derive input. Returns a fully validated schema —
/// the emit functions never panic.
pub(crate) fn parse_schema(input: DeriveInput) -> DocumentSchema {
    let row_name = input.ident.clone();

    // #[ok_ref(KeyType)] — the identity struct the document hangs off.
    let key_ty: syn::Type = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("ok_ref") {
                Some(a.parse_args::<syn::Type>().unwrap())
            } else {
                None
            }
        })
        .expect("missing #[ok_ref(KeyType)]");

    // #[ok_ns(N)] — the table's namespace segment, declared on the document
    // (the document is the table's declaration point: #[ok_ref] pins the key
    // type, so the document determines Collection<S, K, R> entirely). Absent = None.
    let ns: Option<u16> = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("ok_ns") {
                Some(
                    a.parse_args::<syn::LitInt>()
                        .expect("ok_ns format: #[ok_ns(N)]")
                        .base10_parse()
                        .expect("ok_ns must be a u16 literal"),
                )
            } else {
                None
            }
        });

    // #[ok_partition] / #[ok_partition(N)] — the table's partition id.
    // None = no partition segment (default). Declared on the document.
    let partition: Option<u8> = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("ok_partition") {
                Some(
                    a.parse_args::<syn::LitInt>()
                        .ok()
                        .and_then(|l| l.base10_parse::<u8>().ok())
                        .unwrap_or(0),
                )
            } else {
                None
            }
        });
    // `None` attr vs bare `#[ok_partition]` distinction: bare form = Some(0)
    // is NOT wanted (partition 0 = no segment). So: attr present → Some(N)
    // (bare = Some(0) is disallowed to avoid ambiguity — enforce below).

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => panic!("DocumentEncode only supports structs with named fields"),
        },
        _ => panic!("DocumentEncode only supports structs"),
    };
    let fs = field_encoders(named, "DocumentEncode");
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();

    // #[ok_layout(version = N)] — document header layout version. Absent = 1.
    // Bumping it is the signal that the hot/cold field set changed; decode
    // accepts payloads written by any *older* version (append-only rule:
    // new fields go to the tail of their segment, missing ones get their
    // declared default) and rejects anything newer.
    let layout_version: u8 = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("ok_layout"))
        .map(|a| {
            let mut v = None;
            let _ = a.parse_nested_meta(|meta| {
                if meta.path.is_ident("version") {
                    let lit: syn::LitInt = meta.value()?.parse()?;
                    v = Some(lit.base10_parse::<u8>().expect("ok_layout: version must be a u8 literal"));
                }
                Ok(())
            });
            v.expect("ok_layout: expected `version = <u8 literal>`")
        })
        .unwrap_or(1);

    // #[ok_index(...)]: slots start at 1 (0 is the primary table) and
    // increment in attribute-declaration order.
    let idx_decls: Vec<IdxDecl> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("ok_index"))
        .flat_map(parse_index_attr)
        .collect();

    // Validate that every index field/includes name refers to a real document
    // payload field (compile-time; the name list is right here).
    for idx in &idx_decls {
        for n in idx.fields.iter().chain(&idx.includes) {
            if !name_strs.contains(n) {
                panic!("ok_index[{}]: field `{n}` is not a document payload field", idx.ident);
            }
        }
        // Variable-length payload fields (String, VarInt — width 0) have no
        // static width, so a field declared AFTER one cannot be located
        // within the index segment: the variable-length field may appear
        // at most once and must be the LAST field. Fields before it are
        // fixed-width and locate fine (text-first regime, ADR-0005: the
        // trailing primary key still cuts off cleanly from the tail).
        // The `includes` value segment has the same shape, same rule.
        for (list, kw) in [(&idx.fields, "fields"), (&idx.includes, "includes")] {
            let fslice: &[String] = list;
            if let Some((vi, _)) = fslice
                .iter()
                .enumerate()
                .find(|(_, n)| variable_width(&fs, &name_strs, n))
            {
                if vi != fslice.len() - 1 {
                    panic!(
                        "ok_index[{}]: variable-length field `{}` in {kw} must be the last field — fields after it cannot be located (no static width)",
                        idx.ident, list[vi]
                    );
                }
                if fslice[..vi].iter().any(|n| variable_width(&fs, &name_strs, n)) {
                    panic!(
                        "ok_index[{}]: at most one variable-length field allowed in {kw}",
                        idx.ident
                    );
                }
            }
        }
    }

    // #[ok_reduce(name { group(a, b) })]: slots CONTINUE the index
    // counter (append-only, never reused) — slot = last index slot + 1 + n.
    let agg_decls: Vec<ReduceDecl> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("ok_reduce"))
        .map(parse_reduce_attr)
        .collect();
    for red in &agg_decls {
        for n in &red.group {
            if !name_strs.contains(n) {
                panic!("ok_reduce[{}]: group field `{n}` is not a document payload field", red.ident);
            }
        }
    }

    // #[ok_subscribe] (bare) + optional #[ok_event_enum(Alias)] — at most
    // one subscribe per document type (two declarations = one channel send per
    // write, ambiguous shape; reject rather than multiply sends).
    let mut sub_iter = input.attrs.iter().filter(|a| a.path().is_ident("ok_subscribe"));
    let sub_attr = sub_iter.next();
    if sub_iter.next().is_some() {
        panic!("ok_subscribe: duplicate declaration — at most one per document type");
    }
    let sub_decl = sub_attr.map(|a| {
        let mut decl = parse_subscribe_attr(a);
        for attr in input.attrs.iter().filter(|a| a.path().is_ident("ok_event_enum")) {
            decl.enum_name = parse_event_enum_attr(attr);
        }
        decl
    });

    DocumentSchema {
        row_name,
        key_ty,
        ns,
        partition,
        layout_version,
        fields: fs,
        indexes: idx_decls,
        reduces: agg_decls,
        subscribe: sub_decl,
    }
}
