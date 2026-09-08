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

    // Wire shape: [ver u8][hot_len u16 BE][hot segment][cold TLV]. Hot =
    // score (u32 @3..7) then flag (u8 @7..8) in declaration order →
    // hot_len = 5; the name TLV frame header starts at 8.
    assert_eq!(p[0], 1, "layout version 1");
    assert_eq!(&p[1..3], &[0u8, 5], "hot segment = 5 bytes (u32 score + u8 flag)");
    assert_eq!(p[7], 0xA5, "flag hot byte");
    assert_eq!(p[8], 1, "field tag 1 (name)");
    let name_len = u32::from_be_bytes(p[9..13].try_into().unwrap()) as usize;
    assert_eq!(name_len, 5, "len = byte length of the value");
    assert_eq!(&p[13..18], b"alice");
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
    // [ver u8][hot_len u16 BE][score u16 BE @3..5][Reverse u64 @5..13].
    // Plain BE of the inner value would be 01 02 03 04 05 06 07 08; the
    // wrapped encoding is the full-width bitwise NOT.
    assert_eq!(&p[1..3], &[0, 10], "hot segment = 10 bytes");
    assert_eq!(
        &p[5..13],
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

// ================= Hex snapshot: wire format guard =================

#[test]
fn payload_wire_hex_snapshot() {
    // Pins the exact byte layout: [ver u8][hot_len u16 BE][hot seg][cold TLV].
    // Any change here is a wire break — bump LAYOUT_VERSION and update this.
    let row = SRow {
        score: 0x0102_0304,
        name: "ab".into(),
        note: String::new(),
        flag: 0xFF,
    };
    let p = row.encode_payload();
    // hot = score(4) + flag(1) → hot_len 5; cold = name frame (5+2), note frame (5+0).
    let expect = [
        1u8, 0, 5,             // version 1, hot_len 5
        1, 2, 3, 4,            // score BE (hot)
        0xFF,                  // flag (hot)
        1, 0, 0, 0, 2, b'a', b'b',   // name frame: tag 1, len 2, value
        2, 0, 0, 0, 0,               // note frame: tag 2, len 0
    ]
    .to_vec();
    assert_eq!(p, expect, "wire format snapshot (hex: {})", p.iter().map(|b| format!("{b:02x}")).collect::<String>());
    assert_eq!(SRow::decode_payload(&p), row);
}

// ================= Version compatibility =================

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(SKey)]
#[kv_layout(version = 2)]
pub struct EvolvedRow {
    pub score: u32,
    pub name: String,
    pub note: String,
    pub flag: u8,
    // Appended at the tail at v2: old payloads lack them → defaults.
    pub visits: u32,
    pub memo: String,
}

#[test]
fn older_payload_decodes_with_appended_defaults() {
    // A v1 SRow payload, decoded through the v2 schema (both are
    // score/name/note/flag declarations — same prefix, same wire prefix).
    let old = SRow {
        score: 42,
        name: "old".into(),
        note: "n".into(),
        flag: 7,
    };
    let p = old.encode_payload();

    let evolved = EvolvedRow {
        score: 42,
        name: "old".into(),
        note: "n".into(),
        flag: 7,
        visits: <u32 as Default>::default(), // no #[kv_default] → T::default()
        memo: String::default(),
    };
    assert_eq!(EvolvedRow::decode_payload(&p), evolved);
    // Header version is the OLD row's (1), which is <= schema's 2 → accepted.
    assert_eq!(p[0], 1);
    assert_eq!(<EvolvedRow as Row>::LAYOUT_VERSION, 2);
}

#[test]
fn newer_payload_version_is_rejected() {
    let row = SRow { score: 1, name: "x".into(), note: String::new(), flag: 0 };
    let mut p = row.encode_payload();
    p[0] = 99; // a future layout version
    let err = std::panic::catch_unwind(|| SRow::decode_payload(&p));
    assert!(err.is_err(), "payload from a newer layout must panic");
}

#[test]
fn explicit_layout_version_constant() {
    // #[kv_layout(version = 2)] feeds the Row trait constant.
    assert_eq!(<SRow as Row>::LAYOUT_VERSION, 1);
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(SKey)]
#[kv_layout(version = 3)]
pub struct DefaultedRow {
    pub score: u32,
    pub name: String,
    #[kv_default(77)]
    pub flag: u8,
}

#[test]
fn kv_default_expression_used_for_missing_tail_field() {
    let row = DefaultedRow { score: 5, name: "x".into(), flag: 77 };
    let p = row.encode_payload();
    assert_eq!(p[0], 3, "explicit layout version 3");
    // Simulate an older payload without flag: strip the hot tail byte AND
    // fix the header hot_len (an older schema's header recorded 4, not 5).
    let mut old: Vec<u8> = p[..3 + 4].to_vec();
    old[1..3].copy_from_slice(&4u16.to_be_bytes());
    let back = DefaultedRow::decode_payload(&old);
    assert_eq!(back.flag, 77, "#[kv_default(77)] applied");
    assert_eq!(back.score, 5);
}
