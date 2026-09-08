//! Derive macros for OKM (object-keyspace mapping).
//!
//! Three macros, each a pure single-item function with zero I/O:
//!
//! - `KeyEncode`: fixed-width key encoding (`key_encode.rs`).
//! - `RowEncode`: value/payload encoding + index declarations
//!   (`row_encode.rs`).
//! - `EdgeEncode`: bidirectional edges (`edge_encode.rs`).
//!
//! Schema stability is locked by hex assertions in the test suite
//! (docs/adr/0002, docs/adr/0005, docs/adr/0006).

mod edge_encode;
mod key_encode;
mod row_encode;

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

#[proc_macro_derive(KeyEncode, attributes(kv_ns))]
pub fn derive_key_encode(input: TokenStream) -> TokenStream {
    let out = key_encode::derive(input.clone());
    dump("KeyEncode", input, &out.clone().into());
    out
}

#[proc_macro_derive(RowEncode, attributes(kv_ref, kv_index, kv_offset, kv_layout, kv_default))]
pub fn derive_row_encode(input: TokenStream) -> TokenStream {
    let out = row_encode::derive(input.clone());
    dump("RowEncode", input, &out.clone().into());
    out
}

#[proc_macro_derive(EdgeEncode, attributes(kv_ns, kv_head))]
pub fn derive_edge(input: TokenStream) -> TokenStream {
    let out = edge_encode::derive(input.clone());
    dump("EdgeEncode", input, &out.clone().into());
    out
}
