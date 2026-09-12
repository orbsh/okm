//! Schema IR for `RowEncode` — parse once, emit many.
//!
//! `parse_schema` turns the derive input into a validated `RowSchema`
//! (attributes, payload field encoders, index declarations). All attribute
//! parsing and all compile-time validation happens here; the emit functions
//! in `row_encode.rs` consume the schema and only generate code.
//!
//! The IR holds pre-compiled `TokenStream` fragments (enc/dec/width): the
//! macro's discipline is mechanical expansion of declaration info, and the
//! per-kind encoding snippets ARE that expansion — re-deriving them as a
//! structured enum would only move `field_encoders`'s match without
//! changing its nature.

use proc_macro2::{Delimiter, TokenStream as TS2, TokenTree};
use quote::{quote, ToTokens};
use syn::{Data, DeriveInput, Fields};
/// One parsed `#[kv_index(...)]` declaration. The attribute body uses
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
    /// generated `KvIndex` impl, which calls it as `#path(&row)`.
    pub func: String,
}

pub(crate) fn parse_index_attr(attr: &syn::Attribute) -> Vec<IdxDecl> {
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
        let mut key = Vec::new();
        let mut func = String::new();
        let toks: Vec<TokenTree> = body.into_iter().collect();
        let mut j = 0usize;
        while j < toks.len() {
            let kw = match &toks[j] {
                TokenTree::Ident(id) => id.to_string(),
                t => panic!("kv_index[{ident}]: expected fields/includes/key, got {t}"),
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
                "key" => key = list,
                "func" => {
                    // func(path) — the function path spliced verbatim into
                    // the generated impl (called as `path(&row)`).
                    if list.len() != 1 {
                        panic!("kv_index[{ident}].func: expected exactly one function path");
                    }
                    func = list[0].clone();
                }
                other => {
                    panic!(
                        "kv_index[{ident}]: unknown key {other} (supported: fields/includes/key/func)"
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
                    "kv_index[{ident}]: func(...) and fields/includes are exclusive — the function result IS the sort segment"
                );
            }
        } else if fields.is_empty() {
            panic!("kv_index[{ident}]: fields must not be empty");
        }
        // key(...) prefix validation happens at encode time (the generated
        // encode_prefix_named match panics on non-prefix names), same
        // discipline as the edge macro's kv_head.
        out.push(IdxDecl {
            ident,
            fields,
            includes,
            key,
            func,
        });
    }
    out
}

/// Parse `#[kv_reduce(name { group(a, b) })]` — same Ident + brace
/// shape as one `kv_index` declaration, only the keyword set differs.
pub(crate) fn parse_reduce_attr(attr: &syn::Attribute) -> ReduceDecl {
    let ts: Vec<TokenTree> = attr.to_token_stream().into_iter().collect();
    let outer = ts
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => Some(g.stream()),
            _ => None,
        })
        .expect("kv_reduce: missing attribute brackets");
    let body = outer
        .into_iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => Some(g.stream()),
            _ => None,
        })
        .expect("kv_reduce: missing argument parentheses");
    let toks: Vec<TokenTree> = body.into_iter().collect();

    let ident = match toks.first() {
        Some(TokenTree::Ident(id)) => id.clone(),
        t => panic!("kv_reduce: expected reduce name Ident, got {t:?}"),
    };
    let group_body = match toks.get(1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
        t => panic!("kv_reduce[{ident}]: expected {{ group(…) }} block, got {t:?}"),
    };
    let gtoks: Vec<TokenTree> = group_body.into_iter().collect();
    let kw = match gtoks.first() {
        Some(TokenTree::Ident(id)) => id.to_string(),
        t => panic!("kv_reduce[{ident}]: expected group(...), got {t:?}"),
    };
    if kw != "group" {
        panic!("kv_reduce[{ident}]: unknown key {kw} (supported: group)");
    }
    let group: Vec<String> = match gtoks.get(1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => g
            .stream()
            .into_iter()
            .filter_map(|t| match t {
                TokenTree::Ident(id) => Some(id.to_string()),
                TokenTree::Punct(_) => None,
                t => panic!("kv_reduce[{ident}].group: illegal token {t}"),
            })
            .collect(),
        t => panic!("kv_reduce[{ident}]: expected paren group, got {t:?}"),
    };
    if group.is_empty() {
        panic!("kv_reduce[{ident}]: group must not be empty — a global single-group reduce has no group key to scan by");
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
    pub enc: TS2,
    pub dec: TS2,
    pub width: TS2,
    /// TLV frame `len` expression: the declared width for fixed-width kinds,
    /// the actual value byte length for variable-length kinds (`String`).
    pub len_expr: TS2,
    /// `okm_core::FieldType` variant path, for the FieldDesc table (None = unsupported).
    pub kind: Option<TS2>,
    /// Hot/cold split: `true` = fixed-width hot segment (contiguous region
    /// after the row header, O(1) offsets); `false` = variable-width cold
    /// segment (TLV frames, tag = declaration index). Width 0 == cold.
    pub hot: bool,
    /// Expression producing the field's default value — used when a
    /// payload written by an older layout version lacks this field
    /// (append-only evolution fills the tail with defaults). From
    /// `#[kv_default(expr)]`, else `<T as Default>::default()`.
    pub default_expr: TS2,
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
        // #[kv_default(expr)] or #[kv_default = expr] — value used when an
        // older-layout payload lacks this field (append-only schema
        // evolution). Optional; fallback is `<T as Default>::default()`.
        let kv_default: Option<TS2> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("kv_default"))
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
                    .expect("kv_default: expected `#[kv_default(expr)]` or `#[kv_default = expr]`")
            });
        // #[kv_offset(base = <i64 literal>)] — the Offset wrapper's static
        // base, required exactly when the type is Offset-shaped.
        let offset_base: Option<i64> = f
            .attrs
            .iter()
            .find(|a| a.path().is_ident("kv_offset"))
            .map(|a| {
                let mut b = None;
                let _ = a.parse_nested_meta(|meta| {
                    if meta.path.is_ident("base") {
                        let v: syn::LitInt = meta.value()?.parse()?;
                        b = Some(v.base10_parse::<i64>().expect("kv_offset: base must be an i64 literal"));
                    }
                    Ok(())
                });
                b.expect("kv_offset: missing `base = <i64>`")
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
                // Offset — #[kv_offset(base = N)] i64 fields stored as a u32
                // displacement from the static base. Base and shape must
                // agree: missing either half is a declaration error.
                let base = offset_base
                    .unwrap_or_else(|| panic!("{ctx}: {id} is Offset but lacks #[kv_offset(base = <i64>)]"));
                if ty_str != "Offset" {
                    panic!("{ctx}: {id} has #[kv_offset] but is not an Offset field");
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
            other => panic!("{ctx}: unsupported type {other} (field {id})"),
        };
        fs.push(FieldSchema {
            ident: id,
            enc,
            dec,
            width: width.clone(),
            len_expr,
            kind,
            // Fixed width → hot segment; width 0 (Str/VarInt) → cold.
            hot: width.to_string() != "0",
            default_expr: kv_default
                .unwrap_or_else(|| quote! { <#ty as ::core::default::Default>::default() }),
        });
    }
    fs
}

/// One `#[kv_reduce(name { group(a, b) })]` declaration. The fold /
/// unfold callbacks and the accumulator type come from a user-implemented
/// `Reduce` impl on a marker struct named `__OkmReduce_{row}_{name}`;
/// this IR only carries the declaration (slot allocation + group fields).
pub(crate) struct ReduceDecl {
    pub ident: syn::Ident,
    /// The user's `ReduceLogic` impl type (the attribute's name token).
    pub logic: String,
    pub group: Vec<String>,
}

/// One `#[kv_subscribe]` declaration: bare only. The event enum name comes
/// from `#[kv_event_enum(Alias)]` (default `RowEvent`); the variant IS the
/// row type name (build.rs derives it — no hand-written mapping, no
/// per-row-type fallback channel; the bare-channel degraded shape is
/// explicitly unsupported).
pub(crate) struct SubDecl {
    /// Generated event enum name for this row's events (`RowEvent` unless
    /// overridden by `#[kv_event_enum]`).
    pub enum_name: String,
}

/// Parse `#[kv_subscribe]` (bare). Any parenthesized argument is rejected:
/// the enum name lives in `#[kv_event_enum]`, not here.
pub(crate) fn parse_subscribe_attr(attr: &syn::Attribute) -> SubDecl {
    if attr.parse_args::<syn::ExprPath>().is_ok() {
        panic!(
            "kv_subscribe: variant paths are not supported — declare `#[kv_subscribe]` \
             (bare); the event enum is `{}` and the variant is the row type name \
             (rename the enum with `#[kv_event_enum(...)]`)",
            DEFAULT_EVENT_ENUM
        );
    }
    SubDecl { enum_name: DEFAULT_EVENT_ENUM.to_string() }
}

pub(crate) const DEFAULT_EVENT_ENUM: &str = "RowEvent";

/// Optional `#[kv_event_enum(Alias)]`: renames the generated event enum
/// this row's events dispatch through. Only read on rows that also carry
/// `#[kv_subscribe]`.
pub(crate) fn parse_event_enum_attr(attr: &syn::Attribute) -> String {
    let ts = match attr.parse_args::<syn::ExprPath>() {
        Ok(p) => p.to_token_stream().to_string().replace(' ', ""),
        Err(_) => panic!("kv_event_enum: expected an enum name, e.g. `#[kv_event_enum(MyEvents)]`"),
    };
    if ts.split("::").count() != 1 {
        panic!("kv_event_enum: expected a bare enum name, got `{ts}`");
    }
    ts
}

/// Whole-macro IR: parse once, consumed by every emit function.
pub(crate) struct RowSchema {
    pub row_name: syn::Ident,
    pub key_ty: syn::Type,
    /// `#[kv_ns(N)]` — the row's table namespace. None = not declared
    /// (layout-only row; table-less usage keeps an empty prefix).
    pub ns: Option<u16>,
    pub layout_version: u8,
    /// Payload fields, declaration order. The TLV tag = vec index, so
    /// order is a wire contract here.
    pub fields: Vec<FieldSchema>,
    /// Index declarations, attribute order (slot = position + 1).
    pub indexes: Vec<IdxDecl>,
    /// Reduce declarations, attribute order (slots continue after
    /// the last index — same append-only counter, never reused).
    pub reduces: Vec<ReduceDecl>,
    /// Subscribe declaration — at most one per row type.
    pub subscribe: Option<SubDecl>,
}

/// Parse + validate the derive input. Returns a fully validated schema —
/// the emit functions never panic.
pub(crate) fn parse_schema(input: DeriveInput) -> RowSchema {
    let row_name = input.ident.clone();

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

    // #[kv_ns(N)] — the table's namespace segment, declared on the row
    // (the row is the table's declaration point: #[kv_ref] pins the key
    // type, so the row determines Table<S, K, R> entirely). Absent = None.
    let ns: Option<u16> = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("kv_ns") {
                Some(
                    a.parse_args::<syn::LitInt>()
                        .expect("kv_ns format: #[kv_ns(N)]")
                        .base10_parse()
                        .expect("kv_ns must be a u16 literal"),
                )
            } else {
                None
            }
        });

    let named = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f,
            _ => panic!("RowEncode only supports structs with named fields"),
        },
        _ => panic!("RowEncode only supports structs"),
    };
    let fs = field_encoders(named, "RowEncode");
    let name_strs: Vec<_> = fs.iter().map(|f| f.ident.to_string()).collect();

    // #[kv_layout(version = N)] — row header layout version. Absent = 1.
    // Bumping it is the signal that the hot/cold field set changed; decode
    // accepts payloads written by any *older* version (append-only rule:
    // new fields go to the tail of their segment, missing ones get their
    // declared default) and rejects anything newer.
    let layout_version: u8 = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident("kv_layout"))
        .map(|a| {
            let mut v = None;
            let _ = a.parse_nested_meta(|meta| {
                if meta.path.is_ident("version") {
                    let lit: syn::LitInt = meta.value()?.parse()?;
                    v = Some(lit.base10_parse::<u8>().expect("kv_layout: version must be a u8 literal"));
                }
                Ok(())
            });
            v.expect("kv_layout: expected `version = <u8 literal>`")
        })
        .unwrap_or(1);

    // #[kv_index(...)]: slots start at 1 (0 is the primary table) and
    // increment in attribute-declaration order.
    let idx_decls: Vec<IdxDecl> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("kv_index"))
        .flat_map(parse_index_attr)
        .collect();

    // Validate that every index field/includes name refers to a real row
    // payload field (compile-time; the name list is right here).
    for idx in &idx_decls {
        for n in idx.fields.iter().chain(&idx.includes) {
            if !name_strs.contains(n) {
                panic!("kv_index[{}]: field `{n}` is not a row payload field", idx.ident);
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
                        "kv_index[{}]: variable-length field `{}` in {kw} must be the last field — fields after it cannot be located (no static width)",
                        idx.ident, list[vi]
                    );
                }
                if fslice[..vi].iter().any(|n| variable_width(&fs, &name_strs, n)) {
                    panic!(
                        "kv_index[{}]: at most one variable-length field allowed in {kw}",
                        idx.ident
                    );
                }
            }
        }
    }

    // #[kv_reduce(name { group(a, b) })]: slots CONTINUE the index
    // counter (append-only, never reused) — slot = last index slot + 1 + n.
    let agg_decls: Vec<ReduceDecl> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("kv_reduce"))
        .map(parse_reduce_attr)
        .collect();
    for red in &agg_decls {
        for n in &red.group {
            if !name_strs.contains(n) {
                panic!("kv_reduce[{}]: group field `{n}` is not a row payload field", red.ident);
            }
        }
    }

    // #[kv_subscribe] (bare) + optional #[kv_event_enum(Alias)] — at most
    // one subscribe per row type (two declarations = one channel send per
    // write, ambiguous shape; reject rather than multiply sends).
    let mut sub_iter = input.attrs.iter().filter(|a| a.path().is_ident("kv_subscribe"));
    let sub_attr = sub_iter.next();
    if sub_iter.next().is_some() {
        panic!("kv_subscribe: duplicate declaration — at most one per row type");
    }
    let sub_decl = sub_attr.map(|a| {
        let mut decl = parse_subscribe_attr(a);
        for attr in input.attrs.iter().filter(|a| a.path().is_ident("kv_event_enum")) {
            decl.enum_name = parse_event_enum_attr(attr);
        }
        decl
    });

    RowSchema {
        row_name,
        key_ty,
        ns,
        layout_version,
        fields: fs,
        indexes: idx_decls,
        reduces: agg_decls,
        subscribe: sub_decl,
    }
}
