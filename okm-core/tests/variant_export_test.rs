//! Variant column export acceptance: `to_record_batch_with_variant`
//! produces one Variant value per document from its dynamic segment —
//! declared fields stay typed columns, dynamic fields ride the Variant
//! column, and documents without dynamic entries read as null.
use okm_core::{DocumentEncode, KeyEncode, obj_dynamic::DynamicValue, TestStore};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub name: String,
}

#[test]
fn variant_column_export() {
    let mut t: okm_core::Collection<TestStore, UserKey, User> =
        okm_core::Collection::new(TestStore::slatedb_mem());

    // Document 1: declared fields + dynamic segment.
    let mut dyn1: BTreeMap<String, DynamicValue> = BTreeMap::new();
    dyn1.insert("score".into(), DynamicValue::UInt(99));
    dyn1.insert(
        "nested".into(),
        DynamicValue::Obj(BTreeMap::from([
            ("x".into(), DynamicValue::Int(-3)),
            ("flag".into(), DynamicValue::Bool(true)),
        ])),
    );
    let k1 = UserKey { org_id: 1, user_id: 10 };
    t.put(&k1, &User { level: 4, name: "alice".into() });
    t.put_fields(&k1, &dyn1);

    // Document 2: declared fields only -> variant column null.
    let k2 = UserKey { org_id: 1, user_id: 11 };
    t.put(&k2, &User { level: 9, name: "bob".into() });

    let batch = t.to_record_batch_with_variant();

    // Schema: typed columns + variant.
    let schema = batch.schema().clone();
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        vec![
            "org_id".to_string(),
            "user_id".to_string(),
            "level".to_string(),
            "name".to_string(),
            "variant".to_string()
        ]
    );
    let vfield = schema.field(4);
    assert_eq!(
        vfield.metadata().get("ARROW:extension:name").map(|s| s.as_str()),
        Some("parquet.variant"),
        "variant extension type must be marked"
    );

    // Typed columns unchanged.
    let level = batch
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::UInt32Array>()
        .unwrap();
    assert_eq!(level.value(0), 4);
    assert_eq!(level.value(1), 9);

    // Variant column: row 0 carries the dynamic tree, row 1 is null.
    let vcol_raw = batch.column(4);
    assert!(!vcol_raw.is_null(0), "row 0 has dynamic fields");
    assert!(vcol_raw.is_null(1), "row 1 has no dynamic fields");
    let vcol = vcol_raw
        .as_any()
        .downcast_ref::<arrow::array::BinaryArray>()
        .unwrap();

    // Decode row 0's variant. Wire = [md_len u32 BE][metadata][value].
    let wire = vcol.value(0);
    let md_len = u32::from_be_bytes(wire[0..4].try_into().unwrap()) as usize;
    let variant =
        parquet_variant::Variant::try_new(&wire[4..4 + md_len], &wire[4 + md_len..]).unwrap();
    let obj = variant.as_object().unwrap();
    let score = obj.get("score").unwrap();
    assert_eq!(score, parquet_variant::Variant::Int64(99));
    let nested_v = obj.get("nested").unwrap();
    let nested = nested_v.as_object().unwrap();
    let x = nested.get("x").unwrap();
    assert_eq!(x, parquet_variant::Variant::Int64(-3));
    let flag = nested.get("flag").unwrap();
    assert_eq!(flag, parquet_variant::Variant::BooleanTrue);
}
