//! Engine-matrix verification: every backend in `TestStore::matrix()`
//! (slatedb-mem / fjall / redb, per feature flags) must produce
//! identical behavior for the core operations — key round trips,
//! prefix scans, batch atomicity, SharedVirtualStorage sharing, and the
//! repositioning of a range scan (`ScanIter::seek`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::{DocumentEncode, KeyEncode, VirtualStorage};

fn verify_engine(store: TestStore) {
    let name = store.name();

    // Point ops.
    let s = store.clone();
    s.put(b"k1".to_vec(), b"v1".to_vec()).unwrap();
    s.put(b"k2".to_vec(), b"v2".to_vec()).unwrap();
    assert_eq!(
        s.get(b"k1").unwrap().as_deref(),
        Some(b"v1".as_slice()),
        "{name}: point get"
    );
    assert_eq!(s.get(b"missing").unwrap(), None, "{name}: absent key");

    // Prefix scan: suffixes in key order.
    let sfx = s.scan_suffix(b"k").expect("a scan on a working store");
    assert_eq!(
        sfx,
        vec![b"1".to_vec(), b"2".to_vec()],
        "{name}: scan_suffix"
    );

    // Handle sharing: a clone sees the same keyspace.
    let s2 = store.shared_handle();
    assert_eq!(
        s2.get(b"k1").unwrap().as_deref(),
        Some(b"v1".as_slice()),
        "{name}: shared handle"
    );

    // Batch atomicity: 100 ops in ONE commit_batch.
    let mut batch = okm_core::Batch::default();
    for i in 0..100u64 {
        batch.put(format!("batch:{i}").into_bytes(), b"ok".to_vec());
    }
    batch.del(b"k1");
    s.commit_batch(batch).expect("{name}: batch commit");
    assert_eq!(
        s.get(b"batch:50").unwrap().as_deref(),
        Some(b"ok".as_slice()),
        "{name}: batch write"
    );
    assert_eq!(s.get(b"k1").unwrap(), None, "{name}: batch delete");
    assert_eq!(
        s.scan_suffix(b"batch:")
            .expect("a scan on a working store")
            .len(),
        100,
        "{name}: all batch entries"
    );

    // Batch failure handling: empty batch commits clean.
    let empty = okm_core::Batch::default();
    s.commit_batch(empty).expect("{name}: empty batch");
}

#[test]
fn engine_matrix_core_ops() {
    for (name, store) in TestStore::matrix() {
        let _ = name;
        verify_engine(store);
    }
}

/// Typed Collection over each engine: the compose-key put/get path.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct MKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MKey)]
#[ok_ns(3)]
pub struct MRow {
    pub score: u32,
}

#[test]
fn engine_matrix_table_round_trip() {
    for (name, store) in TestStore::matrix() {
        let t: okm_core::Collection<TestStore, MKey, MRow> = okm_core::Collection::new(store);
        t.put(&MKey { id: 7 }, &MRow { score: 55 }).unwrap();
        let back = t
            .get_row(&MKey { id: 7 })
            .expect("a store read")
            .expect("{name}: document round trip");
        assert_eq!(back.score, 55, "{name}");
        let keys: Vec<MKey> = t
            .scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(keys, vec![MKey { id: 7 }], "{name}: scan_keys");
    }
}

// ============ ScanIter::seek ============
//
// One range `seek:` .. `seek;`, twenty entries `seek:00` .. `seek:19`. Every
// case runs on every engine in the matrix: the reposition has a different
// mechanism per backend (same-snapshot reopen, same-transaction reopen, buffer
// drain) and the contract is the one thing that must not differ.

use okm_core::engine::storage::ScanIter;

const SEEK_BEGIN: &[u8] = b"seek:";
const SEEK_END: &[u8] = b"seek;";

/// Write `n` entries `seek:00..` and answer their keys in scan order.
fn seed_seek_keys(store: &TestStore, n: u8) -> Vec<Vec<u8>> {
    let keys: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("seek:{i:02}").into_bytes())
        .collect();
    for (i, k) in keys.iter().enumerate() {
        store
            .put(k.clone(), format!("v{i}").into_bytes())
            .expect("a put on a working store");
    }
    keys
}

fn expect_item(
    item: Option<okm_core::error::Result<(Vec<u8>, Vec<u8>)>>,
    name: &str,
) -> (Vec<u8>, Vec<u8>) {
    item.unwrap_or_else(|| panic!("{name}: expected an item"))
        .unwrap_or_else(|e| panic!("{name}: a faulty scan item: {e}"))
}

fn drain(it: &mut ScanIter) -> Vec<(Vec<u8>, Vec<u8>)> {
    it.map(|r| r.expect("a scan item")).collect()
}

fn seek_to_existing_key() {
    for (name, store) in TestStore::matrix() {
        let keys = seed_seek_keys(&store, 10);
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(it.seek(&keys[3]), "{name}: seek to an existing key");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[3],
            "{name}: the seek target"
        );
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[4],
            "{name}: the entry after the seek target"
        );
    }
}

fn seek_between_entries() {
    for (name, store) in TestStore::matrix() {
        let keys = seed_seek_keys(&store, 10);

        // Between seek:03 and seek:04: the first key at or after it is seek:04.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(it.seek(b"seek:035"), "{name}: seek between two entries");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[4],
            "{name}: the next entry up"
        );

        // Shorter than the first entry: seek:0 sorts before seek:00.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(it.seek(b"seek:0"), "{name}: seek below the first key");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[0],
            "{name}: the first key"
        );
    }
}

fn seek_past_the_end_is_false() {
    for (name, store) in TestStore::matrix() {
        let _ = seed_seek_keys(&store, 10);

        // Past the last key in the range.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(!it.seek(b"seek:99"), "{name}: seek past the last key");
        assert!(it.next().is_none(), "{name}: nothing after a past-end seek");

        // At the exclusive end: nothing can live at or after it.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(!it.seek(SEEK_END), "{name}: seek to the exclusive end");
        assert!(it.next().is_none(), "{name}: nothing at the exclusive end");

        // The end bound still wins when the target exists outside it.
        let mut it = store.scan_range(SEEK_BEGIN, Some(b"seek:05"));
        assert!(!it.seek(b"seek:06"), "{name}: seek past the bounded end");
        assert!(it.next().is_none(), "{name}: nothing past the bounded end");
    }
}

fn seek_backwards_is_a_noop() {
    for (name, store) in TestStore::matrix() {
        let keys = seed_seek_keys(&store, 10);
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        for expected in &keys[..3] {
            assert_eq!(expect_item(it.next(), name).0, *expected, "{name}: warm-up");
        }

        // A lower bound behind the position keeps the position: the next item
        // is the one that would have arrived anyway.
        assert!(
            it.seek(&keys[0]),
            "{name}: backwards seek keeps the scan alive"
        );
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[3],
            "{name}: position kept"
        );

        // Same position (the key just handed out) is also a no-op.
        assert!(it.seek(&keys[2]), "{name}: seek to the current position");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[4],
            "{name}: position still kept"
        );

        // Below the range's own begin: still no-op, still forward.
        assert!(it.seek(SEEK_BEGIN), "{name}: seek below the range start");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[5],
            "{name}: position kept again"
        );

        // Once the forward walk is spent, a backwards seek answers "nothing
        // left" and hands out nothing.
        let rest = drain(&mut it);
        assert_eq!(rest.len(), 4, "{name}: the remaining entries");
        assert_eq!(rest[0].0, keys[6], "{name}: the drain resumed in place");
        assert!(
            !it.seek(&keys[0]),
            "{name}: backwards seek on an exhausted scan"
        );
        assert!(it.next().is_none(), "{name}: exhausted means empty");
    }
}

fn seek_then_next_back() {
    for (name, store) in TestStore::matrix() {
        let keys = seed_seek_keys(&store, 10);

        // A seek resets the back cursor to the range's end.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert!(it.seek(&keys[5]), "{name}: seek before walking back");
        assert_eq!(
            expect_item(it.next_back(), name).0,
            keys[9],
            "{name}: back from the top"
        );
        // The probed item survives a backward pull and comes out first.
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[5],
            "{name}: the seek target survives"
        );
        assert_eq!(
            expect_item(it.next_back(), name).0,
            keys[8],
            "{name}: back again"
        );

        // Both directions meet: 5, 6, 7 forward, 9, 8 backward.
        assert_eq!(expect_item(it.next(), name).0, keys[6], "{name}: forward");
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[7],
            "{name}: forward to the meet"
        );
        assert!(it.next().is_none(), "{name}: the front is spent");
        assert!(it.next_back().is_none(), "{name}: the back is spent");

        // A back pull, then a forward seek. The arms that reopen the range
        // start the back at the end again (fjall, redb, remote); a drain keeps
        // where the back already was (buffered, external). The claim that must
        // hold everywhere: the back never drops below the seek target, and the
        // front starts on it.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        assert_eq!(
            expect_item(it.next_back(), name).0,
            keys[9],
            "{name}: prime the back"
        );
        assert!(it.seek(&keys[3]), "{name}: seek after a back pull");
        let back = expect_item(it.next_back(), name).0;
        assert!(
            back.as_slice() >= keys[3].as_slice(),
            "{name}: the back stays at or after the seek target, got {back:?}"
        );
        assert_eq!(
            expect_item(it.next(), name).0,
            keys[3],
            "{name}: the front is at the target"
        );
    }
}

fn seek_answers_like_a_fresh_range() {
    for (name, store) in TestStore::matrix() {
        let keys = seed_seek_keys(&store, 20);

        let lowers: Vec<Vec<u8>> = vec![
            SEEK_BEGIN.to_vec(),  // the range start (a no-op)
            keys[7].clone(),      // an existing key
            b"seek:075".to_vec(), // between two entries
            b"seek:076".to_vec(), // just above an existing key
            b"seek:99".to_vec(),  // past the last entry
            SEEK_END.to_vec(),    // the exclusive end
        ];
        for lower in &lowers {
            let fresh = drain(&mut store.scan_range(lower, Some(SEEK_END)));
            let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
            let moved = it.seek(lower);
            let seeked = drain(&mut it);
            assert_eq!(
                seeked,
                fresh,
                "{name}: seek to {} answers the fresh range",
                String::from_utf8_lossy(lower)
            );
            assert_eq!(
                moved,
                !fresh.is_empty(),
                "{name}: seek to {} reports whether anything is left",
                String::from_utf8_lossy(lower)
            );
        }

        // A partly-consumed scan seeks to the same answer a fresh range gives:
        // already-consumed entries are not replayed.
        let mut it = store.scan_range(SEEK_BEGIN, Some(SEEK_END));
        for expected in &keys[..3] {
            assert_eq!(expect_item(it.next(), name).0, *expected, "{name}: warm-up");
        }
        assert!(it.seek(&keys[10]), "{name}: seek forward mid-scan");
        assert_eq!(
            drain(&mut it),
            drain(&mut store.scan_range(&keys[10], Some(SEEK_END))),
            "{name}: a mid-scan seek answers exactly the fresh tail"
        );
    }
}

/// The reposition contract, once per engine in the matrix. Each helper owns its
/// keyspace slice, so the shapes cannot lean on each other's position.
#[test]
fn engine_matrix_seek() {
    seek_to_existing_key();
    seek_between_entries();
    seek_past_the_end_is_false();
    seek_backwards_is_a_noop();
    seek_then_next_back();
    seek_answers_like_a_fresh_range();
}
