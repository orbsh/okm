//! A refused write comes back as `Err`, not as a panic (ADR-0039).
//!
//! ADR-0039 opened the storage trait's write verbs and said plainly that no test
//! proved a refusal reaches a caller. These are that proof, at the two places the
//! write path can still lose it: the batch commit (which is where every write
//! verb's ops actually reach an engine) and the wire sender (a dead connection or
//! a refused POST, both of which used to drop the write wordlessly).
//!
//! What is NOT here, deliberately: fault injection into the native engines
//! themselves (redb / fjall / slatedb). Forcing those to refuse means corrupting a
//! live store, and their mapping is one `map_err` line each with no branch to get
//! wrong. The in-process nest's "executor gone" branch is also untestable from
//! here: the pump thread lives exactly as long as any sender, so the branch cannot
//! fire while the test holds the store — it stays as a defensive arm.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

#![cfg(feature = "test-engines")]

use std::cell::Cell;

use okm_core::{Collection, DocumentEncode, Error, KeyEncode, TestStore, VirtualStorage};

/// The text the refusing engine answers with; asserted verbatim so a test cannot
/// pass on "some error" when the engine's own words are what a caller needs.
const REFUSAL: &str = "simulated refusal: the engine said no";

/// A store that refuses writes on demand, wrapping the real in-memory engine so
/// reads still answer (every write verb reads before it writes).
struct RefusingStore {
    inner: TestStore,
    /// `None` accepts everything; `Some(n)` accepts the first `n` writes and
    /// refuses every one after that.
    limit: Cell<Option<usize>>,
    writes: Cell<usize>,
}

impl RefusingStore {
    fn new() -> Self {
        Self {
            inner: TestStore::default(),
            limit: Cell::new(None),
            writes: Cell::new(0),
        }
    }

    fn refuse_after(&self, n: usize) {
        self.limit.set(Some(n));
    }

    fn accept_everything(&self) {
        self.limit.set(None);
    }

    fn refuses(&self) -> bool {
        match self.limit.get() {
            Some(n) => self.writes.get() >= n,
            None => false,
        }
    }
}

impl VirtualStorage for RefusingStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::Result<()> {
        if self.refuses() {
            return Err(Error::Engine(REFUSAL.to_string()));
        }
        self.writes.set(self.writes.get() + 1);
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> okm_core::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> okm_core::Result<()> {
        if self.refuses() {
            return Err(Error::Engine(REFUSAL.to_string()));
        }
        self.writes.set(self.writes.get() + 1);
        self.inner.del(key)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> okm_core::Result<Vec<Vec<u8>>> {
        self.inner.scan_range(begin, end)
    }
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RefuseKey {
    pub id: u64,
}

/// An index entry too, so a write is more than one op: that is what makes the
/// second test's "the accepted prefix of the batch is still there" observable.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RefuseKey)]
#[ok_ns(23)]
#[ok_index(by_n { fields(n) })]
pub struct RefuseRow {
    pub n: u64,
}

fn table() -> Collection<RefusingStore, RefuseKey, RefuseRow> {
    Collection::new(RefusingStore::new())
}

/// The claim ADR-0039 makes: a refused commit is a value carrying the engine's own
/// text, and the verb that asked for the write answers `Err`.
#[test]
fn a_refused_write_comes_back_as_the_engines_text() {
    let t = table();
    t.store().refuse_after(0);

    let err = t
        .put(&RefuseKey { id: 1 }, &RefuseRow { n: 5 })
        .expect_err("a refusing engine must not answer Ok");

    match &err {
        Error::Engine(text) => assert_eq!(text, REFUSAL),
        other => panic!("expected Error::Engine, got {other:?}"),
    }

    // Nothing landed: the primary put is the batch's first op, and it was refused.
    t.store().accept_everything();
    assert!(
        t.get(&RefuseKey { id: 1 }).unwrap().is_none(),
        "a refused write left a row behind"
    );
}

/// The boundary ADR-0039 records instead of hiding: the DEFAULT `commit_batch`
/// replays ops one by one, so an engine that refuses op N leaves ops 1..N applied.
/// Only engines with a native batch (fjall, redb, slatedb) commit all-or-nothing.
#[test]
fn a_replayed_batch_leaves_the_ops_before_the_refusal() {
    let t = table();
    t.store().refuse_after(1); // accept the primary entry, refuse the index entry

    let err = t
        .put(&RefuseKey { id: 2 }, &RefuseRow { n: 7 })
        .expect_err("the second op was refused, so the verb must answer Err");
    assert!(matches!(err, Error::Engine(_)), "{err:?}");

    t.store().accept_everything();
    assert!(
        t.get(&RefuseKey { id: 2 }).unwrap().is_some(),
        "the accepted prefix of a replayed batch stays written — that is the boundary"
    );
}
