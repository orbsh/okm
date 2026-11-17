//! Export of a secret (ADR-0069 §4): the column is declared and present, its
//! values are the ciphertext the store holds by DEFAULT, and `#[ok_redact]` turns
//! every value in that column into NULL without moving the column or its type.
//!
//! Nothing here decrypts anything — the engine holds no key. What the test locks
//! is the policy: ciphertext travels unless the declaration says otherwise, and
//! redaction is per field rather than a switch on the export.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::schema::CollectionSchema;
use okm_core::{Bytes, Collection, DocumentEncode, KeyEncode, Secret};
use okm_lake::bridge::ToRecordBatch;

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub id: u64,
}

/// `token` travels (ciphertext), `seed` does not (redacted).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(78)]
#[ok_name("secret_rows")]
pub struct Row {
    pub label: Bytes,
    pub token: Secret,
    #[ok_redact]
    pub seed: Secret,
}

#[test]
fn a_secret_column_is_ciphertext_and_a_redacted_one_is_null() {
    let t: Collection<TestStore, K, Row> = Collection::new(TestStore::slatedb_mem());
    for id in 1..=3u64 {
        t.put(
            &K { id },
            &Row {
                label: Bytes::new(format!("label-{id}").into_bytes()),
                token: Secret::new(vec![0xAA, id as u8]),
                seed: Secret::new(vec![0xBB, id as u8]),
            },
        )
        .expect("put");
    }
    let batch = t.to_record_batch().expect("typed bridge");

    // Every declared field still owns a column: redaction hides values, not the
    // shape (a missing column is far harder to notice than a null).
    let names: Vec<String> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(names, ["id", "label", "token", "seed"]);

    let token = batch.column(2);
    assert_eq!(token.len(), 3);
    for row in 0..3 {
        assert!(!token.is_null(row), "a secret column carries its ciphertext");
    }

    // The plain `Bytes` column still lands: variable-length columns copy
    // verbatim, and this is the case that used to panic on export.
    let label = batch.column(1);
    assert_eq!(label.len(), 3);
    assert!(!label.is_null(0), "a variable-length column exports its bytes");

    let seed = batch.column(3);
    assert_eq!(seed.len(), 3);
    for row in 0..3 {
        assert!(seed.is_null(row), "row {row}: `#[ok_redact]` writes NULL");
    }
    // The redacted column keeps its declared type, so a downstream reader's
    // schema does not change between a redacted and a plain table.
    assert_eq!(
        seed.data_type(),
        token.data_type(),
        "same column type, different values"
    );

    // And the schema says which field is redacted, so an outside reader does not
    // have to guess from a null column.
    let schema = CollectionSchema::of::<K, Row>();
    assert_eq!(schema.redacted, vec!["seed".to_string()]);
}