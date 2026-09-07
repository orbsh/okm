//! PLAN Phase 2 codec extensions: variable-length `String` payload fields
//! (TLV frame `len` is the prefix) and the `Reverse<T>` descending-order
//! wrapper. Covers raw wire round trips, macro expansion, and the
//! Arrow/Parquet bridge through the new kinds.
//!
//! Compile-time rejections (String / Reverse on key side, non-whitelist
//! Reverse inner) live in `codec_compilefail.rs` via trybuild.

use okm::{KeyEncode, MockStore, Reversible, Reverse, Row, RowEncode, Table, parquet_io};

// ================= String (variable length) =================

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(3)]
pub struct SKey {
    pub shard: u16,
    pub id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(SKey)]
pub struct SRow {
    pub score: u32,
    pub name: String,
    pub note: String,
    pub flag: u8,
}

#[test]
fn string_tlv_round_trip_through_payload() {
    let row = SRow {
        score: 7,
        name: "alice".into(),
        note: "多字节 UTF-8 ✓".into(),
        flag: 0xA5,
    };
    let p = row.encode_payload();
    let back = SRow::decode_payload(&p);
    assert_eq!(back, row);

    // Wire shape: the TLV frame's len IS the length prefix (no second one).
    // score frame (5 hdr + 4) then name frame header at offset 9.
    assert_eq!(p[9], 1, "field tag 1 (name)");
    let name_len = u32::from_be_bytes(p[10..14].try_into().unwrap()) as usize;
    assert_eq!(name_len, 5, "len = byte length of the value");
    assert_eq!(&p[14..19], b"alice");
}

#[test]
fn string_field_desc_marks_variable_width() {
    let rf = <SRow as Row>::FIELDS;
    let name = rf.iter().find(|f| f.name == "name").unwrap();
    assert!(matches!(name.ty, okm::FieldType::Str));
    assert_eq!(name.width, 0, "static width meaningless for Str");
}

// ================= Reverse<T> =================

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(4)]
pub struct RKey {
    pub org: u32,
    pub user: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(RKey)]
#[kv_index(by_newest { fields(org, ts_rev) })]
pub struct RRow {
    pub score: u16,
    pub ts_rev: Reverse<u64>,
}

#[test]
fn reverse_wire_encoding_is_bit_flipped_and_involutive() {
    let row = RRow {
        score: 9,
        ts_rev: Reverse(0x0102_0304_0506_0708u64),
    };
    let p = row.encode_payload();
    // score frame: 5 hdr + 2 bytes → Reverse value starts at offset 12.
    // Plain BE of the inner value would be 01 02 03 04 05 06 07 08; the
    // wrapped encoding is the full-width bitwise NOT.
    assert_eq!(
        &p[12..20],
        &[!0x01, !0x02, !0x03, !0x04, !0x05, !0x06, !0x07, !0x08]
    );
    let back = RRow::decode_payload(&p);
    assert_eq!(back, row);
}

#[test]
fn reverse_bytes_sort_descending_by_value() {
    // The whole point: encoded bytes order like DESC on the value.
    let lo = Reverse::<u64>(100).encode();
    let hi = Reverse::<u64>(9000).encode();
    assert!(
        lo > hi,
        "smaller value must encode to LARGER bytes (newest-first prefix scan)"
    );

    // Signed domain: bias-then-flip must also invert order.
    let neg = Reverse::<i64>(-1).encode();
    let pos = Reverse::<i64>(1).encode();
    assert!(neg > pos);

    // Inverse holds across the integer whitelist.
    for v in [0i64, -1, 1, i64::MIN, i64::MAX, 0x0123_4567_89AB_CDEF] {
        assert_eq!(Reverse::<i64>::decode(&Reverse(v).encode()).0, v);
        assert_eq!(<i64 as Reversible>::WIDTH, 8);
    }
}

// ================= Arrow / Parquet through the new kinds =================

#[test]
fn parquet_roundtrip_with_string_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.parquet");

    let mut t1: Table<MockStore, SKey, SRow> = Table::new(MockStore::default(), 3);
    let names = ["", "alice", "多字节 ✓ 名称"];
    for (i, name) in names.iter().enumerate() {
        t1.put(
            &SKey { shard: 1, id: i as u64 },
            &SRow {
                score: i as u32,
                name: name.to_string(),
                note: format!("note-{i}"),
                flag: i as u8,
            },
        );
    }
    parquet_io::export_parquet(&t1, &path).unwrap();

    let mut t2: Table<MockStore, SKey, SRow> = Table::new(MockStore::default(), 3);
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 3);
    for (i, name) in names.iter().enumerate() {
        let r = t2.get(&SKey { shard: 1, id: i as u64 }).expect("row");
        assert_eq!(&r.name, name);
        assert_eq!(r.note, format!("note-{i}"));
    }
}
