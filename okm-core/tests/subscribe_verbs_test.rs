//! Point verbs and the subscription (ADR-0008 / ADR-0047): every document verb
//! that leaves a typed row behind announces it — `put`, `merge`, `replace` — and
//! the ones that remove a row announce the `Delete` carrying the row they
//! removed. A write that only touches the dynamic segment has no document to
//! hand the event, so it announces nothing.
//!
//! Its own binary: the sink cell is a process-global registry, so a second test
//! in this file would race for the registration.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;

use okm_async::TestStore;
use okm_core::Collection;
use okm_value::Variant;

#[path = "subscribe_common.rs"]
mod subscribe_common;
use subscribe_common::okm_subscribe;
use subscribe_common::*;

/// `(op, epoch, balance)` per event, in arrival order.
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, u64, u64)>>>;

fn dmap(pairs: &[(&str, Variant)]) -> BTreeMap<String, Variant> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}

#[test]
fn every_point_verb_announces_the_row_it_leaves() {
    let seen: Seen = Default::default();
    let sink = seen.clone();
    okm_subscribe::SINK_ROWEVENT.register(move |ev: okm_subscribe::RowEvent| {
        if let okm_subscribe::RowEvent::Account(ev) = ev {
            sink.lock()
                .unwrap()
                .push((format!("{:?}", ev.op), ev.epoch, ev.document.balance));
        }
        true
    });

    let t: Collection<TestStore, AccountKey, Account> = Collection::new(TestStore::slatedb_mem());
    let key = AccountKey { id: 1 };

    t.put(&key, &Account { balance: 10 }).unwrap();
    // A declared name is a row RMW: Put, carrying the row the merge left.
    t.put(&key, &Variant::Map(dmap(&[("balance", Variant::U64(20))])))
        .unwrap();
    // Dynamic-only merge: slot 1 moves, no row is left for the event to carry.
    t.put(
        &key,
        &Variant::Map(dmap(&[("note", Variant::Str("x".into()))])),
    )
    .unwrap();
    // Declared names rebuild the row: Put, carrying the rebuilt document.
    t.put(
        &key,
        (&Variant::Map(dmap(&[("balance", Variant::U64(30))])),),
    )
    .unwrap();
    // No declared name removes the row: Delete, carrying the removed row — the
    // same signal `purge` gives for the same write.
    t.put(&key, (&Variant::Map(dmap(&[])),)).unwrap();
    // Nothing left to remove, so nothing announced.
    t.put(&key, (&Variant::Map(dmap(&[])),)).unwrap();
    assert!(t.get_row(&key).unwrap().is_none());

    t.put(&key, &Account { balance: 40 }).unwrap();
    t.purge(&key).unwrap();

    // Epochs 3 and 6 belong to the writes that announced nothing: the counter
    // is the table's, so an event-less commit still advances it.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("Put".to_string(), 1, 10),
            ("Put".to_string(), 2, 20),
            ("Put".to_string(), 4, 30),
            ("Delete".to_string(), 5, 30),
            ("Put".to_string(), 7, 40),
            ("Delete".to_string(), 8, 40),
        ]
    );
}
