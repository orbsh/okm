//! The layout identity, and the one place it is checked (ADR-0042 as amended).
//!
//! A payload carries no header, so nothing inside a row says which generation wrote
//! it. The number lives in the SEGMENT header, and both doors that open a log compare
//! it before decoding anything: the store's `register` and the replay driver. These
//! tests are the proof that the comparison is wired — without them the guard is a
//! comment.
//!
//! The two declarations below are the same table at two generations: same ns, same
//! key, same fields, different `#[ok_layout]`. That is exactly the pair an operator
//! produces by upgrading a build while pointing it at the old log directory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore, replay};
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

/// Generation 1 — what wrote the log.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(95)]
#[ok_name("generation_rows")]
pub struct Gen1 {
    pub level: u32,
}

/// Generation 2 — the same table, declared afresh. Nothing about the bytes is
/// compatible, and nothing needs to be: the log is refused, not reinterpreted.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(95)]
#[ok_layout(version = 2)]
#[ok_name("generation_rows")]
pub struct Gen2 {
    pub level: u32,
}

fn config(root: &std::path::Path) -> ChangelogConfig {
    ChangelogConfig::new(root)
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), config(root))
}

/// Write one row through generation 1 and seal, so the directory holds a real
/// segment stamped `layout_version = 1` and the sidecar that names it.
fn write_a_generation_one_log(root: &std::path::Path) {
    let c: Collection<_, K, Gen1> = Collection::captured(store(root)).expect("assemble");
    c.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
    c.put(&K { id: 2 }, &Gen1 { level: 8 }).expect("put");
    // Dropping the collection drops the store's writer, which seals the tail.
    drop(c);
}

#[test]
fn a_fresh_log_registers_at_whatever_version_its_declaration_says() {
    let root = tempfile::tempdir().expect("root");
    // No segment to compare against: the first build to touch a directory defines
    // the generation, and generation 2 is free to be that build.
    let store = store(root.path());
    store
        .register::<K, Gen2>()
        .expect("a fresh table is any generation");
}

#[test]
fn a_log_of_another_generation_is_refused_before_a_row_is_decoded() {
    let root = tempfile::tempdir().expect("root");
    write_a_generation_one_log(root.path());

    let err = store(root.path())
        .register::<K, Gen2>()
        .expect_err("generation 1 bytes under a generation 2 declaration");
    let text = format!("{err:?}");
    assert!(
        matches!(err, okm_core::error::Error::LayoutMismatch { .. }),
        "the refusal is the typed one, not a generic io error: {text}"
    );
    assert!(
        text.contains('1') && text.contains('2'),
        "both numbers are named, so an operator can tell which side is old: {text}"
    );
}

#[test]
fn the_same_generation_registers_against_its_own_log() {
    let root = tempfile::tempdir().expect("root");
    write_a_generation_one_log(root.path());
    // A restart of the SAME build: nothing to refuse, and nothing to replay.
    store(root.path())
        .register::<K, Gen1>()
        .expect("generation 1 reads its own log");
}

#[test]
fn replay_refuses_a_sidecar_that_claims_another_generation() {
    let root = tempfile::tempdir().expect("root");
    write_a_generation_one_log(root.path());

    // What an in-place upgrade looks like from the log's side: the segments say
    // generation 1, and beside them sits a declaration that says 2 — a build pointed at
    // the old directory instead of making a new one. Replay refuses the table instead
    // of decoding its bytes under the new shape.
    let dir = okm_core::changelog::table_dir(root.path(), <Gen1 as Document>::NS_PREFIX);
    std::fs::remove_file(dir.join(okm_core::changelog::sidecar_name(1)))
        .expect("drop the old sidecar");
    okm_core::changelog::write_sidecar(&dir, &okm_core::schema::CollectionSchema::of::<K, Gen2>())
        .expect("stamp the new declaration");

    let err = replay::replay_all(
        root.path(),
        |_ns, _schema: &okm_core::schema::CollectionSchema| Never(TestStore::slatedb_mem()),
    )
    .expect_err("the replay driver refuses generation 1 bytes under a generation 2 sidecar");
    assert!(
        err.contains("layout") && err.contains("ADR-0042"),
        "the message says what is wrong and where the rule lives: {err}"
    );
}

/// A target that must never be handed a row: `replay_all` builds it only after the
/// generation check, so reaching any method here means the check ran too late.
struct Never(TestStore);

impl replay::ReplayTarget for Never {
    type Store = TestStore;

    fn store(&self) -> &TestStore {
        &self.0
    }

    fn name(&self) -> &str {
        "never"
    }

    fn merge(&self, _pkey: &[u8], _row: &okm_core::obj_dynamic::ValueMap) -> Result<(), String> {
        panic!("a foreign-generation table must not reach the write path")
    }

    fn purge(&self, _pkey: &[u8]) -> Result<bool, String> {
        panic!("a foreign-generation table must not reach the write path")
    }

    fn fetch(&self, _pkey: &[u8]) -> Result<Option<okm_core::obj_dynamic::ValueMap>, String> {
        panic!("a foreign-generation table must not be read")
    }

    fn document_keys(&self) -> Result<Vec<Vec<u8>>, String> {
        panic!("a foreign-generation table must not be scanned")
    }
}

#[test]
fn several_sidecars_do_not_shadow_the_one_the_log_was_written_under() {
    let root = tempfile::tempdir().expect("root");
    write_a_generation_one_log(root.path());
    let dir = okm_core::changelog::table_dir(root.path(), <Gen1 as Document>::NS_PREFIX);

    // An upgrade leaves the old stamp and adds a new one. The log is still generation 1,
    // so that is the declaration its bytes are read through — alphabetical order would
    // pick by file name, and `schema-v10.json` sorts before `schema-v2.json`.
    okm_core::changelog::write_sidecar(&dir, &okm_core::schema::CollectionSchema::of::<K, Gen2>())
        .expect("stamp the newer declaration beside the old one");
    let loaded = okm_core::changelog::window::load_sidecar(&dir).expect("the generation 1 stamp");
    assert_eq!(
        loaded.layout_version, 1,
        "the log's own generation picks the sidecar, not the file name"
    );
    assert_eq!(loaded.name, "generation_rows");

    // And the same directory replays: still generation 1 on both sides.
    store(root.path())
        .register::<K, Gen1>()
        .expect("the log is generation 1 and its declaration says so");
}
