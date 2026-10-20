//! The dynamic reader names categorical values (ADR-0032): a `Vocab<N>` field
//! lands as the id on the wire, and the store-aware read pass turns it into the
//! label — the promise that a booth sees categories, not bare numbers.

use okm_core::schema::CollectionSchema;
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore, Vocab};
use okm_dynamic::{DynamicCollection, Value};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct VKey {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VKey)]
#[ok_ns(13)]
pub struct VRow {
    pub status: Vocab<u8>,
    /// The log-table shape from the modeling guide: an event type and an access
    /// path are categorical, the note is free text.
    pub event: Vocab<u16>,
    pub note: String,
}

#[test]
fn dynamic_read_names_vocab_labels() {
    let store = TestStore::slatedb_mem();

    // The typed side writes: interning happens on the write path, so the
    // vocabulary faces exist by the time anybody reads.
    let mut typed = Collection::<_, VKey, VRow>::new(store.clone());
    typed.put(
        &VKey { org: 1, id: 1 },
        &VRow {
            status: Vocab::of("warn"),
            event: Vocab::of("http.request"),
            note: "slow upstream".into(),
        },
    );
    typed.put(
        &VKey { org: 1, id: 2 },
        &VRow {
            status: Vocab::of("error"),
            event: Vocab::of("db.timeout"),
            note: "pool exhausted".into(),
        },
    );

    // The dynamic side reads the same table through the exported schema.
    let schema = CollectionSchema::of::<VKey, VRow>();
    let dynamic = DynamicCollection::new(store, 13, schema, vec![]);

    let first = dynamic
        .get(&VKey { org: 1, id: 1 }.encode())
        .expect("dynamic read")
        .expect("row exists");
    assert_eq!(
        first.get("status"),
        Some(&Value::Str("warn".into())),
        "a u8 vocabulary names its label"
    );
    assert_eq!(
        first.get("event"),
        Some(&Value::Str("http.request".into())),
        "a u16 vocabulary names its label"
    );
    assert_eq!(first.get("note"), Some(&Value::Str("slow upstream".into())));

    // Ids stay per field: the second row's status is id 1 while its event is
    // also id 1 — two independent vocabularies, two different labels.
    let second = dynamic
        .get(&VKey { org: 1, id: 2 }.encode())
        .expect("dynamic read")
        .expect("row exists");
    assert_eq!(second.get("status"), Some(&Value::Str("error".into())));
    assert_eq!(second.get("event"), Some(&Value::Str("db.timeout".into())));
}