//! The handover hook (ADR-0056 §6): the step of a cutover okm cannot perform.
//!
//! The claim says who may write. It cannot say where requests come from — a gateway in
//! front of two ports, a load balancer in front of two containers. So the SUCCESSOR runs
//! one program after it is the writer, and these tests are the contract for that program:
//! it runs once, it runs after the claim, a fresh log announces nothing, and a hook that
//! says no does not undo the claim.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::time::Duration;

use okm_async::TestStore;
use okm_core::changelog::owner::{self, State};
use okm_core::changelog::{ChangelogConfig, ChangelogStore, HandoverHook, table_dir};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RowKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowKey)]
#[ok_ns(96)]
#[ok_name("hooked_rows")]
pub struct Row {
    pub title: String,
}

fn store(root: &std::path::Path, hook: Option<HandoverHook>) -> ChangelogStore<TestStore> {
    let mut cfg = ChangelogConfig::new(root);
    cfg.handover = hook;
    ChangelogStore::new(TestStore::slatedb_mem(), cfg)
}

/// A hook that records what it was told, in one line of `|`-separated fields.
///
/// `sh -c` rather than a checked-in script: the test needs to see the environment, and a
/// script file would be a second copy of the assertion.
fn recorder(out: &std::path::Path) -> HandoverHook {
    let script = format!(
        "printf '%s|%s|%s|%s|%s' \"$OKM_ROLE\" \"$OKM_HOLDER\" \"$OKM_DATA\" \"$OKM_TABLES\" \
         \"$OKM_CHANGELOG_ROOT\" > {}",
        out.display()
    );
    HandoverHook::new("sh").args(["-c", &script])
}

fn write_a_row(collection: &Collection<ChangelogStore<TestStore>, RowKey, Row>, id: u64) {
    collection
        .put(
            &RowKey { id },
            &Row {
                title: "x".to_string(),
            },
        )
        .unwrap();
}

/// A holds the claim and has written; B is asked for a handover and A answers it. Returns
/// the log root and the successor's store, ready to `take_over`.
fn a_log_waiting_for_its_successor(
    log: &std::path::Path,
    hook: Option<HandoverHook>,
) -> (Vec<u8>, ChangelogStore<TestStore>) {
    let header = <Row as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log, &header);

    let mut a = store(log, None);
    a.register::<RowKey, Row>().unwrap();
    a.take_over("A", "/data/a", None, Duration::from_secs(5))
        .unwrap();
    let a = Collection::<_, RowKey, Row>::captured(a).unwrap();
    write_a_row(&a, 1);

    owner::announce_drain(&dir, "B", "/data/b").unwrap();
    a.store().drain_and_hand_over("A", "/data/a").unwrap();

    let b = store(log, hook);
    b.register::<RowKey, Row>().unwrap();
    (header, b)
}

#[test]
fn the_successor_runs_the_hook_once_it_is_the_writer() {
    let log = tempfile::tempdir().unwrap();
    let told = log.path().join("told.txt");
    let (header, mut b) = a_log_waiting_for_its_successor(log.path(), Some(recorder(&told)));

    b.take_over("B", "/data/b", None, Duration::from_secs(5))
        .unwrap();

    let seen = std::fs::read_to_string(&told).expect("the hook ran");
    let fields: Vec<&str> = seen.split('|').collect();
    assert_eq!(fields.len(), 5, "role|holder|data|tables|root: {seen}");
    assert_eq!(fields[0], "successor", "it caught up from another instance");
    assert_eq!(fields[1], "B", "the instance that became the writer");
    assert_eq!(fields[2], "/data/b", "its own store, as configured");
    assert_eq!(fields[3], "1", "one table took over");
    assert_eq!(
        fields[4],
        log.path().to_str().unwrap(),
        "the log root, so a script can read the owner files beside it"
    );

    // The claim was taken BEFORE the hook ran, so the instance that announced the switch
    // is the one that may write.
    let claim = owner::read(&table_dir(log.path(), &header))
        .unwrap()
        .unwrap();
    assert_eq!(claim.holder, "B");
    assert_eq!(claim.state, State::Writing);
    assert_eq!(claim.data, "/data/b");
}

#[test]
fn a_fresh_log_announces_nothing() {
    let log = tempfile::tempdir().unwrap();
    let told = log.path().join("told.txt");
    let mut first = store(log.path(), Some(recorder(&told)));
    first.register::<RowKey, Row>().unwrap();
    let roles = first
        .take_over("A", "/data/a", None, Duration::from_secs(5))
        .unwrap();
    assert_eq!(roles[0].1, owner::Role::First);
    assert!(
        !told.exists(),
        "there is no predecessor and no traffic to move: the deploy that created the log \
         is the one that points at it"
    );
}

#[test]
fn a_hook_that_says_no_does_not_undo_the_claim() {
    let log = tempfile::tempdir().unwrap();
    let refusing = HandoverHook::new("sh").args(["-c", "exit 3"]);
    let (header, mut b) = a_log_waiting_for_its_successor(log.path(), Some(refusing));

    let err = b
        .take_over("B", "/data/b", None, Duration::from_secs(5))
        .unwrap_err();
    let text = err.to_string();
    assert!(
        matches!(err, okm_core::error::Error::HandoverHook { .. }),
        "a failed announcement is its own answer, not a generic log error: {text}"
    );
    assert!(text.contains("sh"), "the program is named: {text}");
    assert!(
        text.contains("nothing is undone"),
        "the caller must not read this as a rolled-back cutover: {text}"
    );

    // The state the message promises: this instance holds the claim, so the single-writer
    // invariant is intact and the cutover is half done (owned, not announced).
    let claim = owner::read(&table_dir(log.path(), &header))
        .unwrap()
        .unwrap();
    assert_eq!(claim.holder, "B");
    assert_eq!(claim.state, State::Writing);

    // And writing works — what is missing is the routing, not the right to write.
    let b = Collection::<_, RowKey, Row>::captured(b).unwrap();
    write_a_row(&b, 2);
    assert_eq!(b.get_row(&RowKey { id: 2 }).unwrap().unwrap().title, "x");
}

#[test]
fn a_hook_that_hangs_is_killed_and_reported() {
    let log = tempfile::tempdir().unwrap();
    let slow = HandoverHook::new("sh").args(["-c", "sleep 10"]);
    let (_header, mut b) = a_log_waiting_for_its_successor(log.path(), Some(slow));

    let started = std::time::Instant::now();
    let err = b
        .take_over("B", "/data/b", None, Duration::from_millis(300))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the budget is the caller's, not the script's"
    );
    assert!(
        err.to_string().contains("did not finish"),
        "a program that never answers is still an answer: {err}"
    );
}
