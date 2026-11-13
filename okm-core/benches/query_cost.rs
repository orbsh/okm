//! Where a query's time goes: engine read vs fetch vs materialisation.
//!
//! The schema-aware-index proposal (`docs/ideas/schema-aware-index.md`) claims
//! 3-100x from replacing the index layer. That can only be true if the engine's
//! block/index read is a large share of a query's end-to-end time, so this bench
//! splits one realistic corpus into arms whose difference attributes one layer
//! each:
//!
//! | arm | pays for |
//! |---|---|
//! | `RawStore::get` | one engine point read, no document layer |
//! | `get_row` | that read + key encode + payload decode |
//! | `scan_exact(..).parts::<(Key,)>()` | engine range read + entry-key decode, no fetch, no payload decode |
//! | `scan_exact(..)` default parts | the same + one `get_row` per entry |
//! | `.collect()` vs draining | the `Vec` and its growth |
//! | `OrderedSets::intersect` + fetch loop | two entry streams + one fetch per survivor |
//! | `seek` vs `next` | what a reposition is worth on this backend |
//!
//! No mocks: a real `fjall` store in a temp dir under the target dir, seeded once
//! and reused. Numbers are warm-cache (the whole corpus fits in the page cache on
//! the machine this was written for); cold-cache I/O is explicitly NOT measured.
//!
//! Run:
//! `cargo bench -p okm-core --features fjall --bench query_cost`
//! or one group: `... --bench query_cost -- point`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use criterion::{BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::time::Duration;

use okm_core::engine::fjall_backend::FjallStore;
use okm_core::engine::storage::VirtualStorage;
use okm_core::{Collection, DocumentEncode, KeyEncode, KvIndex, parts};
use okm_query::OrderedSets;

// ---------- the corpus ----------
//
// 200 000 rows, one fixed-width key (4B tenant + 8B row id = 12B) and two
// fixed-width index segments. Tenant 1 holds 90% (every row whose ordinal is not
// a multiple of 10); the other 20 000 rows are spread over tenants 2..=21, one
// row in ten, i.e. INTERLEAVED with the big tenant's rows rather than contiguous
// — so the fetch-back reads of a small tenant are scattered over the primary
// table, which is the honest shape for a multi-tenant collection.

const ROWS: u64 = 200_000;
const BIG_TENANT: u32 = 1;
const SCATTERED: u64 = 20;
/// The tenant every scan case probes: exactly 1 000 rows.
const PROBE_ORG: u32 = 2;
/// The status the join's second stage filters on (~125 of the 1 000 rows).
const PROBE_STATUS: u8 = 3;
/// A tenant id no row carries — the "prefix exists nowhere" case.
const ABSENT_ORG: u32 = 9_999;
/// A status no row carries — with a tenant that does exist, this is "prefix
/// exists (the tenant), zero entries match the full segment".
const ABSENT_STATUS: u8 = 200;

fn org_of(i: u64) -> u32 {
    if !i.is_multiple_of(10) {
        BIG_TENANT
    } else {
        2 + ((i / 10) % SCATTERED) as u32
    }
}

/// Three bits of a multiplicative hash: independent of the tenant assignment, so
/// a small tenant still sees every status.
fn status_of(i: u64) -> u8 {
    ((i.wrapping_mul(2_654_435_761) >> 29) & 7) as u8
}

#[derive(KeyEncode, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct CtKey {
    pub org_id: u32, // 4B  — leading, low cardinality (21 tenants)
    pub row_id: u64, // 8B  — trailing, high cardinality (200 000 distinct)
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(CtKey)]
#[ok_ns(11)]
#[ok_index(by_org { fields(org_id) })]
#[ok_index(by_org_status { fields(org_id, status) })]
// The covered twin of `by_org`: same segment, `amount` carried in the entry, so
// the same answer needs no fetch at all. It is here because the fetch stage is
// what the numbers say dominates — and this is the existing mechanism that
// removes it, priced against the same corpus.
#[ok_index(by_org_covered { fields(org_id), includes(amount) })]
pub struct CtRow {
    // Mirrored into the payload because an index can only name payload fields.
    pub org_id: u32,
    pub status: u8,
    pub amount: u64,
    pub label: String,
}

use __OkmIndex_CtRow_by_org as ByOrg;
use __OkmIndex_CtRow_by_org_covered as ByOrgCovered;
use __OkmIndex_CtRow_by_org_status as ByOrgStatus;

fn key_of(i: u64) -> CtKey {
    CtKey {
        org_id: org_of(i),
        row_id: i,
    }
}

fn row_of(i: u64) -> CtRow {
    CtRow {
        org_id: org_of(i),
        status: status_of(i),
        amount: i.wrapping_mul(1_000_003),
        label: format!("order-{i}"),
    }
}

fn org_probe(org: u32) -> [u8; 4] {
    org.to_be_bytes()
}

fn org_status_probe(org: u32, status: u8) -> [u8; 5] {
    let mut b = [0u8; 5];
    b[..4].copy_from_slice(&org.to_be_bytes());
    b[4] = status;
    b
}

// ---------- one seeded store, shared by every group ----------

type Col = Collection<FjallStore, CtKey, CtRow>;

struct Fixture {
    // Declared first: the store must be dropped before the directory.
    col: Col,
    ks: fjall::Keyspace,
    db: fjall::Database,
    dir: tempfile::TempDir,
    /// Entry keys of the 1 000-entry probe, ascending: what the seek case targets.
    probe_keys: Vec<Vec<u8>>,
    /// The single row the point-read case uses (a scattered tenant's row).
    point_key: CtKey,
    /// Its PRIMARY key bytes, pre-encoded so the raw arm pays no document layer.
    point_pk: Vec<u8>,
    join_survivors: usize,
}

fn dir_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

impl Fixture {
    fn seed() -> Self {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("temp dir");
        // The Database and Keyspace handles are kept so the memtable can be
        // SEALED before measuring: until it is, every read is served from the
        // in-memory memtable and no SSTable block index is exercised at all —
        // which is the structure the proposal is about.
        let db = fjall::Database::create_or_recover(fjall::Config::new(dir.path()))
            .expect("open fjall db");
        let ks = db
            .keyspace("query-cost", fjall::KeyspaceCreateOptions::default)
            .expect("keyspace");
        let store = FjallStore::from_db(db.clone(), "query-cost").expect("open fjall");
        let col: Col = Collection::new(store);

        let t0 = std::time::Instant::now();
        for i in 0..ROWS {
            col.put(&key_of(i), &row_of(i)).expect("seed");
        }
        let seed = t0.elapsed();
        // One journal write per batch is not enough: the rows are in the WAL and
        // the memtable. Rotate the memtable and wait for the flush, so the corpus
        // lives in SSTables with a real block index, bloom filter and separators.
        col.store().persist().ok();
        ks.rotate_memtable_and_wait().expect("seal memtable");
        col.store().persist().ok();
        // The flush leaves L0 tables behind and the worker merges them; a merge
        // landing inside a sample is exactly the noise this bench must not have.
        // Wait until nothing is running and the completed counter has stopped.
        let mut last = db.compactions_completed();
        let mut stable = 0u32;
        for _ in 0..600 {
            std::thread::sleep(Duration::from_millis(50));
            let now = db.compactions_completed();
            if db.active_compactions() == 0 && now == last {
                stable += 1;
                if stable >= 10 {
                    break;
                }
            } else {
                stable = 0;
            }
            last = now;
        }

        let mut f = Fixture {
            col,
            ks,
            db,
            dir,
            probe_keys: Vec::new(),
            point_key: key_of(10), // ordinal 10: tenant 3, scattered
            point_pk: Vec::new(),
            join_survivors: 0,
        };
        f.point_pk = f.col.primary_key(&f.point_key);
        f.probe_keys = f
            .col
            .seek_scan::<ByOrg>(&org_probe(PROBE_ORG))
            .take(1_000)
            .collect();
        f.join_survivors = join_streams(&f.col, PROBE_STATUS).count();

        // Warm every page the measured cases touch, and check the shapes the doc
        // claims (a silent zero would make every delta below meaningless).
        let probe_rows = f
            .col
            .scan_exact::<ByOrg>(&org_probe(PROBE_ORG))
            .parts::<()>()
            .count();
        let absent_rows = f
            .col
            .scan_exact::<ByOrg>(&org_probe(ABSENT_ORG))
            .parts::<()>()
            .count();
        let absent_status = f
            .col
            .scan_exact::<ByOrgStatus>(&org_status_probe(PROBE_ORG, ABSENT_STATUS))
            .parts::<()>()
            .count();
        assert_eq!(probe_rows, 1_000, "the probed tenant's fanout");
        assert_eq!(absent_rows, 0, "an absent tenant has no entry");
        assert_eq!(absent_status, 0, "an absent suffix has no entry");
        assert_eq!(f.probe_keys.len(), 1_000);
        assert!(f.join_survivors > 0);

        println!("--- corpus ---");
        println!("rows:                 {ROWS}");
        println!(
            "tenants:              #{BIG_TENANT} holds {} rows ({:.1}%), {SCATTERED} more hold {:.0} each",
            (0..ROWS).filter(|i| org_of(*i) == BIG_TENANT).count(),
            100.0 * (0..ROWS).filter(|i| org_of(*i) == BIG_TENANT).count() as f64 / ROWS as f64,
            (ROWS as f64 * 0.1) / SCATTERED as f64,
        );
        println!(
            "key:                  {} bytes (4B org + 8B row id)",
            CtKey::KEY_LEN
        );
        println!("indexes:              by_org (4B segment), by_org_status (5B segment)");
        println!("probe #{PROBE_ORG}:           {probe_rows} entries");
        println!("join survivors:       {}", f.join_survivors);
        println!("seed time:            {seed:?}");
        // What the store actually holds: the journal is a fixed allocation, so
        // the file list says more than the total does. SSTables live in a
        // subdirectory, hence the walk.
        let mut files: Vec<(u64, String)> = Vec::new();
        let mut stack = vec![f.dir.path().to_path_buf()];
        while let Some(p) = stack.pop() {
            for e in std::fs::read_dir(&p).unwrap().flatten() {
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    stack.push(e.path());
                } else {
                    let rel = e
                        .path()
                        .strip_prefix(f.dir.path())
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    files.push((md.len(), rel));
                }
            }
        }
        files.sort_unstable_by_key(|f| std::cmp::Reverse(f.0));
        for (len, name) in files.iter().take(6) {
            println!(
                "  file {name:24} {:>10.1} MiB",
                *len as f64 / (1024.0 * 1024.0)
            );
        }
        println!(
            "store total:          {} MiB",
            dir_bytes(f.dir.path()) / (1024 * 1024)
        );
        println!(
            "sealed memtables:     {} (0 = every read goes to an SSTable)",
            f.ks.sealed_memtable_count()
        );
        println!(
            "compactions:          {} completed, {} running",
            f.db.compactions_completed(),
            f.db.active_compactions()
        );
        // One entry, byte for byte: the key is the ns+slot header, the 4B
        // segment and the 12B key tail; the value is empty (no `includes`).
        let first = f.probe_keys[0].clone();
        let mut past_first = first.clone();
        past_first.push(0xFF);
        let entry = f
            .col
            .store()
            .scan_range(&first, Some(past_first.as_slice()))
            .next();
        if let Some(Ok((k, v))) = entry {
            println!(
                "entry key:            {} bytes, entry value: {} bytes",
                k.len(),
                v.len()
            );
        }
        f
    }
}

/// The two-stage join: two entry-key streams, aligned by the set operator, and
/// the caller fetches the rows of the survivors.
fn join_streams<'a>(col: &'a Col, status: u8) -> impl Iterator<Item = CtKey> + 'a {
    let left = col
        .scan_exact::<ByOrg>(&org_probe(PROBE_ORG))
        .parts::<(parts::Key,)>()
        .map(|r| r.unwrap().0.decoded);
    let right = col
        .scan_exact::<ByOrgStatus>(&org_status_probe(PROBE_ORG, status))
        .parts::<(parts::Key,)>()
        .map(|r| r.unwrap().0.decoded);
    left.intersect(right)
}

// ---------- case 1: one point read, with and without the document layer ----------

fn point(c: &mut Criterion, f: &Fixture) {
    let mut g = c.benchmark_group("point");
    g.sample_size(100);
    g.measurement_time(Duration::from_secs(3));
    g.bench_function("store_get_raw", |b| {
        b.iter(|| black_box(f.col.store().get(black_box(&f.point_pk)).unwrap()))
    });
    g.bench_function("get_row", |b| {
        b.iter(|| black_box(f.col.get_row(black_box(&f.point_key)).unwrap()))
    });
    // The key encode `get_row` pays and the pre-encoded arm does not: it is part
    // of the delta above and it is measured here so the split is visible.
    g.bench_function("primary_key_encode", |b| {
        b.iter(|| black_box(f.col.primary_key(black_box(&f.point_key))))
    });
    // The other half of the document layer, alone: the payload's TLV decode with
    // the key encode and the engine read both out of the picture.
    let payload = f.col.get_row(&f.point_key).unwrap().unwrap();
    let bytes = okm_core::Document::encode_payload(&payload);
    g.bench_function("decode_payload", |b| {
        b.iter(|| {
            black_box(<CtRow as okm_core::Document>::decode_payload(black_box(
                &bytes,
            )))
        })
    });
    g.finish();
}

// ---------- cases 2-4: one 1 000-entry probe, three payloads ----------

fn scan(c: &mut Criterion, f: &Fixture) {
    let probe = org_probe(PROBE_ORG);
    // The same bound the door builds: the entry prefix, and its exclusive end.
    let mut begin = ByOrg::entry_prefix(&f.col.header(), &[]);
    begin.extend_from_slice(&probe);
    let end = okm_core::engine::storage::prefix_end(&begin);

    let mut g = c.benchmark_group("scan-1000");
    g.sample_size(50);
    g.measurement_time(Duration::from_secs(5));
    g.warm_up_time(Duration::from_secs(2));
    g.throughput(Throughput::Elements(1_000));

    // The engine arm proper: the keyspace's own range iterator over the entries,
    // below OKM's `Scan` (no RawHit, no bound check, no part decode). Its item is
    // still an adapter allocation (key + value bytes per entry).
    g.bench_function("engine_range_pairs", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .store()
                    .scan_range(black_box(&begin), end.as_deref())
                    .count(),
            )
        })
    });

    // 2. Engine read + entry-key decode. No fetch, no payload decode.
    g.bench_function("engine_only_key", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .scan_exact::<ByOrg>(black_box(&probe))
                    .parts::<(parts::Key,)>()
                    .count(),
            )
        })
    });

    // 2b. The door + the entry split, asking for no part at all: the delta
    //     against `engine_range_pairs` is `Scan`'s own per-entry copy.
    g.bench_function("engine_only_nothing", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .scan_exact::<ByOrg>(black_box(&probe))
                    .parts::<()>()
                    .count(),
            )
        })
    });

    // 3. The default payload, drained: each entry is fetched and decoded.
    g.bench_function("plus_fetch_drained", |b| {
        b.iter(|| {
            let mut n = 0u64;
            for hit in f.col.scan_exact::<ByOrg>(black_box(&probe)) {
                let (_fields, _key, row) = hit.unwrap();
                n += row.map(|r| r.amount).unwrap_or(0);
            }
            black_box(n)
        })
    });

    // 3b. The fetch stage alone, with neither Fields nor Key in the tuple: the
    //     delta against `plus_fetch_drained` is the hit's other parts.
    g.bench_function("row_only_drained", |b| {
        b.iter(|| {
            let mut n = 0u64;
            for r in f
                .col
                .scan_exact::<ByOrg>(black_box(&probe))
                .parts::<(parts::Row,)>()
            {
                n += r.unwrap().0.map(|r| r.amount).unwrap_or(0);
            }
            black_box(n)
        })
    });

    // 4. The same, materialised: the delta against `plus_fetch_drained` is the
    //    Vec, its growth and its destruction.
    g.bench_function("plus_fetch_collected", |b| {
        b.iter(|| {
            let v: Vec<_> = f
                .col
                .scan_exact::<ByOrg>(black_box(&probe))
                .collect::<okm_core::error::Result<Vec<_>>>()
                .unwrap();
            black_box(
                v.iter()
                    .map(|(_, _, r)| r.as_ref().map(|r| r.amount).unwrap_or(0))
                    .sum::<u64>(),
            )
        })
    });
    // The covered arm: the same segment, the projected value carried in the
    // entry, no fetch at all. This is the existing mechanism the fetch-stage
    // numbers point at, priced on the same corpus.
    g.bench_function("covered_include", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .scan_exact::<ByOrgCovered>(black_box(&probe))
                    .parts::<(parts::Include,)>()
                    .filter(|r| r.is_ok())
                    .count(),
            )
        })
    });
    g.finish();
}

// ---------- case 5: two stages, joined ----------

fn join(c: &mut Criterion, f: &Fixture) {
    let mut g = c.benchmark_group("join");
    g.sample_size(80);
    g.measurement_time(Duration::from_secs(5));
    g.warm_up_time(Duration::from_secs(2));

    g.bench_function("intersect_only", |b| {
        b.iter(|| black_box(join_streams(&f.col, PROBE_STATUS).count()))
    });

    // The fetch stage: one `get_row` per survivor, which is what the caller's
    // loop does after the set operator hands it the key.
    g.bench_function("intersect_plus_fetch", |b| {
        b.iter(|| {
            let mut acc = 0u64;
            for k in join_streams(&f.col, PROBE_STATUS) {
                acc += f.col.get_row(&k).unwrap().map(|r| r.amount).unwrap_or(0);
            }
            black_box(acc)
        })
    });
    g.finish();
}

// ---------- case 6: the probe that answers nothing ----------

fn empty(c: &mut Criterion, f: &Fixture) {
    let absent = org_probe(ABSENT_ORG);
    let absent_suffix = org_status_probe(PROBE_ORG, ABSENT_STATUS);
    let mut g = c.benchmark_group("empty-probe");
    g.sample_size(100);
    g.measurement_time(Duration::from_secs(3));

    g.bench_function("absent_tenant", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .scan_exact::<ByOrg>(black_box(&absent))
                    .parts::<()>()
                    .count(),
            )
        })
    });
    g.bench_function("existing_tenant_absent_suffix", |b| {
        b.iter(|| {
            black_box(
                f.col
                    .scan_exact::<ByOrgStatus>(black_box(&absent_suffix))
                    .parts::<()>()
                    .count(),
            )
        })
    });
    g.finish();
}

// ---------- case 7: walking n entries vs seeking to the n-th ----------

fn seek_vs_walk(c: &mut Criterion, f: &Fixture) {
    use okm_core::seek::Seekable;
    let probe = org_probe(PROBE_ORG);
    let mut g = c.benchmark_group("seek-vs-walk");
    g.sample_size(50);
    g.measurement_time(Duration::from_secs(4));
    g.warm_up_time(Duration::from_secs(1));

    for n in [10usize, 100, 1_000] {
        let target = f.probe_keys[n - 1].clone();
        g.bench_with_input(BenchmarkId::new("walk", n), &target, |b, _| {
            b.iter(|| {
                let mut cur = f.col.seek_scan::<ByOrg>(&probe);
                for _ in 1..n {
                    if !cur.step() {
                        break;
                    }
                }
                black_box(cur.key().is_some())
            })
        });
        g.bench_with_input(BenchmarkId::new("seek", n), &target, |b, target| {
            b.iter(|| {
                let mut cur = f.col.seek_scan::<ByOrg>(&probe);
                black_box(cur.seek(black_box(target)))
            })
        });
    }
    g.finish();
}

fn main() {
    // Seed before criterion parses anything, and keep the fixture alive for the
    // whole run: one corpus, one page cache, seven groups.
    let f = Fixture::seed();
    benches_main(&f);
}

fn benches_main(f: &Fixture) {
    let mut c: Criterion<_> = Criterion::default().configure_from_args();
    point(&mut c, f);
    scan(&mut c, f);
    join(&mut c, f);
    empty(&mut c, f);
    seek_vs_walk(&mut c, f);
    c.final_summary();
}
