//! A dynamic reader opens from the store (ADR-0074 §5).
//!
//! The declaration the typed path puts in the meta row is the whole view a dynamic reader
//! needs: field positions, widths, key length, the ns pair. `DynamicCollection::open` reads it
//! there instead of asking the caller to carry a copy — and refuses the row that does not
//! describe the ns it was found at.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::model::dynamic_collection::DynamicCollection;
use okm_core::schema::CollectionSchema;
use okm_core::testing::TestStore;
use okm_core::{Collection, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(76)]
#[ok_name("opened_rows")]
pub struct R {
    pub level: u32,
    pub name: String,
}

/// The typed path attaches and writes; the dynamic handle reads the row back through the
/// declaration it found in the store, with no schema handed in.
#[test]
fn a_dynamic_handle_reads_what_the_typed_path_declared() {
    let store = TestStore::default();
    let typed: Collection<TestStore, K, R> = Collection::open(store.clone()).expect("attach");
    typed
        .put(
            &K { id: 1 },
            &R {
                level: 3,
                name: "opened".to_string(),
            },
        )
        .expect("put");
    drop(typed);

    let dynamic = DynamicCollection::open(store.clone(), 76).expect("the store describes it");
    assert_eq!(
        dynamic.schema(),
        &CollectionSchema::of::<K, R>(),
        "the declaration the dynamic handle reads is the one the derive wrote"
    );

    let row = dynamic
        .fetch(&K { id: 1 }.encode())
        .expect("fetch")
        .expect("the typed row");
    assert_eq!(row.len(), 2, "level and name: {row:?}");

    // The route is compiled from the same bytes, so a key of this table passes and another
    // table's key does not.
    let plan = dynamic.plan().expect("open compiles the plan");
    // `carries_key` takes the key the store sees: the header the plan was compiled from, then
    // the caller-encoded key.
    let mut own = vec![0x00, 0x4c];
    own.extend(K { id: 1 }.encode());
    assert!(plan.carries_key(&own), "this table's key");
    let mut foreign = vec![0x00, 0x4d];
    foreign.extend(K { id: 1 }.encode());
    assert!(!plan.carries_key(&foreign), "another ns is another table");
}

/// A store that never described the table has no declaration to open, and the refusal says
/// what to do about it.
#[test]
fn a_store_with_no_row_is_refused() {
    let store = TestStore::default();
    let refused = match DynamicCollection::<TestStore>::open(store, 76) {
        Ok(_) => panic!("a store that described nothing must not open"),
        Err(e) => e,
    };
    assert!(refused.contains("no meta row"), "{refused}");
    assert!(refused.contains("attach it first"), "{refused}");
}

/// The ns and the declaration travel as a pair: a row whose schema names another ns is a
/// refusal, not an override (ADR-0017 §4).
#[test]
fn a_row_that_describes_another_ns_is_refused() {
    let store = TestStore::default();
    let mut lying = CollectionSchema::of::<K, R>();
    lying.ns_prefix = vec![0x00, 0x4d];
    let declared = DynamicCollection::new(store.clone(), 76, lying, Vec::new());
    declared
        .ensure_layout()
        .expect("the row is written at ns 76");

    let refused = match DynamicCollection::<TestStore>::open(store, 76) {
        Ok(_) => panic!("a row that describes another ns must not open"),
        Err(e) => e,
    };
    assert!(
        refused.contains("004d"),
        "the refusal names the ns it found: {refused}"
    );
    assert!(refused.contains("pair disagrees"), "{refused}");
}

/// A handle the caller declared carries no compiled route: it carries the access methods the
/// store never saw, so the shape is the caller's to know.
#[test]
fn a_declared_handle_has_no_compiled_route() {
    let store = TestStore::default();
    let declared = DynamicCollection::new(store, 76, CollectionSchema::of::<K, R>(), Vec::new());
    assert!(declared.plan().is_none());
}
