//! Field descriptors — the dependency-free field vocabulary shared by
//! `KeyEncode` / `RowEncode` and downstream layout consumers (snapshot
//! columns, Arrow schema, column builders; ADR-0007).
//!
//! The derive macros emit one `FieldDesc` per declared field (declaration
//! order); `okm::field` (this module) interprets them. Keeping the enum in
//! core — not in the macro crate — means consumers need no proc-macro
//! dependency to read a struct's field layout.

/// Primitive field kinds currently supported by the codec family
/// (big-endian). `Str` is the variable-length kind: it only appears in
/// payload (TLV) positions — the frame's `len u32` IS the length prefix —
/// and is rejected on the fixed-width key side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    U8,
    U16,
    U32,
    U64,
    /// `[u8; N]` — byte width is `FieldDesc::width`.
    FixedBytes,
    /// `String` — variable length; `FieldDesc::width` is 0 (meaningless).
    Str,
    /// `VarInt<T>` — LEB128, variable length; `FieldDesc::width` is 0.
    /// Logical type is the inner unsigned integer.
    VarInt,
    /// `Quant<f64, P>` — fixed-point i64 wire (8 bytes); the payload is the
    /// decimal precision P. Logical type is f64 (wire / 10^P).
    Quant(u32),
    /// `Enum<T>` — one-byte explicit tag (see `EnumTag`).
    Enum,
    /// `Offset<T>` — `value − base` stored as u32; the payload is the
    /// static base. Logical type is i64 (base + wire).
    Offset(i64),
}

/// One declared field: name, primitive kind, byte width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: &'static str,
    pub ty: FieldType,
    pub width: usize,
}
