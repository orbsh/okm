//! `Uuid7` / `Uuid4` as a KEY — the id lives in the key, never in the payload, and never as text.
//!
//! Two things this locks: the derive accepts a uuid key (16 fixed bytes, no mirror field, no
//! `KeyEncode` wrapper struct), and the keyspace orders by time because v7's byte order is its
//! millisecond prefix. `Uuid4` is the same shape with no order — that is the token case.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::{Collection, DocumentEncode, KeyEncode, Ref, TestStore, Uuid4, Uuid7};

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(Uuid7)]
#[ok_ns(70)]
pub struct Device {
    /// No `id` mirror: the key IS the device, and `scan`/`get` hand it back alongside the row.
    pub alias: String,
    pub seen: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(Uuid4)]
#[ok_ns(71)]
pub struct Token {
    /// A payload reference to the device row; the key bytes ride the existing `Ref` branch.
    pub device: Ref<Device, Uuid7>,
    pub expires: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(Uuid7)]
#[ok_ns(72)]
pub struct Session {
    /// uuids as DECLARED PAYLOAD FIELDS (not just keys, not just `Ref`s).
    pub token: Uuid4,
    pub device: Uuid7,
    pub note: String,
}

#[test]
fn a_uuid_payload_field_keeps_its_kind_and_round_trips() {
    use okm_core::{Document, FieldType};
    assert_eq!(
        <Session as Document>::FIELDS[0].ty,
        FieldType::Uuid4,
        "the declaration reports the uuid kind, not raw fixed bytes"
    );
    assert_eq!(<Session as Document>::FIELDS[0].width, 16);
    assert_eq!(<Session as Document>::FIELDS[1].ty, FieldType::Uuid7);

    let store = TestStore::fjall_tmp();
    let rows: Collection<_, Uuid7, Session> = Collection::new(store);
    let device = Uuid7::with_millis(5_000);
    let token = Uuid4::new();
    rows.put(
        &device,
        &Session {
            token,
            device,
            note: "kept".into(),
        },
    )
    .unwrap();
    let got = rows.get(&device).unwrap().expect("row");
    assert_eq!(got.token, token);
    assert_eq!(got.device, device);
    assert_eq!(got.note, "kept");
}

#[cfg(feature = "arrow")]
#[test]
fn a_uuid_exports_as_a_fixed_sixteen_byte_column() {
    use okm_core::{FieldType, arrow_backend::arrow_type};
    assert_eq!(
        arrow_type(FieldType::Uuid7),
        arrow::datatypes::DataType::FixedSizeBinary(16),
        "a uuid is fixed width in the warehouse column too, not a variable binary blob"
    );
    assert_eq!(
        arrow_type(FieldType::Uuid4),
        arrow::datatypes::DataType::FixedSizeBinary(16)
    );
}

#[test]
fn a_uuid7_key_is_sixteen_bytes_and_its_order_is_time_order() {
    assert_eq!(<Uuid7 as KeyEncode>::KEY_LEN, 16);
    let early = Uuid7::with_millis(1_000);
    let late = Uuid7::with_millis(2_000);
    let (early_b, late_b) = (early.encode(), late.encode());
    assert_eq!(early_b.len(), 16);
    assert!(early_b < late_b, "byte order is time order");
    assert_eq!(Uuid7::decode(&early_b), early);
}

#[test]
fn rows_round_trip_by_uuid_key_and_scans_return_the_key() {
    let store = TestStore::fjall_tmp();
    let devices: Collection<_, Uuid7, Device> = Collection::new(store.clone());
    let a = Uuid7::with_millis(1_000);
    let b = Uuid7::with_millis(2_000);
    devices
        .put(
            &a,
            &Device {
                alias: "laptop".into(),
                seen: 10,
            },
        )
        .unwrap();
    devices
        .put(
            &b,
            &Device {
                alias: "phone".into(),
                seen: 20,
            },
        )
        .unwrap();

    let got = devices.get(&a).unwrap().expect("row a");
    assert_eq!(got.alias, "laptop");

    // The keyspace is already in clock order: no re-sorting, no index needed to read it that way.
    let keys = devices.scan_keys().unwrap();
    assert_eq!(keys, vec![a, b], "v7 keys sort by their millisecond prefix");

    let tokens: Collection<_, Uuid4, Token> = Collection::new(store);
    let t = Uuid4::new();
    tokens
        .put(
            &t,
            &Token {
                device: Ref::ref_key(a),
                expires: 99,
            },
        )
        .unwrap();
    assert_eq!(tokens.get(&t).unwrap().expect("token row").device.key, a);
}
