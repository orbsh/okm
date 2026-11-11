//! PLAN Phase 2 codec extensions: variable-length `String` payload fields
//! (TLV frame `len` is the prefix) and the `Reverse<T>` descending-order
//! wrapper. Covers raw wire round trips, macro expansion, and the
//! Arrow/Parquet bridge through the new kinds.
//!
//! The payload has NO header (ADR-0042 as amended): the hot segment's width is a
//! compile-time fact of the declaration (`HOT_WIDTH`), so the bytes are the two
//! segments back to back — `[hot segment][cold TLV]`. Nothing in a row says which
//! generation wrote it; the segment header does, and a log of another generation is
//! refused where it is opened.
//!
//! Compile-time rejections (String / Reverse on key side, non-whitelist
//! Reverse inner) live in `codec_compilefail.rs` via trybuild.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode, Reverse, Reversible};
use okm_lake::snapshot::parquet_io;
use okm_value::CodecError;

// ================= String (variable length) =================

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct SKey {
    pub shard: u16,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(SKey)]
#[ok_ns(3)]
pub struct SRow {
    pub score: u32,
    pub name: String,
    pub note: String,
    pub flag: u8,
}

#[test]
fn string_tlv_round_trip_through_payload() {
    let document = SRow {
        score: 7,
        name: "alice".into(),
        note: "多字节 UTF-8 ✓".into(),
        flag: 0xA5,
    };
    let p = document.encode_payload();
    let back = SRow::decode_payload(&p);
    assert_eq!(back, document);

    // Wire shape: [hot segment][cold TLV]. Hot = score (u32 @0..4) then flag
    // (u8 @4..5) in declaration order, so the cold frames start at 5. Frame length
    // uses the P2 varint ("alice" = 5 bytes → single byte [0x05], < 64).
    assert_eq!(
        &p[0..4],
        &[0, 0, 0, 7],
        "score BE, first bytes of the payload"
    );
    assert_eq!(p[4], 0xA5, "flag hot byte");
    assert_eq!(p[5], 1, "field tag 1 (name)");
    assert_eq!(p[6], 5, "varint len: 5 fits the 1-byte lane (< 64)");
    assert_eq!(&p[7..12], b"alice");
    assert_eq!(<SRow as Document>::HOT_WIDTH, 5);
}

#[test]
fn string_field_desc_marks_variable_width() {
    let rf = <SRow as Document>::FIELDS;
    let name = rf.iter().find(|f| f.name == "name").unwrap();
    assert!(matches!(name.ty, okm_core::FieldType::Str));
    assert_eq!(name.width, 0, "static width meaningless for Str");
}

// ================= Reverse<T> =================

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct RKey {
    pub org: u32,
    pub user: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RKey)]
#[ok_index(by_newest { fields(ts_rev), key(org) })]
#[ok_ns(4)]
pub struct RRow {
    pub score: u16,
    pub ts_rev: Reverse<u64>,
}

#[test]
fn reverse_wire_encoding_is_bit_flipped_and_involutive() {
    let document = RRow {
        score: 9,
        ts_rev: Reverse(0x0102_0304_0506_0708u64),
    };
    let p = document.encode_payload();
    // [score u16 BE @0..2][Reverse u64 @2..10] — both hot, so no cold half.
    // Plain BE of the inner value would be 01 02 03 04 05 06 07 08; the
    // wrapped encoding is the full-width bitwise NOT.
    assert_eq!(p.len(), 10, "the whole payload is hot");
    assert_eq!(&p[0..2], &[0, 9]);
    assert_eq!(
        &p[2..10],
        &[!0x01, !0x02, !0x03, !0x04, !0x05, !0x06, !0x07, !0x08]
    );
    let back = RRow::decode_payload(&p);
    assert_eq!(back, document);
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

    let t1: Collection<TestStore, SKey, SRow> = Collection::new(TestStore::slatedb_mem());
    let names = ["", "alice", "多字节 ✓ 名称"];
    for (i, name) in names.iter().enumerate() {
        t1.put(
            &SKey {
                shard: 1,
                id: i as u64,
            },
            &SRow {
                score: i as u32,
                name: name.to_string(),
                note: format!("note-{i}"),
                flag: i as u8,
            },
        )
        .unwrap();
    }
    parquet_io::export_parquet(&t1, &path).unwrap();

    let mut t2: Collection<TestStore, SKey, SRow> = Collection::new(TestStore::slatedb_mem());
    let n = parquet_io::import_parquet(&mut t2, &path).unwrap();
    assert_eq!(n, 3);
    for (i, name) in names.iter().enumerate() {
        let r = t2
            .get(&SKey {
                shard: 1,
                id: i as u64,
            })
            .unwrap()
            .expect("document");
        assert_eq!(&r.name, name);
        assert_eq!(r.note, format!("note-{i}"));
    }
}

// ================= Hex snapshot: wire format guard =================

#[test]
fn payload_wire_hex_snapshot() {
    // Pins the exact byte layout: [hot segment][cold TLV], no header at all.
    // Any change here is a wire break — bump LAYOUT_VERSION and update this.
    let document = SRow {
        score: 0x0102_0304,
        name: "ab".into(),
        note: String::new(),
        flag: 0xFF,
    };
    let p = document.encode_payload();
    // hot = score(4) + flag(1) = 5 bytes, cold = name frame (5+2), note frame (5+0).
    // P2 varint frame lengths: the len prefix is a prefix-monotonic
    // varint — values < 64 are ONE byte (len 2 → [0x02], len 0 → [0x00]).
    let expect = [
        1, 2, 3, 4,    // score BE (hot)
        0xFF, // flag (hot)
        1, 2, b'a', b'b', // name frame: tag 1, len 2 (1-byte varint), value
        2, 0, // note frame: tag 2, len 0
    ]
    .to_vec();
    assert_eq!(
        p,
        expect,
        "wire format snapshot (hex: {})",
        p.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    assert_eq!(SRow::decode_payload(&p), document);
}

// ================= Layout identity and its two surviving tolerances =================

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns]
#[ok_ref(SKey)]
#[ok_layout(version = 2)]
#[ok_ns(3)]
pub struct EvolvedRow {
    pub score: u32,
    pub name: String,
    pub note: String,
    pub flag: u8,
    // Appended at the tail at v2, and COLD: an absent cold frame is a field that was
    // never written, so an older payload still decodes. A HOT append would move the
    // cold boundary and needs a generation of its own.
    pub memo: String,
}

#[test]
fn an_appended_cold_field_decodes_from_a_payload_that_lacks_it() {
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
        memo: String::default(),
    };
    assert_eq!(EvolvedRow::decode_payload(&p), evolved);
    assert_eq!(
        <EvolvedRow as Document>::HOT_WIDTH,
        5,
        "the hot half is unchanged"
    );
    assert_eq!(<EvolvedRow as Document>::LAYOUT_VERSION, 2);
}

#[test]
fn a_payload_shorter_than_the_hot_segment_is_truncation() {
    let document = SRow {
        score: 1,
        name: "x".into(),
        note: String::new(),
        flag: 0,
    };
    let p = document.encode_payload();
    // No header to carry a length, so the hot half is exactly `hot_width` bytes:
    // anything shorter is a cut payload, and the schema-driven reader says so.
    let schema = CollectionSchema::of::<SKey, SRow>();
    let short = &p[..3];
    let err = okm_core::decode::decode_payload(&schema, short);
    assert!(matches!(err, Err(CodecError::Truncated { .. })), "{err:?}");
}

#[test]
fn explicit_layout_version_constant() {
    // #[ok_layout(version = N)] feeds the Document trait constant — the number a
    // segment header carries, not one a payload carries.
    assert_eq!(<SRow as Document>::LAYOUT_VERSION, 1);
    assert_eq!(<EvolvedRow as Document>::LAYOUT_VERSION, 2);
    assert_eq!(<DefaultedRow as Document>::LAYOUT_VERSION, 3);
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns]
#[ok_ref(SKey)]
#[ok_layout(version = 3)]
#[ok_ns(3)]
pub struct DefaultedRow {
    pub score: u32,
    pub name: String,
    #[ok_default(String::from("anon"))]
    pub memo: String,
}

#[test]
fn ok_default_expression_used_for_missing_tail_field() {
    let document = DefaultedRow {
        score: 5,
        name: "x".into(),
        memo: "written".into(),
    };
    let p = document.encode_payload();
    // Drop the last cold frame (`[tag 2][len 7]["written"]`, 9 bytes): exactly the
    // payload a declaration without `memo` writes.
    let old = &p[..p.len() - 9];
    let back = DefaultedRow::decode_payload(old);
    assert_eq!(back.memo, "anon", "#[ok_default(..)] applied");
    assert_eq!(back.score, 5);
    assert_eq!(back.name, "x");
}
