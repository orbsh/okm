//! The slatedb driver-thread shape (4.16b's root-cause fix): EVERY
//! sync VirtualStorage op must be callable from a thread WITH A TOKIO
//! CONTEXT ENTERED — the old runtime-held-and-block_on shape panicked
//! there ("Cannot start a runtime from within a runtime"), which is
//! exactly what aura's realm (spawn_blocking keeps the context) hit
//! when the injection face constructed and drove collections. This
//! locks the consumer-side contract, not just the happy path.
#![cfg(feature = "test-engines")]

use okm_core::TestStore;
use okm_core::storage::VirtualStorage;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn slatedb_sync_ops_run_inside_a_tokio_context() {
    // The async fn body runs WITH the runtime context entered — the
    // panic zone for the old shape. All four ops + the lazy iterator.
    let store = TestStore::slatedb_mem();
    let handle = store.shared_handle();
    store.put(b"a".to_vec(), b"1".to_vec());
    store.put(b"b".to_vec(), b"2".to_vec());
    assert_eq!(handle.get(b"a").as_deref(), Some(&b"1"[..]));
    assert_eq!(handle.scan_range(b"a", Some(b"c")).len(), 2);
    assert_eq!(handle.scan_suffix(b""), vec![b"a".to_vec(), b"b".to_vec()]);
    let items: Vec<_> = handle.scan_range_iter(b"", None).collect();
    assert_eq!(items.len(), 2);
    handle.del(b"a");
    assert_eq!(handle.get(b"a"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn slatedb_backwards_walk_drains_to_completion() {
    // next_back must BLOCK-drain the driver stream to its end sentinel
    // — a try_iter race would silently truncate (100 rows is plenty
    // for the driver to lag the consumer).
    let store = TestStore::slatedb_mem();
    for i in 0..100u32 {
        store.put(i.to_be_bytes().to_vec(), vec![0u8]);
    }
    let last_three: Vec<_> = store
        .scan_range_iter(&0u32.to_be_bytes(), None)
        .rev()
        .take(3)
        .collect();
    assert_eq!(last_three.len(), 3);
    assert_eq!(last_three[0].0, 99u32.to_be_bytes().to_vec());
    assert_eq!(last_three[2].0, 97u32.to_be_bytes().to_vec());
}
