//! `okm-value`: the schema-free values a dynamic segment carries, the codec over
//! them, and the name vocabulary they are written in.
//!
//! A dynamic segment stores ids, not field names, so the label ↔ number mapping
//! is part of the value's currency rather than the store's. This crate owns all
//! three pieces — the value vocabulary ([`DynamicValue`]), the segment's bytes
//! ([`encode`] / [`decode`]), and the cache behind the ids ([`Vocabulary`]) —
//! and asks the caller for exactly one thing: the four methods of [`Names`],
//! implemented where the key layout lives.
//!
//! ```text
//! value  = toc_len u16 BE | blocks | values      (ADR-0057, ADR-0060)
//! names  = the caller's vocabulary, ids crossing both ways (ADR-0012)
//! ```

pub mod segment;
pub mod value;
pub mod varint;
pub mod vocab;

mod error;

pub use error::Error;
pub use segment::{decode, encode};
pub use value::{
    CodecError, DynamicField, DynamicValue, ObjValueType, ValueMap, decode_named, decode_variants,
    dyn_kind, vocab_id_from_u64,
};
pub use vocab::{Id, Names, Vocabulary};