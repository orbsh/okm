//! One write verb, and the payload type chooses the path (ADR-0035 as amended).
//!
//! `Collection::put` used to be three verbs — `put(&R)` for a typed row,
//! `merge(&map)` for a partial document, `replace(&map)` for a whole one — so a
//! caller had to translate its data into a verb name first. The three face
//! lifecycles never followed from the names anyway: they follow from the
//! payload. These tests pin that mapping, one payload shape at a time:
//!
//! - `&R` writes the row and its faces and does NOT sweep the previous values'
//!   entries (the fresh-row fast path; `purge` and `save_with_prev_into` are the
//!   verbs that clean).
//! - `&DynamicValue::Obj(map)` merges: a declared name is a row read-modify-write
//!   whose superseded face is removed, every other name overwrites just itself,
//!   and names the map omits are untouched.
//! - `(&DynamicValue::Obj(map),)` replaces: the map IS the document, so a name it
//!   omits is gone.
//! - A scalar or array payload is refused before anything reaches the store.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_value::DynamicValue;
use std::collections::BTreeMap;

use okm_async::TestStore;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DocKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_index(by_owner { fields(owner) })]
#[ok_ns(69)]
pub struct Doc {
    pub owner: u64,
    pub tag: String,
}

use __OkmIndex_Doc_by_owner as Doc_ByOwner;

fn table() -> Collection<TestStore, DocKey, Doc> {
    <Doc as Document>::collection(TestStore::slatedb_mem())
}

fn map(pairs: &[(&str, DynamicValue)]) -> BTreeMap<String, DynamicValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

/// How many entries the `by_owner` face holds for `owner`.
fn faces(t: &Collection<TestStore, DocKey, Doc>, owner: u64) -> usize {
    t.scan::<Doc_ByOwner>(&owner.to_be_bytes())
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap()
        .len()
}

/// The whole document as a map, or the empty one for `None` — a missing
/// document and an empty one answer the same question here. The composition
/// is the two primitives: a point read answers parts, the row converts to a
/// value, values merge.
fn doc_map(t: &Collection<TestStore, DocKey, Doc>, k: &DocKey) -> BTreeMap<String, DynamicValue> {
    let (row, seg) = t
        .get::<(okm_core::parts::Row, okm_core::parts::Value)>(k)
        .unwrap();
    let doc = row
        .map(|r| r.to_value())
        .unwrap_or_default()
        .merged(seg.unwrap_or_default());
    match doc {
        DynamicValue::Obj(map) => map,
        other => panic!("a document composes to an object, got {other:?}"),
    }
}

#[test]
fn a_typed_payload_writes_the_row_and_its_faces_and_sweeps_nothing() {
    let t = table();
    let k = DocKey { id: 1 };
    t.put(
        &k,
        &Doc {
            owner: 1,
            tag: "a".into(),
        },
    )
    .unwrap();

    assert_eq!(faces(&t, 1), 1, "the face was written from the row");
    assert_eq!(t.get_row(&k).unwrap().unwrap().owner, 1);

    // The struct door's documented contract: a changed indexed value leaves the
    // old entry dangling. `put` is the one write that does not read the row
    // first, so it is the one that cannot sweep — this is the `&R` arm's
    // lifecycle, not an accident of the new spelling.
    t.put(
        &k,
        &Doc {
            owner: 2,
            tag: "b".into(),
        },
    )
    .unwrap();

    assert_eq!(t.get_row(&k).unwrap().unwrap().owner, 2);
    assert_eq!(faces(&t, 2), 1, "the new value's face is here");
    assert_eq!(
        faces(&t, 1),
        1,
        "the superseded face is left in place — no read, no sweep"
    );
}

#[test]
fn an_object_payload_merges_declared_names_and_sweeps_the_old_face() {
    let t = table();
    let k = DocKey { id: 1 };
    t.put(
        &k,
        &Doc {
            owner: 1,
            tag: "a".into(),
        },
    )
    .unwrap();

    t.put(
        &k,
        &DynamicValue::Obj(map(&[
            ("owner", DynamicValue::U64(2)),
            ("note", DynamicValue::Str("first".into())),
            ("alt", DynamicValue::Str("keep".into())),
        ])),
    )
    .unwrap();

    let row = t.get_row(&k).unwrap().unwrap();
    assert_eq!(row.owner, 2, "the named declared field was rewritten");
    assert_eq!(row.tag, "a", "an unnamed declared field kept its value");
    assert_eq!(
        faces(&t, 1),
        0,
        "a declared name is a row RMW: the previous value's face is swept"
    );
    assert_eq!(faces(&t, 2), 1);
    assert_eq!(
        doc_map(&t, &k).get("note").cloned(),
        Some(DynamicValue::Str("first".into()))
    );

    // A second, smaller merge: only the names it carries move.
    t.put(
        &k,
        &DynamicValue::Obj(map(&[("note", DynamicValue::Str("second".into()))])),
    )
    .unwrap();

    assert_eq!(t.get_row(&k).unwrap().unwrap().owner, 2, "row untouched");
    let doc = doc_map(&t, &k);
    assert_eq!(
        doc.get("note").cloned(),
        Some(DynamicValue::Str("second".into())),
        "the named dynamic field moved"
    );
    assert_eq!(
        doc.get("alt").cloned(),
        Some(DynamicValue::Str("keep".into())),
        "an unnamed dynamic field was left alone"
    );
}

#[test]
fn a_one_element_tuple_payload_replaces_the_whole_document() {
    let t = table();
    let k = DocKey { id: 1 };
    t.put(
        &k,
        &Doc {
            owner: 1,
            tag: "a".into(),
        },
    )
    .unwrap();
    t.put(
        &k,
        &DynamicValue::Obj(map(&[
            ("note", DynamicValue::Str("first".into())),
            ("alt", DynamicValue::Str("keep".into())),
        ])),
    )
    .unwrap();

    t.put(
        &k,
        (&DynamicValue::Obj(map(&[("owner", DynamicValue::U64(3))])),),
    )
    .unwrap();

    let row = t.get_row(&k).unwrap().unwrap();
    assert_eq!(row.owner, 3);
    assert_eq!(row.tag, "", "an absent declared name takes the default");
    let doc = doc_map(&t, &k);
    assert!(
        !doc.contains_key("note") && !doc.contains_key("alt"),
        "a dynamic name the map omits is gone: {:?}",
        doc.keys().collect::<Vec<_>>()
    );
    assert_eq!(faces(&t, 3), 1, "the replacement's face is here");
    assert_eq!(
        faces(&t, 1),
        0,
        "the stored row's superseded face was swept"
    );

    // `replace` with no declared name at all is `purge`'s write, through the
    // same verb.
    t.put(&k, (&DynamicValue::Obj(BTreeMap::new()),)).unwrap();
    assert!(t.get_row(&k).unwrap().is_none());
    let (row, seg) = t
        .get::<(okm_core::parts::Row, okm_core::parts::Value)>(&k)
        .unwrap();
    assert!(
        row.is_none() && seg.is_none(),
        "an empty replace is a purge"
    );
}

#[test]
fn a_payload_that_is_not_an_object_is_refused_and_writes_nothing() {
    let t = table();
    let k = DocKey { id: 1 };
    t.put(
        &k,
        &Doc {
            owner: 1,
            tag: "a".into(),
        },
    )
    .unwrap();
    t.put(
        &k,
        &DynamicValue::Obj(map(&[("note", DynamicValue::U64(7))])),
    )
    .unwrap();
    let before = doc_map(&t, &k);
    let faces_before = faces(&t, 1);

    for payload in [
        DynamicValue::U64(9),
        DynamicValue::Str("a document-shaped value is an Obj".into()),
        DynamicValue::Array(vec![DynamicValue::U64(1)]),
        DynamicValue::Null,
    ] {
        let err = t.put(&k, &payload).unwrap_err();
        assert!(
            matches!(err, okm_core::Error::NotADocument { .. }),
            "a {} payload is not a document: {err}",
            okm_value::dyn_kind(&payload)
        );

        let err = t.put(&k, (&payload,)).unwrap_err();
        assert!(
            matches!(err, okm_core::Error::NotADocument { .. }),
            "the whole-document form refuses a {} payload too: {err}",
            okm_value::dyn_kind(&payload)
        );
    }

    // Nothing was written, not even a vocabulary frame: the refusal happens
    // before the first op enters the batch.
    assert_eq!(doc_map(&t, &k), before, "the document is untouched");
    assert_eq!(faces(&t, 1), faces_before, "no face moved");
}
