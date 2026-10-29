//! The drift lock for the schema-driven column builder (ADR-0045 §9): a drainer
//! has no row type, so the export it builds must equal the one the typed bridge
//! builds from the same declaration — same column order, same types, same values.
//!
//! The row below is deliberately awkward: a COLD field is declared first and
//! another sits between hot ones, so `CollectionSchema`'s hot/cold split has to be
//! put back into declaration order rather than emitted hot-then-cold.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::{
    Collection, DocumentEncode, KeyEncode, Offset, Quant, TestStore, VarInt, VirtualStorage,
};
use okm_dynamic::arrow::{ordered_fields, record_batch};
use okm_dynamic::{Value, decode_payload};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(61)]
#[ok_name("drift_rows")]
pub struct R {
    pub note: String, // cold, DECLARED FIRST
    pub n: u32,       // hot
    #[ok_offset(base = 1_700_000_000)]
    pub created: Offset, // hot
    pub price: Quant<3>, // hot
    pub extra: String, // cold, between hot and cold
    pub views: VarInt<u64>, // cold
}

fn rows() -> Vec<(K, R)> {
    (0u64..3)
        .map(|i| {
            (
                K {
                    org: 7,
                    id: 100 + i,
                },
                R {
                    note: format!("note-{i}"),
                    n: 40 + i as u32,
                    created: Offset(1_700_000_000 + 50 + i as i64),
                    price: Quant::<3>(12.345 + i as f64),
                    extra: if i == 1 {
                        String::new()
                    } else {
                        format!("x{i}")
                    },
                    views: VarInt(300 + i),
                },
            )
        })
        .collect()
}

#[test]
fn the_declaration_order_survives_the_hot_cold_split() {
    let schema = CollectionSchema::of::<K, R>();
    let names: Vec<&str> = ordered_fields(&schema)
        .expect("order reconstructs")
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["note", "n", "created", "price", "extra", "views"],
        "a cold field declared first stays first: the tag is its declaration index"
    );
}

#[test]
fn the_schema_driven_batch_equals_the_typed_one() {
    let t: Collection<TestStore, K, R> = Collection::new(TestStore::slatedb_mem());
    let schema = CollectionSchema::of::<K, R>();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut payloads = Vec::new();
    for (k, r) in rows() {
        t.put(&k, &r).expect("put");
        let pkey = t.primary_key(&k);
        let bytes = t.store().get(&pkey).expect("read").expect("row landed");
        keys.push(k.encode());
        payloads.push(Some(
            decode_payload(&schema, &bytes).expect("payload decodes"),
        ));
    }

    let typed = t.to_record_batch().expect("typed bridge");
    let dynamic = record_batch(&schema, &keys, &payloads, None).expect("schema-driven batch");

    assert_eq!(dynamic.schema(), typed.schema(), "same columns, same types");
    assert_eq!(dynamic.num_rows(), typed.num_rows());
    for i in 0..dynamic.num_columns() {
        assert_eq!(
            dynamic.column(i).to_data(),
            typed.column(i).to_data(),
            "column {i} ({}) must hold the same values",
            dynamic.schema().field(i).name()
        );
    }
}

#[test]
fn an_absent_payload_is_a_null_half_and_a_wrong_kind_is_an_error() {
    let schema = CollectionSchema::of::<K, R>();
    let key = K { org: 1, id: 2 }.encode();

    // A document with no row: the payload half is NULL, keys are still there.
    let batch = record_batch(&schema, std::slice::from_ref(&key), &[None], None).expect("batch");
    assert_eq!(batch.num_rows(), 1);
    assert!(batch.column(2).is_null(0), "first payload column is NULL");
    assert!(!batch.column(0).is_null(0), "keys are never NULL");

    // A row whose value is the wrong kind is a refusal, not a silent NULL.
    let mut wrong = okm_dynamic::ValueMap::new();
    wrong.insert("note".into(), Value::Str("ok".into()));
    wrong.insert("n".into(), Value::Str("not a number".into()));
    wrong.insert("created".into(), Value::I64(1_700_000_050));
    wrong.insert("price".into(), Value::F64(1.5));
    wrong.insert("extra".into(), Value::Str("e".into()));
    wrong.insert("views".into(), Value::U64(3));
    assert!(
        record_batch(&schema, std::slice::from_ref(&key), &[Some(wrong)], None).is_err(),
        "a u32 column fed a string is an error"
    );
}
