//! Derive macros for OKM (object-keyspace mapping).
//!
//! Three macros, each a pure single-item function with zero I/O:
//!
//! - `KeyEncode`: fixed-width key encoding (`key_encode.rs`).
//! - `DocumentEncode`: value/payload encoding + index declarations
//!   (`document_encode.rs`).
//! - `JunctionEncode`: junctions (`junction_encode.rs`).
//!
//! Schema stability is locked by hex assertions in the test suite
//! (docs/adr/0002, docs/adr/0005, docs/adr/0006).

// Diagnostic regime: a malformed DECLARATION is a `syn::Error` carrying the
// span of the token that got it wrong, returned up through the module's
// `expand` function to the entry point below, which renders it as
// `compile_error!` sitting on the offending attribute — not the crate-level
// "proc macro panicked" that a `panic!` inside the macro body produces.
// A `panic!` inside the GENERATED CODE is a different thing entirely (the
// decode paths are allowed to abort, ADR-0040) and stays in the emitted
// tokens; it never reaches this crate's own control flow.

mod document_encode;
mod graph_encode;
mod junction_encode;
mod key_encode;
mod schema;
mod storage_encode;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TS2;
use syn::{parse_macro_input, DeriveInput};

/// Debug facility (PLAN Phase 4 — macro expansion audit): when
/// `OKM_DERIVE_DUMP` is set to a directory path, the formatted expansion of
/// every derive invocation is written there as
/// `<struct>_<macro>.rs`. Zero cost when unset; cargo's macro caching
/// applies — touch the source to re-dump.
// reason: a debug facility that only runs when OKM_DERIVE_DUMP is set — a
// bad DUMP is a bug in this crate, never a user's declaration error, so it
// must not be folded into the `compile_error!` path above.
#[allow(clippy::panic, clippy::unwrap_used)]
fn dump(macro_name: &str, input: TokenStream, tokens: &TS2) {
    if let Ok(dir) = std::env::var("OKM_DERIVE_DUMP") {
        let struct_name: Option<String> = syn::parse::<syn::DeriveInput>(input.clone())
            .ok()
            .map(|di| di.ident.to_string());
        let file: Option<syn::File> = match syn::parse2(tokens.clone()) {
            Ok(f) => Some(f),
            Err(e) => {
                // Write the RAW token string so the offending site is
                // greppable even when pretty-parsing fails.
                let name_raw = input
                    .clone()
                    .into_iter()
                    .next()
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let _ = std::fs::create_dir_all(&dir);
                let path = std::path::Path::new(&dir)
                    .join(format!("{name_raw}_{macro_name}.raw.rs"));
                let _ = std::fs::write(path, tokens.to_string());
                panic!("derive expansion must parse as a file: {e}");
            }
        };
        let file = file.unwrap();
        let src = prettyplease::unparse(&file);
        let _ = std::fs::create_dir_all(&dir);
        let name = struct_name.as_deref().unwrap_or("unknown");
        let path = std::path::Path::new(&dir).join(format!("{name}_{macro_name}.rs"));
        let _ = std::fs::write(path, src);
    }
}

#[proc_macro_derive(KeyEncode, attributes(ok_ns))]
pub fn derive_key_encode(input: TokenStream) -> TokenStream {
    let parsed = input.clone();
    let di = parse_macro_input!(parsed as DeriveInput);
    match key_encode::expand(di) {
        // Only a real expansion is worth auditing — a rejected declaration has
        // none, and its `compile_error!` stays untouched on the bad token.
        Ok(out) => {
            dump("KeyEncode", input, &out);
            out.into()
        }
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_derive(DocumentEncode, attributes(ok_ref, ok_ns, ok_name, ok_partition, ok_index, ok_reduce, ok_offset, ok_layout, ok_default, ok_subscribe, ok_event_enum, ok_len))]
pub fn derive_document_encode(input: TokenStream) -> TokenStream {
    let parsed = input.clone();
    let di = parse_macro_input!(parsed as DeriveInput);
    match document_encode::expand(di) {
        // Only a real expansion is worth auditing — a rejected declaration has
        // none, and its `compile_error!` stays untouched on the bad token.
        Ok(out) => {
            dump("DocumentEncode", input, &out);
            out.into()
        }
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_derive(JunctionEncode, attributes(ok_junction, ok_head))]
pub fn derive_junction(input: TokenStream) -> TokenStream {
    let parsed = input.clone();
    let di = parse_macro_input!(parsed as DeriveInput);
    match junction_encode::expand(di) {
        // Only a real expansion is worth auditing — a rejected declaration has
        // none, and its `compile_error!` stays untouched on the bad token.
        Ok(out) => {
            dump("JunctionEncode", input, &out);
            out.into()
        }
        Err(e) => e.to_compile_error().into(),
    }
}

/// Graph Edge (ADR-0017 §4): one declaration IS the graph — the edge
/// collection's ns (`#[ok_ns]`), its own document payload, the attribute
/// faces it declares explicitly (`#[ok_index]`), and the member node
/// collections (`#[ok_nodes]` — a list of the node TABLES' types, from which
/// the member-set enum and its `NodeSet` impl are generated with each table's
/// own ns). Endpoints need no declaration — refs are self-describing; they
/// travel as runtime `NodeRef`s, never as declared struct fields.
#[proc_macro_derive(GraphEncode, attributes(ok_ns, ok_nodes, ok_name, ok_ref, ok_partition, ok_index, ok_reduce, ok_offset, ok_layout, ok_default, ok_subscribe, ok_event_enum, ok_len))]
pub fn derive_graph_encode(input: TokenStream) -> TokenStream {
    let parsed = input.clone();
    let di = parse_macro_input!(parsed as DeriveInput);
    match graph_encode::expand(di) {
        // Only a real expansion is worth auditing — a rejected declaration has
        // none, and its `compile_error!` stays untouched on the bad token.
        Ok(out) => {
            dump("GraphEncode", input, &out);
            out.into()
        }
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_derive(NestStorage, attributes(ok_ns))]
pub fn derive_nest(input: TokenStream) -> TokenStream {
    let parsed = input.clone();
    let di = parse_macro_input!(parsed as DeriveInput);
    match storage_encode::expand(di) {
        // Only a real expansion is worth auditing — a rejected declaration has
        // none, and its `compile_error!` stays untouched on the bad token.
        Ok(out) => {
            dump("NestStorage", input, &out);
            out.into()
        }
        Err(e) => e.to_compile_error().into(),
    }
}