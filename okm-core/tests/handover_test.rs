//! Two instances, one log: the handover end to end (ADR-0056 §6).
//!
//! The shape is blue/green with the log as the only thing they share. A holds the claim,
//! serves and appends; B opens on its own store, finds it is the successor, asks A to
//! stop, waits for the position A stopped at, applies the log — and only then claims.
//!
//! A reacts to the request by polling, which is what an instance without a watcher thread
//! does: the request is a file, so noticing it is a `read`, and sealing is the same verb
//! its shutdown path uses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::time::Duration;

use okm_core::changelog::owner::{self, Role, State};
use okm_core::changelog::{ChangelogConfig, ChangelogStore, Location, table_dir};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(59)]
#[ok_name("handover_rows")]
pub struct Row {
    pub title: String,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root))
}

fn write(collection: &Collection<ChangelogStore<TestStore>, RowKey, Row>, id: u64, title: &str) {
    collection
        .put(
            &RowKey { id },
            &Row {
                title: title.to_string(),
            },
        )
        .unwrap();
}

#[test]
fn the_successor_asks_applies_and_then_claims() {
    let log = tempfile::tempdir().unwrap();
    let header = <Row as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log.path(), &header);
    let timeout = Duration::from_secs(5);

    // --- A: the holder. It claims, then serves. ---
    let mut holder_store = store(log.path());
    holder_store.register::<RowKey, Row>().unwrap();
    assert_eq!(
        holder_store.take_over("A", "/data/a", timeout).unwrap()[0].1,
        Role::First,
        "nothing claimed the log yet"
    );
    let holder = Collection::<_, RowKey, Row>::captured(holder_store).unwrap();
    write(&holder, 1, "one");
    write(&holder, 2, "two");

    // --- B asks. This is the successor's first write, and it is all it takes. ---
    let request = owner::announce_drain(&dir, "B", "/data/b").unwrap();
    assert_eq!(request.state, State::Draining);

    // --- A notices (an instance without a watcher thread polls) and hands over. ---
    let drained = holder
        .store()
        .drain_and_hand_over("A", "/data/a")
        .unwrap();
    let position = drained[0].1.sealed.clone().expect("the tail was sealed");
    let claim = owner::read(&dir).unwrap().unwrap();
    assert_eq!(claim.state, State::HandedOver);
    assert_eq!(claim.epoch, request.epoch, "the answer is to this round");
    assert_eq!(claim.position.as_deref(), Some(position.as_str()));

    // --- B finishes: apply the log, then claim. ---
    let mut successor_store = store(log.path());
    successor_store.register::<RowKey, Row>().unwrap();
    let roles = successor_store.take_over("B", "/data/b", timeout).unwrap();
    assert!(
        matches!(roles[0].1, Role::HandedOver { .. }),
        "{:?}",
        roles[0].1
    );

    let claim = owner::read(&dir).unwrap().unwrap();
    assert_eq!(claim.holder, "B");
    assert_eq!(claim.state, State::Writing);
    assert_eq!(claim.data, "/data/b");

    let successor = Collection::<_, RowKey, Row>::captured(successor_store).unwrap();
    assert_eq!(
        successor.get(&RowKey { id: 1 }).unwrap().unwrap().title,
        "one"
    );
    assert_eq!(
        successor.get(&RowKey { id: 2 }).unwrap().unwrap().title,
        "two"
    );

    // And it keeps writing to the same log, above what A left behind.
    write(&successor, 3, "three");
    assert_eq!(
        successor.get(&RowKey { id: 3 }).unwrap().unwrap().title,
        "three"
    );
}

#[test]
fn a_second_instance_over_the_same_store_is_refused() {
    let log = tempfile::tempdir().unwrap();
    let mut first = store(log.path());
    first.register::<RowKey, Row>().unwrap();
    assert_eq!(
        first.take_over("A", "/data/a", Duration::from_secs(1)).unwrap()[0].1,
        Role::First
    );

    let mut intruder = store(log.path());
    intruder.register::<RowKey, Row>().unwrap();
    let err = intruder
        .take_over("B", "/data/a", Duration::from_secs(1))
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("share one store"), "{text}");
}

#[test]
fn capture_off_opens_nothing_and_writes_nothing() {
    let log = tempfile::tempdir().unwrap();
    let off = ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig {
            location: Location::Off,
            ..ChangelogConfig::new(log.path())
        },
    );
    off.register::<RowKey, Row>().unwrap();
    let app = Collection::<_, RowKey, Row>::captured(off).unwrap();
    write(&app, 1, "one");

    assert_eq!(app.store().dir(), None, "no root, so nothing to open");
    assert!(
        app.store().status::<Row>().is_none(),
        "no writer was ever opened"
    );
    assert_eq!(
        std::fs::read_dir(log.path()).unwrap().count(),
        0,
        "the log directory is untouched"
    );
    // The write itself still happened: capture is a hint, the store is the record.
    assert_eq!(app.get(&RowKey { id: 1 }).unwrap().unwrap().title, "one");
}