//! Structural faces carry a fingerprint (ADR-0048): the derived state that lives
//! outside the primary table gets a number that says WHICH declaration built it.
//!
//! Locks: a fresh store is stamped with the declaration's own fingerprint; an
//! untouched store and one stamped by an older build both answer `Unknown` while
//! still opening on `version` alone; a second declaration in the same ns answers
//! `Stale`; and the exported schema carries the same number — so an outside
//! reader and the store cannot disagree about what "current" means.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::{
    Collection, Document, DocumentEncode, FaceStatus, KeyEncode, TableMeta, TestStore, meta,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct NoteKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NoteKey)]
#[ok_ns(91)]
#[ok_index(by_author { fields(author) })]
pub struct Note {
    pub author: u64,
    pub body: String,
}

/// The same ns and the same payload byte layout, one more face. From the store's
/// side this is exactly what an added `#[ok_index]` looks like: nothing in any
/// payload moves, and the new face has no entries.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NoteKey)]
#[ok_ns(91)]
#[ok_index(by_author { fields(author) })]
#[ok_index(by_body_len { fields(author), includes(body) })]
pub struct NoteWithOneMoreIndex {
    pub author: u64,
    pub body: String,
}

#[test]
fn a_fresh_store_is_stamped_with_its_declarations_faces() {
    let store = TestStore::default();
    let table = Note::collection(store.clone());

    assert_eq!(
        table.faces_status().unwrap(),
        FaceStatus::Unknown {
            current: Note::FACES
        },
        "nothing is stamped before the first-touch verb runs"
    );

    table.ensure_layout().unwrap();
    assert_eq!(table.faces_status().unwrap(), FaceStatus::Match);
    assert!(table.faces_status().unwrap().is_current());
}

#[test]
fn a_store_written_before_faces_existed_opens_but_answers_unknown() {
    let store = TestStore::default();
    let table = Note::collection(store.clone());
    // Exactly what an older build wrote: the shape stamp and nothing else.
    meta::write(
        &store,
        &meta::document_header::<Note>(),
        &TableMeta::stamped(Note::LAYOUT_VERSION),
    )
    .unwrap();

    assert!(
        table.ensure_layout().is_ok(),
        "version still matches, so the store opens — faces are not a refusal"
    );
    assert_eq!(
        table.faces_status().unwrap(),
        FaceStatus::Unknown {
            current: Note::FACES
        },
        "untrusted, not wrong: nothing ever recorded a fingerprint"
    );
}

#[test]
fn another_declaration_in_the_same_ns_answers_stale() {
    let store = TestStore::default();
    let one = Note::collection(store.clone());
    one.ensure_layout().unwrap();
    one.put(
        &NoteKey { id: 1 },
        &Note {
            author: 7,
            body: "hi".into(),
        },
    )
    .unwrap();

    let two: Collection<_, NoteKey, NoteWithOneMoreIndex> =
        NoteWithOneMoreIndex::collection(store.clone());
    assert_eq!(
        two.faces_status().unwrap(),
        FaceStatus::Stale {
            stored: Note::FACES,
            current: NoteWithOneMoreIndex::FACES,
        },
        "the store's faces answer for another key layout"
    );
    assert!(
        two.ensure_layout().is_ok(),
        "a stale face is repairable drift, not a refusal: the payload still reads"
    );
}

#[test]
fn a_declaration_with_more_faces_is_not_the_one_that_stamped_the_store() {
    let store = TestStore::default();
    Note::collection(store.clone()).ensure_layout().unwrap();
    assert_ne!(Note::FACES, NoteWithOneMoreIndex::FACES);
    assert!(
        Note::collection(store.clone())
            .faces_status()
            .unwrap()
            .is_current(),
        "the declaration that stamped it recognises its own faces"
    );
    assert!(
        !NoteWithOneMoreIndex::collection(store.clone())
            .faces_status()
            .unwrap()
            .is_current(),
        "one more index is a different set of faces"
    );
}

#[test]
fn the_export_carries_the_same_number_the_store_is_stamped_with() {
    use okm_core::schema::CollectionSchema;

    let schema = CollectionSchema::of::<NoteKey, Note>();
    assert_eq!(schema.faces, Note::FACES.to_vec());
    assert_eq!(schema.indexes.len(), 1);
    let idx = &schema.indexes[0];
    assert_eq!(idx.name, "by_author");
    assert_eq!(idx.slot, Note::INDEX_DECLS[0].slot);
    assert_eq!(idx.fields, vec!["author".to_string()]);
    assert!(idx.includes.is_empty());
    assert!(!idx.deprecated, "declaration state travels with the face");

    // The store agrees, which is what makes the export useful to an outside
    // reader: same number, same verdict.
    let store = TestStore::default();
    let table = Note::collection(store.clone());
    table.ensure_layout().unwrap();
    assert_eq!(table.faces_status().unwrap(), FaceStatus::Match);
}

#[test]
fn a_schema_json_written_before_faces_existed_still_reads() {
    use okm_core::schema::CollectionSchema;

    // The shape a writer before ADR-0048 produced: no `indexes`, no `faces`.
    let old = serde_json::json!({
        "key_len": 8,
        "key_fields": [
            { "name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": null }
        ],
        "layout_version": 1,
        "hot_width": 8,
        "payload_header_len": 3,
        "hot_fields": [
            { "name": "author", "ty": "U64", "width": 8, "offset": 0, "tag": null }
        ],
        "cold_fields": [
            { "name": "body", "ty": "Str", "width": 0, "offset": 0, "tag": 1 }
        ],
        "slots": {
            "primary": 0,
            "dynamic": 1,
            "field_vocab_id": 2,
            "field_vocab_name": 3,
            "declared_index_base": 12289,
            "declared_reduce_base": 16385,
            "junction_base": 20481
        }
    });
    let schema: CollectionSchema = serde_json::from_value(old).expect("an old schema still reads");
    assert!(
        schema.indexes.is_empty() && schema.faces.is_empty(),
        "absent reads as undescribed — never as 'this table declares no index'"
    );

    // And the full form round-trips with both halves intact.
    let full = CollectionSchema::of::<NoteKey, Note>();
    let text = serde_json::to_string(&full).unwrap();
    let back: CollectionSchema = serde_json::from_str(&text).unwrap();
    assert_eq!(back.indexes, full.indexes);
    assert_eq!(back.faces, full.faces);
}
