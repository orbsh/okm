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
//!
//! Additionally, both the key struct and the row struct expose a
//! `FieldDesc` table (name / byte width / primitive kind, declaration
//! order) — the single field list feeding every downstream consumer that
//! needs to lay out or interpret fields without a derive on their side
//! (snapshot columns, Arrow schema, column builders; ADR-0007). The kind
//! enum lives in `okm` core as `okm::FieldType` — dependency-free, so the
//! derive stays pure.

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
/// `String` is the variable-length kind (TLV `len` is the prefix);
/// `Reverse<T>` applies the descending-order bit-flip of `T`.
///
/// `dec_val` is a BLOCK EXPRESSION that reads the field's value from
/// `b[offset..]`, advances `offset` past it, and yields the value — the
/// uniform shape decode needs for the default-filling `if` branches.
struct FieldEnc {
    ident: syn::Ident,
    enc: TS2,
    dec: TS2,
    width: TS2,
    /// TLV frame `len` expression: the declared width for fixed-width kinds,
    /// the actual value byte length for variable-length kinds (`String`).
    len_expr: TS2,
    /// `okm::FieldType` variant path, for the FieldDesc table (None = unsupported).
    kind: Option<TS2>,
    /// Hot/cold split: `true` = fixed-width hot segment (contiguous region
    /// after the row header, O(1) offsets); `false` = variable-width cold
    /// segment (TLV frames, tag = declaration index). Width 0 == cold.
    hot: bool,
    /// Expression producing the field's default value — used when a
    /// payload written by an older layout version lacks this field
    /// (append-only evolution fills the tail with defaults). From
    /// `#[kv_default(expr)]`, else `<T as Default>::default()`.
    default_expr: TS2,
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
        "u8" | "i8" => quote! { ::okm::FieldType::U8 },
        "u16" | "i16" => quote! { ::okm::FieldType::U16 },
        "u32" | "i32" => quote! { ::okm::FieldType::U32 },
        "u64" | "i64" => quote! { ::okm::FieldType::U64 },
        other => panic!("Reverse<{other}>: inner type not on the Reversible whitelist"),
    }
}

fn field_encoders(named: &syn::FieldsNamed, ctx: &str) -> Vec<FieldEnc> {
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
                Some(quote! { ::okm::FieldType::U64 }),
            ),
            "u32" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! {{ let v = u32::from_be_bytes(b[offset..offset+4].try_into().unwrap()); offset += 4; v }},
                quote! { 4 },
                quote! { 4 },
                Some(quote! { ::okm::FieldType::U32 }),
            ),
            "u16" => (
                quote! { buf.extend_from_slice(&self.#id.to_be_bytes()); },
                quote! {{ let v = u16::from_be_bytes(b[offset..offset+2].try_into().unwrap()); offset += 2; v }},
                quote! { 2 },
                quote! { 2 },
                Some(quote! { ::okm::FieldType::U16 }),
            ),
            "u8" => (
                quote! { buf.push(self.#id); },
                quote! {{ let v = b[offset]; offset += 1; v }},
                quote! { 1 },
                quote! { 1 },
                Some(quote! { ::okm::FieldType::U8 }),
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
                    Some(quote! { ::okm::FieldType::FixedBytes }),
                )
            }
            _ if ty_str.starts_with("Reverse<") => {
                // Reverse<T> — bit-flipped descending-order encoding applied
                // at every destination of this field (payload value here,
                // key/index positions rejected in the key macro). Inner type
                // must be on the Reversible whitelist (compile-time check:
                // the generated code calls ::okm::Reversible::rev_encode).
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
                    quote! {{ let v = ::okm::Reverse(#inner_ty::rev_decode(&b[offset..offset+#w])); offset += #w; v }},
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
                        let (raw, n) = <#inner_ty as ::okm::VarIntEnc>::varint_decode(&b[offset..]);
                        offset += n;
                        ::okm::VarInt(raw)
                    }},
                    quote! { 0 },
                    // Variable-length frame: len = actual byte length.
                    quote! { self.#id.encode().len() },
                    Some(quote! { ::okm::FieldType::VarInt }),
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
                        let v = ::okm::Quant::<#p>::decode(&b[offset..offset+8]);
                        offset += 8;
                        v
                    }},
                    quote! { 8 },
                    quote! { 8 },
                    Some(quote! { ::okm::FieldType::Quant(#plit) }),
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
                        let v = ::okm::Enum::<#inner_ty>::decode(&b[offset..offset+1]);
                        offset += 1;
                        v
                    }},
                    quote! { 1 },
                    quote! { 1 },
                    Some(quote! { ::okm::FieldType::Enum }),
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
                    quote! { buf.extend_from_slice(&::okm::offset_encode(self.#id.0, #blit)); },
                    quote! {{ let v = ::okm::Offset(::okm::offset_decode(&b[offset..offset+4], #blit)); offset += 4; v }},
                    quote! { 4 },
                    quote! { 4 },
                    Some(quote! { ::okm::FieldType::Offset(#blit) }),
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
                    Some(quote! { ::okm::FieldType::Str }),
                )
            }
            other => panic!("{ctx}: unsupported type {other} (field {id})"),
        };
        fs.push(FieldEnc {
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

/// FieldDesc table entries: `(name, FieldType, width)`, declaration order.
fn field_desc_entries(fs: &[FieldEnc]) -> TS2 {
    let rows = fs.iter().map(|f| {
        let name = f.ident.to_string();
        let kind = f.kind.as_ref().expect("field kind");
        let w = &f.width;
        quote! { (::okm::FieldDesc { name: #name, ty: #kind, width: #w }) }
    });
    quote! { &[ #(#rows),* ] }
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
    let row_desc = field_desc_entries(&fs);

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
    let ver_lit = proc_macro2::Literal::u8_unsuffixed(layout_version);
    let ver_str = layout_version.to_string();

    // Segment split. Hot = fixed-width fields (contiguous, O(1) offsets);
    // cold = variable-width fields (TLV frames, tag = declaration index
    // over ALL fields so tags stay stable across the hot/cold split).
    let hot_fields: Vec<&FieldEnc> = fs.iter().filter(|f| f.hot).collect();
    let cold_fields: Vec<&FieldEnc> = fs.iter().filter(|f| !f.hot).collect();
    // Hot width as a numeric literal — widths are fixed `quote!{ N }`
    // tokens, so parsing the token text is exact for every supported kind.
    let hot_width_total: usize = hot_fields
        .iter()
        .map(|f| f.width.to_string().parse::<usize>().unwrap_or(0))
        .sum();
    let hot_width_lit = proc_macro2::Literal::usize_unsuffixed(hot_width_total);

    // ---- encode: [version u8][hot_len u16 BE][hot segment][cold TLV] ----
    let mut hot_enc = quote! {};
    for f in &hot_fields {
        let enc = &f.enc;
        hot_enc.extend(quote! { #enc });
    }
    let mut cold_enc = quote! {};
    for f in &cold_fields {
        let tag = proc_macro2::Literal::u8_unsuffixed(
            fs.iter().position(|x| std::ptr::eq(x, *f)).unwrap() as u8,
        );
        let len = &f.len_expr;
        let enc = &f.enc;
        cold_enc.extend(quote! {
            buf.push(#tag);
            buf.extend_from_slice(&(#len as u32).to_be_bytes());
            #enc
        });
    }
    let encode_body = if hot_fields.is_empty() && cold_fields.is_empty() {
        // Degenerate: header still present, hot_len 0, cold empty.
        quote! {
            buf.push(#ver_lit);
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
    } else {
        quote! {
            buf.push(#ver_lit);
            buf.extend_from_slice(&0u16.to_be_bytes()); // hot_len placeholder
            let hot_start = buf.len();
            #hot_enc
            let hot_len = (buf.len() - hot_start) as u16;
            buf[1..3].copy_from_slice(&hot_len.to_be_bytes());
            #cold_enc
        }
    };

    // ---- decode: version check, hot walk, cold TLV walk, defaults ----
    // Each field becomes `let <name> = if <present> { <dec_val> } else {
    // <default> };` — dec blocks read from b[offset..], advance offset, and
    // yield the value.
    let mut hot_dec = quote! {};
    for f in &hot_fields {
        let id = &f.ident;
        let dec = &f.dec;
        let dflt = &f.default_expr;
        let w = &f.width;
        hot_dec.extend(quote! {
            let #id = if offset + #w < hot_end + 1 {
                #dec
            } else {
                // Truncated tail: field appended after this payload's
                // hot segment was written → declared default.
                #dflt
            };
        });
    }
    let mut cold_dec = quote! {};
    for f in &cold_fields {
        let tag = proc_macro2::Literal::u8_unsuffixed(
            fs.iter().position(|x| std::ptr::eq(x, *f)).unwrap() as u8,
        );
        let id = &f.ident;
        let dec = &f.dec;
        let dflt = &f.default_expr;
        cold_dec.extend(quote! {
            let #id = if cold_pos < cold_end && b[cold_pos] == #tag {
                cold_pos += 1;
                let len = u32::from_be_bytes(b[cold_pos..cold_pos+4].try_into().unwrap()) as usize;
                cold_pos += 4;
                // dec blocks advance `offset`; alias it to the cold cursor
                // for the duration of the frame, then write the position
                // back (String/VarInt decs move it past the value).
                offset = cold_pos;
                let v = #dec;
                cold_pos = offset;
                v
            } else {
                #dflt
            };
        });
    }
    let decode_body = quote! {
        let mut offset = 0usize;
        let ver = b[0];
        assert!(
            ver <= #ver_lit,
            concat!("payload layout version ", "{:03}", " is newer than this schema's ", #ver_str),
            ver
        );
        let hot_len = u16::from_be_bytes(b[1..3].try_into().unwrap()) as usize;
        offset = 3;
        let hot_end = offset + hot_len;
        #hot_dec
        offset = hot_end;
        let cold_end = b.len();
        let mut cold_pos = offset;
        #cold_dec
        let _ = cold_pos;
    };

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
            const LAYOUT_VERSION: u8 = #ver_lit;
            const PAYLOAD_FIELDS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];
            const FIELDS: &'static [::okm::FieldDesc] = #row_desc;
            const HOT_WIDTH: usize = #hot_width_lit;
            fn encode_payload(&self) -> Vec<u8> {
                let mut buf = Vec::new();
                #encode_body
                buf
            }
            fn decode_payload(b: &[u8]) -> Self {
                #decode_body
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
