//! The `DynamicCollection` vocabulary mirror: one load per handle, kept current
//! by PROBES.
//!
//! Two properties are what make a cached mirror safe, and each has a lock here:
//!
//! - the allocation WATERMARK is re-read from the store before a plan, so a
//!   second handle on the same engine cannot be handed an id it already took;
//! - both doors resolve a MISS against the store — a name the planner does not
//!   know is adopted before it can allocate a second id, and an id the decoder
//!   does not know is looked up on the id face instead of dropping the field.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::{DocumentEncode, KeyEncode, TestStore, write_slot};
use okm_dynamic::{DynamicCollection, Value, ValueMap};

const NS: u16 = 47;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DocKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_ns(47)]
pub struct Doc {
    pub title: String,
}

fn table(store: TestStore) -> DynamicCollection<TestStore> {
    DynamicCollection::new(store, NS, CollectionSchema::of::<DocKey, Doc>(), Vec::new())
}

fn key(id: u64) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

fn map(pairs: &[(&str, Value)]) -> ValueMap {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

/// Every label the ID face holds, in id order — one entry per allocated label,
/// so a duplicate allocation for one name shows up as a longer list.
fn labels(store: &TestStore) -> Vec<String> {
    let mut prefix = NS.to_be_bytes().to_vec();
    write_slot(&mut prefix, 0x02);
    okm_core::engine::storage::scan_suffix_kv(store, &prefix)
        .unwrap()
        .into_iter()
        // The cursor is the same prefix with no suffix; an id entry is 2 bytes.
        .filter(|(suffix, _)| suffix.len() == 2)
        .filter_map(|(_, name)| String::from_utf8(name).ok())
        .collect()
}

/// Two handles, one engine: the cached watermark must follow the other
/// handle's allocation, and a cached name map must ADOPT a label the other
/// handle wrote instead of allocating a second id for it.
#[test]
fn two_handles_allocate_one_id_per_label() {
    let store = TestStore::slatedb_mem();
    let a = table(store.shared_handle());
    let b = table(store.shared_handle());

    // b loads its mirror here: it knows nothing yet.
    b.merge(&key(2), &map(&[("beta", Value::Str("y".into()))]))
        .unwrap();
    // a allocates after b's id — a cached watermark would reuse it.
    a.merge(&key(1), &map(&[("alpha", Value::Str("x".into()))]))
        .unwrap();
    a.merge(&key(3), &map(&[("gamma", Value::Str("z".into()))]))
        .unwrap();
    // b's mirror never saw "alpha": it must adopt the stored id, not mint one.
    b.merge(&key(4), &map(&[("alpha", Value::Str("w".into()))]))
        .unwrap();

    // Ids follow allocation order (beta first here); what the lock is about is
    // that each label owns exactly ONE, whichever handle wrote it.
    let mut got = labels(&store);
    got.sort();
    assert_eq!(
        got,
        vec!["alpha", "beta", "gamma"],
        "one id per label, whichever handle wrote it"
    );
    assert_eq!(
        a.fetch(&key(3)).unwrap().unwrap()["gamma"],
        Value::Str("z".into())
    );
    assert_eq!(
        b.fetch(&key(2)).unwrap().unwrap()["beta"],
        Value::Str("y".into())
    );
}

/// A mirror that loaded BEFORE a name existed still reads it back: the decode
/// resolves an unknown id on the id face instead of dropping the field (a
/// dropped field looks exactly like data loss to the caller).
#[test]
fn a_stale_mirror_resolves_an_unknown_id_from_the_engine() {
    let store = TestStore::slatedb_mem();
    let writer = table(store.shared_handle());
    let reader = table(store.shared_handle());

    writer
        .merge(&key(1), &map(&[("early", Value::U64(1))]))
        .unwrap();
    // Loads the reader's mirror: "early" and nothing else.
    assert_eq!(
        reader.fetch(&key(1)).unwrap().unwrap()["early"],
        Value::U64(1)
    );

    writer
        .merge(&key(2), &map(&[("late", Value::U64(2))]))
        .unwrap();
    assert_eq!(
        reader.fetch(&key(2)).unwrap().unwrap()["late"],
        Value::U64(2),
        "an id outside the mirror is probed, not dropped"
    );
}
