//! Wrapper codecs: VarInt / Quant / Enum / Offset — payload TLV framing,
//! FieldDesc reporting, and Arrow/Parquet round trips (ADR-0007 Phase 2).

use okm::{
    Enum, EnumTag, FieldType, KeyEncode, MockStore, Offset, Quant, Reverse, Row,
    RowEncode, Table, VarInt, offset_decode, offset_encode,
};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(7)]
pub struct WKey {
    pub org: u32,
    pub id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    Suspended,
    Closed,
}

// Tags are an explicit wire contract — non-contiguous on purpose (9
// reserves room) and independent of declaration order.
impl EnumTag for State {
    const TAGS: &'static [(Self, u8)] =
        &[(State::Active, 0), (State::Suspended, 1), (State::Closed, 9)];
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(WKey)]
pub struct WRow {
    pub hits: VarInt<u64>,
    pub ratio: Quant<3>,
    pub state: Enum<State>,
    #[kv_offset(base = 1_700_000_000)]
    pub created: Offset,
}

fn sample(i: u64) -> WRow {
    WRow {
        hits: VarInt(1_000 + i * 7),
        ratio: Quant::<3>::new(0.5 + i as f64 / 10.0),
        state: Enum([State::Active, State::Suspended, State::Closed][i as usize % 3]),
        created: Offset(1_700_000_000 + i as i64 * 86_400),
    }
}

// ================= payload framing =================

#[test]
fn payload_round_trip_all_wrappers() {
    for i in 0..6u64 {
        let row = sample(i);
        assert_eq!(WRow::decode_payload(&row.encode_payload()), row);
    }
}

#[test]
fn varint_frame_is_variable_length() {
    // hits=1000 → 2 LEB128 bytes; total frame = 1 tag + 4 len + 2 val.
    let row = WRow {
        hits: VarInt(1_000),
        ratio: Quant::<3>::new(0.0),
        state: Enum(State::Active),
        created: Offset(1_700_000_000),
    };
    let p = row.encode_payload();
    assert_eq!(p[0], 0); // tag 0 = first field
    assert_eq!(&p[1..5], &2u32.to_be_bytes()); // len = 2
    assert_eq!(&p[5..7], &[0xE8, 0x07]); // LEB128(1000)
}

#[test]
fn offset_wire_is_four_byte_displacement() {
    let base = 1_700_000_000i64;
    let enc = offset_encode(1_700_000_500, base);
    assert_eq!(enc.len(), 4);
    assert_eq!(offset_decode(&enc, base), 1_700_000_500);
}

// ================= FieldDesc reporting =================

#[test]
fn field_desc_reports_wrapper_kinds() {
    let fs = <WRow as okm::Row>::FIELDS;
    assert_eq!(fs[0].name, "hits");
    assert_eq!(fs[0].ty, FieldType::VarInt);
    assert_eq!(fs[0].width, 0); // variable-length regime
    assert_eq!(fs[1].ty, FieldType::Quant(3));
    assert_eq!(fs[1].width, 8);
    assert_eq!(fs[2].ty, FieldType::Enum);
    assert_eq!(fs[2].width, 1);
    assert_eq!(fs[3].ty, FieldType::Offset(1_700_000_000));
    assert_eq!(fs[3].width, 4);
}

// ================= table round trip =================

#[test]
fn table_round_trip_with_wrappers() {
    let mut t: Table<MockStore, WKey, WRow> = Table::new(MockStore::default(), 7);
    for i in 0..5u64 {
        t.put(&WKey { org: 1, id: i }, &sample(i));
    }
    for i in 0..5u64 {
        let r = t.get(&WKey { org: 1, id: i }).expect("row");
        assert_eq!(r, sample(i));
    }
}

// ================= Arrow / Parquet =================

#[cfg(feature = "parquet")]
mod parquet {
    use super::*;
    use okm::parquet_io;

    #[test]
    fn parquet_roundtrip_with_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrappers.parquet");

        let mut t1: Table<MockStore, WKey, WRow> = Table::new(MockStore::default(), 7);
        for i in 0..4u64 {
            t1.put(&WKey { org: 2, id: i }, &sample(i));
        }
        parquet_io::export_parquet(&t1, &path).unwrap();

        let mut t2: Table<MockStore, WKey, WRow> = Table::new(MockStore::default(), 7);
        let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
        assert_eq!(n, 4);
        for i in 0..4u64 {
            assert_eq!(t2.get(&WKey { org: 2, id: i }).unwrap(), sample(i));
        }
    }

    #[test]
    fn record_batch_columns_are_logical_types() {
        let mut t: Table<MockStore, WKey, WRow> = Table::new(MockStore::default(), 7);
        for i in 0..3u64 {
            t.put(&WKey { org: 3, id: i }, &sample(i));
        }
        let batch = t.to_record_batch();
        let schema = batch.schema();
        // hits (key: org, id) then payload columns.
        let names: Vec<_> = schema
            .fields()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        assert_eq!(&names[2..], &["hits", "ratio", "state", "created"]);
        let dts: Vec<_> = schema.fields().iter().map(|f| f.data_type().clone()).collect();
        assert_eq!(dts[2], arrow::datatypes::DataType::UInt64); // VarInt → logical integer
        assert_eq!(dts[3], arrow::datatypes::DataType::Float64); // Quant → dequantized
        assert_eq!(dts[4], arrow::datatypes::DataType::UInt8); // Enum → tag
        assert_eq!(dts[5], arrow::datatypes::DataType::Int64); // Offset → absolute

        // Spot-check row 0's logical values.
        let hits = batch
            .column(2)
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .unwrap()
            .value(0);
        assert_eq!(hits, 1_000);
        let ratio = batch
            .column(3)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap()
            .value(0);
        assert_eq!(ratio, 0.5);
        let created = batch
            .column(5)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(created, 1_700_000_000);
    }
}

// ================= Reverse × Quant composition =================

#[test]
fn reverse_quant_sorts_descending() {
    // Documented composition: descending float order via quantize-then-
    // flip, the path IEEE-754 tricks can't give directly.
    let lo = Reverse(Quant::<3>::new(-1.5).wire()).encode();
    let hi = Reverse(Quant::<3>::new(9.75).wire()).encode();
    assert!(lo > hi, "smaller float must encode to LARGER bytes");
}
