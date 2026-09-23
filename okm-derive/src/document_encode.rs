//! `DocumentEncode` — value/payload encoding + index declarations.
//!
//! One macro, three concerns (ADR-0006):
//!
//! 1. `#[ok_ref(KeyType)]` — the identity struct this document hangs off.
//! 2. Payload fields — encoded as TLV: `[tag u8][len u32 BE][value BE]`
//!    per field, `tag` = field declaration index (unique within the document,
//!    decoupled from field names). `len` is a redundant check for
//!    fixed-width fields today but keeps the same frame for the
//!    variable-length regime later.
//! 3. `#[ok_index(idx_name { fields(a, b), includes(c) })]` — one access
//!    method per declaration, slots start at DECLARED_SLOT_BASE (16) in attribute order
//!    (`0` is reserved for the primary table, ADR-0005). Generates a
//!    marker struct per index plus `Document::index_entries`, so `put`/
//!    `delete` cover every declared access method with no runtime
//!    registry — the declaration *is* the registry.
//!
//! Additionally, both the key struct and the document struct expose a
//! `FieldDesc` table (name / byte width / primitive kind, declaration
//! order) — the single field list feeding every downstream consumer that
//! needs to lay out or interpret fields without a derive on their side
//! (snapshot columns, Arrow schema, column builders; ADR-0007). The kind
//! enum lives in `okm` core as `okm_core::FieldType` — dependency-free, so the
//! derive stays pure.
//!
//! Structure: parse once into the schema IR (`schema.rs`, which also owns
//! all validation), then one emit function per generated artifact. Adding
//! a generated artifact = adding an `emit_*(&DocumentSchema)` function; the
//! emit functions share no temporary state.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TS2;

/// Declared-slot segment bases (ADR-0016): slot = 4-bit segment + 12-bit
/// counter; indexes allocate from the 0x1 segment, reduces from the 0x2
/// segment (independent counters). Literals, not runtime constants — a
/// proc-macro crate cannot name okm_core at its own compile time. Keep in
/// sync with `okm_core::index::DECLARED_SLOT_BASE` / `REDUCE_SLOT_BASE`.
const DECLARED_SLOT_BASE: u16 = 0x1001;
const REDUCE_SLOT_BASE: u16 = 0x2001;
use quote::{format_ident, quote};
use syn::{parse_macro_input, DeriveInput};

use crate::schema::{parse_schema, DocumentSchema};

/// One match arm per payload field name — each arm appends that field's
/// raw encoding (no TLV frame; the index segment is a plain
/// concatenation, order = the requested name order). Shared by all
/// index declarations and the inherent `__okm_encode_named` walk.
fn emit_named_walk(schema: &DocumentSchema) -> TS2 {
    let mut enc_arms = quote! {};
    let mut enc_names = Vec::new();
    for f in &schema.fields {
        let fname = f.ident.to_string();
        let enc = &f.enc;
        enc_names.push(fname.clone());
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

        /// Two-source named walk for reduce GROUP segments (ADR-0024):
        /// key names slice the key's full encoding by its FIELD_WIDTHS
        /// offsets (key fields are fixed-width — the slice IS the field
        /// encoding); payload names encode from `self`. A name in BOTH
        /// sources resolves to the KEY (key wins). A name unknown to
        /// both sources panics.
        #[allow(unused_variables, unused_mut, dead_code)]
        pub fn __okm_encode_group_named(
            key: &<Self as ::okm_core::Document>::Key,
            document: &Self,
            names: &[&str],
            buf: &mut Vec<u8>,
        ) {
            // Key-side names, encoded once and sliced per name.
            let key_bytes = ::okm_core::KeyEncode::encode(key);
            let mut key_off = 0usize;
            let mut key_slices: std::collections::HashMap<&str, (usize, usize)> =
                std::collections::HashMap::new();
            for f in <<Self as ::okm_core::Document>::Key as ::okm_core::KeyEncode>::FIELDS {
                key_slices.insert(f.name, (key_off, key_off + f.width));
                key_off += f.width;
            }
            let payload_fields: &[&str] = &[#(#enc_names),*];
            for n in names {
                if key_slices.contains_key(n) {
                    // Ambiguous name (in key AND payload): key wins — the
                    // same shadowing rule as the dynamic side.
                    let (a, b) = key_slices[n];
                    buf.extend_from_slice(&key_bytes[a..b]);
                } else if payload_fields.contains(&n) {
                    document.__okm_encode_named(&[n], buf);
                } else {
                    panic!("unknown group field: {n}");
                }
            }
        }
    }
}

/// FieldDesc table entries: `(name, FieldType, width)`, declaration order.
fn field_desc_entries(schema: &DocumentSchema) -> TS2 {
    let documents = schema.fields.iter().map(|f| {
        let name = f.ident.to_string();
        let kind = f.kind.as_ref().expect("field kind");
        let w = &f.width;
        quote! { (::okm_core::FieldDesc { name: #name, ty: #kind, width: #w }) }
    });
    quote! { &[ #(#documents),* ] }
}

/// Const DEFAULTS entries: literal `#[ok_default]` per field, name-keyed.
/// The `Str` case needs &'static str — emitted from the inner str literal
/// of `"x".to_string()` or a bare "x"; non-literal exprs are skipped
/// (dynamic reader falls back to zero).
fn defaults_entries(schema: &DocumentSchema) -> TS2 {
    let documents = schema.fields.iter().filter_map(|f| {
        let name = f.ident.to_string();
        let lit = f.default_lit.as_ref()?;
        Some(quote! { (#name, #lit) })
    });
    quote! { &[ #(#documents),* ] }
}

/// ---- encode: [version u8][hot_len u16 BE][hot segment][cold TLV] ----
fn emit_payload_encode(schema: &DocumentSchema) -> TS2 {
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
            ::okm_core::put_len(&mut buf, #len);
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
fn emit_payload_decode(schema: &DocumentSchema) -> TS2 {
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
                let (len, len_n) = ::okm_core::take_len(&b[cold_pos..])
                    .expect("cold frame: truncated length prefix");
                cold_pos += len_n;
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
/// order). A `where(path)` clause contributes an `admits` override
/// (partial index); a `func(path)` clause contributes an `entry_pairs`
/// override (multi-entry fan-out) and the function-index `encode_named`
/// body. Both may coexist on one declaration.
fn emit_index_structs(schema: &DocumentSchema) -> TS2 {
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
        let slot_lit = proc_macro2::Literal::u16_unsuffixed(DECLARED_SLOT_BASE + n as u16);
        let iname = &idx.ident;
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, iname);
        let fields: Vec<&String> = idx.fields.iter().collect();
        let includes: Vec<&String> = idx.includes.iter().collect();
        let key_names: Vec<&String> = idx.key.iter().collect();
        let slot_doc = format!("{}", n + 1);
        // Function indexes may produce multiple values per document (multi-
        // entry regime): override entry_pairs to fan out. Plain field
        // indexes use the trait default (one pair).
        let pairs_impl = if idx.func.is_empty() {
            quote! {}
        } else {
            let fpath = syn::parse_str::<syn::Expr>(&idx.func)
                .unwrap_or_else(|e| panic!("ok_index[{}]: bad func path `{}`: {e}", idx.ident, idx.func));
            quote! {
                fn entry_pairs(
                    ns_prefix: &[u8],
                    key: &Self::Key,
                    document: &Self::Document,
                ) -> Vec<(Vec<u8>, Vec<u8>)> {
                    // Partial index (where(path)): a rejected document
                    // contributes no entries at all. Checked once per
                    // document, before any fan-out.
                    if !Self::admits(document) {
                        return Vec::new();
                    }
                    // One (key, value) pair per produced value; every
                    // entry shares the same includes value. Key =
                    // [ns 2B][slot][value][key prefix].
                    let __okm_fv = #fpath(document);
                    let __okm_vals = ::okm_core::IndexFuncValues::func_values(__okm_fv);
                    let mut __okm_out = Vec::with_capacity(__okm_vals.len());
                    let __okm_value = Self::entry_value(key, document);
                    for __okm_seg in __okm_vals {
                        let mut __okm_k = Vec::with_capacity(
                            ns_prefix.len() + 1 + __okm_seg.len() + Self::key_prefix_width(),
                        );
                        __okm_k.extend_from_slice(ns_prefix);
                        __okm_k.extend_from_slice(&Self::SLOT.to_be_bytes());
                        __okm_k.extend_from_slice(&__okm_seg);
                        __okm_k.extend_from_slice(&Self::key_prefix_bytes(key));
                        __okm_out.push((__okm_k, __okm_value.clone()));
                    }
                    __okm_out
                }
            }
        };
        let func_str = idx.func.clone();
        // Partial index: `where(path)` on the declaration → generated
        // `admits` override delegating to the declared predicate. Absent =
        // the trait default (every document admitted). Legal on plain and
        // function indexes alike — the trait default `entry_pairs` (plain
        // path) consults `admits` too.
        let admits_impl = if idx.filter.is_empty() {
            quote! {}
        } else {
            let fpath = syn::parse_str::<syn::Expr>(&idx.filter).unwrap_or_else(|e| {
                panic!("ok_index[{}]: bad where path `{}`: {e}", idx.ident, idx.filter)
            });
            quote! {
                fn admits(document: &Self::Document) -> bool {
                    #fpath(document)
                }
            }
        };
        // Function-index regime: the sort segment is `func(&document)`'s
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
            quote! { <Self::Document>::__okm_encode_named(document, names, buf) }
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
                let __okm_fv = #fpath(document);
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
                const SLOT: ::okm_core::index::Slot = #slot_lit;
                const FIELDS: &'static [&'static str] = &[#(#fields),*];
                const INCLUDES: &'static [&'static str] = &[#(#includes),*];
                const KEY_PREFIX: &'static [&'static str] = &[#(#key_names),*];
                const FUNC: &'static str = #func_str;

                fn encode_named(
                    _key: &Self::Key,
                    document: &Self::Document,
                    names: &[&str],
                    buf: &mut Vec<u8>,
                ) {
                    #encode_named_body
                }
                #pairs_impl
                #admits_impl
            }
        });
    }
    index_out
}

/// index_entries: statically expands every (entry_key, entry_value) pair
/// per declared #[ok_index] (slot order) — no runtime registry needed;
/// the declaration is the registry. Function indexes may fan out to
/// multiple pairs per document (multi-entry regime).
fn emit_index_entries(schema: &DocumentSchema) -> TS2 {
    let row_name = &schema.row_name;
    let entry_calls = schema.indexes.iter().filter(|idx| !idx.deprecated).map(|idx| {
        let struct_ident = format_ident!("__OkmIndex_{}_{}", row_name, idx.ident);
        quote! {
            out.extend(<#struct_ident as ::okm_core::KvIndex>::entry_pairs(ns, key, document));
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
            let lit = proc_macro2::Literal::u16_unsuffixed(DECLARED_SLOT_BASE + n as u16);
            quote! { #lit }
        })
        .collect();
    let dep_const = quote! {
        /// Slots reserved by `deprecated` index declarations — entries
        /// here are stale (written before the deprecation) and are
        /// cleared by `Collection::prune_deprecated_slots`.
        const DEPRECATED_SLOTS: &'static [::okm_core::index::Slot] = &[#(#dep_slots),*];
    };
    quote! {
        fn index_entries(key: &Self::Key, document: &Self, ns: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
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
/// (SLOT/GROUP come from the declaration) plus the Document hook override
/// running each reduce's read-modify-write. Returns
/// (trait impls, hook fn body to splice inside `impl Document`).
fn emit_reduces(schema: &DocumentSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let _n_idx = schema.indexes.len();

    let mut impls = quote! {};
    let mut calls = quote! {};
    for (n, red) in schema.reduces.iter().enumerate() {
        let slot_lit = proc_macro2::Literal::u16_unsuffixed(REDUCE_SLOT_BASE + n as u16);
        let group: Vec<&String> = red.group.iter().collect();

        // Preset combinators (ADR-0023): `Count`/`Sum(f)`/`HighWater(f)`/
        // `LowWater(f)` expand to a LOCAL zero-size logic type whose
        // `ReduceLogic` impl forwards to the okm-core preset (a foreign
        // generic type cannot receive a local trait impl — coherence),
        // keeping the accumulator semantics in core: written, reviewed,
        // tested once. Field-based presets additionally get a marker
        // type whose generated `ReduceFieldSource` impl resolves the
        // declared field by the two-source rule (KEY WINS, ADR-0024).
        // User-written logic names pass through unchanged.
        const PRESETS: &[&str] = &["Count", "Sum", "HighWater", "LowWater"];
        let is_preset = PRESETS.contains(&red.logic.as_str());
        let red_doc = &red.logic;
        let marker = quote::format_ident!("__OkmReduce_{}_{}", row_name, n);
        let field_marker: Option<syn::Ident> = red.field.as_ref().map(|f| {
            quote::format_ident!("__OkmReduceSource_{}_{}_{}", row_name, n, f)
        });
        let logic: TS2 = if is_preset {
            quote! { #marker }
        } else {
            let ty: syn::Type = syn::parse_str(&red.logic).unwrap_or_else(|e| {
                panic!("ok_reduce[{}]: bad logic type `{}`: {e}", red.ident, red.logic)
            });
            quote! { #ty }
        };

        if is_preset {
            let forward = match red.logic.as_str() {
                "Count" => quote! { ::okm_core::Count::<#row_name> },
                other => {
                    let fm = field_marker.as_ref().unwrap();
                    let preset = quote::format_ident!("{}", other);
                    quote! { ::okm_core::#preset::<#fm> }
                }
            };
            if let Some(fm) = &field_marker {
                let field = red.field.as_ref().unwrap();
                // Field source: resolve the declared name by the two-source
                // walk and widen to u64; a non-integer field panics with
                // its name at the first fold (compile-time checking is
                // impossible — the key struct is a separate expansion).
                impls.extend(quote! {
                    #[doc = concat!("Marker for the `", #field, "` aggregate source (ADR-0023 preset).")]
                    pub struct #fm;
                    impl ::okm_core::ReduceFieldSource for #fm {
                        type Doc = #row_name;
                        fn reduce_field_u64(
                            key: &<#row_name as ::okm_core::Document>::Key,
                            document: &#row_name,
                        ) -> u64 {
                            let mut buf = Vec::new();
                            <#row_name>::__okm_encode_group_named(key, document, &[#field], &mut buf);
                            match buf.len() {
                                1 => buf[0] as u64,
                                2 => u16::from_be_bytes(buf.as_slice().try_into().unwrap()) as u64,
                                4 => u32::from_be_bytes(buf.as_slice().try_into().unwrap()) as u64,
                                8 => u64::from_be_bytes(buf.as_slice().try_into().unwrap()),
                                other => panic!(
                                    "ok_reduce preset: field `{}` must be an integer (1/2/4/8-byte) field, got {other} bytes",
                                    #field
                                ),
                            }
                        }
                    }
                });
            }
            // Local forwarding logic: semantics live in the core preset.
            impls.extend(quote! {
                #[doc = concat!("ADR-0023 preset `", #red_doc, "` (local forward marker).")]
                pub struct #marker;
                impl ::okm_core::ReduceLogic for #marker {
                    type Document = #row_name;
                    type Acc = <#forward as ::okm_core::ReduceLogic>::Acc;
                    fn fold(acc: &mut Self::Acc, key: &<#row_name as ::okm_core::Document>::Key, item: &#row_name) {
                        <#forward as ::okm_core::ReduceLogic>::fold(acc, key, item);
                    }
                    fn unfold(acc: &mut Self::Acc, key: &<#row_name as ::okm_core::Document>::Key, item: &#row_name) {
                        <#forward as ::okm_core::ReduceLogic>::unfold(acc, key, item);
                    }
                }
            });
        }

        impls.extend(quote! {
            impl ::okm_core::Reduce for #logic {
                const SLOT: ::okm_core::index::Slot = #slot_lit;
                const GROUP: &'static [&'static str] = &[ #(#group),* ];
                fn group_bytes(
                    key: &<#row_name as ::okm_core::Document>::Key,
                    document: &#row_name,
                ) -> Vec<u8> {
                    // Two-source named walk (ADR-0024): a group name
                    // resolves to the KEY's encoding or the payload's —
                    // whichever struct declares it; a name in both is a
                    // compile error (schema validation below).
                    let mut buf = Vec::new();
                    <#row_name>::__okm_encode_group_named(key, document, Self::GROUP, &mut buf);
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
                <#logic as ::okm_core::ReduceLogic>::fold(&mut __okm_acc, _key, _row);
            } else {
                <#logic as ::okm_core::ReduceLogic>::unfold(&mut __okm_acc, _key, _row);
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
/// variant (variant = document type name) and goes through that enum's global
/// channel cell (`::okm_subscribe::` module generated by the build
/// script). The per-document-type bare channel is explicitly unsupported.
fn emit_subscribe(schema: &DocumentSchema) -> (TS2, TS2) {
    let row_name = &schema.row_name;
    let Some(sub) = &schema.subscribe else {
        return (quote! {}, quote! {});
    };
    let en = syn::parse_str::<syn::Ident>(&sub.enum_name)
        .unwrap_or_else(|e| panic!("ok_subscribe: bad enum name `{}`: {e}", sub.enum_name));
    // The variant IS the document type name — derived by build.rs, so a send
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
            // (ADR-0008). Clone only happens on subscribed documents.
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
    // The enum cell lives in ::okm_subscribe (build script) — no per-document
    // statics in the document's own module.
    (quote! {}, hook)
}

/// The inherent named-field walk + the full `Document` trait impl, assembled
/// from the other emit functions' output.
fn emit_row_impl(schema: &DocumentSchema) -> TS2 {
    let row_name = &schema.row_name;
    let key_ty = &schema.key_ty;
    // #[ok_ns(N)] → the document's table ns prefix bytes (big-endian u16,
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
    // FIELD_CONTRACTS: Vector fields with #[ok_len(N)] — the dynamic
    // reader enforces the same count check the Rust decoder inlines.
    let contracts: Vec<(String, proc_macro2::Literal)> = schema
        .fields
        .iter()
        .filter_map(|f| {
            f.ok_len
                .map(|n| (f.ident.to_string(), proc_macro2::Literal::usize_unsuffixed(n)))
        })
        .collect();
    let contracts_const = {
        let names = contracts.iter().map(|(n, _)| n);
        let lens = contracts.iter().map(|(_, l)| l);
        quote! {
            const FIELD_CONTRACTS: &'static [(&'static str, usize)] = &[ #((#names, #lens)),* ];
        }
    };
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
    // ---- document <-> map bridge (ADR-0012): per-field lift into
    // DynamicValue and back. The derive owns the concrete Rust types, so
    // each arm emits the exact cast; from_map fills missing fields from
    // `#[ok_default]` / `Default` — same evolution rule as the payload
    // decoder. `String`/`Bytes` clone; fixed `[u8; N]` converts.
    let mut to_map_arms = quote! {};
    let mut from_map_arms = quote! {};
    // Ref fields: (ident, D type tokens, K type tokens) — feeds the
    // deref/write/release hooks.
    let mut embed_fields: Vec<(syn::Ident, syn::Type, syn::Type, bool)> = Vec::new(); // (ident, D, K, is_list)
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
        let is_bytes = ty_str.starts_with("Bytes");
        let is_fixedbytes = ty_str.starts_with("[u8;");
        let is_f64 = ty_str.starts_with("Quant<") || ty_str.starts_with("Quant <");
        let is_varint = ty_str.starts_with("VarInt<") || ty_str.starts_with("VarInt <");
        let is_enum = ty_str.starts_with("Enum<") || ty_str.starts_with("Enum <");
        let is_offset = ty_str.starts_with("Offset");
        let is_embed = ty_str.starts_with("Ref<") || ty_str.starts_with("Ref <") || ty_str.starts_with("Refs<");
        // Ref<D, K>: extract the K type for key encode/decode.
        let embed_default = if ty_str.starts_with("Ref<") || ty_str.starts_with("Ref <") {
            let k_ty_str = ty_str
                .trim_start_matches("Ref <")
                .trim_start_matches("Ref<")
                .trim_end_matches('>')
                .rsplit_once(',')
                .map(|(_, k)| k.trim().to_string())
                .expect("ref: <D, K>");
            let k_ty: syn::Type = syn::parse_str(&k_ty_str)
                .unwrap_or_else(|e| panic!("bridge: bad Ref key type `{k_ty_str}`: {e}"));
            quote! { ::okm_core::Ref { key: <#k_ty as ::core::default::Default>::default(), value: None } }
        } else if ty_str.starts_with("Refs<") {
            quote! { ::okm_core::Refs { keys: ::std::vec::Vec::new(), values: ::std::vec::Vec::new() } }
        } else {
            quote! {}
        };
        if is_embed {
            let d_k = ty_str
                .trim_start_matches("Ref <")
                .trim_start_matches("Ref<")
                .trim_start_matches("List <")
                .trim_start_matches("Refs<")
                .trim_end_matches('>')
                .to_string();
            let (d_str, k_str) = d_k
                .rsplit_once(',')
                .map(|(d, k)| (d.trim().to_string(), k.trim().to_string()))
                .expect("ref: <D, K>");
            let d_ty: syn::Type = syn::parse_str(&d_str)
                .unwrap_or_else(|e| panic!("bridge: bad Ref doc type `{d_str}`: {e}"));
            let k_ty: syn::Type = syn::parse_str(&k_str)
                .unwrap_or_else(|e| panic!("bridge: bad Ref key type `{k_str}`: {e}"));
            embed_fields.push((id.clone(), d_ty, k_ty, ty_str.starts_with("Refs")));
        }
        let embed_k: Option<syn::Type> = if is_embed {
            let generics = ty_str
                .trim_start_matches("Ref <")
                .trim_start_matches("Ref<")
                .trim_end_matches('>')
                .to_string();
            generics.rsplit_once(',').map(|(_, k)| k.trim().to_string()).and_then(|k| syn::parse_str(&k).ok())
        } else {
            None
        };
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
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bytes(self.#id.0.clone()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bytes(v)) => ::okm_core::Bytes(v.clone()),
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
        } else if ty_str.starts_with("Vector<") || ty_str.starts_with("Vector <") {
            // Vector<T, N>: map view = the flat LE wire (Bytes). Element
            // arithmetic is a consumer concern; the map bridge carries
            // the opaque fixed-width encoding.
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bytes(self.#id.encode_payload()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bytes(b)) => {
                        ::okm_core::Vector::decode_payload(b)
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
            // from_map: build the inner T from UInt, wrap Reverse(.0).
            let inner = ty_str
                .trim_start_matches("Reverse <")
                .trim_start_matches("Reverse<")
                .trim_end_matches('>')
                .trim()
                .to_string();
            let inner_ty: syn::Type = syn::parse_str(&inner)
                .unwrap_or_else(|e| panic!("bridge: bad Reverse inner type `{inner}`: {e}"));
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::UInt(
                    (!self.#id.0) as u64
                ));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    // The map carries the LOGICAL value; Reverse's wire flips
                    // bits — construct the wrapper by flipping back.
                    Some(::okm_core::obj_dynamic::DynamicValue::UInt(v)) => {
                        let logical = <#inner_ty as ::core::convert::TryFrom<u64>>::try_from(*v)
                            .unwrap_or_else(|_| 0);
                        ::okm_core::Reverse(!logical)
                    }
                    _ => #dflt,
                },
            });
        } else if is_embed && (ty_str.starts_with("Ref<") || ty_str.starts_with("Ref <")) {
            // Ref field: map view = the child key bytes (the wire
            // truth). The child VALUE is a document relation, not data in
            // this map — readers who want it call the child collection.
            let kt = embed_k.clone().expect("ref: key type");
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Bytes(self.#id.key.encode()));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Bytes(b)) => {
                        ::okm_core::Ref {
                            key: <#kt as ::okm_core::KeyEncode>::decode(b),
                            value: None,
                        }
                    }
                    _ => {
                                #embed_default
                    }
                },
            });
        } else if is_embed {
            // Refs field: map view = Array of child key bytes.
            let kt = embed_k.clone().expect("list: key type");
            to_map_arms.extend(quote! {
                out.insert(#name.to_string(), ::okm_core::obj_dynamic::DynamicValue::Array(
                    self.#id.keys.iter().map(|k| ::okm_core::obj_dynamic::DynamicValue::Bytes(k.encode())).collect()
                ));
            });
            from_map_arms.extend(quote! {
                #id: match map.get(#name) {
                    Some(::okm_core::obj_dynamic::DynamicValue::Array(items)) => {
                        ::okm_core::Refs {
                            keys: items.iter().filter_map(|v| match v {
                                ::okm_core::obj_dynamic::DynamicValue::Bytes(b) =>
                                    Some(<#kt as ::okm_core::KeyEncode>::decode(b)),
                                _ => None,
                            }).collect(),
                            values: (0..items.len()).map(|_| ::core::option::Option::None).collect(),
                        }
                    }
                    _ => #embed_default,
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


    // ---- ref hooks (no-ops when the document has no embedded fields) ----
    let embed_hooks = if embed_fields.is_empty() {
        quote! {}
    } else {
        // ---- deref arms (Ref: single; List: per key) ----
        let deref_arms = embed_fields.iter().map(|(fid, d_ty, _k_ty, is_list)| {
            if *is_list {
                quote! {
                    for i in 0..self.#fid.keys.len() {
                        if self.#fid.values.get(i).map_or(true, |v| v.is_none()) {
                            let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                            pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                            pkey.extend_from_slice(&self.#fid.keys[i].encode());
                            if let Some(payload) = store.get(&pkey) {
                                let mut child = <#d_ty as ::okm_core::Document>::decode_payload(&payload);
                                <#d_ty as ::okm_core::Document>::__okm_embed_deref(&mut child, store);
                                self.#fid.values[i] = Some(child);
                            }
                        }
                    }
                }
            } else {
                quote! {
                    if self.#fid.value.is_none() {
                        let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                        pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                        pkey.extend_from_slice(&self.#fid.key.encode());
                        if let Some(payload) = store.get(&pkey) {
                            let mut child = <#d_ty as ::okm_core::Document>::decode_payload(&payload);
                            <#d_ty as ::okm_core::Document>::__okm_embed_deref(&mut child, store);
                            self.#fid.value = Some(child);
                        }
                    }
                }
            }
        });
        // ---- entry arms (Ref: single child; List: per Some value) ----
        let entry_arms = embed_fields.iter().map(|(fid, d_ty, k_ty, is_list)| {
            if *is_list {
                quote! {
                    for (k, child) in self.#fid.keys.iter().zip(self.#fid.values.iter()) {
                        if let Some(child) = child {
                            let raw = k.encode();
                            let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                            pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                            pkey.extend_from_slice(&raw);
                            entries.push((pkey, child.encode_payload()));
                            let child_key = <#k_ty as ::okm_core::KeyEncode>::decode(&raw);
                            for (ek, ev) in <#d_ty as ::okm_core::Document>::index_entries(&child_key, child, <#d_ty as ::okm_core::Document>::NS_PREFIX) {
                                entries.push((ek, ev));
                            }
                        }
                    }
                }
            } else {
                quote! {
                    if let Some(child) = &self.#fid.value {
                        let raw = self.#fid.key.encode();
                        let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                        pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                        pkey.extend_from_slice(&raw);
                        entries.push((pkey, child.encode_payload()));
                        let child_key = <#k_ty as ::okm_core::KeyEncode>::decode(&raw);
                        for (ek, ev) in <#d_ty as ::okm_core::Document>::index_entries(&child_key, child, <#d_ty as ::okm_core::Document>::NS_PREFIX) {
                            entries.push((ek, ev));
                        }
                    }
                }
            }
        });
        // ---- key arms (for stale-release) ----
        let key_arms = embed_fields.iter().map(|(fid, d_ty, _k_ty, is_list)| {
            if *is_list {
                quote! {
                    for k in &self.#fid.keys {
                        let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                        pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                        pkey.extend_from_slice(&k.encode());
                        keys.push(pkey);
                    }
                }
            } else {
                quote! {
                    {
                        let mut pkey = <#d_ty as ::okm_core::Document>::NS_PREFIX.to_vec();
                        pkey.extend_from_slice(&::okm_core::PRIMARY_SLOT.to_be_bytes());
                        pkey.extend_from_slice(&self.#fid.key.encode());
                        keys.push(pkey);
                    }
                }
            }
        });
        quote! {
            fn __okm_embed_deref(&mut self, store: &dyn ::okm_core::VirtualStorage) {
                #(#deref_arms)*
            }
            fn __okm_embed_entries(&self) -> ::std::vec::Vec<(::std::vec::Vec<u8>, ::std::vec::Vec<u8>)> {
                let mut entries = ::std::vec::Vec::new();
                #(#entry_arms)*
                entries
            }
            fn __okm_embed_keys(&self) -> ::std::vec::Vec<::std::vec::Vec<u8>> {
                let mut keys = ::std::vec::Vec::new();
                #(#key_arms)*
                keys
            }
        }
    };

    quote! {
        #index_structs
        #agg_impls
        #sub_statics
        impl #row_name {
            #named_walk
        }
        impl ::okm_core::Document for #row_name {
            #embed_hooks
            type Key = #key_ty;
            #part_const
            #ns_const
            const LAYOUT_VERSION: u8 = #ver_lit;
            const PAYLOAD_FIELDS: &'static [(&'static str, usize)] = &[ #((#name_strs, #widths)),* ];
            const FIELDS: &'static [::okm_core::FieldDesc] = #row_desc;
            const DEFAULTS: &'static [(&'static str, ::okm_core::field::DefaultValueConst)] = #row_defaults;
            #contracts_const
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
            /// document -> map: every declared field lifted into DynamicValue
            /// (ADR-0012 document-map bridge).
            fn to_map(&self) -> ::std::collections::BTreeMap<String, ::okm_core::obj_dynamic::DynamicValue> {
                let mut out = ::std::collections::BTreeMap::new();
                #to_map_arms
                out
            }
            /// map -> document: matched fields assigned from DynamicValue,
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
