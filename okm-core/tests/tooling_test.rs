//! Tooling interfaces (PLAN Phase 4): schema export and the
//! Parquet snapshot round trip (export → import through `Collection::put`).

use okm_core::{FieldDesc, FieldType, KeyEncode, TestStore, Document, DocumentEncode, Collection, parquet_io};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct TKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TKey)]
#[ok_index(by_org { fields(reputation) })]
#[ok_ns(7)]
pub struct TRow {
    pub reputation: u32,
    pub level: u16,
    pub tag: [u8; 4],
}


// ================= Parquet round trip =================

#[test]
fn parquet_export_import_roundtrip_restores_rows_and_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.parquet");

    let t1: Collection<TestStore, TKey, TRow> = Collection::new(TestStore::slatedb_mem());
    for i in 0..5u64 {
        let k = TKey { org_id: 1, user_id: i };
        let r = TRow {
            reputation: (i * 100) as u32,
            level: (i as u16) + 7,
            tag: [i as u8, 0, 0xFF, 0x42],
        };
        t1.put(&k, &r);
    }

    parquet_io::export_parquet(&t1, &path).unwrap();

    // Import into a FRESH table: document and index entries must both come back
    // (put contract) and the scan over the index must find all 5 rows.
    let mut t2: Collection<TestStore, TKey, TRow> = Collection::new(TestStore::slatedb_mem());
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 5, "restored document count");

    for i in 0..5u64 {
        let k = TKey { org_id: 1, user_id: i };
        let r = t2.get(&k).expect("document restored");
        assert_eq!(r.reputation, (i * 100) as u32);
        assert_eq!(r.level, (i as u16) + 7);
        assert_eq!(r.tag, [i as u8, 0, 0xFF, 0x42]);
    }

    // Index entries were rewritten by put; the hand impl sorts by payload
    // `reputation` = i*100, so matching the first value requires its BE
    // bytes — here just scan the whole index (empty prefix).
    let scanned = t2.scan::<__OkmIndex_TRow_by_org>(&[]);
    assert_eq!(scanned.len(), 5, "index rebuilt on import");

    // Exported column names = field names in declaration order (key
    // fields then payload fields) with Arrow types — the snapshot's
    // schema is the declaration, viewable from any Parquet reader.
    assert_eq!(
        t1.export_columns(),
        vec![
            ("org_id", arrow::datatypes::DataType::UInt32),
            ("user_id", arrow::datatypes::DataType::UInt64),
            ("reputation", arrow::datatypes::DataType::UInt32),
            ("level", arrow::datatypes::DataType::UInt16),
            ("tag", arrow::datatypes::DataType::Binary),
        ]
    );
}


// ================= FieldDesc sanity (schema export's data source) =================

#[test]
fn json_schema_is_valid_and_matches_parquet_columns() {
    let schema = okm_core::tooling::json_schema::<TKey, TRow>();

    // Must parse as JSON.
    let v: serde_json::Value =
        serde_json::from_str(&schema).expect("json_schema must be valid JSON");

    // Column set identical to the Parquet export (key fields first, then
    // payload fields); authoritative column ORDER is in x-okm-column-order
    // (JSON property objects are unordered).
    let props = v["properties"].as_object().expect("properties object");
    let mut names: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
    names.sort_unstable();
    let mut expected = vec!["org_id", "user_id", "reputation", "level", "tag"];
    expected.sort_unstable();
    assert_eq!(names, expected);
    let order: Vec<&str> = v["x-okm-column-order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        vec!["org_id", "user_id", "reputation", "level", "tag"]
    );

    // Type mapping mirrors the Arrow bridge.
    assert_eq!(props["org_id"]["type"], "integer");
    assert_eq!(props["user_id"]["type"], "integer");
    assert_eq!(props["tag"]["type"], "string"); // [u8;4] → base64

    // Free function form agrees.
    assert_eq!(okm_core::tooling::json_schema::<TKey, TRow>(), schema);
}

#[test]
fn fielddesc_tables_back_the_audit() {
    let kf = <TKey as KeyEncode>::FIELDS;
    assert_eq!(kf.len(), 2);
    assert_eq!((kf[0].name, kf[0].width), ("org_id", 4));
    assert_eq!((kf[1].name, kf[1].width), ("user_id", 8));
    assert!(matches!(kf[1].ty, FieldType::U64));

    let rf = <TRow as Document>::FIELDS;
    assert_eq!(rf.len(), 3);
    assert!(matches!(rf[2].ty, FieldType::FixedBytes));
    assert_eq!(rf[2].width, 4);

    // Silence the unused-warning for the FieldDesc import used above.
    let _: Option<FieldDesc> = None;
}

// ================= Categorical values in the snapshot (ADR-0032) =================
//
// The exported column carries the LABEL, not the id: a warehouse can read the
// name, and Parquet's own per-chunk dictionary does the compressing. Restoring
// re-interns labels in the destination, so the ids there are its own numbering.

use okm_core::Vocab;

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct VKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VKey)]
#[ok_ns(21)]
pub struct VRow {
    pub status: Vocab<u8>,
    pub color: Vocab<u16>,
    pub score: u32,
}

#[test]
fn parquet_roundtrip_carries_labels_not_ids() {
    use arrow::array::{Array, StringArray};
    use arrow::datatypes::DataType;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vocab.parquet");

    let statuses = ["active", "suspended", "active", ""];
    let colors = ["red", "green", "多字节 ✓", "red"];
    let t1: Collection<TestStore, VKey, VRow> = Collection::new(TestStore::slatedb_mem());
    for (i, (s, c)) in statuses.iter().zip(colors.iter()).enumerate() {
        t1.put(
            &VKey { id: i as u64 },
            &VRow { status: Vocab::of(*s), color: Vocab::of(*c), score: i as u32 },
        );
    }

    // The exported column is the label, and it is NULLABLE — an id the
    // vocabulary cannot name has nowhere else to go.
    let batch = t1.to_record_batch();
    let field = batch.schema().field_with_name("status").unwrap().clone();
    assert_eq!(field.data_type(), &DataType::Utf8);
    assert!(field.is_nullable(), "an unnamed id exports as null");
    let status_col = batch
        .column_by_name("status")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(status_col.value(0), "active");
    assert_eq!(status_col.value(3), "", "the empty label is a value, not a null");

    parquet_io::export_parquet(&t1, &path).unwrap();

    // Restore into a FRESH table: labels come back through the destination's own
    // vocabulary (which the import rebuilds), and the restored rows resolve.
    let mut t2: Collection<TestStore, VKey, VRow> = Collection::new(TestStore::slatedb_mem());
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 4);
    for (i, (s, c)) in statuses.iter().zip(colors.iter()).enumerate() {
        let r = t2.get(&VKey { id: i as u64 }).expect("row restored");
        assert_eq!(r.status.as_str(), Some(*s));
        assert_eq!(r.color.as_str(), Some(*c));
        assert_eq!(r.score, i as u32);
    }

    // The declaration's JSON view agrees with the column: a categorical field is
    // a string, not a code.
    let schema: serde_json::Value =
        serde_json::from_str(&okm_core::tooling::json_schema::<VKey, VRow>()).unwrap();
    assert_eq!(schema["properties"]["status"]["type"], "string");
}

#[test]
fn an_unnamed_id_exports_as_null_and_import_refuses_it() {
    use arrow::array::{Array, StringArray};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unnamed.parquet");

    // `from_id` names an id directly; the vocabulary was never told about it, so
    // the row carries a value nothing can name (the state the old id-based
    // export→import produced by design).
    let t1: Collection<TestStore, VKey, VRow> = Collection::new(TestStore::slatedb_mem());
    t1.put(
        &VKey { id: 0 },
        &VRow { status: Vocab::from_id(7), color: Vocab::of("red"), score: 1 },
    );

    let batch = t1.to_record_batch();
    let status_col = batch
        .column_by_name("status")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(status_col.is_null(0), "unnamed id → NULL, never an invented label");

    parquet_io::export_parquet(&t1, &path).unwrap();
    let mut t2: Collection<TestStore, VKey, VRow> = Collection::new(TestStore::slatedb_mem());
    let err = parquet_io::import_parquet(&mut t2, &path).unwrap_err();
    assert!(
        err.to_string().contains("NULL label"),
        "import must refuse an unnamed value, got: {err}"
    );
}

/// A backup carries the WHOLE document (ADR-0035's pair of slots): the typed
/// half in the typed columns, the open half in the `variant` column — and a
/// document that has only a segment comes back as a segment, with no invented
/// row of default values.
#[test]
fn parquet_with_variant_restores_both_halves_and_segment_only_documents() {
    use okm_core::model::obj_dynamic::DynamicValue;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backup.parquet");

    let mut t1: Collection<TestStore, TKey, TRow> = Collection::new(TestStore::slatedb_mem());
    let with_row = TKey { org_id: 1, user_id: 1 };
    t1.put(
        &with_row,
        &TRow { reputation: 100, level: 7, tag: [1, 0, 0xFF, 0x42] },
    );
    let mut open = BTreeMap::new();
    open.insert("note".to_string(), DynamicValue::Str("hello".into()));
    t1.merge(&with_row, &open);
    let segment_only = TKey { org_id: 1, user_id: 2 };
    t1.merge(&segment_only, &open);

    parquet_io::export_parquet_with_variant(&mut t1, &path).unwrap();

    let mut t2: Collection<TestStore, TKey, TRow> = Collection::new(TestStore::slatedb_mem());
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 1, "one document has a row; the other is segment-only");

    assert_eq!(t2.get(&with_row).expect("the row came back").reputation, 100);
    assert_eq!(
        t2.fetch(&with_row).expect("the document").get("note"),
        Some(&DynamicValue::Str("hello".into())),
        "the open half came back"
    );

    assert!(t2.get(&segment_only).is_none(), "no row was invented");
    assert_eq!(
        t2.fetch(&segment_only).expect("the segment-only document").get("note"),
        Some(&DynamicValue::Str("hello".into())),
        "a segment-only document restores as a segment-only document"
    );
    // ...and the two enumeration modes agree with what came back.
    assert_eq!(t2.scan_keys(), vec![with_row.clone()]);
    assert_eq!(t2.scan_document_keys(), vec![with_row, segment_only]);
}
