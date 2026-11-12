//! The dynamic segment's value vocabulary (ADR-0012, ADR-0057).
//!
//! Slot 1 holds ONE entry per obj. Its value is a block table followed by the
//! leaf bodies; the layout lives in [`crate::segment`], which owns both
//! directions. What stays here is the currency the codec spends: the value
//! vocabulary ([`Variant`]), the value-type tags, and the field id + value
//! pair readers get back ([`DynamicField`]).
//!
//! Ids are the field NAME's vocabulary number (slots 2/3); u8 ids with `0xFF`
//! escaping to `[0xFF][u16 id]`. Unknown value types on decode are normal input
//! for a dynamic reader: the field is skipped, not fatal — a newer writer may
//! store types an older reader cannot see.
//!
//! The object form is [`Dict`]: a TYPE, not an alias for the container it
//! happens to use today. Its names iterate in INSERTION order, its backing map
//! is private, its methods are written out and forwarded one by one (no
//! `Deref`), and no method signature names the container — so it can be swapped
//! without a breaking change, and the ordering it promises is the model's, not
//! a dependency's.

use indexmap::IndexMap;

/// The value a document holds: a tagged union of the kinds the document model
/// admits — `U8`/`U16`/`U32`/`U64`/`I64`/`F64`/`Str`/`Bytes`/`Bool`/`Null`/`Array`
/// and `Map` (ONE tree for both halves of the crate, ADR-0056 §8).
///
/// It used to exist twice — one tree for dynamic-segment frames (widthless
/// integers) and one as the schema-driven codec's currency (width-typed integers,
/// re-exported by `okm_dynamic` as `Value`). The shapes had drifted apart for a
/// historical reason, not a structural one, and the decoder had to live above core
/// to reach the second. Now there is one:
///
/// - the WIDTH variants are what a typed value carries (`u8`..`u64`, `i64`); the
///   dynamic segment's frame coding is widthless by design (minimal big-endian), so
///   a frame reads back as `U64`/`I64` and a typed field's width comes from its
///   declaration, never from the wire.
/// - `Map` is the name-keyed object: the in-memory counterpart of a nested nTLV list
///   (ADR-0012 — a map IS an nTLV list; the field NAME → id mapping reuses the same
///   field-name vocabulary).
#[derive(Clone, Debug, PartialEq)]
pub enum Variant {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    /// Signed integer (typed path: `i8`..`i64` fields, `Offset` lifts).
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    /// Nothing stored; presence itself is the information.
    Null,
    Array(Vec<Variant>),
    /// Nested object: name-keyed map (see the type's doc comment).
    Map(Dict),
}

/// The object a document holds: an insertion-ordered name → value map, and the
/// only spelling of "the map a `Map` variant carries".
///
/// It is a TYPE, not an alias, because [`Variant::Map`] has to name some type.
/// Naming the backing map there would freeze that map into the public API —
/// callers would name it in their own signatures, and swapping it later would
/// be a breaking change. The field is private and no public signature mentions
/// it, so the backing type is NOT part of this contract: replacing it must not
/// change one signature, one name, one value.
///
/// # The surface is the impl block below
///
/// Every method this type has is written in that block and forwards to the
/// backing map; anything not written there does not exist. It is deliberately
/// NOT a `Deref`, which would leak the container two ways: coercion would let a
/// caller's `fn f(&<container>)` accept a `&Dict`, naming the backing type in
/// someone else's signature, and every backing method (`swap_remove`,
/// `sort_keys`, `swap_indices`, …) would become reachable and could break the
/// insertion-order promise below. The container's name survives in exactly two
/// places beside the bodies — `From<..>` and the `IntoIterator` associated
/// types — and both are conversions the caller drives, never a way to pass a
/// `&Dict` off as the container. Swapping the container rewrites this one impl
/// block plus those conversions, and nothing any caller wrote.
///
/// # Iteration order is INSERTION order
///
/// `iter` and `keys` walk names in the order they were first
/// inserted: an overwrite keeps the name's place, and a `remove` followed by an
/// insert moves it to the end. Nothing sorts them. A site that needs a
/// canonical order — an assertion, a diff, a snapshot column — sorts
/// explicitly and says so, because the order a document's names were assembled
/// in is how it was built, not what it is.
///
/// # Equality ignores order
///
/// Two maps are equal when they carry the same names with the same values. A
/// document is a set of name→value pairs; insertion order is provenance, and
/// comparing two documents must not depend on the path that produced them.
///
/// # Why this is a type while `Array` stays a `Vec`
///
/// [`Variant::Array`] holds a plain `Vec<Variant>`: a vector has one canonical
/// implementation whose behaviour is fully specified by the standard library,
/// so naming it costs the model nothing. An ordered name→value map is a real
/// design choice — sorted, hashed, insertion-ordered, and memory-per-name all
/// differ, and the choice is visible through iteration order. That is the one
/// thing a reader of a document can observe, so it is the one thing the model
/// must own rather than inherit from a dependency.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict(IndexMap<String, Variant>);

impl Dict {
    /// No names.
    pub fn new() -> Self {
        Self(IndexMap::new())
    }

    /// No names, with room for `capacity` of them.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(IndexMap::with_capacity(capacity))
    }

    /// Number of names.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No names at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Put `value` under `name`, answering the value it displaced.
    ///
    /// A new name lands LAST; an overwrite keeps the name's place.
    pub fn insert(&mut self, name: String, value: Variant) -> Option<Variant> {
        self.0.insert(name, value)
    }

    /// The value stored under `name`.
    pub fn get(&self, name: &str) -> Option<&Variant> {
        self.0.get(name)
    }

    /// The value stored under `name`, for in-place edit.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Variant> {
        self.0.get_mut(name)
    }

    /// Whether `name` is present.
    pub fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Remove `name`, answering the value it carried.
    ///
    /// This is the one method whose NAME is restated rather than forwarded
    /// verbatim: the container's `remove` is its swap form, which moves the
    /// LAST name into the hole and so breaks the insertion order this type
    /// promises. `shift_remove` closes the gap instead.
    pub fn remove(&mut self, name: &str) -> Option<Variant> {
        self.0.shift_remove(name)
    }

    /// Names and values in INSERTION order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Variant)> {
        self.0.iter()
    }

    /// Names in INSERTION order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

impl From<IndexMap<String, Variant>> for Dict {
    fn from(map: IndexMap<String, Variant>) -> Self {
        Self(map)
    }
}

impl FromIterator<(String, Variant)> for Dict {
    fn from_iter<I: IntoIterator<Item = (String, Variant)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<(String, Variant)> for Dict {
    fn extend<I: IntoIterator<Item = (String, Variant)>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl<const N: usize> From<[(String, Variant); N]> for Dict {
    fn from(pairs: [(String, Variant); N]) -> Self {
        Self(IndexMap::from(pairs))
    }
}

impl IntoIterator for Dict {
    type Item = (String, Variant);
    type IntoIter = indexmap::map::IntoIter<String, Variant>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Dict {
    type Item = (&'a String, &'a Variant);
    type IntoIter = indexmap::map::Iter<'a, String, Variant>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Variant {
    /// Object merge: the composition rule for a document assembled from two
    /// halves (a decoded row and a dynamic segment).
    ///
    /// - two `Map`s merge NAME BY NAME: a name both sides carry recurses, a
    ///   name only the right side carries is added, a name only the left side
    ///   carries keeps its value;
    /// - anything else REPLACES the left side outright. An `Array` is a VALUE,
    ///   not a list of names, so it is replaced rather than concatenated; a
    ///   scalar cannot carry names at all, so the right side wins.
    ///
    /// `merge` is left-biased in the absence of a name: the left side is the
    /// document, the right side is what is laid over it.
    pub fn merge(&mut self, other: Variant) {
        match (self, other) {
            (Variant::Map(left), Variant::Map(right)) => {
                for (name, value) in right {
                    match left.get_mut(&name) {
                        Some(slot) => slot.merge(value),
                        None => {
                            left.insert(name, value);
                        }
                    }
                }
            }
            (slot, other) => *slot = other,
        }
    }

    /// [`merge`](Self::merge), owning the receiver: `a.merged(b)` is `b` laid
    /// over `a`.
    pub fn merged(mut self, other: Variant) -> Variant {
        self.merge(other);
        self
    }
}

/// The empty document — a `Map` with no names, not `Null`.
///
/// It exists so a half that is ABSENT contributes nothing when a document is
/// composed (`row.map(to_value).unwrap_or_default().merged(segment
/// .unwrap_or_default())`): the identity of [`Variant::merge`] is "no
/// names", which is an empty object. `Null` would not do — it replaces the left
/// side, which is a documented value, not an absence marker.
impl Default for Variant {
    fn default() -> Self {
        Variant::Map(Dict::new())
    }
}

/// Codec errors: schema/value mismatches, out-of-range values, truncated bytes.
/// Codec mistakes are normal input — errors, never panics (ADR-0037).
///
/// It lives here, beside the value vocabulary, because every producer of it is a
/// walk over [`Variant`]: the decoder, the encoder, the payload planner and
/// the column builder.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// Field present in the value tree but absent from the schema.
    UnknownField(String),
    /// Schema field missing from the value tree.
    MissingField(String),
    /// Value variant does not match the field's declared kind.
    TypeMismatch {
        field: String,
        expected: &'static str,
    },
    /// Encoded bytes shorter than the schema requires.
    Truncated {
        field: String,
        needed: usize,
        got: usize,
    },
    /// Cold TLV frame with a tag outside the schema's cold field set.
    UnknownTag(u8),
    /// Invalid UTF-8 in a Str field.
    InvalidUtf8(String),
    /// A cold payload field carries no TLV tag. The schema is caller-supplied
    /// (it round-trips through JSON), so this is input, not an invariant — and an
    /// untagged cold frame would encode to a payload the reader cannot walk back.
    ColdFieldWithoutTag(String),
    /// A column could not be built (the arrow-level assembly, not the codec):
    /// carries its own text because there is no single field to point at.
    Column { what: String },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnknownField(n) => write!(f, "unknown field `{n}` (not in schema)"),
            CodecError::MissingField(n) => write!(f, "missing field `{n}`"),
            CodecError::TypeMismatch { field, expected } => {
                write!(f, "field `{field}`: expected {expected}")
            }
            CodecError::Truncated { field, needed, got } => {
                write!(
                    f,
                    "field `{field}` truncated: need {needed} bytes, got {got}"
                )
            }
            CodecError::UnknownTag(t) => write!(f, "unknown cold TLV tag {t}"),
            CodecError::InvalidUtf8(n) => write!(f, "field `{n}`: invalid UTF-8"),
            CodecError::ColdFieldWithoutTag(n) => write!(
                f,
                "cold field `{n}` carries no tag (the schema's cold fields are ordered TLV frames)"
            ),
            CodecError::Column { what } => write!(f, "column: {what}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// One dynamic field: vocabulary id + decoded value.
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicField {
    pub id: u16,
    pub value: Variant,
}

/// The kind's NAME — the bridge's vocabulary for its own refusals
/// (`from_map` says what it got, not what it wanted).
pub fn dyn_kind(v: &Variant) -> &'static str {
    match v {
        Variant::U8(_) => "U8",
        Variant::U16(_) => "U16",
        Variant::U32(_) => "U32",
        Variant::U64(_) => "U64",
        Variant::I64(_) => "I64",
        Variant::F64(_) => "F64",
        Variant::Str(_) => "Str",
        Variant::Bytes(_) => "Bytes",
        Variant::Bool(_) => "Bool",
        Variant::Null => "Null",
        Variant::Array(_) => "Array",
        Variant::Map(_) => "Map",
    }
}

/// Narrow a `u64` id back into a vocabulary's declared width, `None` when it
/// does not fit. Used by `from_map` for the unresolved form `to_map` emits (an
/// id without its label): a silent truncation would point at another label's
/// id, which is the one thing an id must never do.
pub fn vocab_id_from_u64<N: crate::Id>(v: u64) -> Option<N> {
    let w = <N as crate::Id>::WIDTH;
    let mut img = [0u8; 16];
    img[16 - 8..].copy_from_slice(&v.to_be_bytes());
    // Everything left of the width must be zero, otherwise the value does not
    // fit — the same check a big-endian narrowing would do.
    if img[..16 - w].iter().any(|b| *b != 0) {
        return None;
    }
    <N as crate::Id>::from_be(&img[16 - w..])
}

// Name resolver: returns the vocabulary id for `name`, allocating through the
// closure on first sight (the caller's vocabulary owns allocation discipline —
// single writer, engine mutex).
// A writer hands the encoder a closure, not a trait object: the closure's error
// type is the caller's own, and the encoder carries it out unchanged
// ([`crate::Error::Names`]).

/// Decode one dynamic segment (ADR-0057) into its fields. `name_of` resolves
/// vocabulary ids to names; an id it cannot resolve keeps its number.
pub fn decode_named(
    bytes: &[u8],
    name_of: &mut dyn FnMut(u16) -> Option<String>,
) -> Vec<DynamicField> {
    crate::segment::decode(bytes, name_of)
}

/// [`decode_named`] without a vocabulary: names come back as id strings.
pub fn decode_variants(bytes: &[u8]) -> Vec<DynamicField> {
    crate::segment::decode(bytes, &mut |_| None)
}

#[cfg(feature = "serde")]
mod serde_impls {
    //! The value form's serde bridge (feature `schema-serde`), for consumers
    //! that cross a boundary in another representation — the packet a memory
    //! consumer renders for a model, an MQ payload, a binding's argument.
    //!
    //! This is `Serialize`/`Deserialize`, NOT `to_json`: the format is the
    //! caller's choice (JSON, CBOR, …), and the storage encoding is
    //! untouched — the dynamic segment still stores its block table and value
    //! area (ADR-0057), with names in the vocabulary and fixed widths where the
    //! declaration says so. The impl is the adapter at the edge, so a consumer stops
    //! hand-writing a `match` per direction.
    //!
    //! `Deserialize` uses `deserialize_any`, so it needs a SELF-DESCRIBING
    //! format (JSON, CBOR, MessagePack — not bincode). Two asymmetries no
    //! self-describing text format can avoid, and neither loses data the
    //! storage path keeps:
    //! - `UInt` and a non-negative `Int` are one number in JSON: a `UInt`
    //!   round-trips, a positive `Int` comes back as `UInt`.
    //! - `Bytes` has no JSON spelling: it serializes as an array of numbers
    //!   there (CBOR keeps a byte string), so it does not round-trip through
    //!   JSON as `Bytes`.
    //!
    //! Object keys must be strings on the way in: a `Map` is name-keyed.

    use super::Variant;
    use serde::de::{MapAccess, SeqAccess, Visitor};
    use serde::ser::{SerializeMap, SerializeSeq};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::fmt;

    impl Serialize for Variant {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self {
                Variant::U8(v) => s.serialize_u64(u64::from(*v)),
                Variant::U16(v) => s.serialize_u64(u64::from(*v)),
                Variant::U32(v) => s.serialize_u64(u64::from(*v)),
                Variant::U64(v) => s.serialize_u64(*v),
                Variant::I64(v) => s.serialize_i64(*v),
                Variant::F64(v) => s.serialize_f64(*v),
                Variant::Str(v) => s.serialize_str(v),
                Variant::Bytes(v) => s.serialize_bytes(v),
                Variant::Bool(v) => s.serialize_bool(*v),
                Variant::Null => s.serialize_unit(),
                Variant::Array(items) => {
                    let mut seq = s.serialize_seq(Some(items.len()))?;
                    for item in items {
                        seq.serialize_element(item)?;
                    }
                    seq.end()
                }
                Variant::Map(map) => {
                    let mut m = s.serialize_map(Some(map.len()))?;
                    for (name, value) in map {
                        m.serialize_entry(name, value)?;
                    }
                    m.end()
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for Variant {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct ValueVisitor;

            impl<'de> Visitor<'de> for ValueVisitor {
                type Value = Variant;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(
                        "a dynamic value (null, bool, integer, float, string, bytes, array, or object)",
                    )
                }

                fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(Variant::Bool(v))
                }

                fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(Variant::I64(v))
                }

                fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(Variant::U64(v))
                }

                fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                    Ok(Variant::F64(v))
                }

                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(Variant::Str(v.to_owned()))
                }

                fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                    Ok(Variant::Str(v))
                }

                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                    Ok(Variant::Bytes(v.to_vec()))
                }

                fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                    Ok(Variant::Bytes(v))
                }

                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(Variant::Null)
                }

                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(Variant::Null)
                }

                fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                    Variant::deserialize(d)
                }

                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                    let mut items = Vec::new();
                    while let Some(item) = seq.next_element::<Variant>()? {
                        items.push(item);
                    }
                    Ok(Variant::Array(items))
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut out = super::Dict::new();
                    while let Some((name, value)) = map.next_entry::<String, Variant>()? {
                        out.insert(name, value);
                    }
                    Ok(Variant::Map(out))
                }
            }

            d.deserialize_any(ValueVisitor)
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::{Dict, Variant};

    fn round_trip(value: &Variant) -> Variant {
        let text = serde_json::to_string(value).expect("serializes");
        serde_json::from_str(&text).expect("deserializes")
    }

    #[test]
    fn every_scalar_round_trips_through_json() {
        let cases = [
            Variant::U64(0),
            Variant::U64(u64::MAX),
            Variant::I64(-7),
            Variant::F64(3.25),
            Variant::Str("你好".into()),
            Variant::Bool(true),
            Variant::Null,
        ];
        for case in cases {
            assert_eq!(round_trip(&case), case, "case {case:?}");
        }
    }

    #[test]
    fn composites_round_trip_and_names_survive() {
        // The shape k10r hands to a model: a record with a nested object,
        // an array of mixed values, and duplicate-free names.
        let value = Variant::Map(Dict::from([
            ("name".to_string(), Variant::Str("alpha".into())),
            (
                "props".to_string(),
                Variant::Map(Dict::from([
                    ("vcs".to_string(), Variant::Str("gitea".into())),
                    ("stars".to_string(), Variant::U64(7)),
                ])),
            ),
            (
                "mixed".to_string(),
                Variant::Array(vec![
                    Variant::U64(1),
                    Variant::Str("x".into()),
                    Variant::Null,
                    Variant::Array(vec![Variant::Bool(false)]),
                ]),
            ),
        ]));
        assert_eq!(round_trip(&value), value);
        // The JSON text is the obvious one — a consumer can read it as is.
        assert_eq!(
            serde_json::to_string(&Variant::Map(Dict::from([("a".into(), Variant::U64(1))])))
                .unwrap(),
            r#"{"a":1}"#
        );
    }

    /// The documented asymmetry: bytes have no JSON spelling, so they ride as
    /// a number array there and come back as an `Array` — the storage path
    /// (nTLV) is what keeps them `Bytes`.
    #[test]
    fn bytes_degrade_to_a_number_array_in_json_and_that_is_documented() {
        let bytes = Variant::Bytes(vec![0, 1, 255]);
        let text = serde_json::to_string(&bytes).unwrap();
        assert_eq!(text, "[0,1,255]");
        assert_eq!(
            serde_json::from_str::<Variant>(&text).unwrap(),
            Variant::Array(vec![Variant::U64(0), Variant::U64(1), Variant::U64(255)])
        );
    }

    /// A non-negative `Int` comes back as `UInt` — the other documented
    /// collapse (JSON has one number type).
    #[test]
    fn a_non_negative_int_reads_back_as_uint() {
        let text = serde_json::to_string(&Variant::I64(5)).unwrap();
        assert_eq!(text, "5");
        assert_eq!(
            serde_json::from_str::<Variant>(&text).unwrap(),
            Variant::U64(5)
        );
        // Negative stays `Int`.
        assert_eq!(round_trip(&Variant::I64(-5)), Variant::I64(-5));
    }

    /// A non-string key cannot become a `Map` name: the deserializer refuses
    /// rather than stringifying it.
    #[test]
    fn a_non_string_object_key_is_refused() {
        assert!(serde_json::from_str::<Variant>(r#"{1: "x"}"#).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::encode;

    /// Names in declaration order: first sight claims the next id, the way a
    /// real vocabulary allocates.
    fn stub(names: &'static [&'static str]) -> impl FnMut(&str) -> Result<u16, ()> {
        move |name: &str| {
            let at = names.iter().position(|n| *n == name).unwrap_or(names.len());
            Ok((at + 1) as u16)
        }
    }

    fn names_of(names: &'static [&'static str]) -> impl FnMut(u16) -> Option<String> {
        move |id| {
            names
                .get((id as usize).checked_sub(1)?)
                .map(|s| (*s).to_string())
        }
    }

    #[test]
    fn obj_in_array_roundtrip() {
        // A `Map` nested inside an `Array`: the shape aura's interface_schema
        // carried in production.
        let nested = Dict::from([("c".to_string(), Variant::I64(0))]);
        let val = Variant::Map(Dict::from([("item".to_string(), Variant::Map(nested))]));
        let fields = Dict::from([("a".to_string(), Variant::Array(vec![val]))]);
        let bytes = encode(&fields, &mut stub(&["a", "item", "c"])).unwrap();
        let got = decode_named(&bytes, &mut names_of(&["a", "item", "c"]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 1);
        match &got[0].value {
            Variant::Array(items) => match &items[0] {
                Variant::Map(m) => match m.get("item") {
                    Some(Variant::Map(inner)) => {
                        assert_eq!(inner.get("c"), Some(&Variant::I64(0)));
                    }
                    other => panic!("expected nested Map, got {other:?}"),
                },
                other => panic!("expected Map in the array, got {other:?}"),
            },
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn short_ids_take_one_byte_and_large_escape() {
        // The id lane is unchanged by the block table: 254 is one byte, 255 and
        // above escape through 0xFF + u16.
        fn ids_of(id: u16) -> Vec<u8> {
            let fields = Dict::from([("x".to_string(), Variant::U64(1))]);
            let mut vocab = move |_: &str| Ok::<u16, ()>(id);
            encode(&fields, &mut vocab).unwrap()
        }
        let short = ids_of(0xFE);
        assert!(short[2..].contains(&0xFE), "id 254 stays in the u8 lane");
        let escaped = ids_of(0x1234);
        let at = escaped.windows(3).position(|w| w == [0xFF, 0x12, 0x34]);
        assert!(at.is_some(), "id 0x1234 escapes through 0xFF");
        let got = decode_variants(&ids_of(0x1234));
        assert_eq!(got[0].id, 0x1234);
    }

    #[test]
    fn nested_deep_recursion_is_capped() {
        // A reader refuses nesting deeper than the cap instead of walking a
        // hostile value down the stack.
        let mut value = Variant::U64(1);
        for _ in 0..80 {
            value = Variant::Map(Dict::from([("k".to_string(), value)]));
        }
        let fields = Dict::from([("root".to_string(), value)]);
        let mut next = 1u16;
        let bytes = encode(&fields, &mut |_: &str| -> Result<u16, ()> {
            let id = next;
            next += 1;
            Ok(id)
        })
        .unwrap();
        assert!(
            decode_variants(&bytes).is_empty(),
            "over-deep value is refused"
        );
    }

    #[test]
    fn objects_merge_name_by_name_and_nested_objects_recurse() {
        let left = Variant::Map(Dict::from([
            ("a".to_string(), Variant::U32(1)),
            (
                "inner".to_string(),
                Variant::Map(Dict::from([
                    ("x".to_string(), Variant::Str("kept".into())),
                    ("y".to_string(), Variant::Null),
                ])),
            ),
        ]));
        let right = Variant::Map(Dict::from([
            ("b".to_string(), Variant::Bool(true)),
            (
                "inner".to_string(),
                Variant::Map(Dict::from([("y".to_string(), Variant::U8(9))])),
            ),
        ]));
        let got = left.merged(right);
        assert_eq!(
            got,
            Variant::Map(Dict::from([
                ("a".to_string(), Variant::U32(1)),
                ("b".to_string(), Variant::Bool(true)),
                (
                    "inner".to_string(),
                    Variant::Map(Dict::from([
                        ("x".to_string(), Variant::Str("kept".into())),
                        ("y".to_string(), Variant::U8(9)),
                    ]))
                ),
            ]))
        );
    }

    #[test]
    fn an_array_replaces_instead_of_appending() {
        let left = Variant::Map(Dict::from([(
            "tags".to_string(),
            Variant::Array(vec![Variant::Str("a".into())]),
        )]));
        let right = Variant::Map(Dict::from([(
            "tags".to_string(),
            Variant::Array(vec![Variant::Str("b".into())]),
        )]));
        assert_eq!(left.merged(right.clone()), right);
    }

    #[test]
    fn a_scalar_replaces_the_left_side() {
        // The right side is what the caller spelled; a scalar cannot carry
        // names, so there is nothing to merge into.
        assert_eq!(
            Variant::U32(1).merged(Variant::Str("x".into())),
            Variant::Str("x".into())
        );
        assert_eq!(
            Variant::Map(Dict::new()).merged(Variant::Null),
            Variant::Null
        );
    }

    #[test]
    fn the_empty_object_is_the_merge_identity() {
        // An absent half contributes nothing: `unwrap_or_default()` on either
        // side of the composition leaves the other side untouched.
        let doc = Variant::Map(Dict::from([("a".to_string(), Variant::U32(1))]));
        assert_eq!(doc.clone().merged(Variant::default()), doc);
        assert_eq!(
            Variant::default().merged(doc.clone()),
            doc,
            "an absent row contributes no names"
        );
    }
}

/// Value types of the dynamic segment. One byte per frame header.
///
/// Integers: signless framing — the caller's Rust type decides width;
/// the wire stores the value big-endian and the reader casts. `Array`
/// is a count-prefixed list of self-describing frames (each element
/// re-carries its type); nested objects use `Obj` with the same
/// reserved contract (added when nesting ships — the variant exists so
/// stored data can already carry the tag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjValueType {
    /// Unsigned integer, big-endian, width implied by the frame length.
    UInt = 0,
    /// IEEE 754 f64, 8 bytes.
    F64 = 1,
    /// UTF-8 string, `len` bytes.
    Str = 2,
    /// Raw bytes, `len` bytes.
    Bytes = 3,
    /// One byte: 0 = false, non-zero = true.
    Bool = 4,
    /// Zero-length frame (len must be 0).
    Null = 5,
    /// Array: `len` counts the ELEMENT count prefix (u32 BE) plus the
    /// concatenated self-describing element frames.
    Array = 6,
    /// Nested object (reserved for the nesting milestone — frames carry
    /// the same shape as the top-level dynamic segment).
    Obj = 7,
    /// Signed integer, big-endian two's complement, width implied by the
    /// frame length (sign-extended on read).
    Int = 8,
}

impl ObjValueType {
    /// Wire byte → type. Unknown tags are normal input for a dynamic
    /// reader (a newer writer stored something it cannot see) — return
    /// `None`, never panic.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::UInt),
            1 => Some(Self::F64),
            2 => Some(Self::Str),
            3 => Some(Self::Bytes),
            4 => Some(Self::Bool),
            5 => Some(Self::Null),
            6 => Some(Self::Array),
            7 => Some(Self::Obj),
            8 => Some(Self::Int),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_unknown_is_none() {
        for b in 0u8..=8 {
            let t = ObjValueType::from_byte(b).expect("0-8 are defined");
            assert_eq!(t.to_byte(), b);
        }
        assert_eq!(ObjValueType::from_byte(9), None);
        assert_eq!(ObjValueType::from_byte(0xFF), None);
    }

    #[test]
    fn tags_are_stable_contract() {
        // Explicit byte values, hex-locked: stored frames reference these
        // forever (append-only, never renumber).
        assert_eq!(ObjValueType::UInt.to_byte(), 0);
        assert_eq!(ObjValueType::F64.to_byte(), 1);
        assert_eq!(ObjValueType::Str.to_byte(), 2);
        assert_eq!(ObjValueType::Bytes.to_byte(), 3);
        assert_eq!(ObjValueType::Bool.to_byte(), 4);
        assert_eq!(ObjValueType::Null.to_byte(), 5);
        assert_eq!(ObjValueType::Array.to_byte(), 6);
        assert_eq!(ObjValueType::Obj.to_byte(), 7);
        assert_eq!(ObjValueType::Int.to_byte(), 8);
    }
}
