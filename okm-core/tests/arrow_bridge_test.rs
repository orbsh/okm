//! Arrow RecordBatch bridge (ADR-0007 Phase 1): schema generation from
//! FieldDesc, byte-level column extraction, BE→LE value conversion,
//! round trip against struct decode.

use arrow::datatypes::DataType;
use okm_core::{FieldDesc, FieldType, KeyEncode, MockStore, Row, RowEncode, Table};

/// UserKey：org 内的用户身份（主键）。字段类型覆盖四种 kind。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct ExportKey {
    pub org_id: u32,
    pub user_id: u64,
    pub status: u8,
}

/// User 行：载荷字段含 u32/u16（多字节 BE→LE 换位路径）。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(ExportKey)]
#[kv_ns(11)]
pub struct ExportRow {
    pub reputation: u32,
    pub bio_len: u16,
    pub flags: u8,
}

fn put_rows(t: &mut Table<MockStore, ExportKey, ExportRow>) -> Vec<(ExportKey, ExportRow)> {
    let mut out = Vec::new();
    for i in 0u64..5 {
        let k = ExportKey {
            org_id: 7,
            user_id: 100 + i,
            status: (i % 3) as u8,
        };
        let r = ExportRow {
            reputation: 1000 + i as u32,
            bio_len: 20 + i as u16,
            flags: (i * 7) as u8,
        };
        t.put(&k, &r);
        out.push((k, r));
    }
    out
}

#[test]
fn schema_is_generated_from_field_desc() {
    let t: Table<MockStore, ExportKey, ExportRow> = Table::new(MockStore::default());
    let cols = t.export_columns();
    // key half then payload half, declaration order within each half
    let names: Vec<_> = cols.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        ["org_id", "user_id", "status", "reputation", "bio_len", "flags"]
    );
    let types: Vec<_> = cols.iter().map(|(_, t)| t.clone()).collect();
    assert_eq!(
        types,
        [
            DataType::UInt32,
            DataType::UInt64,
            DataType::UInt8,
            DataType::UInt32,
            DataType::UInt16,
            DataType::UInt8,
        ]
    );
}

#[test]
fn field_desc_tables_match_declaration() {
    // key 侧：三个字段，宽度与声明一致
    let kf = <ExportKey as KeyEncode>::FIELDS;
    assert_eq!(
        kf,
        &[
            FieldDesc { name: "org_id", ty: FieldType::U32, width: 4 },
            FieldDesc { name: "user_id", ty: FieldType::U64, width: 8 },
            FieldDesc { name: "status", ty: FieldType::U8, width: 1 },
        ]
    );
    // row 侧：载荷三个字段
    let rf = <ExportRow as Row>::FIELDS;
    assert_eq!(rf.len(), 3);
    assert_eq!(rf[0].name, "reputation");
    assert_eq!(rf[0].width, 4);
}

#[test]
fn batch_values_roundtrip_through_struct_decode() {
    let mut t: Table<MockStore, ExportKey, ExportRow> = Table::new(MockStore::default());
    let rows = put_rows(&mut t);

    let batch = t.to_record_batch();
    assert_eq!(batch.num_rows(), rows.len());

    let cols = t.export_columns();
    for (i, (k, r)) in rows.iter().enumerate() {
        // key 字段：直接对照 struct 值
        let org = batch.column(0).as_any().downcast_ref::<arrow::array::UInt32Array>().unwrap();
        let uid = batch.column(1).as_any().downcast_ref::<arrow::array::UInt64Array>().unwrap();
        let status = batch.column(2).as_any().downcast_ref::<arrow::array::UInt8Array>().unwrap();
        assert_eq!(org.value(i), k.org_id, "org_id row {i}");
        assert_eq!(uid.value(i), k.user_id, "user_id row {i}");
        assert_eq!(status.value(i), k.status, "status row {i}");

        // 载荷字段：对照 struct 值（BE→LE 换位正确性的行为证明）
        let rep = batch.column(3).as_any().downcast_ref::<arrow::array::UInt32Array>().unwrap();
        let bio = batch.column(4).as_any().downcast_ref::<arrow::array::UInt16Array>().unwrap();
        let flg = batch.column(5).as_any().downcast_ref::<arrow::array::UInt8Array>().unwrap();
        assert_eq!(rep.value(i), r.reputation, "reputation row {i}");
        assert_eq!(bio.value(i), r.bio_len, "bio_len row {i}");
        assert_eq!(flg.value(i), r.flags, "flags row {i}");
        // 列名对齐：第 3 列确实是 reputation
        assert_eq!(cols[3].0, "reputation");
    }
}

#[test]
fn batch_respects_key_order() {
    // scan_suffix 返回 key 序，导出应保持该顺序（user_id 升序）
    let mut t: Table<MockStore, ExportKey, ExportRow> = Table::new(MockStore::default());
    put_rows(&mut t);
    let batch = t.to_record_batch();
    let uid = batch.column(1).as_any().downcast_ref::<arrow::array::UInt64Array>().unwrap();
    for i in 1..batch.num_rows() {
        assert!(uid.value(i - 1) < uid.value(i), "row {i} out of key order");
    }
}

/// FixedBytes 字段映射为 Binary 列（byte-for-byte 保真）。
#[test]
fn fixed_bytes_maps_to_binary() {
    #[derive(KeyEncode, Clone, PartialEq, Debug)]
    #[kv_ns(12)]
    pub struct BinKey {
        pub id: u64,
        pub name: [u8; 4],
    }
    #[derive(RowEncode, Clone, PartialEq, Debug)]
    #[kv_ref(BinKey)]
    pub struct BinRow {
        pub n: u32,
    }

    let mut t: Table<MockStore, BinKey, BinRow> = Table::new(MockStore::default());
    let k = BinKey { id: 1, name: *b"abcd" };
    t.put(&k, &BinRow { n: 9 });

    let batch = t.to_record_batch();
    let name = batch.column(1).as_any().downcast_ref::<arrow::array::BinaryArray>().unwrap();
    assert_eq!(name.value(0), b"abcd");
    let n = batch.column(2).as_any().downcast_ref::<arrow::array::UInt32Array>().unwrap();
    assert_eq!(n.value(0), 9);
}

/// 空表导出：batch 零行、schema 完整。
#[test]
fn empty_table_exports_full_schema() {
    let t: Table<MockStore, ExportKey, ExportRow> = Table::new(MockStore::default());
    let batch = t.to_record_batch();
    assert_eq!(batch.num_rows(), 0);
    assert_eq!(batch.num_columns(), 6);
}
