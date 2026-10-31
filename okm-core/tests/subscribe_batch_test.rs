//! A batch announces nothing (ADR-0031 §5, ADR-0047) — its own test binary,
//! because the subscribe sink is a process-global registry: two tests in one
//! binary race for the registration, and the loser's assertions describe a sink
//! that is no longer installed. Separate binaries, separate statics.
//!
//! Document types + the generated module come from subscribe_common.rs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::{Collection, TestStore};

#[path = "subscribe_common.rs"]
mod subscribe_common;
use subscribe_common::okm_subscribe;
use subscribe_common::*;

#[test]
fn a_batch_emits_no_event_and_still_lands() {
    // ADR-0031 §5 / ADR-0047: a batch writer owns its events. A batch is one
    // write set the caller built, so the core has no per-row moment at which to
    // announce "this row happened", and no boundary a consumer could fold to.
    // The face that carries a batch's writes is the change log (ADR-0045).
    use okm_core::Batch;

    let seen: std::sync::Arc<std::sync::Mutex<usize>> = Default::default();
    let sink = seen.clone();
    okm_subscribe::SINK_ROWEVENT.register(move |_ev: okm_subscribe::RowEvent| {
        *sink.lock().unwrap() += 1;
        true
    });

    let t: Collection<TestStore, AccountKey, Account> = Collection::new(TestStore::slatedb_mem());

    // Direction check first: this collection DOES announce a point write. Without
    // it, "the queue stayed empty" would also be true of a sink that was never
    // registered (another test in this binary may have replaced it) and the
    // negative assertion below would prove nothing.
    t.put(&AccountKey { id: 9 }, &Account { balance: 1 })
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        1,
        "the point path announces its write"
    );

    let mut batch = Batch::default();
    t.save_with_prev_into(
        &mut batch,
        &AccountKey { id: 1 },
        &Account { balance: 5 },
        None,
    )
    .unwrap();
    t.save_with_prev_into(
        &mut batch,
        &AccountKey { id: 2 },
        &Account { balance: 6 },
        None,
    )
    .unwrap();
    t.commit_batch(batch).unwrap();

    assert!(
        t.get(&AccountKey { id: 1 }).unwrap().is_some(),
        "the batch's rows landed"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        1,
        "and it announced nothing: a batch is silent by decision, not by accident"
    );
}
