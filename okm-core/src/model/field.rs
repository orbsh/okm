//! Field descriptors — the dependency-free field vocabulary shared by
//! `KeyEncode` / `DocumentEncode` and downstream layout consumers (snapshot
//! columns, Arrow schema, column builders; ADR-0007).
//!
//! The derive macros emit one `FieldDesc` per declared field (declaration
//! order); `okm_core::field` (this module) interprets them. Keeping the enum in
//! core — not in the macro crate — means consumers need no proc-macro
//! dependency to read a struct's field layout.

/// Primitive field kinds currently supported by the codec family
/// (big-endian). `Str` is the variable-length kind: it only appears in
/// payload (TLV) positions — the frame's `len varint` IS the length prefix —
/// and is rejected on the fixed-width key side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema-serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FieldType {
    U8,
    U16,
    U32,
    U64,
    /// `[u8; N]` — byte width is `FieldDesc::width`.
    FixedBytes,
    /// `String` — variable length; `FieldDesc::width` is 0 (meaningless).
    Str,
    /// `Vec<u8>` — variable length raw bytes (TLV, same frame shape as
    /// `Str` minus the UTF-8 constraint); `FieldDesc::width` is 0.
    Bytes,
    /// `VarInt<T>` — LEB128, variable length; `FieldDesc::width` is 0.
    /// Logical type is the inner unsigned integer.
    VarInt,
    /// `Quant<f64, P>` — fixed-point i64 wire (8 bytes); the payload is the
    /// decimal precision P. Logical type is f64 (wire / 10^P).
    Quant(u32),
    /// `Vocab<N>` — an interned categorical value (ADR-0032): one byte, the id
    /// in that FIELD's data vocabulary. The payload is the field's id face, so
    /// a dynamic reader resolves names with `VocabFaces { id_slot, name_slot:
    /// id_slot + 1 }` without knowing the rest of the declaration.
    Vocab(u16),
    /// `Offset<T>` — `value − base` stored as u32; the payload is the
    /// static base. Logical type is i64 (base + wire).
    Offset(i64),
    /// `Uuid4` — 16 raw bytes, 128 random bits. A CREDENTIAL-shaped id (a token): no structure,
    /// nothing to order by, nothing to predict from a neighbour.
    Uuid4,
    /// `Uuid7` — 16 raw bytes, 48-bit millisecond prefix then randomness. An IDENTITY-shaped id:
    /// byte order is time order, so inserts land at the end of the keyspace.
    Uuid7,
    /// `Vector<T>` — typed homogeneous list (ADR-0015 §4): a variable-
    /// length frame `[count u32 BE][elements per T]`; `elem` names the
    /// element type. Logical type is the element sequence. No length in the
    /// schema — the frame carries its own count; application-level count
    /// contracts are `#[ok_len]` / `FieldSchema::expect_len` (decode-time
    /// checks).
    ///
    /// A newtype, not `{ elem: &'static str }`: the element kind is a CLOSED
    /// set, and a closed set is an enum. That is what lets this enum derive its
    /// own serde impls (below) instead of being mirrored by hand — the mirror
    /// had drifted twice, once by missing variants and once by disagreeing about
    /// a shape, and each time a written sidecar became unreadable.
    Vector(Elem),
}

/// One declared field: name, primitive kind, byte width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: &'static str,
    pub ty: FieldType,
    pub width: usize,
}

/// Const-constructible default literal for `FieldDesc`-adjacent export
/// (`&'static str` instead of `String` — FIELDS is a const). Converted to
/// the owned `schema::DefaultValue` at `CollectionSchema::of` time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultValueConst {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(&'static str),
}

/// A `Vector`'s element kind — the closed set the codewalk supports
/// (`okm-derive`'s `Vector<T>` arm is the one place the declaration meets it).
///
/// Closed on purpose: a sidecar is read with no Rust types present, so the name
/// has to be a vocabulary this crate owns. The lowercase aliases are the spelling
/// earlier sidecars wrote (the type's own name, `"f32"`, `"String"`), so a log
/// written before this enum existed still reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "schema-serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Elem {
    #[cfg_attr(feature = "schema-serde", serde(alias = "f32"))]
    F32,
    #[cfg_attr(feature = "schema-serde", serde(alias = "u32"))]
    U32,
    #[cfg_attr(feature = "schema-serde", serde(alias = "i32"))]
    I32,
    #[cfg_attr(feature = "schema-serde", serde(alias = "u64"))]
    U64,
    #[cfg_attr(feature = "schema-serde", serde(alias = "i64"))]
    I64,
    #[cfg_attr(feature = "schema-serde", serde(alias = "String", alias = "str"))]
    Str,
}

impl Elem {
    /// The Rust type name this element is written from — what a message or a
    /// column description says.
    pub const fn name(self) -> &'static str {
        match self {
            Elem::F32 => "f32",
            Elem::U32 => "u32",
            Elem::I32 => "i32",
            Elem::U64 => "u64",
            Elem::I64 => "i64",
            Elem::Str => "String",
        }
    }
}
