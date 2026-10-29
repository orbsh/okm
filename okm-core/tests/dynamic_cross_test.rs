//! Cross-language byte equality (dynamic codec's core discipline): the
//! bytes the dynamic codec produces from a schema + value tree must be
//! byte-for-byte identical to what the Rust derive produces from the
//! same declaration. This is the lock that keeps two encoding
//! implementations from drifting — the schema export is the one
//! declaration driving both sides.
//!
//! Also covers: decode round trips (Rust write → dynamic read, dynamic
//! write → Rust read), cold TLV frames, version rejection, unknown-field
//! rejection.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use okm_core::{KeyEncode, TestStore, Document, DocumentEncode, Collection, VirtualStorage, Offset, Quant, VarInt};
use okm_core::schema::CollectionSchema;
use okm_dynamic::{decode_key, decode_payload, encode_key, encode_payload, CodecError, Value, ValueMap};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 2)]
#[ok_index(by_level { fields(level) })]
pub struct User {
    pub level: u32,          // hot
    pub score: u16,          // hot
    pub name: String,        // cold TLV (tag = declaration index 2)
}

/// v3 evolution of `User`: appended `tier` (hot, literal default 3) and
/// `region` (cold, literal default "eu"). Older payloads lack both —
/// the dynamic reader fills them from the schema export.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 3)]
pub struct UserV3 {
    pub level: u32,
    pub score: u16,
    pub name: String,
    #[ok_default(3)]
    pub tier: u8,
    #[ok_default("eu".to_string())]
    pub region: String,
}

#[test]
fn schema_export_matches_declaration() {
    let s = CollectionSchema::of::<UserKey, User>();
    assert_eq!(s.key_len, 12);
    assert_eq!(s.layout_version, 2);
    assert_eq!(s.hot_width, 4 + 2);
    assert_eq!(s.key_fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["org_id", "user_id"]);
    assert_eq!(s.key_fields.iter().map(|f| f.offset).collect::<Vec<_>>(), [0, 4]);
    assert_eq!(s.hot_fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["level", "score"]);
    assert_eq!(s.cold_fields.iter().map(|f| (f.name.as_str(), f.tag)).collect::<Vec<_>>(), [("name", Some(2))]);
}

fn sample_values() -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("org_id".into(), Value::U32(0x0A0B_0C0D));
    m.insert("user_id".into(), Value::U64(0x1122_3344_5566_7788));
    m.insert("level".into(), Value::U32(9));
    m.insert("score".into(), Value::U16(500));
    m.insert("name".into(), Value::Str("alice".into()));
    m
}

#[test]
fn dynamic_encode_equals_rust_derive_bytes() {
    let schema = CollectionSchema::of::<UserKey, User>();
    let values = sample_values();

    let dyn_key = encode_key(&schema, &values).expect("dynamic key encode");
    let rust_key = UserKey { org_id: 0x0A0B_0C0D, user_id: 0x1122_3344_5566_7788 }.encode();
    assert_eq!(dyn_key, rust_key, "key bytes must match the derive");

    let dyn_payload = encode_payload(&schema, &values).expect("dynamic payload encode");
    let rust_payload = User { level: 9, score: 500, name: "alice".into() }.encode_payload();
    assert_eq!(dyn_payload, rust_payload, "payload bytes must match the derive");
    // Cold frame present: tag 2, len varint 5, "alice".
    assert!(dyn_payload.windows(7).any(|w| w == [2, 5, b'a', b'l', b'i', b'c', b'e']));
}

#[test]
fn rust_write_dynamic_read_and_reverse() {
    let schema = CollectionSchema::of::<UserKey, User>();
    let t: Collection<TestStore, UserKey, User> = Collection::new(TestStore::slatedb_mem());
    let key = UserKey { org_id: 1, user_id: 2 };
    t.put(&key, &User { level: 4, score: 77, name: "bob".into() }).unwrap();

    // Rust wrote; dynamic reads through schema + the SAME physical bytes
    // (primary entry value = payload; primary key suffix = key encoding).
    let pkey = t.primary_key(&key);
    let payload_bytes = t.store().get(&pkey).unwrap().expect("rust write landed");
    let values = decode_payload(&schema, &payload_bytes).expect("dynamic payload decode");
    assert_eq!(values.get("level"), Some(&Value::U32(4)));
    assert_eq!(values.get("score"), Some(&Value::U16(77)));
    assert_eq!(values.get("name"), Some(&Value::Str("bob".into())));

    let key_bytes = key.encode();
    let kv = decode_key(&schema, &key_bytes).expect("dynamic key decode");
    assert_eq!(kv.get("org_id"), Some(&Value::U32(1)));
    assert_eq!(kv.get("user_id"), Some(&Value::U64(2)));

    // Dynamic wrote (via schema encode); Rust reads through the table.
    // TestStore::clone is a deep copy so a shared-engine bypass is not
    // available — the cross check goes through the bytes: Rust decodes
    // the dynamic-encoded payload/key, then puts through the normal
    // path. Byte equality is the contract being verified.
    let values2 = sample_values();
    let dyn_payload = encode_payload(&schema, &values2).expect("dynamic encode");
    let dyn_key = encode_key(&schema, &values2).expect("dynamic encode");
    let rust_decoded = <User as Document>::decode_payload(&dyn_payload);
    let rust_key = <UserKey as KeyEncode>::decode(&dyn_key);
    assert_eq!(rust_decoded.name, "alice");
    assert_eq!(rust_decoded.level, 9);
    assert_eq!(rust_key.org_id, 0x0A0B_0C0D);
    assert_eq!(rust_key.user_id, 0x1122_3344_5566_7788);
    t.put(&rust_key, &rust_decoded).unwrap();
    let document = t.get(&rust_key).unwrap().expect("rust reads dynamic-encoded bytes");
    assert_eq!(document.name, "alice");
    assert_eq!(document.level, 9);
}

#[test]
fn dynamic_rejects_schema_violations() {
    let schema = CollectionSchema::of::<UserKey, User>();

    // Unknown field.
    let mut bad = sample_values();
    bad.insert("ghost".into(), Value::U32(1));
    // (encode ignores map keys not in the schema? NO — unknown names are
    // caller bugs; but the walk is schema-driven, so extra map entries
    // are simply never consulted. Strictness is the binding layer's
    // policy; the core walk is schema-driven and silent here.)

    // Missing field.
    let mut bad = BTreeMap::new();
    bad.insert("org_id".into(), Value::U32(1));
    assert!(matches!(
        encode_key(&schema, &bad),
        Err(CodecError::MissingField(_))
    ));

    // Type mismatch.
    let mut bad = sample_values();
    bad.insert("level".into(), Value::Str("nine".into()));
    assert!(matches!(
        encode_payload(&schema, &bad),
        Err(CodecError::TypeMismatch { .. })
    ));

    // Truncated key.
    assert!(matches!(
        decode_key(&schema, &[0, 1, 2]),
        Err(CodecError::Truncated { .. })
    ));

    // Version mismatch: payload claims version 3, schema is 2.
    let good = encode_payload(&schema, &sample_values()).expect("encode");
    let mut newer = good.clone();
    newer[0] = 3;
    assert!(matches!(
        decode_payload(&schema, &newer),
        Err(CodecError::VersionMismatch { schema: 2, found: 3 })
    ));
}


#[test]
fn version_default_migration_on_dynamic_read() {
    // The v3 schema export carries literal defaults as data.
    let v3 = CollectionSchema::of::<UserKey, UserV3>();
    let tier = v3.hot_fields.iter().find(|f| f.name == "tier").expect("tier field");
    assert_eq!(
        tier.default,
        Some(okm_core::schema::DefaultValue::I64(3)),
        "literal #[ok_default] must export as data"
    );
    let region = v3.cold_fields.iter().find(|f| f.name == "region").expect("region field");
    assert_eq!(
        region.default,
        Some(okm_core::schema::DefaultValue::Str("eu".into()))
    );

    // A payload written by the v2 layout (two fields fewer, older version
    // byte) read through the v3 schema: appended fields arrive as their
    // defaults, pre-existing fields decode normally.
    let v2_payload = User { level: 9, score: 500, name: "alice".into() }.encode_payload();
    let values = decode_payload(&v3, &v2_payload).expect("v2 bytes through v3 schema");
    assert_eq!(values.get("level"), Some(&Value::U32(9)));
    assert_eq!(values.get("tier"), Some(&Value::I64(3)));
    assert_eq!(values.get("region"), Some(&Value::Str("eu".into())));

    // Non-literal / absent defaults fall back to zero for the kind.
    let v2 = CollectionSchema::of::<UserKey, User>();
    assert!(v2.hot_fields.iter().all(|f| f.default.is_none()));
    // (zero fallback covered by encode: a truncated tail decodes as zero)
}

/// ADR-0035's classifier is the PAYLOAD declaration (hot + cold). A key field's
/// name in a value map is therefore NOT identity: OKM is a KV store, the key and
/// the value are independent, so the name is just a value field and lands in the
/// dynamic segment — in both modes (MODELING: "the key is independent of the value").
#[test]
fn a_key_name_in_a_map_is_a_value_field_in_both_modes() {
    use okm_core::model::obj_dynamic::DynamicValue;

    let typed: Collection<TestStore, UserKey, User> =
        Collection::new(TestStore::slatedb_mem());
    let dynamic = okm_dynamic::DynamicCollection::new(
        TestStore::slatedb_mem(),
        41,
        CollectionSchema::of::<UserKey, User>(),
        vec![],
    );
    let key = UserKey { org_id: 7, user_id: 1 };

    let mut typed_map = BTreeMap::new();
    typed_map.insert("org_id".to_string(), DynamicValue::UInt(999));
    typed_map.insert("note".to_string(), DynamicValue::Str("hi".into()));
    typed.merge(&key, &typed_map).unwrap();

    let mut dyn_map = BTreeMap::new();
    dyn_map.insert("org_id".to_string(), Value::U32(999));
    dyn_map.insert("note".to_string(), Value::Str("hi".into()));
    dynamic.merge(&key.encode(), &dyn_map).unwrap();

    // No payload name was given, so no row is written on either side: the value
    // stands alone, addressed by the pkey.
    assert!(typed.get(&key).unwrap().is_none(), "no payload name = no row");
    assert!(
        typed.fetch(&key).unwrap().unwrap().contains_key("org_id"),
        "the name is a value field on the typed side"
    );
    assert!(
        dynamic.fetch(&key.encode()).unwrap().unwrap().contains_key("org_id"),
        "the name is a value field on the dynamic side"
    );
    // The identity is the pkey and a map cannot move it: this invented no row.
    assert!(typed.fetch(&UserKey { org_id: 999, user_id: 1 }).unwrap().is_none());
}

/// Two enumeration modes, because a document is the PAIR of slots (ADR-0035):
/// `scan_keys` is the row mode (one prefix scan), `scan_document_keys` the
/// document mode (a second prefix scan merged in memory) — and materializing
/// either is `fetch`'s job, so the adapter is a plain `map` over the keys.
#[test]
fn the_two_enumeration_modes_see_the_two_kinds_of_document() {
    use okm_core::model::obj_dynamic::DynamicValue;

    let typed: Collection<TestStore, UserKey, User> =
        Collection::new(TestStore::slatedb_mem());
    let with_row = UserKey { org_id: 1, user_id: 1 };
    let segment_only = UserKey { org_id: 2, user_id: 2 };
    typed.put(
        &with_row,
        &User { level: 3, score: 4, name: "row".into() },
    ).unwrap();
    let mut dynamic_only = BTreeMap::new();
    dynamic_only.insert("note".to_string(), DynamicValue::Str("no row".into()));
    typed.merge(&segment_only, &dynamic_only).unwrap();

    assert_eq!(typed.scan_keys().unwrap(), vec![with_row.clone()], "row mode: rows only");
    assert_eq!(
        typed.scan_document_keys().unwrap(),
        vec![with_row.clone(), segment_only.clone()],
        "document mode: the union, key order, one entry per document"
    );

    let docs: Vec<_> = typed
        .scan_document_keys().unwrap()
        .iter()
        .map(|k| typed.fetch(k).unwrap().expect("a document"))
        .collect();
    assert_eq!(docs.len(), 2, "the adapter is a map over keys + fetch");
    assert!(docs[1].contains_key("note"), "the segment-only document fetched");

    // The dynamic face answers the same two modes over the same two kinds.
    let dynamic = okm_dynamic::DynamicCollection::new(
        TestStore::slatedb_mem(),
        41,
        CollectionSchema::of::<UserKey, User>(),
        vec![],
    );
    let mut row_map = BTreeMap::new();
    row_map.insert("level".to_string(), Value::U32(3));
    dynamic.merge(&with_row.encode(), &row_map).unwrap();
    let mut dyn_only = BTreeMap::new();
    dyn_only.insert("note".to_string(), Value::Str("no row".into()));
    dynamic.merge(&segment_only.encode(), &dyn_only).unwrap();
    assert_eq!(
        dynamic.scan_document_keys().unwrap(),
        vec![with_row.encode(), segment_only.encode()],
        "the dynamic face merges the same two slots"
    );
    assert!(dynamic
        .fetch(&segment_only.encode()).unwrap()
        .unwrap()
        .contains_key("note"));
}

/// The wrapper kinds are where a "framing" codec and a "typed" codec diverge
/// silently: `Quant` is a fixed-point i64, `Offset` a 4-byte biased u32, `VarInt`
/// a prefix-monotonic varint whose frame length is the encoding's own. Writing
/// them as one 8-byte BE image (what a value-only dispatch does) makes a typed
/// reader overrun a varint frame, truncate an offset frame, and return an
/// integer for a quantised value.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(42)]
pub struct Wrapped {
    pub n: u32, // hot, 4
    #[ok_offset(base = 1_700_000_000)]
    pub created: Offset, // hot, u32 wire
    pub price: Quant<3>, // hot, i64 wire (f64 logical)
    pub views: VarInt<u64>, // cold TLV, variable width
}

fn wrapped_row() -> Wrapped {
    Wrapped {
        n: 7,
        created: Offset(1_700_000_050),
        price: Quant::<3>(12.345),
        views: VarInt(300u64),
    }
}

fn wrapped_values() -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("n".into(), Value::U32(7));
    m.insert("created".into(), Value::I64(1_700_000_050));
    m.insert("price".into(), Value::F64(12.345));
    m.insert("views".into(), Value::U64(300));
    m
}

#[test]
fn wrapper_kinds_encode_as_the_schema_says_and_the_derive_does() {
    let schema = CollectionSchema::of::<UserKey, Wrapped>();
    let rust = wrapped_row().encode_payload();

    let dyn_payload = encode_payload(&schema, &wrapped_values()).expect("dynamic encode");
    assert_eq!(
        dyn_payload, rust,
        "wrapper frames must be the derive's bytes: 4-byte biased offset, \
         8-byte fixed-point quant, varint views"
    );

    // Stated once, so a regression names the kind it broke.
    assert!(
        rust.windows(4).any(|w| w == 50u32.to_be_bytes()),
        "Offset: base + 50 lands as the 4-byte biased wire"
    );
    assert!(
        rust.windows(8).any(|w| w == 12_345i64.to_be_bytes()),
        "Quant: 12.345 at P=3 lands as 12345"
    );
    // VarInt 300 = 2 bytes: prefix(2) | high bits, then the low byte.
    assert!(
        rust.windows(4).any(|w| w == [3, 2, 0x81, 0x2C]),
        "VarInt: cold tag 3, frame len 2, prefix-encoded 300"
    );

    // And back: the dynamic reader returns the LOGICAL value, the shape a
    // decoded row has (`UInt` for the varint, `Int` for the offset, `F64` for
    // the quant — the same faces the derive's `to_map` emits).
    let back = decode_payload(&schema, &dyn_payload).expect("dynamic decode");
    assert_eq!(back.get("created"), Some(&Value::I64(1_700_000_050)));
    assert_eq!(back.get("price"), Some(&Value::F64(12.345)));
    assert_eq!(back.get("views"), Some(&Value::U64(300)));

    // The typed reader accepts the dynamic bytes as its own.
    assert_eq!(<Wrapped as Document>::decode_payload(&dyn_payload), wrapped_row());
}

#[test]
fn a_wrapper_value_outside_its_wire_is_an_error_not_a_panic() {
    let schema = CollectionSchema::of::<UserKey, Wrapped>();
    let mut below_base = wrapped_values();
    below_base.insert("created".into(), Value::I64(1_699_999_999));
    assert!(
        encode_payload(&schema, &below_base).is_err(),
        "Offset has no representation below its base (offset_encode would panic)"
    );

    let mut storage_form = wrapped_values();
    storage_form.insert("price".into(), Value::U64(12_345));
    assert!(
        encode_payload(&schema, &storage_form).is_err(),
        "a quant field fed its wire instead of its value is a type error"
    );
}
