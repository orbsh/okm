//! `Collection::rebuild_faces` (ADR-0048): the verb that repairs the drift the
//! face stamp names.
//!
//! The tests are the interesting half of this mechanism, because its promise is
//! about states that only exist mid-run or after a failure: a face that went
//! missing behind the store's back, a rebuild interrupted between commits, and
//! two chunkings of the same table having to agree byte for byte.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::rc::Rc;

use okm_core::{
    Batch, Collection, Document, DocumentEncode, FaceStatus, KeyEncode, TestStore, VirtualStorage,
};

use __OkmIndex_Note_by_author as Note_ByAuthor;
use __OkmIndex_NotePlus_by_author as NotePlus_ByAuthor;
use __OkmIndex_NotePlus_by_body as NotePlus_ByBody;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct NoteKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NoteKey)]
#[ok_ns(77)]
#[ok_index(by_author { fields(author) })]
pub struct Note {
    pub author: u64,
    pub body: String,
}

/// Same ns, same payload bytes, one more face: what an added `#[ok_index]` looks
/// like to a store that was written before it.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NoteKey)]
#[ok_ns(77)]
#[ok_index(by_author { fields(author) })]
#[ok_index(by_body { fields(author), includes(body) })]
pub struct NotePlus {
    pub author: u64,
    pub body: String,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NoteKey)]
#[ok_ns(78)]
pub struct Plain {
    pub value: u64,
}

/// Every key and value in the store, sorted by key — the honest way to say "these
/// two rebuilds produced the same store", rather than trusting one query shape.
fn dump<S: VirtualStorage + ?Sized>(store: &S) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for key in store.scan_range(b"", None).unwrap() {
        let value = store.get(&key).unwrap().unwrap_or_default();
        out.push((key, value));
    }
    out.sort();
    out
}

fn seed(store: &TestStore, rows: &[(u64, u64)]) {
    let t: Collection<TestStore, NoteKey, Note> = Collection::new(store.clone());
    // Attach, as a real caller does at boot: this is what stamps the store's
    // faces, and what makes a later declaration with another face set `Stale`
    // rather than `Unknown`.
    t.ensure_layout().unwrap();
    for (id, author) in rows {
        t.put(
            &NoteKey { id: *id },
            &Note {
                author: *author,
                body: format!("note {id}"),
            },
        )
        .unwrap();
    }
}

#[test]
fn a_rebuild_builds_the_face_a_second_declaration_added() {
    let store = TestStore::default();
    seed(&store, &[(1, 7), (2, 8)]);

    // The store was stamped by `Note`; `NotePlus` declares one more face.
    let plus: Collection<TestStore, NoteKey, NotePlus> = Collection::new(store.clone());
    assert!(matches!(
        plus.faces_status().unwrap(),
        FaceStatus::Stale { .. }
    ));
    let by_body = plus.scan::<NotePlus_ByBody>(&7u64.to_be_bytes()).unwrap();
    assert!(
        by_body.is_empty(),
        "the added face has no entries: nothing has ever derived it"
    );

    let report = plus.rebuild_faces(2).unwrap();
    assert_eq!(report.documents, 2);
    assert_eq!(report.faces_swept, 2, "both declared faces were swept");
    assert!(report.entries_written >= 2, "{report:?}");
    assert!(report.chunks >= 1);
    assert_eq!(plus.faces_status().unwrap(), FaceStatus::Match);

    assert_eq!(
        plus.scan::<NotePlus_ByBody>(&7u64.to_be_bytes())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        plus.scan::<NotePlus_ByBody>(&8u64.to_be_bytes())
            .unwrap()
            .len(),
        1
    );
    // The face the store already had is still right, not doubled.
    assert_eq!(
        plus.scan::<NotePlus_ByAuthor>(&7u64.to_be_bytes())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_face_entry_lost_behind_the_stores_back_comes_back() {
    let store = TestStore::default();
    seed(&store, &[(1, 7), (2, 8)]);
    let t: Collection<TestStore, NoteKey, Note> = Collection::new(store.clone());

    // Take one entry away without writing a row: this is what a face looks like
    // when it fell behind — bytes that no verb of the write path produced.
    let mut face = okm_core::meta::document_header::<Note>();
    okm_core::index::write_slot(&mut face, Note::INDEX_DECLS[0].slot);
    let victim = store
        .scan_range(&face, None)
        .unwrap()
        .into_iter()
        .next()
        .expect("the seeded rows derived entries");
    store.del(&victim).unwrap();
    assert_eq!(
        t.scan::<Note_ByAuthor>(&7u64.to_be_bytes()).unwrap().len(),
        0
    );

    let report = t.rebuild_faces(1).unwrap();
    assert_eq!(report.entries_deleted, 1, "the sweep removed what was left");
    assert_eq!(
        t.scan::<Note_ByAuthor>(&7u64.to_be_bytes()).unwrap().len(),
        1
    );
    assert_eq!(
        t.scan::<Note_ByAuthor>(&8u64.to_be_bytes()).unwrap().len(),
        1
    );
}

#[test]
fn the_chunk_size_does_not_change_what_the_store_ends_up_holding() {
    let one = TestStore::default();
    seed(&one, &[(1, 7), (2, 7), (3, 8), (4, 8), (5, 9)]);
    let all = TestStore::default();
    seed(&all, &[(1, 7), (2, 7), (3, 8), (4, 8), (5, 9)]);

    let t1: Collection<TestStore, NoteKey, Note> = Collection::new(one.clone());
    let t2: Collection<TestStore, NoteKey, Note> = Collection::new(all.clone());

    let small = t1.rebuild_faces(1).unwrap();
    let big = t2.rebuild_faces(usize::MAX).unwrap();

    assert!(
        small.chunks > big.chunks,
        "one op per commit is many commits"
    );
    assert_eq!(small.documents, big.documents);
    assert_eq!(small.entries_deleted, big.entries_deleted);
    assert_eq!(small.entries_written, big.entries_written);
    assert_eq!(dump(&one), dump(&all), "same table, same bytes");
}

/// A store whose commits run out — the interruption a real crash or a full disk
/// would cause, at the one instant that matters: between chunks.
struct Flaky {
    inner: TestStore,
    commits_left: Rc<std::cell::Cell<usize>>,
}

impl VirtualStorage for Flaky {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::Result<()> {
        self.inner.put(key, value)
    }
    fn get(&self, key: &[u8]) -> okm_core::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn del(&self, key: &[u8]) -> okm_core::Result<()> {
        self.inner.del(key)
    }
    fn scan_suffix(&self, prefix: &[u8]) -> okm_core::Result<Vec<Vec<u8>>> {
        self.inner.scan_suffix(prefix)
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> okm_core::Result<Vec<Vec<u8>>> {
        self.inner.scan_range(begin, end)
    }
    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>) -> okm_core::storage::ScanIter {
        self.inner.scan_range_iter(begin, end)
    }
    fn commit_batch(&self, batch: Batch) -> okm_core::Result<()> {
        let left = self.commits_left.get();
        if left == 0 {
            return Err(okm_core::Error::Engine(
                "injected: commit budget spent".into(),
            ));
        }
        self.commits_left.set(left - 1);
        self.inner.commit_batch(batch)
    }
}

#[test]
fn an_interrupted_rebuild_writes_no_stamp_and_a_rerun_finishes_the_job() {
    let inner = TestStore::default();
    seed(&inner, &[(1, 7), (2, 8), (3, 9)]);
    // A row written by another build: `version` only, no face stamp. That is the
    // state a rebuild candidate is in.
    okm_core::meta::write(
        &inner,
        &okm_core::meta::document_header::<NotePlus>(),
        &okm_core::TableMeta::stamped(NotePlus::LAYOUT_VERSION),
    )
    .unwrap();

    let flaky = Flaky {
        inner: inner.clone(),
        commits_left: Rc::new(std::cell::Cell::new(1)),
    };
    let t: Collection<Flaky, NoteKey, NotePlus> = Collection::new(flaky);
    assert_eq!(
        t.faces_status().unwrap(),
        FaceStatus::Unknown {
            current: NotePlus::FACES
        }
    );

    let err = t.rebuild_faces(1).unwrap_err();
    assert!(err.to_string().contains("injected"), "{err}");

    // Nothing claimed completion: the store still refuses to vouch for its faces.
    assert!(!t.faces_status().unwrap().is_current());

    // The same store, a caller that can finish: one run, then a match.
    let t2: Collection<TestStore, NoteKey, NotePlus> = Collection::new(inner.clone());
    t2.rebuild_faces(1).unwrap();
    assert_eq!(t2.faces_status().unwrap(), FaceStatus::Match);
    assert_eq!(
        t2.scan::<NotePlus_ByAuthor>(&8u64.to_be_bytes())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_table_that_declares_no_faces_only_gets_its_stamp() {
    let store = TestStore::default();
    let t: Collection<TestStore, NoteKey, Plain> = Collection::new(store.clone());
    t.put(&NoteKey { id: 1 }, &Plain { value: 3 }).unwrap();

    let report = t.rebuild_faces(1).unwrap();
    assert_eq!(report.faces_swept, 0);
    assert_eq!(report.entries_deleted, 0);
    assert_eq!(report.entries_written, 0);
    assert_eq!(report.documents, 1, "the rows were still walked");
    assert_eq!(t.faces_status().unwrap(), FaceStatus::Match);
}

#[cfg(feature = "fjall")]
#[test]
fn the_chunked_sweep_survives_a_native_snapshot_iterator() {
    // fjall hands out a SNAPSHOT iterator, and the sweep deletes the very keys it
    // is walking. That is legal — the snapshot keeps the versions it opened on —
    // but it is the one backend where a chunked rebuild could plausibly trip, so
    // the claim is asserted rather than assumed.
    let store = TestStore::fjall_tmp();
    seed(&store, &[(1, 7), (2, 8), (3, 7)]);
    let t: Collection<TestStore, NoteKey, Note> = Collection::new(store.clone());

    let report = t.rebuild_faces(1).unwrap();
    assert_eq!(report.documents, 3);
    assert_eq!(report.faces_swept, 1);
    assert_eq!(
        t.scan::<Note_ByAuthor>(&7u64.to_be_bytes()).unwrap().len(),
        2
    );
    assert_eq!(
        t.scan::<Note_ByAuthor>(&8u64.to_be_bytes()).unwrap().len(),
        1
    );
    assert_eq!(t.faces_status().unwrap(), FaceStatus::Match);
}
