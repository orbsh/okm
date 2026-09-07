//! Field descriptors — the dependency-free field vocabulary shared by
//! `KeyEncode` / `RowEncode` and downstream layout consumers (snapshot
//! columns, Arrow schema, column builders; ADR-0007).
//!
//! The derive macros emit one `FieldDesc` per declared field (declaration
//! order); `okm::field` (this module) interprets them. Keeping the enum in
//! core — not in the macro crate — means consumers need no proc-macro
//! dependency to read a struct's field layout.

/// Primitive field kinds currently supported by the codec family
/// (fixed-width, big-endian). Variable-length kinds (String) slot in here
/// when the value-side variable-length regime lands (PLAN Phase 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    U8,
    U16,
    U32,
    U64,
    /// `[u8; N]` — byte width is `FieldDesc::width`.
    FixedBytes,
}

/// One declared field: name, primitive kind, byte width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: &'static str,
    pub ty: FieldType,
    pub width: usize,
}
