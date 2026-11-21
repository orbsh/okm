//! ADR-0078: a store attests a table once, and every later open of that table on that store is
//! a lookaside hit. What this file pins is the *scope* of the claim — one store, not one process.
//! A process-wide claim would leave the second store's meta row unwritten, and a table with no
//! row is invisible to every reader that has no declaration to compare against.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::engine::storage::VirtualStorage;
use okm_core::model::meta;
use okm_core::testing::TestStore;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
struct RowKey {
    id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(0x7D)]
struct Row {
    title: String,
}

fn header() -> &'static [u8] {
    Row::PLAN.get().expect("plan is filled by attach").header()
}

/// The claim is per store, and it is made only once the row exists: the second store gets its
/// own row even though the same declaration already attached somewhere else in the process.
#[test]
fn the_claim_is_per_store_and_follows_the_row() {
    let a = TestStore::default();
    let _: Collection<TestStore, RowKey, Row> = Collection::open(a.clone()).expect("open a");
    assert!(
        a.attached(header(), std::any::type_name::<Row>()),
        "a attested the table"
    );
    assert!(
        meta::read(&a, header()).unwrap().is_some(),
        "a wrote its own row"
    );

    // b has never seen this table: same declaration, same process, no claim.
    let b = TestStore::default();
    assert!(
        !b.attached(header(), std::any::type_name::<Row>()),
        "the claim did not cross to another store"
    );

    let _: Collection<TestStore, RowKey, Row> = Collection::open(b.clone()).expect("open b");
    assert!(b.attached(header(), std::any::type_name::<Row>()));
    assert!(
        meta::read(&b, header()).unwrap().is_some(),
        "b seeded its own row, it did not borrow a's"
    );
}

/// The fast path must be indistinguishable from the slow one at the surface: same row, same
/// bytes, the second open no longer verifies.
#[test]
fn a_second_open_leaves_the_row_alone() {
    let store = TestStore::default();
    let _: Collection<TestStore, RowKey, Row> = Collection::open(store.clone()).expect("first");
    let row = meta::read(&store, header()).unwrap().expect("row");
    let _: Collection<TestStore, RowKey, Row> = Collection::open(store.clone()).expect("second");
    assert_eq!(
        meta::read(&store, header()).unwrap(),
        Some(row),
        "the attestation did not rewrite the row"
    );
}
