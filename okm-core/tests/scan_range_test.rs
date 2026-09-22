//! Range scan: `1 < a < 100` as a key interval — byte order == value
//! order, so the engine reads only rows inside the interval. Locks the
//! Collection::scan_range semantics (inclusive begin, exclusive end)
//! against the real engine matrix, through the DERIVE-generated access
//! method (no hand-written KvIndex).

use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct RowId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowId)]
#[ok_ns(21)]
#[ok_index(by_a { fields(a) })]
pub struct Row {
    pub a: u32,
    pub tag: u32,
}

#[test]
fn range_scan_reads_only_the_interval() {
    let store = TestStore::default();
    let mut col: Collection<_, RowId, Row> = Collection::new(store.clone());

    let rows: Vec<(u32, u32)> = (0..100).map(|i| (i, i * 7)).collect();
    for (id, (a, tag)) in rows.iter().enumerate() {
        let row = Row { a: *a, tag: *tag };
        col.put(&RowId { id: id as u64 }, &row);
    }

    // 1 <= a < 100 (u32 BE; value order == byte order).
    let hits = col.scan_range::<__OkmIndex_Row_by_a>(
        &1u32.to_be_bytes(),
        Some(&100u32.to_be_bytes()),
    );
    assert_eq!(hits.len(), 99); // a = 1..=99
    assert_eq!(hits.first().unwrap().0.decoded.id, 1);
    assert_eq!(hits.last().unwrap().0.decoded.id, 99);
    // In key order: a ascending.
    for w in hits.windows(2) {
        assert!(w[0].0.decoded.id < w[1].0.decoded.id);
    }
    // The documents ride along, fetched back.
    assert_eq!(hits[0].1.as_ref().unwrap().a, 1);

    // Unbounded end: a >= 50.
    let hits = col.scan_range::<__OkmIndex_Row_by_a>(&50u32.to_be_bytes(), None);
    assert_eq!(hits.len(), 50);

    // Empty interval: end <= begin.
    let hits = col.scan_range::<__OkmIndex_Row_by_a>(
        &100u32.to_be_bytes(),
        Some(&1u32.to_be_bytes()),
    );
    assert!(hits.is_empty());
}


#[cfg(feature = "fjall")]
#[test]
fn range_scan_fjall() {
    let store = TestStore::fjall_tmp();
    let mut col: Collection<_, RowId, Row> = Collection::new(store);
    for i in 0..50u32 {
        col.put(&RowId { id: i as u64 }, &Row { a: i, tag: 0 });
    }
    let hits = col.scan_range::<__OkmIndex_Row_by_a>(&10u32.to_be_bytes(), Some(&20u32.to_be_bytes()));
    assert_eq!(hits.len(), 10);
}

#[cfg(feature = "redb")]
#[test]
fn range_scan_redb() {
    let store = TestStore::redb_tmp();
    let mut col: Collection<_, RowId, Row> = Collection::new(store);
    for i in 0..50u32 {
        col.put(&RowId { id: i as u64 }, &Row { a: i, tag: 0 });
    }
    let hits = col.scan_range::<__OkmIndex_Row_by_a>(&10u32.to_be_bytes(), Some(&20u32.to_be_bytes()));
    assert_eq!(hits.len(), 10);
}

/// Lazy iteration: the same bounds as `scan_range`, entries pulled on
/// demand — documents fetched per pulled entry. Locks the streaming
/// contract (`ScanIter`, opaque enum, DoubleEnded) against the engine
/// matrix.
#[test]
fn lazy_range_scan_matches_buffered() {
    let store = TestStore::default();
    let mut col: Collection<_, RowId, Row> = Collection::new(store.clone());
    for i in 0..100u32 {
        col.put(&RowId { id: i as u64 }, &Row { a: i, tag: i * 7 });
    }

    let lazy: Vec<_> = col
        .scan_range_iter::<__OkmIndex_Row_by_a>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()))
        .collect();
    assert_eq!(lazy.len(), 99);
    assert_eq!(lazy[0].1.as_ref().unwrap().a, 1);
    assert_eq!(lazy.last().unwrap().1.as_ref().unwrap().a, 99);

    // Backwards walk (rev() requires DoubleEndedIterator) — "last N
    // entries" without buffering the forward pass.
    let last_three: Vec<_> = col
        .scan_range_iter::<__OkmIndex_Row_by_a>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()))
        .rev()
        .take(3)
        .collect();
    assert_eq!(last_three.len(), 3);
    assert_eq!(last_three[0].1.as_ref().unwrap().a, 99);
    assert_eq!(last_three[2].1.as_ref().unwrap().a, 97);

    // Early abandon costs only the pulled entries — lazy by construction
    // (take before collect); just lock the semantics.
    let first_two: Vec<_> = col
        .scan_range_iter::<__OkmIndex_Row_by_a>(&1u32.to_be_bytes(), None)
        .take(2)
        .collect();
    assert_eq!(first_two.len(), 2);
    assert_eq!(first_two[1].1.as_ref().unwrap().a, 2);
}

#[cfg(feature = "fjall")]
#[test]
fn lazy_range_scan_fjall_native_double_ended() {
    let store = TestStore::fjall_tmp();
    let mut col: Collection<_, RowId, Row> = Collection::new(store);
    for i in 0..50u32 {
        col.put(&RowId { id: i as u64 }, &Row { a: i, tag: 0 });
    }
    let fwd: Vec<(u64, u32)> = col
        .scan_range_iter::<__OkmIndex_Row_by_a>(&10u32.to_be_bytes(), Some(&20u32.to_be_bytes()))
        .map(|(k, d)| (k.decoded.id, d.unwrap().a))
        .collect();
    let mut bwd: Vec<(u64, u32)> = col
        .scan_range_iter::<__OkmIndex_Row_by_a>(&10u32.to_be_bytes(), Some(&20u32.to_be_bytes()))
        .rev()
        .map(|(k, d)| (k.decoded.id, d.unwrap().a))
        .collect();
    bwd.reverse();
    assert_eq!(fwd, bwd);
}
