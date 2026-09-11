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
//! enum lives in `okm` core as `okm_core::FieldType` — dependency-free, so the
//! derive stays pure.
//!
//! Structure: parse once into the schema IR (`schema.rs`, which also owns
//! all validation), then one emit function per generated artifact. Adding
//! a generated artifact = adding an `emit_*(&RowSchema)` function; the
//! emit functions share no temporary state.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TS2;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput};

use crate::schema::{parse_schema, RowSchema};

/// One match arm per payload field name — each arm appends that field's
/// raw encoding (no TLV frame; the index segment is a plain
/// concatenation, order = the requested name order). Shared by all
/// index declarations and the inherent `__okm_encode_named` walk.
fn emit_named_walk(schema: &RowSchema) -> TS2 {
    let mut enc_arms = quote! {};
    for f in &schema.fields {
        let fname = f.ident.to_string();
        let enc = &f.enc;
        enc_arms.extend(quote! {
            #fname => { #enc }
        });
    }
    quote! {
        /// Named-field walk over the payload encoders (each arm body
        /// reads `self.<field>`); the access methods call this with the
        /// requested name order. Used by index key/value encoding.
        #[allow(unused_variables, unused_mut, dead_code)]
        pub fn __okm_encode_named(&self, names: &[&str], buf: &mut Vec<u8>) {
            for n in names {
                match *n {
                    #enc_arms
                    other => panic!("unknown field name: {other}"),
                }
            }
        }
    }
}

/// FieldDesc table entries: `(name, FieldType, width)`, declaration order.
fn field_desc_entries(schema: &RowSchema) -> TS2 {
    let rows = schema.fields.iter().map(|f| {
        let name = f.ident.to_string();
        let kind = f.kind.as_ref().expect("field kind");
        let w = &f.width;
        quote! { (::okm_core::FieldDesc { name: #name, ty: #kind, width: #w }) }
    });
    quote! { &[ #(#rows),* ] }
}

/// ---- encode: [version u8][hot_len u16 BE][hot segment][cold TLV] ----
fn emit_payload_encode(schema: &RowSchema) -> TS2 {
    let ver_lit = proc_macro2::Literal::u8_unsuffixed(schema.layout_version);

    // Segment split. Hot = fixed-width fields (contiguous, O(1) offsets);
    // cold = variable-width fields (TLV frames, tag = declaration index
    // over ALL fields so tags stay stable across the hot/cold split).
    let hot_fields: Vec<_> = schema.fields.iter().filter(|f| f.hot).collect();
    let cold_fields: Vec<_> = schema.fields.iter().filter(|f| !f.hot).collect();

    let mut hot_enc = quote! {};
    for f in &hot_fields {
        let enc = &f.enc;
        hot_enc.extend(quote! { #enc });
    }
    let mut cold_enc = quote! {};
    for f in &cold_fields {
        let tag = proc_macro2::Literal::u8_unsuffixed(
            schema.fields.iter().position(|x| std::ptr::eq(x, *f)).unwrap() as u8,
        );
        let len = &f.len_expr;
        let enc = &f.enc;
        cold_enc.extend(quote! {
            buf.push(#tag);
            buf.extend_from_slice(&(#len as u32).to_be_bytes());
            #enc
        });
    }
    if hot_fields.is_empty() && cold_fields.is_empty() {
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
    }
}

/// ---- decode: version check, hot walk, cold TLV walk, defaults ----
/// Each field becomes `let <name> = if <present> { <dec_val> } else {
/// <default> };` — dec blocks read from b[offset..], advance offset, and
/// yield the value.
fn emit_payload_decode(schema: &RowSchema) -> TS2 {
    let ver_lit = proc_macro2::Literal::u8_unsuffixed(schema.layout_version);
    let ver_str = schema.layout_version.to_string();

    let hot_fields: Vec<_> = schema.fields.iter().filter(|f| f.hot).collect();
    let cold_fields: Vec<_> = schema.fields.iter().filter(|f| !f.hot).collect();

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
            schema.fields.iter().position(|x| std::ptr::eq(x, *f)).unwrap() as u8,
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
    quote! {
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
    }
}

/// One marker struct + `KvIndex` impl per declared `#[kv_index]` (slot
/// order).
fn emit_index_structs(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;

    let mut index_out = quote! {};
    for (n, idx) in schema.indexes.iter().enumerate() {
        let slot_lit = proc_macro2::Literal::u8_unsuffixed(n as u8 + 1);
        let iname = &idx.ident;
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, iname);
        let fields: Vec<&String> = idx.fields.iter().collect();
        let includes: Vec<&String> = idx.includes.iter().collect();
        let key_names: Vec<&String> = idx.key.iter().collect();
        let slot_doc = format!("{}", n + 1);
        // Function indexes may produce multiple values per row (multi-
        // entry regime): override entry_pairs to fan out. Plain field
        // indexes use the trait default (one pair).
        let pairs_impl = if idx.func.is_empty() {
            quote! {}
        } else {
            let fpath = syn::parse_str::<syn::Expr>(&idx.func)
                .unwrap_or_else(|e| panic!("kv_index[{}]: bad func path `{}`: {e}", idx.ident, idx.func));
            quote! {
                fn entry_pairs(
                    table_ns: u16,
                    key: &Self::Key,
                    row: &Self::Row,
                ) -> Vec<(Vec<u8>, Vec<u8>)> {
                    // One (key, value) pair per produced value; every
                    // entry shares the same includes value. Key =
                    // [ns 2B][slot][value][key prefix].
                    let __okm_fv = #fpath(row);
                    let __okm_vals = ::okm_core::IndexFuncValues::func_values(__okm_fv);
                    let mut __okm_out = Vec::with_capacity(__okm_vals.len());
                    let __okm_value = Self::entry_value(key, row);
                    for __okm_seg in __okm_vals {
                        let mut __okm_k = Vec::with_capacity(
                            3 + __okm_seg.len() + Self::key_prefix_width(),
                        );
                        __okm_k.extend_from_slice(&table_ns.to_be_bytes());
                        __okm_k.push(Self::SLOT);
                        __okm_k.extend_from_slice(&__okm_seg);
                        __okm_k.extend_from_slice(&Self::key_prefix_bytes(key));
                        __okm_out.push((__okm_k, __okm_value.clone()));
                    }
                    __okm_out
                }
            }
        };
        let func_str = idx.func.clone();
        // Function-index regime: the sort segment is `func(&row)`'s
        // result(s), each encoded via IndexFuncResult — a single value
        // yields one entry, an iterator yields one entry per element
        // (multi-entry regime: inverted index, multi-valued fields).
        // The generated encode_named ignores the requested names (the
        // function replaces them); scan_covered still works because the
        // value segment (includes) stays field encoded — with no
        // includes the value is empty.
        let encode_named_body = if idx.func.is_empty() {
            // The field encoders reference `self.#id` (shared with the
            // payload TLV loop), so the walk lives in an inherent method
            // with a real `self` receiver.
            quote! { <Self::Row>::__okm_encode_named(row, names, buf) }
        } else {
            let fpath = syn::parse_str::<syn::Expr>(&idx.func)
                .unwrap_or_else(|e| panic!("kv_index[{}]: bad func path `{}`: {e}", idx.ident, idx.func));
            quote! {{
                // Function index: result(s) → index-segment encoding. A
                // single value encodes once; a Vec encodes its first value
                // here (encode_named only feeds scan_covered's sort
                // segment) — the multi-entry fan-out lives in
                // entry_pairs. The same path is what the query side calls
                // on its probe value.
                let __okm_fv = #fpath(row);
                match ::okm_core::IndexFuncValues::func_values(__okm_fv).pop() {
                    Some(__okm_seg) => buf.extend_from_slice(&__okm_seg),
                    None => {}
                }
            }}
        };
        // For Reverse<T> payload fields the payload encoder reads
        // `self.field`; __okm_encode_named is an inherent method with a
        // real `self`, so the arm bodies work verbatim.
        index_out.extend(quote! {
            #[doc = concat!("Access method `", stringify!(#iname), "` over `", stringify!(#row_name), "` (slot ", #slot_doc, ", ADR-0005/0006).")]
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy, Debug)]
            pub struct #struct_ident;

            impl ::okm_core::KvIndex for #struct_ident {
                type Key = #key_ty;
                type Row = #row_name;
                const SLOT: u8 = #slot_lit;
                const FIELDS: &'static [&'static str] = &[#(#fields),*];
                const INCLUDES: &'static [&'static str] = &[#(#includes),*];
                const KEY_PREFIX: &'static [&'static str] = &[#(#key_names),*];
                const FUNC: &'static str = #func_str;

                fn encode_named(
                    _key: &Self::Key,
                    row: &Self::Row,
                    names: &[&str],
                    buf: &mut Vec<u8>,
                ) {
                    #encode_named_body
                }
                #pairs_impl
            }
        });
    }
    index_out
}

/// index_entries: statically expands every (entry_key, entry_value) pair
/// per declared #[kv_index] (slot order) — no runtime registry needed;
/// the declaration is the registry. Function indexes may fan out to
/// multiple pairs per row (multi-entry regime).
fn emit_index_entries(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let entry_calls = schema.indexes.iter().map(|idx| {
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, idx.ident);
        quote! {
            out.extend(<#struct_ident as ::okm_core::KvIndex>::entry_pairs(ns, key, row));
        }
    });
    quote! {
        fn index_entries(key: &Self::Key, row: &Self, ns: u16) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut out = Vec::new();
            #(#entry_calls)*
            out
        }
    }
}

/// Aggregates: `#[kv_aggregate(MyLogic { group(a, b) })]` — the user
/// implements `okm_core::AggregateLogic` on `MyLogic` (Acc + fold/unfold);
/// the derive generates the `okm_core::Aggregate` impl on the SAME type
/// (SLOT/GROUP come from the declaration) plus the Row hook override
/// running each aggregate's read-modify-write. Returns
/// (trait impls, hook fn body to splice inside `impl Row`).
fn emit_aggregates(schema: &RowSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let n_idx = schema.indexes.len();

    let mut impls = quote! {};
    let mut calls = quote! {};
    for (n, agg) in schema.aggregates.iter().enumerate() {
        let slot_lit = proc_macro2::Literal::u8_unsuffixed(n_idx as u8 + 1 + n as u8);
        let logic = syn::parse_str::<syn::Type>(&agg.logic)
            .unwrap_or_else(|e| panic!("kv_aggregate[{}]: bad logic type `{}`: {e}", agg.ident, agg.logic));
        let group: Vec<&String> = agg.group.iter().collect();
        impls.extend(quote! {
            impl ::okm_core::Aggregate for #logic {
                const SLOT: u8 = #slot_lit;
                const GROUP: &'static [&'static str] = &[ #(#group),* ];
                fn group_bytes(
                    _key: &<#row_name as ::okm_core::Row>::Key,
                    row: &#row_name,
                ) -> Vec<u8> {
                    // The row's named-field walk — same encoders as the
                    // index layer, byte-compatible with read-side probes.
                    let mut buf = Vec::new();
                    <#row_name>::__okm_encode_named(row, Self::GROUP, &mut buf);
                    buf
                }
            }
        });
        calls.extend(quote! {{
            let __okm_ek = <#logic as ::okm_core::Aggregate>::entry_key(_ns, _key, _row);
            let mut __okm_acc = match _store.get(&__okm_ek) {
                Some(b) => <#logic as ::okm_core::AggregateLogic>::Acc::decode_acc(&b),
                None => ::core::default::Default::default(),
            };
            if _add {
                <#logic as ::okm_core::AggregateLogic>::fold(&mut __okm_acc, _row);
            } else {
                <#logic as ::okm_core::AggregateLogic>::unfold(&mut __okm_acc, _row);
            }
            _store.put(__okm_ek, <#logic as ::okm_core::AggregateLogic>::Acc::encode_acc(&__okm_acc));
        }});
    }
    if schema.aggregates.is_empty() {
        return (quote! {}, quote! {});
    }
    let hook = quote! {
        fn __okm_apply_aggregates<S: ::okm_core::KvEngine>(
            _store: &mut S,
            _key: &Self::Key,
            _row: &Self,
            _ns: u16,
            _add: bool,
        ) {
            #calls
        }
    };
    (impls, hook)
}

/// Subscribe: `#[kv_subscribe]` or `#[kv_subscribe(RowEvent::User)]` —
/// the write-path event send (ADR-0008). The annotation declares the
/// channel entry point; there is NO handler here. Returns (static items
/// to place beside the impl, hook fn body to splice inside `impl Row`).
///
/// Two shapes:
/// - with a variant path: the send wraps the event in the generated
///   enum's variant and goes through that enum's global channel cell
///   (`::okm_subscribe::` module generated by the build script);
/// - bare (no path): a per-row-type channel cell is declared here
///   instead, keyed by the row name — the degraded fallback when no
///   event enum exists yet.
fn emit_subscribe(schema: &RowSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;
    let Some(sub) = &schema.subscribe else {
        return (quote! {}, quote! {});
    };
    let hook = match &sub.variant {
        Some((_en, var)) => {
            let var = syn::parse_str::<syn::Ident>(var)
                .unwrap_or_else(|e| panic!("kv_subscribe: bad variant `{var}`: {e}"));
            quote! {
                fn __okm_emit_event(
                    _op: ::okm_core::subscribe::Op,
                    _key: &Self::Key,
                    _row: &Self,
                ) {
                    // Best-effort: no sink registered / sink rejected =
                    // event dropped; the write path is never blocked
                    // (ADR-0008). Clone only happens on subscribed rows.
                    // Path contract: the consuming crate declares
                    // `mod okm_subscribe` at ITS crate root and bridges
                    // the build.rs-generated module there.
                    crate::okm_subscribe::CHANNEL.emit(
                        crate::okm_subscribe::RowEvent::#var(::okm_core::subscribe::Event::new(
                            _op, _key.clone(), _row.clone(),
                        )),
                    );
                }
            }
        }
        None => {
            let cell = quote::format_ident!(
                "__OKM_CHANNEL_{}",
                row_name.to_string().to_uppercase()
            );
            quote! {
                fn __okm_emit_event(
                    _op: ::okm_core::subscribe::Op,
                    _key: &Self::Key,
                    _row: &Self,
                ) {
                    #cell.emit(::okm_core::subscribe::Event::new(
                        _op, _key.clone(), _row.clone(),
                    ));
                }
            }
        }
    };
    let statics = match &sub.variant {
        Some(_) => quote! {}, // enum cell lives in ::okm_subscribe (build script)
        None => {
            let cell = quote::format_ident!(
                "__OKM_CHANNEL_{}",
                row_name.to_string().to_uppercase()
            );
            let doc = format!(
                "Bare per-row-type subscribe channel for `{row_name}` \
                 (no enum variant annotated — degraded fallback shape). \
                 Register a sink via `register` to consume."
            );
            quote! {
                #[doc = #doc]
                pub static #cell: ::okm_core::subscribe::ChannelCell<
                    ::okm_core::subscribe::Event<#key_ty, #row_name>,
                > = ::okm_core::subscribe::ChannelCell::new();
            }
        }
    };
    (statics, hook)
}

/// The inherent named-field walk + the full `Row` trait impl, assembled
/// from the other emit functions' output.
fn emit_row_impl(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;
    let ver_lit = proc_macro2::Literal::u8_unsuffixed(schema.layout_version);
    let row_desc = field_desc_entries(schema);
    let encode_body = emit_payload_encode(schema);
    let decode_body = emit_payload_decode(schema);
    // Hot width as a numeric literal — widths are fixed `quote!{ N }`
    // tokens, so parsing the token text is exact for every supported kind.
    let hot_width_total: usize = schema
        .fields
        .iter()
        .filter(|f| f.hot)
        .map(|f| f.width.to_string().parse::<usize>().unwrap_or(0))
        .sum();
    let hot_width_lit = proc_macro2::Literal::usize_unsuffixed(hot_width_total);
    let named_walk = emit_named_walk(schema);
    let index_structs = emit_index_structs(schema);
    let index_entries = emit_index_entries(schema);
    let (agg_impls, agg_hook) = emit_aggregates(schema);
    let (sub_statics, sub_hook) = emit_subscribe(schema);
    let names: Vec<_> = schema.fields.iter().map(|f| &f.ident).collect();
    let name_strs: Vec<_> = schema.fields.iter().map(|f| f.ident.to_string()).collect();
    let widths: Vec<_> = schema.fields.iter().map(|f| &f.width).collect();

    quote! {
        #index_structs
        #agg_impls
        #sub_statics
        impl #row_name {
            #named_walk
        }
        impl ::okm_core::Row for #row_name {
            type Key = #key_ty;
            const LAYOUT_VERSION: u8 = #ver_lit;
            const PAYLOAD_FIELDS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];
            const FIELDS: &'static [::okm_core::FieldDesc] = #row_desc;
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
            #index_entries
            #agg_hook
            #sub_hook
        }
    }
}

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let schema = parse_schema(input);
    emit_row_impl(&schema).into()
}
