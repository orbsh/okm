//! `StorageEncode` — the `#[kv_storage]` receiver derive (ADR-0010 §4).
//!
//! An empty struct annotated with `#[derive(StorageEncode)]` +
//! `#[kv_ns(N)]` becomes a receiver host: the derive generates **no data
//! methods** (there is no row type to encode) and exactly one execution
//! surface (`serve`) — take frames, prepend the declared prefix, replay
//! on a plain byte-level engine. The host knows only its prefix; storing
//! garbage is indistinguishable from storing data.
//!
//! Same annotation discipline as `#[kv_subscribe]`: the annotation
//! declares a fact (which prefix this host serves); the macro emits the
//! implementation. The generated host wraps `okm_core::StorageHost`
//! internally — the reference implementation this derive targets.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput};

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    match &input.data {
        Data::Struct(s) if s.fields.is_empty() => {}
        _ => panic!("StorageEncode only supports unit/empty structs — the host carries no data"),
    }

    // `#[kv_ns(N)]` — the prefix this host serves. Required: a host with
    // no prefix would accept any sender's bytes into the root segment,
    // which is exactly the escape the declaration exists to make
    // inexpressible (ADR-0010 §4).
    let ns: u16 = input
        .attrs
        .iter()
        .find_map(|a| {
            if a.path().is_ident("kv_ns") {
                Some(
                    a.parse_args::<syn::LitInt>()
                        .expect("kv_storage format: #[kv_ns(N)]")
                        .base10_parse()
                        .expect("kv_ns must be a u16 literal"),
                )
            } else {
                None
            }
        })
        .expect("missing #[kv_ns(N)] — the declared prefix IS the host's isolation boundary");
    let prefix_lit = {
        let hi = (ns >> 8) as u8;
        let lo = (ns & 0xff) as u8;
        quote! { &[#hi, #lo] }
    };

    quote! {
        impl #name {
            /// The declared namespace prefix this host serves (big-endian
            /// `[ns 2B]`, same encoding as `Row::NS_PREFIX`).
            pub const NS_PREFIX: &'static [u8] = #prefix_lit;

            /// Bind the host to a real engine and start serving: returns
            /// the sender endpoints (`VirtualHandle`). The engine is held
            /// behind the host's single-writer mutex; every key entering
            /// it is `[NS_PREFIX][sender bytes]` — the prefix escape is
            /// not expressible from the outside (ADR-0010 §4).
            pub fn serve<S: ::okm_core::VirtualStorage + Send + 'static>(
                engine: S,
            ) -> ::okm_core::VirtualHandle {
                let (host, handle) = ::okm_core::StorageHost::new(engine, Self::NS_PREFIX);
                host.serve();
                handle
            }
        }
    }
    .into()
}
