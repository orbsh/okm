//! A generation change driven end to end (ADR-0075 §5–§7).
//!
//! The mechanism is three legs and this pins each of them: the verdict refuses a move that
//! does not flip the parity, the copy re-encodes through both declarations, and the flip is
//! one record — after which the new generation reads the rows and the old range is still
//! there with the old declaration's address on it.

#![cfg(feature = "schema-serde")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::migrate::{migrate_table, vacuum_generation, vacuum_preview};
use okm_core::model::dynamic_collection::DynamicCollection;
use okm_core::okm_value::{Dict, Variant};
use okm_core::schema::CollectionSchema;
use okm_core::testing::TestStore;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

/// Generation 1 — the build that wrote first.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(120)]
pub struct Gen1 {
    pub level: u32,
}

/// Generation 2 — the same table declared afresh: an extra field, and the version the address'
/// parity is derived from.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(120)]
#[ok_layout(version = 2)]
pub struct Gen2 {
    pub level: u32,
    pub score: u32,
}

/// Generation 3 — a bump that would land on generation 1's parity, which is the move the
/// verdict exists to refuse.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(120)]
#[ok_layout(version = 3)]
pub struct Gen3 {
    pub level: u32,
    pub score: u32,
}

fn seeded() -> TestStore {
    let store = TestStore::default();
    let one: Collection<_, K, Gen1> = Collection::open(store.clone()).expect("attach");
    one.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
    one.put(&K { id: 2 }, &Gen1 { level: 8 }).expect("put");
    drop(one);
    store
}

#[test]
fn a_table_moves_to_the_next_generation_in_place() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();

    let report = migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target)
        .expect("migrate generation 1 to 2");

    assert_eq!((report.from, report.to), (1, 2));
    assert_eq!(report.copied, 2, "both rows came across");
    assert_eq!(report.verified, 2, "and both were compared before the flip");
    assert_ne!(
        report.from_address, report.to_address,
        "one bit of the address separates the two ranges"
    );
    assert_eq!(report.replayed, 0, "no log was given, so there was no tail");

    // The new generation's own declaration reads what the migration wrote.
    let two: Collection<_, K, Gen2> = Collection::open(store.clone()).expect("attach generation 2");
    assert_eq!(
        two.get_row(&K { id: 1 }).expect("read").expect("the row"),
        Gen2 { level: 7, score: 0 },
        "the field the old layout did not carry takes the declaration's default"
    );
    assert_eq!(
        two.get_row(&K { id: 2 }).expect("read").expect("the row"),
        Gen2 { level: 8, score: 0 }
    );
}

#[test]
fn the_old_range_still_answers_the_old_declaration() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();
    migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target).expect("migrate");

    // A handle declared at the OLD generation addresses the old range, and the rows are still
    // there: a failed verification costs a re-run, not the data (ADR-0075).
    let old = DynamicCollection::new(
        store.clone(),
        120,
        CollectionSchema::of::<K, Gen1>(),
        Vec::new(),
    );
    let row = old
        .fetch(&K { id: 1 }.encode())
        .expect("fetch")
        .expect("the old range still holds the row");
    assert_eq!(row.len(), 1, "one field, still this generation's layout");
}

#[test]
fn a_jump_that_does_not_flip_the_parity_is_refused() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen3>();
    let err = migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target)
        .expect_err("generation 3 sits where generation 1 does");
    assert!(
        err.contains("moves by one"),
        "the refusal names the rule: {err}"
    );
}

#[test]
fn a_target_on_another_table_is_refused() {
    let store = seeded();
    let mut elsewhere = CollectionSchema::of::<K, Gen2>();
    // The ns keeps its generation bit; only the table it names changes.
    elsewhere.ns_prefix[1] = elsewhere.ns_prefix[1].wrapping_add(1);
    let err = migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &elsewhere)
        .expect_err("another ns is another table");
    assert!(
        err.contains("never the table"),
        "the refusal says what a migration may move: {err}"
    );
}

#[test]
fn the_vacuum_takes_the_old_range_and_leaves_the_new_one_whole() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();
    let moved =
        migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target).expect("migrate");

    let report = vacuum_generation(&store, None, &moved.to_address).expect("vacuum");
    assert_eq!((report.kept, report.removed), (2, 1));
    assert!(
        report.keys >= 2,
        "the two rows and the old record: {} keys",
        report.keys
    );

    // The new generation still reads; the old range answers nothing at all.
    let two: Collection<_, K, Gen2> = Collection::open(store.clone()).expect("attach generation 2");
    assert_eq!(
        two.get_row(&K { id: 1 }).expect("read").expect("the row"),
        Gen2 { level: 7, score: 0 }
    );
    let old = DynamicCollection::new(
        store.clone(),
        120,
        CollectionSchema::of::<K, Gen1>(),
        Vec::new(),
    );
    assert!(
        old.fetch(&K { id: 1 }.encode()).expect("fetch").is_none(),
        "the old range is gone, so a stale declaration answers nothing instead of stale rows"
    );
}

#[test]
fn a_second_vacuum_is_the_same_answer() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();
    let moved =
        migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target).expect("migrate");
    vacuum_generation(&store, None, &moved.to_address).expect("vacuum");

    let again = vacuum_generation(&store, None, &moved.to_address).expect("re-run");
    assert_eq!(again.removed, 0, "nothing older is left to remove");
    assert_eq!(again.kept, 2);
}

#[test]
fn the_vacuum_refuses_when_the_old_range_holds_a_row_the_new_one_lacks() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();
    let moved = migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target)
        .expect("migrate generation 1 to 2");

    // A row written into the OLD range after the migration — the case where deleting that range
    // would be the one irreversible way to lose data.
    let old = DynamicCollection::new(
        store.clone(),
        120,
        CollectionSchema::of::<K, Gen1>(),
        Vec::new(),
    );
    let mut late = Dict::new();
    late.insert("level".to_string(), Variant::U32(9));
    old.put(&K { id: 3 }.encode(), &late)
        .expect("write into the old range");

    // The pre-flight answers the same way, and it answers first: an operator reads it before the
    // run that removes data.
    let preview = vacuum_preview(&store, None, &moved.to_address).expect_err("the gate holds");
    assert!(preview.contains("vacuum refused"), "{preview}");

    let err = vacuum_generation(&store, None, &moved.to_address).expect_err("the gate holds");
    assert!(
        err.contains("not in the new one") && err.contains("vacuum refused"),
        "the refusal names what is missing and what it refused: {err}"
    );

    // And the old range is untouched by the refusal.
    assert!(
        old.fetch(&K { id: 3 }.encode()).expect("fetch").is_some(),
        "a refused vacuum deletes nothing"
    );
}

#[test]
fn a_dry_run_reports_what_would_go_and_deletes_nothing() {
    let store = seeded();
    let target = CollectionSchema::of::<K, Gen2>();
    let moved = migrate_table(&store, None, <Gen1 as Document>::NS_PREFIX, &target)
        .expect("migrate generation 1 to 2");

    let preview = vacuum_preview(&store, None, &moved.to_address).expect("the gate passes");
    assert_eq!((preview.kept, preview.removed), (2, 1));
    assert_eq!(preview.verified, 2, "the gate read both ranges");
    assert!(
        preview.keys >= 2,
        "the two rows and the old record: {} keys",
        preview.keys
    );

    // Nothing was deleted: the old range still answers its own declaration.
    let old = DynamicCollection::new(
        store.clone(),
        120,
        CollectionSchema::of::<K, Gen1>(),
        Vec::new(),
    );
    assert!(
        old.fetch(&K { id: 1 }.encode()).expect("fetch").is_some(),
        "a dry run deletes nothing"
    );
}
