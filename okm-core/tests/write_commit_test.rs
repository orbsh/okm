//! One semantic single-collection write = ONE engine commit (ADR-0003's
//! "internal buffer" half of the division of labour).
//!
//! `put`, `delete`, `merge`, `replace` and `purge` accumulate their whole write
//! set — primary
//! entry, declared faces, embedded children with their faces, the releases of
//! children the old document dropped, the reduce RMWs, and vocabulary growth —
//! into one `Batch` and hand it to the engine in one `commit_batch`. The
//! counting wrapper below is the proof: it sees every bare `put`/`del` (one
//! engine op each) and every commit, so a regression back to per-op writes
//! fails here instead of hiding in a latency profile. It also makes the crash
//! story checkable: the row, its faces and its ledgers live in one commit or
//! in none.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::cell::Cell;

use okm_core::engine::storage::ScanIter;
use okm_core::{
    Batch, Document, DocumentEncode, KeyEncode, ReduceCodec, TestStore, VirtualStorage,
};

/// An engine handle that counts how writes arrive: bare single-op writes
/// versus commits (each carrying N ops).
struct Counting<S> {
    inner: S,
    bare_puts: Cell<usize>,
    bare_dels: Cell<usize>,
    commits: Cell<usize>,
    ops_committed: Cell<usize>,
}

impl<S> Counting<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            bare_puts: Cell::new(0),
            bare_dels: Cell::new(0),
            commits: Cell::new(0),
            ops_committed: Cell::new(0),
        }
    }

    /// Zero the counters — a test measures one semantic call at a time.
    fn reset(&self) {
        self.bare_puts.set(0);
        self.bare_dels.set(0);
        self.commits.set(0);
        self.ops_committed.set(0);
    }
}

impl<S: VirtualStorage> VirtualStorage for Counting<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()> {
        self.bare_puts.set(self.bare_puts.get() + 1);
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> okm_core::error::Result<()> {
        self.bare_dels.set(self.bare_dels.get() + 1);
        self.inner.del(key)
    }

    fn scan_suffix(&self, prefix: &[u8]) -> okm_core::error::Result<Vec<Vec<u8>>> {
        self.inner.scan_suffix(prefix)
    }

    fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> okm_core::error::Result<Vec<Vec<u8>>> {
        self.inner.scan_range(begin, end)
    }

    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        self.inner.scan_range_iter(begin, end)
    }

    fn commit_batch(&self, batch: Batch) -> okm_core::error::Result<()> {
        self.commits.set(self.commits.get() + 1);
        self.ops_committed
            .set(self.ops_committed.get() + batch.ops.len());
        self.inner.commit_batch(batch)
    }
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct LedgerKey {
    pub id: u64,
}

/// A row carrying both a face and a ledger: every write the routine path must
/// funnel through the batch is declared here, so one `put` exercises them all.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LedgerKey)]
#[ok_index(by_account { fields(account) })]
#[ok_reduce(Count { group(account) })]
#[ok_ns(47)]
pub struct LedgerRow {
    pub account: u64,
    pub amount: u32,
}

use __OkmIndex_LedgerRow_by_account as Ledger_ByAccount;

fn ledger<S: VirtualStorage>(store: &S, account: u64) -> Option<u64> {
    okm_core::reduce_get::<_, __OkmReduce_LedgerRow_0>(
        store,
        <LedgerRow as Document>::NS_PREFIX,
        &LedgerKey { id: 0 },
        &LedgerRow { account, amount: 0 },
    )
    .expect("a reduce read on a working store")
}

fn row(account: u64) -> LedgerRow {
    LedgerRow { account, amount: 1 }
}

#[test]
fn put_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    t.put(&LedgerKey { id: 1 }, &row(7)).unwrap();

    let c = t.store();
    assert_eq!(c.commits.get(), 1, "a semantic write is one engine commit");
    assert_eq!(c.bare_puts.get(), 0, "no op may bypass the batch");
    assert_eq!(c.bare_dels.get(), 0, "no op may bypass the batch");
    assert_eq!(
        c.ops_committed.get(),
        3,
        "primary + face + ledger ride the one commit"
    );

    // The commit really carried them: row, face and ledger are all visible.
    assert_eq!(t.get(&LedgerKey { id: 1 }).unwrap(), Some(row(7)));
    assert_eq!(
        t.scan::<Ledger_ByAccount>(&7u64.to_be_bytes())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(ledger(t.store(), 7), Some(1));
}

#[test]
fn overwrite_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    let k = LedgerKey { id: 1 };
    t.put(&k, &row(7)).unwrap();
    t.store().reset();

    t.put(
        &k,
        &LedgerRow {
            account: 8,
            amount: 2,
        },
    )
    .unwrap();

    let c = t.store();
    assert_eq!(
        c.commits.get(),
        1,
        "the unfold and the fold share the commit"
    );
    assert_eq!(c.ops_committed.get(), 4, "unfold + primary + face + fold");
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));

    // The ledger compensated: the old group is empty, the new one counts once.
    assert_eq!(ledger(t.store(), 7), Some(0));
    assert_eq!(ledger(t.store(), 8), Some(1));
}

#[test]
fn delete_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    let k = LedgerKey { id: 1 };
    t.put(&k, &row(7)).unwrap();
    t.store().reset();

    t.purge(&k).unwrap();

    let c = t.store();
    assert_eq!(c.commits.get(), 1, "a removal is one engine commit");
    assert_eq!(c.ops_committed.get(), 3, "del primary + del face + unfold");
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));
    assert!(t.get(&k).unwrap().is_none());
    assert!(
        t.scan::<Ledger_ByAccount>(&7u64.to_be_bytes())
            .unwrap()
            .is_empty()
    );
    assert_eq!(ledger(t.store(), 7), Some(0));
}

#[test]
fn dynamic_write_including_vocabulary_growth_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    t.store().reset();

    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "colour".to_string(),
        okm_core::obj_dynamic::DynamicValue::Str("amber".into()),
    );
    t.merge(&LedgerKey { id: 1 }, &fields).unwrap();

    let c = t.store();
    assert_eq!(
        c.commits.get(),
        1,
        "vocabulary growth rides the entry's commit"
    );
    assert_eq!(
        c.ops_committed.get(),
        4,
        "label→id + id→label + cursor + the dynamic entry"
    );
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));
    assert_eq!(
        t.fetch(&LedgerKey { id: 1 })
            .unwrap()
            .and_then(|m| m.get("colour").cloned()),
        Some(okm_core::obj_dynamic::DynamicValue::Str("amber".into()))
    );
}

#[test]
fn merge_of_a_declared_field_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    let k = LedgerKey { id: 1 };
    t.put(&k, &row(7)).unwrap();
    t.store().reset();

    t.merge(
        &k,
        &map_of(&[("amount", okm_core::obj_dynamic::DynamicValue::UInt(9))]),
    )
    .unwrap();

    let c = t.store();
    assert_eq!(c.commits.get(), 1, "merge is one engine commit");
    assert_eq!(c.ops_committed.get(), 4, "unfold + primary + face + fold");
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));
    assert_eq!(t.get(&k).unwrap().unwrap().amount, 9);
    assert_eq!(
        t.get(&k).unwrap().unwrap().account,
        7,
        "unnamed declared field kept"
    );
}

#[test]
fn replace_is_one_engine_commit_across_both_slots() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    let k = LedgerKey { id: 1 };
    t.put(&k, &row(7)).unwrap();
    // Intern the dynamic name first, so the replace below measures the write
    // set only (a first-seen name adds its three vocabulary ops).
    t.merge(
        &k,
        &map_of(&[(
            "colour",
            okm_core::obj_dynamic::DynamicValue::Str("amber".into()),
        )]),
    )
    .unwrap();
    t.store().reset();

    t.replace(
        &k,
        &map_of(&[
            ("account", okm_core::obj_dynamic::DynamicValue::UInt(8)),
            (
                "colour",
                okm_core::obj_dynamic::DynamicValue::Str("slate".into()),
            ),
        ]),
    )
    .unwrap();

    let c = t.store();
    assert_eq!(c.commits.get(), 1, "both slots ride one commit");
    assert_eq!(
        c.ops_committed.get(),
        6,
        "unfold + primary + face put + STALE face del + fold, then the slot-1 entry"
    );
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));
    assert_eq!(t.get(&k).unwrap().unwrap().account, 8);
    assert_eq!(
        t.get(&k).unwrap().unwrap().amount,
        0,
        "unnamed declared field defaults"
    );
    assert_eq!(
        t.fetch(&k).unwrap().and_then(|m| m.get("colour").cloned()),
        Some(okm_core::obj_dynamic::DynamicValue::Str("slate".into()))
    );
}

#[test]
fn purge_is_one_engine_commit() {
    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    let k = LedgerKey { id: 1 };
    t.put(&k, &row(7)).unwrap();
    t.merge(
        &k,
        &map_of(&[(
            "colour",
            okm_core::obj_dynamic::DynamicValue::Str("amber".into()),
        )]),
    )
    .unwrap();
    t.store().reset();

    assert!(t.purge(&k).unwrap());

    let c = t.store();
    assert_eq!(c.commits.get(), 1, "a whole-document removal is one commit");
    assert_eq!(
        c.ops_committed.get(),
        4,
        "del primary + del face + unfold + del slot 1"
    );
    assert_eq!((c.bare_puts.get(), c.bare_dels.get()), (0, 0));
    assert!(t.fetch(&k).unwrap().is_none());
}

fn map_of(
    pairs: &[(&str, okm_core::obj_dynamic::DynamicValue)],
) -> std::collections::BTreeMap<String, okm_core::obj_dynamic::DynamicValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect()
}
/// The verbs take `&self`, so one shared handle drives a whole sequence.
///
/// The exclusivity a `merge` needs for its read-modify-write is NOT `&mut self`
/// any more: it is the collection being `!Sync` (the epoch counter is a `Cell`),
/// so one owner holds it and no second thread can interleave. What this test
/// pins is the API half of that: a plain `&Collection` — no `mut` binding, no
/// `&mut` parameter — writes, and every verb still lands exactly one commit.
#[test]
fn the_verbs_take_a_shared_handle_and_still_commit_once_each() {
    fn drive<S: VirtualStorage>(t: &okm_core::Collection<S, LedgerKey, LedgerRow>) {
        let k = LedgerKey { id: 1 };
        t.put(&k, &row(7)).unwrap();
        t.merge(
            &k,
            &map_of(&[("amount", okm_core::obj_dynamic::DynamicValue::UInt(2))]),
        )
        .unwrap();
        t.replace(
            &k,
            &map_of(&[("amount", okm_core::obj_dynamic::DynamicValue::UInt(3))]),
        )
        .unwrap();
        assert!(
            t.purge(&k).unwrap(),
            "purge addresses the document it was given"
        );
    }

    let t = <LedgerRow as Document>::collection(Counting::new(TestStore::slatedb_mem()));
    drive(&t);

    let c = t.store();
    assert_eq!(
        c.commits.get(),
        4,
        "one commit per verb, on a shared handle"
    );
    assert_eq!(c.bare_puts.get(), 0);
    assert_eq!(c.bare_dels.get(), 0);
    assert_eq!(
        t.get(&LedgerKey { id: 1 }).unwrap(),
        None,
        "purge took the row"
    );
}
