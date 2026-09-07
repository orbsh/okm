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

#[proc_macro_derive(KeyEncode, attributes(kv_ns))]
pub fn derive_key_encode(input: TokenStream) -> TokenStream {
    key_encode::derive(input)
}

#[proc_macro_derive(RowEncode, attributes(kv_ref, kv_index))]
pub fn derive_row_encode(input: TokenStream) -> TokenStream {
    row_encode::derive(input)
}

#[proc_macro_derive(EdgeEncode, attributes(kv_ns, kv_head))]
pub fn derive_edge(input: TokenStream) -> TokenStream {
    edge_encode::derive(input)
}
