//! The dynamic segment's vocabulary and value types (ADR-0012, ADR-0057).
//!
//! Slot 1 holds ONE entry per obj. Its value is a block table followed by the
//! leaf bodies; the layout lives in [`crate::model::obj_segment`], which owns
//! both directions. What stays here is the currency the codec spends: the value
//! vocabulary ([`DynamicValue`]), the field id + value pair readers get back
//! ([`DynamicField`]), and the name resolver a writer hands in.
//!
//! Ids are the field NAME's vocabulary number (slots 2/3); u8 ids with `0xFF`
//! escaping to `[0xFF][u16 id]`. Unknown value types on decode are normal input
//! for a dynamic reader: the field is skipped, not fatal — a newer writer may
//! store types an older reader cannot see.

use std::collections::BTreeMap;

/// The value vocabulary: ONE tree for both halves of the crate (ADR-0056 §8).
///
/// It used to exist twice — `DynamicValue` here (dynamic-segment frames, widthless
/// integers) and `okm_dynamic::Value` (the schema-driven codec's currency,
/// width-typed integers). The shapes had drifted apart for a historical reason, not
/// a structural one, and the decoder had to live above core to reach the second.
/// Now there is one:
///
/// - the WIDTH variants are what a typed value carries (`u8`..`u64`, `i64`); the
///   dynamic segment's frame coding is widthless by design (minimal big-endian), so
///   a frame reads back as `U64`/`I64` and a typed field's width comes from its
///   declaration, never from the wire.
/// - `Obj` is the nested map: the in-memory counterpart of a nested nTLV list
///   (ADR-0012 — a map IS an nTLV list; the field NAME → id mapping reuses the same
///   field-name vocabulary).
#[derive(Clone, Debug, PartialEq)]
pub enum DynamicValue {
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
    Array(Vec<DynamicValue>),
    /// Nested object: name-keyed map (see the type's doc comment).
    Obj(BTreeMap<String, DynamicValue>),
}

/// Field name → value. `BTreeMap` for deterministic iteration (cold TLV frames are
/// written in schema declaration order, not map order).
pub type ValueMap = BTreeMap<String, DynamicValue>;

/// Codec errors: schema/value mismatches, out-of-range values, truncated bytes.
/// Codec mistakes are normal input — errors, never panics (ADR-0037).
///
/// It lives here, beside the value vocabulary, because every producer of it is a
/// walk over [`DynamicValue`]: the decoder, the encoder, the payload planner and
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
    pub value: DynamicValue,
}

/// The kind's NAME — the bridge's vocabulary for its own refusals
/// (`from_map` says what it got, not what it wanted).
pub fn dyn_kind(v: &DynamicValue) -> &'static str {
    match v {
        DynamicValue::U8(_) => "U8",
        DynamicValue::U16(_) => "U16",
        DynamicValue::U32(_) => "U32",
        DynamicValue::U64(_) => "U64",
        DynamicValue::I64(_) => "I64",
        DynamicValue::F64(_) => "F64",
        DynamicValue::Str(_) => "Str",
        DynamicValue::Bytes(_) => "Bytes",
        DynamicValue::Bool(_) => "Bool",
        DynamicValue::Null => "Null",
        DynamicValue::Array(_) => "Array",
        DynamicValue::Obj(_) => "Obj",
    }
}

/// Narrow a `u64` id back into a vocabulary's declared width, `None` when it
/// does not fit. Used by `from_map` for the unresolved form `to_map` emits (an
/// id without its label): a silent truncation would point at another label's
/// id, which is the one thing an id must never do.
pub fn vocab_id_from_u64<N: okm_value::Id>(v: u64) -> Option<N> {
    let w = <N as okm_value::Id>::WIDTH;
    let mut img = [0u8; 16];
    img[16 - 8..].copy_from_slice(&v.to_be_bytes());
    // Everything left of the width must be zero, otherwise the value does not
    // fit — the same check a big-endian narrowing would do.
    if img[..16 - w].iter().any(|b| *b != 0) {
        return None;
    }
    <N as okm_value::Id>::from_be(&img[16 - w..])
}

/// Name resolver: returns the vocabulary id for `name`, allocating through the
/// closure on first sight (the caller's vocabulary owns allocation discipline —
/// single writer, engine mutex).
pub trait NameResolver {
    fn resolve(&mut self, name: &str) -> crate::error::Result<u16>;
}

impl<F: FnMut(&str) -> crate::error::Result<u16>> NameResolver for F {
    fn resolve(&mut self, name: &str) -> crate::error::Result<u16> {
        self(name)
    }
}

/// Decode one dynamic segment (ADR-0057) into its fields. `name_of` resolves
/// vocabulary ids to names; an id it cannot resolve keeps its number.
pub fn decode_named(
    bytes: &[u8],
    name_of: &mut dyn FnMut(u16) -> Option<String>,
) -> Vec<DynamicField> {
    crate::model::obj_segment::decode(bytes, name_of)
}

/// [`decode_named`] without a vocabulary: names come back as id strings.
pub fn decode_variants(bytes: &[u8]) -> Vec<DynamicField> {
    crate::model::obj_segment::decode(bytes, &mut |_| None)
}

#[cfg(feature = "schema-serde")]
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
    //! Object keys must be strings on the way in: an `Obj` is name-keyed.

    use super::DynamicValue;
    use serde::de::{MapAccess, SeqAccess, Visitor};
    use serde::ser::{SerializeMap, SerializeSeq};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;
    use std::fmt;

    impl Serialize for DynamicValue {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self {
                DynamicValue::U8(v) => s.serialize_u64(u64::from(*v)),
                DynamicValue::U16(v) => s.serialize_u64(u64::from(*v)),
                DynamicValue::U32(v) => s.serialize_u64(u64::from(*v)),
                DynamicValue::U64(v) => s.serialize_u64(*v),
                DynamicValue::I64(v) => s.serialize_i64(*v),
                DynamicValue::F64(v) => s.serialize_f64(*v),
                DynamicValue::Str(v) => s.serialize_str(v),
                DynamicValue::Bytes(v) => s.serialize_bytes(v),
                DynamicValue::Bool(v) => s.serialize_bool(*v),
                DynamicValue::Null => s.serialize_unit(),
                DynamicValue::Array(items) => {
                    let mut seq = s.serialize_seq(Some(items.len()))?;
                    for item in items {
                        seq.serialize_element(item)?;
                    }
                    seq.end()
                }
                DynamicValue::Obj(map) => {
                    let mut m = s.serialize_map(Some(map.len()))?;
                    for (name, value) in map {
                        m.serialize_entry(name, value)?;
                    }
                    m.end()
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for DynamicValue {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct ValueVisitor;

            impl<'de> Visitor<'de> for ValueVisitor {
                type Value = DynamicValue;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(
                        "a dynamic value (null, bool, integer, float, string, bytes, array, or object)",
                    )
                }

                fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Bool(v))
                }

                fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(DynamicValue::I64(v))
                }

                fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(DynamicValue::U64(v))
                }

                fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
                    Ok(DynamicValue::F64(v))
                }

                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Str(v.to_owned()))
                }

                fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Str(v))
                }

                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Bytes(v.to_vec()))
                }

                fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Bytes(v))
                }

                fn visit_unit<E>(self) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Null)
                }

                fn visit_none<E>(self) -> Result<Self::Value, E> {
                    Ok(DynamicValue::Null)
                }

                fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                    DynamicValue::deserialize(d)
                }

                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                    let mut items = Vec::new();
                    while let Some(item) = seq.next_element::<DynamicValue>()? {
                        items.push(item);
                    }
                    Ok(DynamicValue::Array(items))
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut out = BTreeMap::new();
                    while let Some((name, value)) = map.next_entry::<String, DynamicValue>()? {
                        out.insert(name, value);
                    }
                    Ok(DynamicValue::Obj(out))
                }
            }

            d.deserialize_any(ValueVisitor)
        }
    }
}

#[cfg(all(test, feature = "schema-serde"))]
mod serde_tests {
    use super::DynamicValue;
    use std::collections::BTreeMap;

    fn round_trip(value: &DynamicValue) -> DynamicValue {
        let text = serde_json::to_string(value).expect("serializes");
        serde_json::from_str(&text).expect("deserializes")
    }

    #[test]
    fn every_scalar_round_trips_through_json() {
        let cases = [
            DynamicValue::U64(0),
            DynamicValue::U64(u64::MAX),
            DynamicValue::I64(-7),
            DynamicValue::F64(3.25),
            DynamicValue::Str("你好".into()),
            DynamicValue::Bool(true),
            DynamicValue::Null,
        ];
        for case in cases {
            assert_eq!(round_trip(&case), case, "case {case:?}");
        }
    }

    #[test]
    fn composites_round_trip_and_names_survive() {
        // The shape k10r hands to a model: a record with a nested object,
        // an array of mixed values, and duplicate-free names.
        let value = DynamicValue::Obj(BTreeMap::from([
            ("name".to_string(), DynamicValue::Str("alpha".into())),
            (
                "props".to_string(),
                DynamicValue::Obj(BTreeMap::from([
                    ("vcs".to_string(), DynamicValue::Str("gitea".into())),
                    ("stars".to_string(), DynamicValue::U64(7)),
                ])),
            ),
            (
                "mixed".to_string(),
                DynamicValue::Array(vec![
                    DynamicValue::U64(1),
                    DynamicValue::Str("x".into()),
                    DynamicValue::Null,
                    DynamicValue::Array(vec![DynamicValue::Bool(false)]),
                ]),
            ),
        ]));
        assert_eq!(round_trip(&value), value);
        // The JSON text is the obvious one — a consumer can read it as is.
        assert_eq!(
            serde_json::to_string(&DynamicValue::Obj(BTreeMap::from([(
                "a".into(),
                DynamicValue::U64(1)
            )])))
            .unwrap(),
            r#"{"a":1}"#
        );
    }

    /// The documented asymmetry: bytes have no JSON spelling, so they ride as
    /// a number array there and come back as an `Array` — the storage path
    /// (nTLV) is what keeps them `Bytes`.
    #[test]
    fn bytes_degrade_to_a_number_array_in_json_and_that_is_documented() {
        let bytes = DynamicValue::Bytes(vec![0, 1, 255]);
        let text = serde_json::to_string(&bytes).unwrap();
        assert_eq!(text, "[0,1,255]");
        assert_eq!(
            serde_json::from_str::<DynamicValue>(&text).unwrap(),
            DynamicValue::Array(vec![
                DynamicValue::U64(0),
                DynamicValue::U64(1),
                DynamicValue::U64(255)
            ])
        );
    }

    /// A non-negative `Int` comes back as `UInt` — the other documented
    /// collapse (JSON has one number type).
    #[test]
    fn a_non_negative_int_reads_back_as_uint() {
        let text = serde_json::to_string(&DynamicValue::I64(5)).unwrap();
        assert_eq!(text, "5");
        assert_eq!(
            serde_json::from_str::<DynamicValue>(&text).unwrap(),
            DynamicValue::U64(5)
        );
        // Negative stays `Int`.
        assert_eq!(round_trip(&DynamicValue::I64(-5)), DynamicValue::I64(-5));
    }

    /// A non-string key cannot become an `Obj` name: the deserializer refuses
    /// rather than stringifying it.
    #[test]
    fn a_non_string_object_key_is_refused() {
        assert!(serde_json::from_str::<DynamicValue>(r#"{1: "x"}"#).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::obj_segment::encode;

    /// Names in declaration order: first sight claims the next id, the way a
    /// real vocabulary allocates.
    fn stub(names: &'static [&'static str]) -> impl FnMut(&str) -> crate::error::Result<u16> {
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
        // An Obj nested inside an Array: the shape aura's interface_schema
        // carried in production.
        let nested = BTreeMap::from([("c".to_string(), DynamicValue::I64(0))]);
        let val = DynamicValue::Obj(BTreeMap::from([(
            "item".to_string(),
            DynamicValue::Obj(nested),
        )]));
        let fields = ValueMap::from([("a".to_string(), DynamicValue::Array(vec![val]))]);
        let bytes = encode(&fields, &mut stub(&["a", "item", "c"])).unwrap();
        let got = decode_named(&bytes, &mut names_of(&["a", "item", "c"]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 1);
        match &got[0].value {
            DynamicValue::Array(items) => match &items[0] {
                DynamicValue::Obj(m) => match m.get("item") {
                    Some(DynamicValue::Obj(inner)) => {
                        assert_eq!(inner.get("c"), Some(&DynamicValue::I64(0)));
                    }
                    other => panic!("expected nested Obj, got {other:?}"),
                },
                other => panic!("expected Obj in the array, got {other:?}"),
            },
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn short_ids_take_one_byte_and_large_escape() {
        // The id lane is unchanged by the block table: 254 is one byte, 255 and
        // above escape through 0xFF + u16.
        fn ids_of(id: u16) -> Vec<u8> {
            let fields = ValueMap::from([("x".to_string(), DynamicValue::U64(1))]);
            let mut vocab = move |_: &str| Ok(id);
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
        let mut value = DynamicValue::U64(1);
        for _ in 0..80 {
            value = DynamicValue::Obj(BTreeMap::from([("k".to_string(), value)]));
        }
        let fields = ValueMap::from([("root".to_string(), value)]);
        let mut next = 1u16;
        let bytes = encode(&fields, &mut |_: &str| {
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
}
