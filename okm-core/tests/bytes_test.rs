//! `Bytes` declared-field spelling — wire hex lock + map bridge +
//! put/get round trip. `Vec<u8>` field spelling is retired (compile-fail
//! suite carries the diagnostic); the wire bytes are IDENTICAL to the
//! old `Vec<u8>` encoding (cold TLV frame), so no storage migration.

use okm_core::{DocumentEncode, Document, KeyEncode, TestStore, Collection, Bytes};

#[derive(KeyEncode, Clone, Debug, PartialEq)]
pub struct BlobId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(BlobId)]
#[ok_ns(7)]
pub struct Blob {
    pub payload: Bytes,
    #[ok_default(Bytes::new(vec![1, 2]))]
    pub tag: Bytes,
}

#[test]
fn bytes_wire_hex_lock() {
    // Same frame layout the Vec<u8> spelling produced: cold TLV
    // [tag][len varint][raw bytes]. Declared fields: payload, tag.
    let doc = Blob {
        payload: Bytes::new(vec![0xDE, 0xAD]),
        tag: Bytes::new(vec![0xAA]),
    };
    // Same frame layout the Vec<u8> spelling produced: payload header
    // [version u8][hot_len u16 BE] + cold TLV frames [tag][len varint][bytes].
    // Fields: payload (tag 0, 2 bytes), tag (tag 1, 1 byte). No hot fields.
    let wire = doc.encode_payload();
    assert_eq!(
        wire,
        vec![1, 0, 0, 0, 0x02, 0xDE, 0xAD, 1, 0x01, 0xAA]
    );

    let back = Blob::decode_payload(&wire);
    assert_eq!(back, doc);
}

#[test]
fn bytes_put_get_roundtrip() {
    let store = TestStore::default();
    let mut col = Collection::new(store.clone());
    let doc = Blob {
        payload: Bytes::new(vec![1, 2, 3, 0xFF]),
        tag: Bytes::default(),
    };
    let key = BlobId { id: 42 };
    col.put(&key, &doc);
    let got = col.get(&key).unwrap();
    assert_eq!(got, doc);
}

#[test]
fn bytes_map_bridge() {
    let doc = Blob {
        payload: Bytes::new(b"xyz".to_vec()),
        tag: Bytes::new(vec![7]),
    };
    let m = doc.to_map();
    assert_eq!(
        m.get("payload"),
        Some(&okm_core::obj_dynamic::DynamicValue::Bytes(b"xyz".to_vec()))
    );
    let back = Blob::from_map(&m);
    assert_eq!(back, doc);
}

#[test]
fn bytes_absent_field_version_default() {
    // A v1 payload with header only and NO field frames: payload (absent
    // cold field, no ok_default) falls to Default = empty Bytes; tag
    // falls to its #[ok_default].
    let header_only: &[u8] = &[1, 0, 0]; // version 1, hot_len 0, cold empty
    let doc = Blob::decode_payload(header_only);
    assert_eq!(doc.payload, Bytes(vec![]));
    assert_eq!(doc.tag, Bytes(vec![1, 2]));
}
