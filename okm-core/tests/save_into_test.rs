//! save_into / commit_batch — the cross-collection atomic path
//! (ADR-0003): a document collection and a junction encode into one batch,
//! one commit makes them live or die together. Covers both orderings
//! (nothing written before commit; everything written after).
//!
//! Plus the SEMANTIC batch path (ADR-0031): `save_with_prev_into` runs the
//! reduce fold/unfold and the face cleanup through `BatchStore`, so a table
//! with a ledger can join a batch — same-group writes accumulate, overwrites
//! leave no stale faces, and an uncommitted batch moves no accumulator.

use okm_core::{
    DocumentEncode, JunctionEncode, KeyEncode, Ref, VirtualStorage, TestStore, Collection,
    Document, ReduceCodec, Vocab,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PostKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_index(by_author { fields(author_id) })]
#[ok_ns(41)]
pub struct Post {
    pub author_id: u64,
    pub title: String,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AuthorKey {
    pub id: u64,
}

/// 端点文档：author（ns=43，仅作 junction 端点，不需要自己的主表）
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(AuthorKey)]
#[ok_ns(43)]
pub struct Author {
    pub id: u64,
}

#[derive(JunctionEncode, Clone)]
#[ok_junction(1)]
pub struct AuthorPost {
    #[ok_head(id)]
    pub author: Ref<Author, AuthorKey>,
    pub post: Ref<Post, PostKey>,
}

#[test]
fn save_into_defers_until_commit() {
    let store = TestStore::slatedb_mem();
    let mut batch = okm_core::Batch::default();
    let t: Collection<TestStore, PostKey, Post> = Collection::new(store.clone());

    // Encode-only: nothing lands in any store.
    t.save_into(
        &mut batch,
        &PostKey { id: 1 },
        &Post { author_id: 7, title: "hello".into() },
    );
    assert!(
        t.get(&PostKey { id: 1 }).is_none(),
        "save_into must not write"
    );

    // Commit: primary + index entries land together.
    store.commit_batch(batch).expect("commit");
    let t2: Collection<TestStore, PostKey, Post> = Collection::new(store.clone());
    assert!(t2.get(&PostKey { id: 1 }).is_some(), "document lands at commit");
}

#[test]
fn cross_collection_one_batch() {
    // Document collection + junction share one batch: both live or neither does.
    let store = TestStore::slatedb_mem();
    let mut batch = okm_core::Batch::default();

    {
        let t: Collection<TestStore, PostKey, Post> = Collection::new(store.clone());
        t.save_into(
            &mut batch,
            &PostKey { id: 1 },
            &Post { author_id: 7, title: "hi".into() },
        );
        let edges: okm_core::Junction<TestStore, AuthorPost> =
            okm_core::Junction::new(store.clone());
        edges.save_into(&mut batch, &AuthorKey { id: 7 }, &PostKey { id: 1 });
    }

    store.commit_batch(batch).expect("commit");

    // Both landed through the ONE committed store instance.
    let t: Collection<TestStore, PostKey, Post> = Collection::new(store.clone());
    let scanned = t.scan::<Post_ByAuthor>(&7u64.to_be_bytes());
    assert_eq!(scanned.len(), 1);
    let edges: okm_core::Junction<TestStore, AuthorPost> = okm_core::Junction::new(store);
    let fwd = edges.forward(&AuthorKey { id: 7 });
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0], PostKey { id: 1 });
}

use __OkmIndex_Post_by_author as Post_ByAuthor;

// ---- the semantic batch path (ADR-0031) ----

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct EntryKey {
    pub id: u64,
}

/// A row carrying both a ledger (reduce) and a face (index): every hook the
/// semantic batch path must honor is declared here, so one call exercises
/// all of them.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntryKey)]
#[ok_index(by_account { fields(account) })]
#[ok_reduce(Count { group(account) })]
#[ok_ns(44)]
pub struct Entry {
    pub account: u64,
    pub amount: u32,
}

use __OkmIndex_Entry_by_account as Entry_ByAccount;

/// Group accumulator as the STORE (not the batch) currently holds it.
fn ledger(t: &Collection<TestStore, EntryKey, Entry>, account: u64) -> Option<u64> {
    okm_core::reduce_get::<_, __OkmReduce_Entry_0>(
        t.store(),
        <Entry as okm_core::Document>::NS_PREFIX,
        &EntryKey { id: 0 },
        &Entry { account, amount: 0 },
    )
}

fn entry(id: u64, account: u64, amount: u32) -> (EntryKey, Entry) {
    (EntryKey { id }, Entry { account, amount })
}

#[test]
fn same_group_twice_in_one_batch_accumulates() {
    // The double-count the ADR removes: without read-through both writes
    // would fold from the pre-batch accumulator and the group would end at 1.
    let store = TestStore::slatedb_mem();
    let t = <Entry as Document>::collection(store.clone());
    let mut batch = okm_core::Batch::default();

    let (k1, r1) = entry(1, 7, 10);
    let (k2, r2) = entry(2, 7, 5);
    t.save_with_prev_into(&mut batch, &k1, &r1, None).unwrap();
    t.save_with_prev_into(&mut batch, &k2, &r2, None).unwrap();

    // Nothing is visible before the commit — the ledger included.
    assert!(ledger(&t, 7).is_none(), "an uncommitted batch owns no accumulator");

    store.commit_batch(batch).expect("commit");
    assert_eq!(ledger(&t, 7), Some(2), "both writes folded, in batch order");
}

#[test]
fn overwrite_in_one_batch_reads_the_pending_old_row() {
    // prev = None reads through the adapter, so the row THIS batch produced
    // is the old row: its fold is undone before the new fold lands.
    let store = TestStore::slatedb_mem();
    let t = <Entry as Document>::collection(store.clone());
    let (k, v1) = entry(1, 7, 10);
    let v2 = Entry { account: 8, amount: 10 };
    let mut batch = okm_core::Batch::default();
    t.save_with_prev_into(&mut batch, &k, &v1, None).unwrap();
    t.save_with_prev_into(&mut batch, &k, &v2, None).unwrap();
    store.commit_batch(batch).expect("commit");

    assert_eq!(ledger(&t, 7), Some(0), "the stale fold was unfolded");
    assert_eq!(ledger(&t, 8), Some(1), "the new fold counted once");
    assert!(
        t.scan::<Entry_ByAccount>(&7u64.to_be_bytes()).is_empty(),
        "the old value's face is gone — no dangling index entry"
    );
    assert_eq!(t.scan::<Entry_ByAccount>(&8u64.to_be_bytes()).len(), 1);
}

#[test]
fn caller_supplied_prev_drives_the_unfold_and_the_cleanup() {
    // The common shape: the caller already read the old row in a prior batch.
    let store = TestStore::slatedb_mem();
    let t = <Entry as Document>::collection(store.clone());
    let (k, v1) = entry(1, 7, 10);

    let mut batch = okm_core::Batch::default();
    t.save_with_prev_into(&mut batch, &k, &v1, None).unwrap();
    store.commit_batch(batch).expect("commit");
    assert_eq!(ledger(&t, 7), Some(1));

    let v2 = Entry { account: 8, amount: 10 };
    let mut batch = okm_core::Batch::default();
    t.save_with_prev_into(&mut batch, &k, &v2, Some(&v1)).unwrap();
    store.commit_batch(batch).expect("commit");

    assert_eq!(ledger(&t, 7), Some(0));
    assert_eq!(ledger(&t, 8), Some(1));
    assert!(t.scan::<Entry_ByAccount>(&7u64.to_be_bytes()).is_empty());
}

#[test]
fn an_uncommitted_batch_moves_neither_the_row_nor_the_ledger() {
    let store = TestStore::slatedb_mem();
    let t = <Entry as Document>::collection(store.clone());
    let (k, v1) = entry(1, 7, 1);
    let mut batch = okm_core::Batch::default();
    t.save_with_prev_into(&mut batch, &k, &v1, None).unwrap();
    store.commit_batch(batch).expect("commit");

    // Abandoned: the unfold and the fold both live only in the dropped batch.
    let mut batch = okm_core::Batch::default();
    t.save_with_prev_into(&mut batch, &k, &Entry { account: 8, amount: 2 }, None).unwrap();
    drop(batch);

    assert_eq!(ledger(&t, 7), Some(1), "the unfold never landed");
    assert_eq!(ledger(&t, 8), None, "the fold never landed");
    assert_eq!(t.get(&k).expect("row").account, 7);
}

#[test]
fn atomic_write_and_reduce_in_one_commit() {
    // The ADR's headline: multi-table atomicity AND a ledger, together.
    let store = TestStore::slatedb_mem();
    let mut batch = okm_core::Batch::default();
    let t = <Entry as Document>::collection(store.clone());
    let edges: okm_core::Junction<TestStore, AuthorPost> = okm_core::Junction::new(store.clone());

    let (k, r) = entry(1, 7, 3);
    t.save_with_prev_into(&mut batch, &k, &r, None).unwrap();
    edges.save_into(&mut batch, &AuthorKey { id: 7 }, &PostKey { id: 1 });
    store.commit_batch(batch).expect("commit");

    assert_eq!(ledger(&t, 7), Some(1));
    assert_eq!(edges.forward(&AuthorKey { id: 7 }), vec![PostKey { id: 1 }]);
}

#[test]
fn batch_store_reads_pending_ops_newest_first() {
    let store = TestStore::slatedb_mem();
    store.put(b"k1".to_vec(), b"stored".to_vec());
    let mut batch = okm_core::Batch::default();
    {
        let view = okm_core::BatchStore::new(&store, &mut batch);
        view.put(b"k1".to_vec(), b"fresh".to_vec());
        view.del(b"k2");
        view.put(b"k2".to_vec(), b"revived".to_vec());
        view.put(b"k3".to_vec(), b"new".to_vec());

        // Pending put shadows the store; del-then-put answers the put.
        assert_eq!(view.get(b"k1").as_deref(), Some(&b"fresh"[..]));
        assert_eq!(view.get(b"k2").as_deref(), Some(&b"revived"[..]));
        assert_eq!(view.get(b"k3").as_deref(), Some(&b"new"[..]));
        // Put-then-del answers the del.
        view.del(b"k1");
        assert_eq!(view.get(b"k1"), None, "the later del shadows the put");
        // The store itself never moved.
        assert_eq!(store.get(b"k1").as_deref(), Some(&b"stored"[..]));
        assert!(store.get(b"k3").is_none());
    }
    // The caller still owns the batch, with every op in order (two puts and
    // one del for k1/k2, one put for k3).
    assert_eq!(batch.ops.len(), 5);
}

#[test]
fn scans_on_a_batch_view_are_refused() {
    let store = TestStore::slatedb_mem();
    let mut batch = okm_core::Batch::default();
    let view = okm_core::BatchStore::new(&store, &mut batch);
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| view.scan_suffix(b"x")));
    assert!(refused.is_err(), "a partial ordered view must never be answered");
}

// ---- embedded children on the batch path ----

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct CardKey {
    pub id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct ContactKey {
    pub card_id: u64,
    pub kind: u8,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ContactKey)]
#[ok_ns(46)]
pub struct Contact {
    pub number: String,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CardKey)]
#[ok_ns(45)]
pub struct Card {
    pub kind: u8,
    pub contact: Ref<Contact, ContactKey>,
}

#[test]
fn embedded_children_are_written_and_released_in_the_batch() {
    let store = TestStore::slatedb_mem();
    let cards = <Card as Document>::collection(store.clone());
    let contacts: Collection<TestStore, ContactKey, Contact> = Collection::new(store.clone());
    let k = CardKey { id: 1 };
    let c1 = ContactKey { card_id: 1, kind: 1 };
    let c2 = ContactKey { card_id: 1, kind: 2 };

    let mut batch = okm_core::Batch::default();
    cards.save_with_prev_into(
        &mut batch,
        &k,
        &Card {
            kind: 1,
            contact: Ref::own(c1.clone(), Contact { number: "111".into() }),
        },
        None,
    ).unwrap();
    store.commit_batch(batch).expect("commit");
    assert!(contacts.get(&c1).is_some(), "child lands with the parent");

    // Overwrite pointing somewhere else: the old child is released in the
    // same batch, exactly as put's overwrite pass does.
    let old = cards.get(&k).expect("old card");
    let mut batch = okm_core::Batch::default();
    cards.save_with_prev_into(
        &mut batch,
        &k,
        &Card {
            kind: 2,
            contact: Ref::own(c2.clone(), Contact { number: "222".into() }),
        },
        Some(&old),
    ).unwrap();
    store.commit_batch(batch).expect("commit");

    assert!(contacts.get(&c1).is_none(), "the dropped child is released");
    assert_eq!(contacts.get(&c2).map(|c| c.number), Some("222".to_string()));
}

// ---------- vocabulary growth rides the batch (ADR-0032 × ADR-0031) ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct LogKey {
    pub id: u64,
}

/// The log-table shape from the modeling guide: a code column, two categorical
/// columns of different widths, free text.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LogKey)]
#[ok_ns(44)]
pub struct LogRow {
    pub ts: u64,
    pub level: u8,
    pub event: Vocab<u16>,
    pub status: Vocab<u8>,
    pub note: String,
}

#[test]
fn save_with_prev_into_interns_inside_the_batch() {
    let store = TestStore::slatedb_mem();
    let c = Collection::<_, LogKey, LogRow>::new(store.clone());
    let k = LogKey { id: 1 };

    let row = LogRow {
        ts: 1_700_000_000_000,
        level: 3,
        event: Vocab::of("http.request"),
        status: Vocab::of("warn"),
        note: "slow upstream".into(),
    };

    // Nothing is live before the commit — not the row, not its vocabulary. The
    // allocation's three writes (label → id, id → label, cursor) go through
    // BatchStore, so they ride the caller's batch.
    let mut batch = okm_core::Batch::default();
    c.save_with_prev_into(&mut batch, &k, &row, None).unwrap();
    assert!(c.get(&k).is_none(), "an uncommitted batch moves nothing");
    store.commit_batch(batch).expect("commit");

    // After the commit the row reads back with LABELS: the id → label entries
    // travelled in the same batch as the payload.
    let back = c.get(&k).expect("row landed");
    assert_eq!(back.event.as_str(), Some("http.request"));
    assert_eq!(back.status.as_str(), Some("warn"));
    assert_eq!(back.level, 3);
    assert_eq!(back.note, "slow upstream");

    // A second row, a second batch, two labels the store never saw: ids
    // continue from the first batch instead of restarting at 0.
    let mut batch = okm_core::Batch::default();
    c.save_with_prev_into(
        &mut batch,
        &LogKey { id: 2 },
        &LogRow {
            ts: 1_700_000_001_000,
            level: 5,
            event: Vocab::of("db.timeout"),
            status: Vocab::of("error"),
            note: "pool exhausted".into(),
        },
        None,
    ).unwrap();
    store.commit_batch(batch).expect("commit");

    let second = c.get(&LogKey { id: 2 }).expect("row landed");
    assert_eq!(second.event.as_str(), Some("db.timeout"));
    assert_eq!(second.status.as_str(), Some("error"));
    assert_ne!(
        second.event.id(),
        back.event.id(),
        "a new label took the next id in that field's vocabulary"
    );

    // And the first row still names both of its own labels.
    let again = c.get(&k).expect("row still there");
    assert_eq!(again.event.as_str(), Some("http.request"));
    assert_eq!(again.status.as_str(), Some("warn"));
}
