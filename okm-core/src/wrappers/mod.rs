//! Field wrappers — per-field transforms applied at every encode
//! destination (payload value, index-carried copies). See
//! `docs/PLAN.md` Phase 2 and ADR-0007.
//!
//! Regime split: the wrappers in this module are *self-contained per
//! document* — decoding one document needs no neighbor. Column-block transforms
//! (`Delta`, `Rle`, `Offset` without a static base) live in the
//! document-group regime, not here.

mod refs;
mod ref_mod;
mod enum_tag;
pub mod obj_value;
mod offset;
mod optional;
mod quant;
pub mod varint;
pub mod vector;

pub use refs::Refs;
pub use ref_mod::Ref;
pub use enum_tag::{enum_from_name, Enum, EnumTag};
pub use obj_value::ObjValueType;
pub use optional::{Option, OptionalEnc, WireWidth};
pub use offset::{Offset, offset_decode, offset_encode};
pub use quant::{Quant, dequantize, quantize, wire_to_be_bytes};
pub use varint::{VarInt, VarIntEnc};
pub use vector::{Vector, VectorElem};
