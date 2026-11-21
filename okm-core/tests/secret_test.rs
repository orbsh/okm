//! `Secret` (ADR-0069): the same wire as `Bytes`, with three guard rails that
//! belong to the type rather than to a document — a redacted `Debug`, no way to
//! index it (a compile-fail case in `compilefail/secret_is_not_indexable.rs`),
//! and an export column that is ciphertext by default and NULL under
//! `#[ok_redact]`.
//!
//! What is locked here: the layout claim (a `Secret` field's payload bytes are
//! byte-identical to the `Bytes` field holding the same bytes), the Debug claim,
//! the round trip through a real engine, and the two declaration surfaces the
//! export reads (`REDACTED` and `CollectionSchema::redacted`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::model::schema::CollectionSchema;
use okm_core::testing::TestStore;
use okm_core::{Bytes, Collection, Document, DocumentEncode, FieldType, KeyEncode, Secret};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct VaultId {
    pub id: u64,
}

/// The twin without the guard rails: same bytes, same wire.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VaultId)]
#[ok_ns(75)]
pub struct Plain {
    pub level: u32,
    pub blob: Bytes,
}

/// The same row with the secret spelling.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VaultId)]
#[ok_ns(76)]
pub struct Guarded {
    pub level: u32,
    pub blob: Secret,
}

/// A secret that must not leave the store at all.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VaultId)]
#[ok_ns(77)]
pub struct Sealed {
    pub level: u32,
    #[ok_redact]
    pub blob: Secret,
    pub note: Bytes,
}

const CIPHERTEXT: [u8; 6] = [0xC1, 0x7E, 0x00, 0xFF, 0x2A, 0x9B];

#[test]
fn a_secret_encodes_exactly_like_the_bytes_it_holds() {
    let plain = Plain {
        level: 9,
        blob: Bytes::new(CIPHERTEXT.to_vec()),
    };
    let guarded = Guarded {
        level: 9,
        blob: Secret::new(CIPHERTEXT.to_vec()),
    };
    assert_eq!(
        plain.encode_payload(),
        guarded.encode_payload(),
        "the secret spelling is the Bytes frame, byte for byte — no layout change"
    );
    // And the declaration agrees: one field type, one width rule.
    let plain_desc = <Plain as Document>::FIELDS[1];
    let guarded_desc = <Guarded as Document>::FIELDS[1];
    assert_eq!(plain_desc.ty, FieldType::Bytes);
    assert_eq!(guarded_desc.ty, FieldType::Bytes);
    assert_eq!(plain_desc.width, guarded_desc.width);

    // The decoder is the twin's decoder plus the wrapper, so the round trip is
    // the same bytes back.
    let back = Guarded::decode_payload(&guarded.encode_payload());
    assert_eq!(back.blob.expose(), &CIPHERTEXT);
}

#[test]
fn debug_shows_the_length_and_never_the_bytes() {
    let row = Guarded {
        level: 9,
        blob: Secret::new(CIPHERTEXT.to_vec()),
    };
    let printed = format!("{row:?}");
    assert!(
        printed.contains("Secret(<redacted>, 6 bytes)"),
        "the debug form states the length so a log line stays diagnosable: {printed}"
    );
    assert!(
        !printed.contains("C1") && !printed.contains("c1") && !printed.contains("194"),
        "no plaintext or decimal form of the bytes: {printed}"
    );
}

#[test]
fn a_secret_round_trips_through_the_store() {
    for (engine, store) in TestStore::matrix() {
        let col: Collection<_, VaultId, Guarded> = Collection::open(&store).unwrap();
        let key = VaultId { id: 1 };
        col.put(
            &key,
            &Guarded {
                level: 4,
                blob: Secret::new(CIPHERTEXT.to_vec()),
            },
        )
        .unwrap_or_else(|e| panic!("{engine}: put: {e}"));
        let got = col
            .get_row(&key)
            .unwrap()
            .unwrap_or_else(|| panic!("{engine}: get: no row"));
        assert_eq!(got.level, 4);
        assert_eq!(got.blob.expose(), &CIPHERTEXT, "{engine}");
    }
}

#[test]
fn redaction_is_declared_per_field_and_reaches_the_export_schema() {
    // A `Secret` is not redacted by default: its column carries the ciphertext.
    assert_eq!(
        <Guarded as Document>::REDACTED,
        &[] as &[&str],
        "ciphertext export is the useful default (ADR-0069 §4)"
    );
    assert_eq!(<Sealed as Document>::REDACTED, &["blob"]);

    let schema = CollectionSchema::of::<VaultId, Sealed>();
    assert_eq!(schema.redacted, vec!["blob".to_string()]);
    // The column is still declared — the export writes the name, and NULL values.
    assert!(
        schema
            .hot_fields
            .iter()
            .chain(schema.cold_fields.iter())
            .any(|f| f.name == "blob"),
        "a redacted field keeps its column"
    );

    let plain = CollectionSchema::of::<VaultId, Guarded>();
    assert!(plain.redacted.is_empty());
}

/// The token-verification shape (ADR-0069 §3): a credential verified BY
/// presentation is hashed by the application, and the 32-byte digest is what the
/// index holds. `fields(token_hash, level)` puts a fixed-width field first and
/// still compiles — that is the claim (`Bytes`/`Str` there would be refused as
/// unlocatable), and it is why a digest is `[u8; 32]` rather than `Bytes`.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VaultId)]
#[ok_ns(79)]
#[ok_index(by_token_hash { fields(token_hash, level) })]
pub struct TokenRow {
    pub token_hash: [u8; 32],
    pub level: u32,
}

/// Not a real digest — a stand-in with the one property the test needs: it is not
/// the token, so a leak of the token into an index key is detectable.
fn stand_in_digest(token: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in token.iter().enumerate() {
        out[i % 32] ^= b.wrapping_mul(i as u8 + 1);
    }
    out
}

#[test]
fn a_digest_is_indexable_and_the_token_never_reaches_the_keyspace() {
    let store = TestStore::default();
    let col: Collection<_, VaultId, TokenRow> = Collection::open(&store).unwrap();
    let token = b"tok_live_0123456789abcdef";
    let digest = stand_in_digest(token);
    let row = TokenRow {
        token_hash: digest,
        level: 4,
    };
    let key = VaultId { id: 7 };
    col.put(&key, &row).unwrap();

    // The derive names each access method `__OkmIndex_<Row>_<index>`.
    let entry = col.index_key::<__OkmIndex_TokenRow_by_token_hash>(&key, &row);
    assert!(
        entry.windows(32).any(|w| w == digest),
        "the entry key carries the digest"
    );
    assert!(
        !entry.windows(token.len()).any(|w| w == token.as_slice()),
        "and never the token"
    );
    assert_eq!(col.get_row(&key).unwrap().unwrap().token_hash, digest);
}
