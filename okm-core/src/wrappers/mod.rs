//! Field wrappers — per-field transforms applied at every encode
//! destination (payload value, index-carried copies). See
//! `docs/PLAN.md` Phase 2 and ADR-0007.
//!
//! Regime split: the wrappers in this module are *self-contained per
//! row* — decoding one row needs no neighbor. Column-block transforms
//! (`Delta`, `Rle`, `Offset` without a static base) live in the
//! row-group regime, not here.

mod enum_tag;
mod offset;
mod quant;
pub mod varint;

pub use enum_tag::{Enum, EnumTag};
pub use offset::{Offset, offset_decode, offset_encode};
pub use quant::{Quant, dequantize, quantize, wire_to_be_bytes};
pub use varint::{VarInt, VarIntEnc};
