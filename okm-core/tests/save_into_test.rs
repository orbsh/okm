//! save_into / commit_batch — the cross-collection atomic path
//! (ADR-0003): a row table and an edge table encode into one batch, one
//! commit makes them live or die together. Covers both orderings (nothing
//! written before commit; everything written after).

use okm_core::{EdgeEncode, EdgeTable, KeyEncode, VirtualStorage, MockStore, RowEncode, Table};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PostKey {
    pub id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(PostKey)]
#[kv_index(by_author { fields(author_id) })]
#[kv_ns(41)]
pub struct Post {
    pub author_id: u64,
    pub title: String,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AuthorKey {
    pub id: u64,
}

#[derive(EdgeEncode, Clone)]
#[kv_ns(42)]
pub struct AuthorEdge {
    #[kv_head(id)]
    pub author: AuthorKey,
    pub post: PostKey,
}

#[test]
fn save_into_defers_until_commit() {
    let mut store = MockStore::default();
    let mut batch = store.batch();
    let t: Table<MockStore, PostKey, Post> = Table::new(store.clone());

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
    let t2: Table<MockStore, PostKey, Post> = Table::new(store.clone());
    assert!(t2.get(&PostKey { id: 1 }).is_some(), "row lands at commit");
}

#[test]
fn cross_collection_one_batch() {
    // Row table + edge table share one batch: both live or neither does.
    let mut store = MockStore::default();
    let mut batch = store.batch();

    {
        let t: Table<MockStore, PostKey, Post> = Table::new(store.clone());
        t.save_into(
            &mut batch,
            &PostKey { id: 1 },
            &Post { author_id: 7, title: "hi".into() },
        );
        let edges: EdgeTable<MockStore, AuthorEdge> = EdgeTable::new(store.clone());
        edges.save_into(&mut batch, &AuthorKey { id: 7 }, &PostKey { id: 1 });
    }

    store.commit_batch(batch).expect("commit");

    // Both landed through the ONE committed store instance.
    let t: Table<MockStore, PostKey, Post> = Table::new(store.clone());
    let scanned = t.scan::<Post_ByAuthor>(&7u64.to_be_bytes());
    assert_eq!(scanned.len(), 1);
    let edges: EdgeTable<MockStore, AuthorEdge> = EdgeTable::new(store);
    let fwd = edges.forward(&AuthorKey { id: 7 });
    assert_eq!(fwd.len(), 1);
    assert_eq!(fwd[0], PostKey { id: 1 });
}

use __OkmIndex_Post_by_author as Post_ByAuthor;
