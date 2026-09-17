//! `DocumentEncode` — value/payload encoding + index declarations.
//!
//! One macro, three concerns (ADR-0006):
//!
//! 1. `#[ok_ref(KeyType)]` — the identity struct this row hangs off.
//! 2. Payload fields — encoded as TLV: `[tag u8][len u32 BE][value BE]`
//!    per field, `tag` = field declaration index (unique within the row,
//!    decoupled from field names). `len` is a redundant check for
//!    fixed-width fields today but keeps the same frame for the
//!    variable-length regime later.
//! 3. `#[ok_index(idx_name { fields(a, b), includes(c) })]` — one access
//!    method per declaration, slots start at DECLARED_SLOT_BASE (16) in attribute order
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

/// Declared-slot base (ADR-0012): fixed roles own 0–15; indexes/reduces
/// allocate from here. A literal, not a runtime constant — a proc-macro
/// crate cannot name okm_core at its own compile time. Keep in sync with
/// `okm_core::index::DECLARED_SLOT_BASE`.
const DECLARED_SLOT_BASE: u8 = 16;
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

/// Const DEFAULTS entries: literal `#[ok_default]` per field, name-keyed.
/// The `Str` case needs &'static str — emitted from the inner str literal
/// of `"x".to_string()` or a bare "x"; non-literal exprs are skipped
/// (dynamic reader falls back to zero).
fn defaults_entries(schema: &RowSchema) -> TS2 {
    let rows = schema.fields.iter().filter_map(|f| {
        let name = f.ident.to_string();
        let lit = f.default_lit.as_ref()?;
        Some(quote! { (#name, #lit) })
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

/// One marker struct + `KvIndex` impl per declared `#[ok_index]` (slot
/// order).
fn emit_index_structs(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;

    let mut index_out = quote! {};
    for (n, idx) in schema.indexes.iter().enumerate() {
        // Deprecated declaration: slot stays reserved (declaration order
        // is a persistent contract), but nothing is generated — no marker
        // struct, no write path, no scan surface. Stale entries are
        // cleared by Collection::prune_deprecated_slots (ADR-0005).
        if idx.deprecated {
            continue;
        }
        let slot_lit = proc_macro2::Literal::u8_unsuffixed(DECLARED_SLOT_BASE + n as u8);
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
                .unwrap_or_else(|e| panic!("ok_index[{}]: bad func path `{}`: {e}", idx.ident, idx.func));
            quote! {
                fn entry_pairs(
                    table_ns: &[u8],
                    key: &Self::Key,
                    row: &Self::Document,
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
                            table_ns.len() + 1 + __okm_seg.len() + Self::key_prefix_width(),
                        );
                        __okm_k.extend_from_slice(table_ns);
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
            quote! { <Self::Document>::__okm_encode_named(row, names, buf) }
        } else {
            let fpath = syn::parse_str::<syn::Expr>(&idx.func)
                .unwrap_or_else(|e| panic!("ok_index[{}]: bad func path `{}`: {e}", idx.ident, idx.func));
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
                type Document = #row_name;
                const SLOT: u8 = #slot_lit;
                const FIELDS: &'static [&'static str] = &[#(#fields),*];
                const INCLUDES: &'static [&'static str] = &[#(#includes),*];
                const KEY_PREFIX: &'static [&'static str] = &[#(#key_names),*];
                const FUNC: &'static str = #func_str;

                fn encode_named(
                    _key: &Self::Key,
                    row: &Self::Document,
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
/// per declared #[ok_index] (slot order) — no runtime registry needed;
/// the declaration is the registry. Function indexes may fan out to
/// multiple pairs per row (multi-entry regime).
fn emit_index_entries(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let entry_calls = schema.indexes.iter().filter(|idx| !idx.deprecated).map(|idx| {
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, idx.ident);
        quote! {
            out.extend(<#struct_ident as ::okm_core::KvIndex>::entry_pairs(ns, key, row));
        }
    });
    // Deprecated slots: declaration positions (1-based) whose entries are
    // stale after the declaration was marked `deprecated` — consumed by
    // `Collection::prune_deprecated_slots` (prefix-scan + delete).
    let dep_slots: Vec<_> = schema
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, idx)| idx.deprecated)
        .map(|(n, _)| {
            let lit = proc_macro2::Literal::u8_unsuffixed(DECLARED_SLOT_BASE + n as u8);
            quote! { #lit }
        })
        .collect();
    let dep_const = quote! {
        /// Slots reserved by `deprecated` index declarations — entries
        /// here are stale (written before the deprecation) and are
        /// cleared by `Collection::prune_deprecated_slots`.
        const DEPRECATED_SLOTS: &'static [u8] = &[#(#dep_slots),*];
    };
    quote! {
        fn index_entries(key: &Self::Key, row: &Self, ns: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut out = Vec::new();
            #(#entry_calls)*
            out
        }
        #dep_const
    }
}

/// Reduces: `#[ok_reduce(MyLogic { group(a, b) })]` — the user
/// implements `okm_core::ReduceLogic` on `MyLogic` (Acc + fold/unfold);
/// the derive generates the `okm_core::Reduce` impl on the SAME type
/// (SLOT/GROUP come from the declaration) plus the Row hook override
/// running each reduce's read-modify-write. Returns
/// (trait impls, hook fn body to splice inside `impl Document`).
fn emit_reduces(schema: &RowSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let n_idx = schema.indexes.len();

    let mut impls = quote! {};
    let mut calls = quote! {};
    for (n, red) in schema.reduces.iter().enumerate() {
        let slot_lit = proc_macro2::Literal::u8_unsuffixed(DECLARED_SLOT_BASE + n_idx as u8 + n as u8);
        let logic = syn::parse_str::<syn::Type>(&red.logic)
            .unwrap_or_else(|e| panic!("ok_reduce[{}]: bad logic type `{}`: {e}", red.ident, red.logic));
        let group: Vec<&String> = red.group.iter().collect();
        impls.extend(quote! {
            impl ::okm_core::Reduce for #logic {
                const SLOT: u8 = #slot_lit;
                const GROUP: &'static [&'static str] = &[ #(#group),* ];
                fn group_bytes(
                    _key: &<#row_name as ::okm_core::Document>::Key,
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
            let __okm_ek = <#logic as ::okm_core::Reduce>::entry_key(_ns, _key, _row);
            let mut __okm_acc = match _store.get(&__okm_ek) {
                Some(b) => <#logic as ::okm_core::ReduceLogic>::Acc::decode_acc(&b),
                None => ::core::default::Default::default(),
            };
            if _add {
                <#logic as ::okm_core::ReduceLogic>::fold(&mut __okm_acc, _row);
            } else {
                <#logic as ::okm_core::ReduceLogic>::unfold(&mut __okm_acc, _row);
            }
            _store.put(__okm_ek, <#logic as ::okm_core::ReduceLogic>::Acc::encode_acc(&__okm_acc));
        }});
    }
    if schema.reduces.is_empty() {
        return (quote! {}, quote! {});
    }
    let hook = quote! {
        fn __okm_apply_reduces<S: ::okm_core::VirtualStorage>(
            _store: &mut S,
            _key: &Self::Key,
            _row: &Self,
            _ns: &[u8],
            _add: bool,
        ) {
            #calls
        }
    };
    (impls, hook)
}

/// Subscribe: `#[ok_subscribe]` or `#[ok_subscribe(RowEvent::User)]` —
/// the write-path event send (ADR-0008). The annotation declares the
/// channel entry point; there is NO handler here. Returns (static items
/// to place beside the impl, hook fn body to splice inside `impl Document`).
///
/// One shape only: the send wraps the event in the generated enum's
/// variant (variant = row type name) and goes through that enum's global
/// channel cell (`::okm_subscribe::` module generated by the build
/// script). The per-row-type bare channel is explicitly unsupported.
fn emit_subscribe(schema: &RowSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let Some(sub) = &schema.subscribe else {
        return (quote! {}, quote! {});
    };
    let en = syn::parse_str::<syn::Ident>(&sub.enum_name)
        .unwrap_or_else(|e| panic!("ok_subscribe: bad enum name `{}`: {e}", sub.enum_name));
    // The variant IS the row type name — derived by build.rs, so a send
    // site can never drift from the declaration (compile error on mismatch).
    let var = row_name;
    let cell = syn::parse_str::<syn::Ident>(&format!("CHANNEL_{}", sub.enum_name.to_uppercase()))
        .unwrap_or_else(|e| panic!("ok_subscribe: bad cell name from `{}`: {e}", sub.enum_name));
    let hook = quote! {
        fn __okm_emit_event(
            _op: ::okm_core::subscribe::Op,
            _epoch: u64,
            _key: &Self::Key,
            _row: &Self,
        ) {
            // Best-effort: no sink registered / sink rejected =
            // event dropped; the write path is never blocked
            // (ADR-0008). Clone only happens on subscribed rows.
            // Path contract: the consuming crate declares
            // `mod okm_subscribe` at ITS crate root and bridges
            // the build.rs-generated module there. The cell name
            // derives from the enum name (CHANNEL_<ENUM>), matching
            // build.rs generation.
            crate::okm_subscribe::#cell.emit(
                crate::okm_subscribe::#en::#var(::okm_core::subscribe::Event::new(
                    _op, _epoch, _key.clone(), _row.clone(),
                )),
            );
        }
    };
    // The enum cell lives in ::okm_subscribe (build script) — no per-row
    // statics in the row's own module.
    (quote! {}, hook)
}

/// The inherent named-field walk + the full `Row` trait impl, assembled
/// from the other emit functions' output.
fn emit_row_impl(schema: &RowSchema) -> TS2 {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;
    // #[ok_ns(N)] → the row's table ns prefix bytes (big-endian u16,
    // matching the raw `[ns 2B]` header). Absent = default empty.
    let ns_const = match schema.ns {
        Some(n) => {
            let hi = (n >> 8) as u8;
            let lo = (n & 0xff) as u8;
            quote! { const NS_PREFIX: &'static [u8] = &[#hi, #lo]; }
        }
        None => quote! {},
    };
    // #[ok_partition(N)] → PARTITION_ID: Option<u8>. None = no partition
    // segment in the key (default). Some(N) prepends a 2-byte ESCAPE
    // segment `[0xFF][N]` before the ns header — 0xFF is a reserved escape
    // byte that legal ns headers (big-endian u16, first byte constrained
    // by the ns dictionary to 0x00-0xFE) never start with, so partitioned
    // and unpartitioned keys are structurally disjoint with zero numbering
    // discipline. Bare `#[ok_partition]` (no argument) is rejected — an id
    // is required to avoid the "Some(0) = no segment" ambiguity.
    let part_const = match schema.partition {
        Some(id) => {
            let lit = proc_macro2::Literal::u8_unsuffixed(id);
            if id == 0 {
                return syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "#[ok_partition(0)] is invalid: partition 0 means \"no partition segment\" — omit the attribute instead",
                )
                .to_compile_error();
            }
            let id_lit = proc_macro2::Literal::u8_unsuffixed(id);
            quote! {
                const PARTITION_ID: Option<u8> = Some(#lit);
                const PARTITION_PREFIX: &'static [u8] = &[0xFFu8, #id_lit];
            }
        }
        None => quote! {
            const PARTITION_ID: Option<u8> = None;
            const PARTITION_PREFIX: &'static [u8] = &[];
        },
    };
    let ver_lit = proc_macro2::Literal::u8_unsuffixed(schema.layout_version);
    let row_desc = field_desc_entries(schema);
    let row_defaults = defaults_entries(schema);
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
    let (agg_impls, agg_hook) = emit_reduces(schema);
    let (sub_statics, sub_hook) = emit_subscribe(schema);
    // ---- row <-> map bridge (ADR-0012): per-field lift into
    // DynamicValue and back. The derive owns the concrete Rust types, so
    // each arm emits the exact cast; from_map fills missing fields from
    // `#[ok_default]` / `Default` — same evolution rule as the payload
    // decoder. `String`/`Vec<u8>` clone; fixed `[u8; N]` converts.
    let mut to_map_arms = quote! {};
    let mut from_map_arms = quote! {};
    for f in &schema.fields {
        let id = &f.ident;
        let name = id.to_string();
        let dflt = &f.default_expr;
        let ty_str = &f.ty_str;
        let t: syn::Type = syn::parse_str(ty_str)
            .unwrap_or_else(|e| panic!("bridge: bad type `{}`: {e}", ty_str));
        // Signed integers: widen to i64, store as UInt of the two's-
        // complement bits? No — DynamicValue has no signed variant yet;
        // negative values round-trip through the typed path, and the map
        // view lifts only unsigned/Str/Bytes/Bool for now. Documented.
        let is_signed = ty_str.starts_with('i');
        let is_bool = ty_str == "bool";
        let is_string = ty_str.starts_with("String");
        let is_bytes = ty_str.starts_with("Vec<u8>") || ty_str.starts_with("Vec < u8 >");
        let is_fixedbytes = ty_str.starts_with("[u8;");
        let is_f64 = ty_str.starts_with("Quant<") || ty_str.starts_with("Quant <");
        let is_varint = ty_str.starts_with("VarInt<") || ty_str.starts_with("VarInt <");
        let is_enum = ty_str.starts_with("Enum<") || ty_str.starts_with("Enum <");
        let is_offset = ty_str.starts_with("Offset");
        if is_signed {
            // Signed integers lift as Int (two's complement preserved).
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Int(self.#id as i64));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Int(v)) => (*v) as #t,
                    _ => #dflt,
                },
            });
        } else if is_bool {
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bool(self.#id));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bool(v)) => *v,
                    _ => #dflt,
                },
            });
        } else if is_string {
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Str(self.#id.clone()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Str(v)) => v.clone(),
                    _ => #dflt,
                },
            });
        } else if is_bytes {
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bytes(self.#id.clone()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bytes(v)) => v.clone(),
                    _ => #dflt,
                },
            });
        } else if is_fixedbytes {
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bytes(self.#id.to_vec()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bytes(v)) => v.clone().try_into().unwrap_or_else(|_| #dflt),
                    _ => #dflt,
                },
            });
        } else if is_varint {
            // VarInt<T>(pub T): lift the inner integer.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::UInt(self.#id.0 as u64));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::UInt(v)) => ::okm_core::VarInt::from_dyn(*v),
                    _ => #dflt,
                },
            });
        } else if is_enum {
            // Enum<T>(pub T): the map view carries the VARIANT NAME
            // (semantic value, not the wire tag). name()/from_name() are
            // EnumTag methods on the enum type itself — defined once per
            // enum, shared by every table that uses it (nothing
            // per-table is generated here).
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Str(self.#id.0.name()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Str(v)) => {
                        match ::okm_core::wrappers::enum_from_name(v) {
                            Some(e) => ::okm_core::Enum::from_dyn(e),
                            None => #dflt,
                        }
                    }
                    _ => #dflt,
                },
            });
        } else if is_offset {
            // Offset(pub i64): the absolute value IS the semantics.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Int(self.#id.0));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Int(v)) => #t(*v),
                    _ => #dflt,
                },
            });
        } else if is_f64 || ty_str.starts_with("Quant <") {
            // Quant<f64, P> stores an i64 wire but the Rust field is f64.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::F64(self.#id.0));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::F64(v)) => ::okm_core::Quant::from_dyn(*v),
                    _ => #dflt,
                },
            });
        } else if ty_str.starts_with("Reverse<") || ty_str.starts_with("Reverse <") {
            // Reverse<T>(pub T): wire is bit-flipped T; lift the inner.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::UInt(self.#id.0 as u64));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::UInt(v)) => ::okm_core::VarInt::from_dyn(*v),
                    _ => #dflt,
                },
            });
        } else {
            // Unsigned integers (u8/u16/u32/u64): the common hot path.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::UInt(self.#id as u64));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::UInt(v)) => (*v) as #t,
                    _ => #dflt,
                },
            });
        }
    }

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
        impl ::okm_core::Document for #row_name {
            type Key = #key_ty;
            #part_const
            #ns_const
            const LAYOUT_VERSION: u8 = #ver_lit;
            const PAYLOAD_FIELDS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];
            const FIELDS: &'static [::okm_core::FieldDesc] = #row_desc;
            const DEFAULTS: &'static [(&'static str, ::okm_core::field::DefaultValueConst)] = #row_defaults;
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
            /// row -> map: every declared field lifted into DynamicValue
            /// (ADR-0012 row-map bridge).
            fn to_map(&self) -> ::std::collections::BTreeMap<String, ::okm_core::obj_dynamic::DynamicValue> {
                let mut out = ::std::collections::BTreeMap::new();
                #to_map_arms
                out
            }
            /// map -> row: matched fields assigned from DynamicValue,
            /// missing fields fall back to `#[ok_default]`/Default — the
            /// same evolution rule as the payload decoder.
            fn from_map(map: &::std::collections::BTreeMap<String, ::okm_core::obj_dynamic::DynamicValue>) -> Self {
                Self {
                    #from_map_arms
                }
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
