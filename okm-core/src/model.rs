//! Data model: the declaration-facing core — keys, documents, indexes,
//! junctions, reduces, schema, dynamic values, and the wrapper types.

// Decode assumes the encoder (ADR-0040): the two are one artifact, so a payload
// this SDK cannot decode was not produced by this SDK — a version or build
// mismatch, or bytes written behind the API's back. There is no "handle it and
// carry on" to be had: the decoder cannot reconstruct what it did not encode, and
// if it knew how to read those bytes it would have written them that way. A
// document decoded from foreign bytes is worse than no document, because the
// derived state written back from it (index entries, reduce accumulators, vocab
// ids) is silently wrong. So the codec aborts instead of answering a half-true
// value, and this module — plus what the derive emits for it — is the one place
// the panic lints are allowed to fire.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

pub mod collection;
pub mod document;
pub mod field;
pub mod graph;
pub mod index;
pub mod junction;
pub mod key;
pub mod obj_vocab;
pub mod obj_dynamic;
pub mod reduce;
pub mod presets;
pub mod reverse;
pub mod schema;
pub mod vocab;
pub mod wrappers;
