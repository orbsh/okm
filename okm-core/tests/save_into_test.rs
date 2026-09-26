//! save_into / commit_batch — the cross-collection atomic path
//! (ADR-0003): a document collection and a junction encode into one batch,
//! one commit makes them live or die together. Covers both orderings
//! (nothing written before commit; everything written after).

use okm_core::{
    DocumentEncode, JunctionEncode, KeyEncode, Ref, VirtualStorage, TestStore, Collection,
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
    let mut store = TestStore::slatedb_mem();
    let mut batch = okm_core::MemBatch::default();
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
    let mut store = TestStore::slatedb_mem();
    let mut batch = okm_core::MemBatch::default();

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
