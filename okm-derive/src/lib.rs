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

mod junction_encode;
mod key_encode;
mod document_encode;
mod schema;
mod storage_encode;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TS2;

/// Debug facility (PLAN Phase 4 — macro expansion audit): when
/// `OKM_DERIVE_DUMP` is set to a directory path, the formatted expansion of
/// every derive invocation is written there as
/// `<struct>_<macro>.rs`. Zero cost when unset; cargo's macro caching
/// applies — touch the source to re-dump.
fn dump(macro_name: &str, input: TokenStream, tokens: &TS2) {
    if let Ok(dir) = std::env::var("OKM_DERIVE_DUMP") {
        let struct_name: Option<String> = syn::parse::<syn::DeriveInput>(input.clone())
            .ok()
            .map(|di| di.ident.to_string());
        let file: syn::File =
            syn::parse2(tokens.clone()).expect("derive expansion must parse as a file");
        let src = prettyplease::unparse(&file);
        let _ = std::fs::create_dir_all(&dir);
        let name = struct_name.as_deref().unwrap_or("unknown");
        let path = std::path::Path::new(&dir).join(format!("{name}_{macro_name}.rs"));
        let _ = std::fs::write(path, src);
    }
}

#[proc_macro_derive(KeyEncode, attributes(ok_ns))]
pub fn derive_key_encode(input: TokenStream) -> TokenStream {
    let out = key_encode::derive(input.clone());
    dump("KeyEncode", input, &out.clone().into());
    out
}

#[proc_macro_derive(DocumentEncode, attributes(ok_ref, ok_ns, ok_partition, ok_index, ok_reduce, ok_offset, ok_layout, ok_default, ok_subscribe, ok_event_enum))]
pub fn derive_document_encode(input: TokenStream) -> TokenStream {
    let out = document_encode::derive(input.clone());
    dump("DocumentEncode", input, &out.clone().into());
    out
}

#[proc_macro_derive(JunctionEncode, attributes(ok_junction, ok_head))]
pub fn derive_junction(input: TokenStream) -> TokenStream {
    let out = junction_encode::derive(input.clone());
    dump("JunctionEncode", input, &out.clone().into());
    out
}

#[proc_macro_derive(NestStorage, attributes(ok_ns))]
pub fn derive_nest(input: TokenStream) -> TokenStream {
    let out = storage_encode::derive(input.clone());
    dump("NestStorage", input, &out.clone().into());
    out
}
