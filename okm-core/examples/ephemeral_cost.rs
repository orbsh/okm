//! What a temporary realm's keyspace costs in both layouts, measured.
//!
//! `BTreeMap<Vec<u8>, Vec<u8>>` is what the engine used to be; `EphemeralStore`
//! is what it is (ADR-0071). Heap bytes come from a counting allocator, so the
//! number is allocation, not RSS.
//!
//! `cargo run --release --example ephemeral_cost -p okm-core -- [rows]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use okm_core::engine::ephemeral::EphemeralStore;
use okm_core::engine::storage::{SharedVirtualStorage, VirtualStorage};

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            CUR.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        CUR.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                CUR.fetch_add(new - l.size(), Ordering::Relaxed);
            } else {
                CUR.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;
fn heap() -> usize {
    CUR.load(Ordering::Relaxed)
}

/// An example that cannot write its numbers has nothing to say, so a failure
/// stops it instead of being printed as a zero.
fn must<T>(r: okm_core::error::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ephemeral_cost: {e}");
            std::process::exit(1);
        }
    }
}

struct Entry {
    key: Vec<u8>,
    val: Vec<u8>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Six families, the shapes an aura realm's okm keyspace actually has.
fn families(rows: u64) -> Vec<(&'static str, Vec<Entry>)> {
    let mut rng = Rng(0x0BAD_C0DE);
    let ns: u16 = 3;
    let mut primary = Vec::new();
    let mut uuid7 = Vec::new();
    let mut by_org = Vec::new();
    let mut by_org_status = Vec::new();
    let mut covered = Vec::new();
    let mut dynamic = Vec::new();

    for i in 0..rows {
        let org = if i % 10 == 0 { rng.below(19) + 1 } else { 0 } as u32;
        let status = rng.below(5) as u8;

        let mut k = Vec::with_capacity(11);
        k.extend_from_slice(&ns.to_be_bytes());
        k.push(0x00);
        k.extend_from_slice(&i.to_be_bytes());
        primary.push(Entry {
            key: k,
            val: vec![i as u8; 120],
        });

        let mut u = [0u8; 16];
        u[..6].copy_from_slice(&(0x0197_0000_0000u64 + i).to_be_bytes()[2..]);
        u[6..14].copy_from_slice(&rng.next().to_be_bytes());
        u[14..].copy_from_slice(&(rng.next() as u16).to_be_bytes());
        let mut k = Vec::with_capacity(19);
        k.extend_from_slice(&ns.to_be_bytes());
        k.push(0x00);
        k.extend_from_slice(&u);
        uuid7.push(Entry {
            key: k,
            val: vec![i as u8; 120],
        });

        let mut k = Vec::with_capacity(16);
        k.extend_from_slice(&ns.to_be_bytes());
        k.extend_from_slice(&[0x40, 0x00]);
        k.extend_from_slice(&org.to_be_bytes());
        k.extend_from_slice(&i.to_be_bytes());
        by_org.push(Entry {
            key: k,
            val: Vec::new(),
        });

        let mut k = Vec::with_capacity(17);
        k.extend_from_slice(&ns.to_be_bytes());
        k.extend_from_slice(&[0x40, 0x01]);
        k.extend_from_slice(&org.to_be_bytes());
        k.push(status);
        k.extend_from_slice(&i.to_be_bytes());
        by_org_status.push(Entry {
            key: k,
            val: Vec::new(),
        });

        let mut k = Vec::with_capacity(13);
        k.extend_from_slice(&ns.to_be_bytes());
        k.extend_from_slice(&[0x40, 0x02]);
        k.push(status);
        k.extend_from_slice(&i.to_be_bytes());
        covered.push(Entry {
            key: k,
            val: i.to_be_bytes().to_vec(),
        });

        let mut k = Vec::with_capacity(48);
        k.extend_from_slice(&ns.to_be_bytes());
        k.push(0x01);
        k.extend_from_slice(format!("meta.scratch.item{:03}.", i % 97).as_bytes());
        k.push(0x00);
        k.extend_from_slice(&i.to_be_bytes());
        dynamic.push(Entry {
            key: k,
            val: vec![0xAB; 60],
        });
    }

    vec![
        ("primary row (u64, 120 B doc)", primary),
        ("uuid7 row (16 B id, 120 B)", uuid7),
        ("index by_org (empty value)", by_org),
        ("index by_org_status", by_org_status),
        ("covered index (8 B value)", covered),
        ("dynamic path (60 B value)", dynamic),
    ]
}

fn main() {
    let rows = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40_000u64);
    println!("rows per family: {rows} (heap bytes from a counting allocator)\n");
    println!(
        "{:<31} {:>6} {:>9} {:>9} {:>7} {:>9} {:>9} {:>9} {:>9}",
        "family", "keys", "map B/e", "eph B/e", "ratio", "map put", "eph put", "map get", "eph get"
    );

    for (name, c) in families(rows) {
        let n = c.len();

        let before = heap();
        let mut m: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let t = Instant::now();
        for e in &c {
            m.insert(e.key.clone(), e.val.clone());
        }
        let map_put = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        let map_bytes = heap() - before;

        let before = heap();
        let store = EphemeralStore::new();
        let t = Instant::now();
        for e in &c {
            must(store.put(e.key.clone(), e.val.clone()));
        }
        let eph_put = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        let eph_bytes = heap() - before;

        let mut rng = Rng(7);
        let mut hits = 0usize;
        let t = Instant::now();
        for _ in 0..100_000 {
            let e = &c[rng.below(n as u64) as usize];
            hits += m.get(e.key.as_slice()).map(|v| v.len()).unwrap_or(0);
        }
        let map_get = t.elapsed().as_secs_f64() * 1e9 / 100_000.0;
        let t = Instant::now();
        for _ in 0..100_000 {
            let e = &c[rng.below(n as u64) as usize];
            hits += must(store.get(&e.key)).map(|v| v.len()).unwrap_or(0);
        }
        let eph_get = t.elapsed().as_secs_f64() * 1e9 / 100_000.0;

        // Both layouts must answer the same scan, and the store must not have
        // lost anything on the way in.
        let mut scanned = 0usize;
        for item in store.scan_range(b"", None) {
            let (k, v) = must(item);
            assert_eq!(must(store.get(&k)).as_deref(), Some(v.as_slice()));
            scanned += 1;
        }
        assert_eq!(scanned, n, "{name}: the scan lost entries");
        assert_eq!(m.len(), n);
        let _ = hits;

        println!(
            "{:<31} {:>6} {:>9.0} {:>9.0} {:>6.2}x {:>9.0} {:>9.0} {:>9.0} {:>9.0}",
            name,
            n,
            map_bytes as f64 / n as f64,
            eph_bytes as f64 / n as f64,
            map_bytes as f64 / eph_bytes as f64,
            map_put,
            eph_put,
            map_get,
            eph_get
        );
        println!(
            "{:<31}                (store.bytes() = {:.0} B/e, allocator sees {:.0} B/e)",
            "",
            store.bytes() as f64 / n as f64,
            eph_bytes as f64 / n as f64
        );
        drop(m);
    }

    // The whole path a host takes: build a realm's worth, scan it, walk it both
    // ways, then drop the store and see the bytes come back. The map's two
    // numbers are the shape the OLD engine had — `BTreeMap` behind a mutex that
    // materialized a range as `Vec<(Vec<u8>, Vec<u8>)>` before handing it over.
    let all: Vec<Entry> = families(5_000).into_iter().flat_map(|(_, v)| v).collect();
    let t = Instant::now();
    let mut map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for e in &all {
        map.insert(e.key.clone(), e.val.clone());
    }
    let map_build = t.elapsed().as_secs_f64() * 1e6;
    let t = Instant::now();
    let map_scan = map
        .range(Vec::new()..)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<Vec<_>>();
    let map_scan_us = t.elapsed().as_secs_f64() * 1e6;
    drop(map);

    let before = heap();
    let store = EphemeralStore::new();
    let t = Instant::now();
    for e in &all {
        must(store.put(e.key.clone(), e.val.clone()));
    }
    let build = t.elapsed().as_secs_f64() * 1e6;
    let held = heap() - before;
    let t = Instant::now();
    let fwd = must(
        store
            .scan_range(b"", None)
            .collect::<okm_core::error::Result<Vec<_>>>(),
    )
    .len();
    let fwd_us = t.elapsed().as_secs_f64() * 1e6;
    assert_eq!(map_scan.len(), fwd);
    let t = Instant::now();
    let bwd = must(
        store
            .scan_range(b"", None)
            .rev()
            .collect::<okm_core::error::Result<Vec<_>>>(),
    )
    .len();
    let bwd_us = t.elapsed().as_secs_f64() * 1e6;
    assert_eq!(fwd, all.len());
    assert_eq!(bwd, all.len());
    let h = store.shared_handle();
    drop(store);
    assert_eq!(h.len(), all.len(), "a shared handle outlived its store");
    drop(h);
    let after = heap();
    println!("\nrealm of {} entries", all.len());
    println!(
        "  build   map {:.2} ms   ephemeral {:.2} ms",
        map_build / 1000.0,
        build / 1000.0
    );
    println!(
        "  scan    map materialized {:.1} µs   ephemeral lazy {:.1} µs fwd / {:.1} µs rev",
        map_scan_us, fwd_us, bwd_us
    );
    println!(
        "  held    ephemeral {:.2} MiB (the map's ranges had no peak to report: the whole \
         result set was the peak)",
        held as f64 / (1 << 20) as f64
    );
    println!(
        "after the last handle dropped: {} bytes still charged to the store",
        after.saturating_sub(before)
    );
}
