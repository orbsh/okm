//! The `VirtualStorage::scan_suffix` DEFAULT must honor its documented return
//! shape — the prefix removed — because an impl that provides only the raw ops
//! (a host-injected engine, a minimal test double) has nothing else to strip it
//! with. Forwarding `scan_range`'s FULL keys kept every tail-slicing caller
//! working (they slice from the end) while turning every LENGTH-based judgement
//! into a no-op: the probe/tail test read a suffix one prefix too long, so a
//! spill was never dropped and exact matching could never fire.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::storage::VirtualStorage;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Raw-ops-only engine: ordered keys, FULL keys out of `scan_range` — the
/// shape a host holds behind the binding's `Engine` face.
#[derive(Default)]
struct RawOnly {
    rows: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl VirtualStorage for RawOnly {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
        Ok(())
    }
    fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>> {
        Ok(self.rows.lock().unwrap().get(key).cloned())
    }
    fn del(&self, key: &[u8]) -> okm_core::error::Result<()> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        Ok(())
    }
    fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> okm_core::error::Result<Vec<Vec<u8>>> {
        let rows = self.rows.lock().unwrap();
        let start = begin.to_vec();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match end {
            Some(e) => Box::new(rows.range(start..e.to_vec())),
            None => Box::new(rows.range(start..)),
        };
        Ok(iter.map(|(k, _)| k.clone()).collect())
    }
}

const NS: &[u8] = &[0x00, 0x2A, 0x10, 0x01];

#[test]
fn default_scan_suffix_strips_the_prefix() {
    let s = RawOnly::default();
    s.put([NS, b"ad\x31\x00"].concat(), b"1".to_vec()).unwrap();
    s.put([NS, b"ad1to"].concat(), b"2".to_vec()).unwrap();
    s.put([NS, b"a"].concat(), b"3".to_vec()).unwrap();
    s.put(
        [&[0x00, 0x2A, 0x10, 0x02][..], b"ad"].concat(),
        b"4".to_vec(),
    )
    .unwrap();

    let got = s.scan_suffix(NS).unwrap();
    // Suffixes, never full keys: the length a probe/tail judgement reads is the
    // entry's remainder, not the whole key.
    assert_eq!(
        got,
        vec![b"a".to_vec(), b"ad\x31\x00".to_vec(), b"ad1to".to_vec()],
        "prefix removed, key order preserved"
    );
    assert!(
        got.iter().all(|sfx| !sfx.starts_with(NS)),
        "the [ns][slot] header must not ride back in a suffix"
    );
}

#[test]
fn default_scan_suffix_handles_the_prefix_itself() {
    // A key exactly equal to the prefix yields an EMPTY suffix (not the key),
    // and an all-0xFF prefix (whose `prefix_end` is `None`, i.e. an unbounded
    // range) still returns matching suffixes only.
    let s = RawOnly::default();
    s.put(NS.to_vec(), b"self".to_vec()).unwrap();
    s.put([NS, b"x"].concat(), b"child".to_vec()).unwrap();
    assert_eq!(
        s.scan_suffix(NS).unwrap(),
        vec![Vec::<u8>::new(), b"x".to_vec()],
        "the self key strips to empty"
    );

    let s = RawOnly::default();
    s.put(vec![0xFF, 0xFF, 0x01], b"a".to_vec()).unwrap();
    s.put(vec![0xFF, 0xFF], b"b".to_vec()).unwrap();
    assert_eq!(
        s.scan_suffix(&[0xFF, 0xFF]).unwrap(),
        vec![Vec::<u8>::new(), vec![0x01]],
        "unbounded range still strips"
    );
}
