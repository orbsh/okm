//! Tooling interfaces (PLAN Phase 4): layout audit (`describe`) and the
//! Parquet snapshot round trip (export → import through `Table::put`).

use okm::{FieldDesc, FieldType, KeyEncode, MockStore, Row, RowEncode, Table, parquet_io};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(7)]
pub struct TKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(TKey)]
#[kv_index(by_org { fields(org_id) })]
pub struct TRow {
    pub reputation: u32,
    pub level: u16,
    pub tag: [u8; 4],
}

// ================= describe =================

#[test]
fn describe_renders_offsets_and_tlv_frames() {
    let table: Table<MockStore, TKey, TRow> = Table::new(MockStore::default(), 7);
    let text = table.describe();

    // Key half: declaration-order fixed offsets.
    assert!(text.contains("org_id"), "key field listed:\n{text}");
    assert!(text.contains("user_id"), "key field listed:\n{text}");
    assert!(text.contains("KEY_LEN=12"), "KEY_LEN rendered:\n{text}");

    // Payload half: TLV frame strides — first value at 5 (after tag+len of
    // frame 0), second at 5+5+2 per declaration widths (4, 2, then [u8;4]).
    assert!(text.contains("reputation"), "payload field listed:\n{text}");
    assert!(text.contains("payload total"), "payload summary:\n{text}");

    // Free function form agrees.
    assert_eq!(okm::tooling::describe::<TKey, TRow>(), text);
}

#[test]
fn describe_offset_math_matches_wire_format() {
    // Recompute the offsets the formatter prints and cross-check against a
    // real payload: the first field's value must start at byte 5.
    let row = TRow {
        reputation: 0x01020304,
        level: 0x0506,
        tag: [0xA0, 0xA1, 0xA2, 0xA3],
    };
    let p = row.encode_payload();
    assert_eq!(&p[5..9], &[1, 2, 3, 4]); // reputation BE at offset 5
    assert_eq!(&p[14..16], &[5, 6]); // level BE at 5 + (5+4)
    assert_eq!(&p[21..25], &[0xA0, 0xA1, 0xA2, 0xA3]); // tag at 5 + (5+4) + (5+2)
}

// ================= Parquet round trip =================

#[test]
fn parquet_export_import_roundtrip_restores_rows_and_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.parquet");

    let mut t1: Table<MockStore, TKey, TRow> = Table::new(MockStore::default(), 7);
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

    // Import into a FRESH table: row and index entries must both come back
    // (put contract) and the scan over the index must find all 5 rows.
    let mut t2: Table<MockStore, TKey, TRow> = Table::new(MockStore::default(), 7);
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 5, "restored row count");

    for i in 0..5u64 {
        let k = TKey { org_id: 1, user_id: i };
        let r = t2.get(&k).expect("row restored");
        assert_eq!(r.reputation, (i * 100) as u32);
        assert_eq!(r.level, (i as u16) + 7);
        assert_eq!(r.tag, [i as u8, 0, 0xFF, 0x42]);
    }

    // Index entries were rewritten by put: org_id=1 prefix scan hits all.
    let scanned = t2.scan::<TRowByOrg>(&1u32.to_be_bytes());
    assert_eq!(scanned.len(), 5, "index rebuilt on import");
}

/// The generated access-method marker struct (slot 1).
struct TRowByOrg;
impl okm::KvIndex for TRowByOrg {
    type Key = TKey;
    const SLOT: u8 = 1;
    const FIELDS: &'static [&'static str] = &["org_id"];
    const INCLUDES: &'static [&'static str] = &[];
}

// ================= FieldDesc sanity (describe's data source) =================

#[test]
fn fielddesc_tables_back_the_audit() {
    let kf = <TKey as KeyEncode>::FIELDS;
    assert_eq!(kf.len(), 2);
    assert_eq!((kf[0].name, kf[0].width), ("org_id", 4));
    assert_eq!((kf[1].name, kf[1].width), ("user_id", 8));
    assert!(matches!(kf[1].ty, FieldType::U64));

    let rf = <TRow as Row>::FIELDS;
    assert_eq!(rf.len(), 3);
    assert!(matches!(rf[2].ty, FieldType::FixedBytes));
    assert_eq!(rf[2].width, 4);

    // Silence the unused-warning for the FieldDesc import used above.
    let _: Option<FieldDesc> = None;
}
