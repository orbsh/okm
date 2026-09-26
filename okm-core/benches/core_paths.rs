//! Baseline benchmarks for the core paths (PLAN Phase 7 "Benchmarks"):
//! the numbers the abstractions claim, recorded once and re-run on
//! engine upgrades. No CI gates — variance in shared-CI runners makes
//! gates noisy; baselines are for relative comparisons across changes.
//!
//! Run: cargo bench -p okm-core
//! (feature-gated engines get their own benches: fjall via
//! `cargo bench -p okm-core --features fjall --bench engine_fjall` when added).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;

use okm_core::{
    KeyEncode, TestStore, ReduceCodec, ReduceLogic, Reversible, Reverse, Document, DocumentEncode,
    Collection, VarInt, VirtualStorage,
};

// ---------- declarations under test ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct BenchKey {
    pub org_id: u32,   // 4B
    pub user_id: u64,  // 8B
    pub tag: [u8; 4],  // 4B  → KEY_LEN = 16 ("typical" key)
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(BenchKey)]
#[ok_ns(9)]
#[ok_index(by_level { fields(level) })]
#[ok_reduce(TagTotals { group(level) })]
pub struct BenchRow {
    pub level: u32,        // hot 4B
    pub score: Reverse<u64>, // hot 8B (bit-flipped BE)
    pub visits: VarInt<u64>, // cold TLV
    pub name: String,      // cold TLV
}

use __OkmIndex_BenchRow_by_level as ByLevel;

// Reduce accumulator (count + sum) — the ADR-0008 example shape.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagTotals {
    pub count: u64,
    pub sum: u64,
}

impl ReduceCodec for TagTotals {
    fn encode_acc(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&self.count.to_be_bytes());
        b.extend_from_slice(&self.sum.to_be_bytes());
        b
    }
    fn decode_acc(b: &[u8]) -> Self {
        Self {
            count: u64::from_be_bytes(b[..8].try_into().unwrap()),
            sum: u64::from_be_bytes(b[8..16].try_into().unwrap()),
        }
    }
}

impl ReduceLogic for TagTotals {
    type Document = BenchRow;
    type Acc = TagTotals;
    fn fold(acc: &mut TagTotals, item: &BenchRow) {
        acc.count += 1;
        acc.sum += item.score.0;
    }
    fn unfold(acc: &mut TagTotals, item: &BenchRow) {
        acc.count = acc.count.saturating_sub(1);
        acc.sum = acc.sum.saturating_sub(item.score.0);
    }
}

// ---------- helpers ----------

fn make_key(i: u64) -> BenchKey {
    BenchKey {
        org_id: (i % 1000) as u32,
        user_id: i,
        tag: [(i % 256) as u8, 0, 0, 0],
    }
}

fn make_row(i: u64) -> BenchRow {
    BenchRow {
        level: (i % 100) as u32,
        score: Reverse(i),
        visits: VarInt(i % 1000),
        name: format!("user-{i}"),
    }
}

// ---------- key encoding paths ----------

fn bench_key_encode(c: &mut Criterion) {
    let mut g = c.benchmark_group("key-encode");
    g.throughput(Throughput::Bytes(BenchKey::KEY_LEN as u64));

    g.bench_function("encode/typical_16B", |b| {
        let k = make_key(42);
        b.iter(|| black_box(k.encode()))
    });
    g.bench_function("encode_prefix_named/full", |b| {
        let k = make_key(42);
        b.iter(|| {
            let mut buf = Vec::with_capacity(BenchKey::KEY_LEN);
            black_box(k.encode_prefix_named(&mut buf, &["org_id", "user_id", "tag"]))
        })
    });
    g.bench_function("encode_prefix_named/first_2", |b| {
        let k = make_key(42);
        b.iter(|| {
            let mut buf = Vec::with_capacity(12);
            black_box(k.encode_prefix_named(&mut buf, &["org_id", "user_id"]))
        })
    });
    g.bench_function("decode/typical_16B", |b| {
        let k = make_key(42);
        let bytes = k.encode();
        b.iter(|| black_box(BenchKey::decode(&bytes)))
    });
    g.finish()
}

// ---------- payload encoding paths ----------

fn bench_payload(c: &mut Criterion) {
    let mut g = c.benchmark_group("payload");
    let row = make_row(42);
    let bytes = row.encode_payload();

    g.throughput(Throughput::Bytes(bytes.len() as u64));
    g.bench_function("encode_payload/hot+cold", |b| b.iter(|| black_box(row.encode_payload())));
    g.bench_function("decode_payload/hot+cold", |b| {
        b.iter(|| black_box(<BenchRow as Document>::decode_payload(black_box(&bytes))))
    });

    // Reverse<T> bit-flip (order-preserving descending index).
    g.throughput(Throughput::Bytes(8));
    g.bench_function("reverse/u64_bitflip", |b| b.iter(|| black_box(Reverse(0x0123_4567_89AB_CDEFu64).encode())));
    g.finish()
}

// ---------- index scan + fetch-back ----------

fn bench_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("index-scan");
    for &fanout in &[1usize, 100, 10_000] {
        let mut t: Collection<TestStore, BenchKey, BenchRow> = Collection::new(TestStore::slatedb_mem());
        for i in 0..fanout as u64 {
            let k = make_key(1_000_000 + i);
            // Same tag for all rows in this group → one prefix value,
            // fanout rows behind it.
            t.put(
                &k,
                &BenchRow {
                    level: 7,
                    score: Reverse(i),
                    visits: VarInt(i),
                    name: format!("u{i}"),
                },
            );
        }
        let probe = 7u32.to_be_bytes(); // level=7 for every seeded row

        g.throughput(Throughput::Elements(fanout as u64));
        g.bench_function(format!("scan_index+fetch_back/fanout_{fanout}"), |b| {
            b.iter(|| black_box(t.scan::<ByLevel>(&probe).len()))
        });
        if fanout == 1 {
            g.bench_function("scan_covered/fanout_1", |b| {
                b.iter(|| black_box(t.scan_covered::<ByLevel>(&probe).len()))
            });
        }
    }
    g.finish()
}

// ---------- write path (TestStore) ----------

fn bench_write_mock(c: &mut Criterion) {
    let mut g = c.benchmark_group("write-mock");
    g.bench_function("put/row_with_index+reduce", |b| {
        let mut t: Collection<TestStore, BenchKey, BenchRow> = Collection::new(TestStore::slatedb_mem());
        let mut i = 0u64;
        b.iter(|| {
            t.put(&make_key(i), &make_row(i));
            i += 1;
            black_box(())
        })
    });
    g.bench_function("get/point", |b| {
        let mut t: Collection<TestStore, BenchKey, BenchRow> = Collection::new(TestStore::slatedb_mem());
        t.put(&make_key(1), &make_row(1));
        b.iter(|| black_box(t.get(&make_key(1)).is_some()))
    });
    g.bench_function("batch_commit/100_ops", |b| {
        let store = TestStore::slatedb_mem();
        let t: Collection<TestStore, BenchKey, BenchRow> = Collection::new(store.clone());
        b.iter_batched(
            || {
                let mut store = store.clone();
                let mut batch = okm_core::MemBatch::default();
                for i in 0..100u64 {
                    let k = make_key(i);
                    batch.put(k.encode(), make_row(i).encode_payload());
                }
                (store, batch)
            },
            |(mut store, batch)| black_box(store.commit_batch(batch).is_ok()),
            BatchSize::SmallInput,
        )
    });
    g.finish()
}

// ---------- dynamic codec tax ----------

fn bench_dynamic(c: &mut Criterion) {
    use okm_core::schema::CollectionSchema;
    use okm_dynamic::{encode_key, encode_payload, Value, ValueMap};

    let schema = CollectionSchema::of::<BenchKey, BenchRow>();
    let mut values = ValueMap::new();
    values.insert("org_id".into(), Value::U32(42));
    values.insert("user_id".into(), Value::U64(42));
    values.insert("tag".into(), Value::Bytes(vec![42, 0, 0, 0]));
    values.insert("level".into(), Value::U32(42));
    values.insert("score".into(), Value::U64(42));
    values.insert("visits".into(), Value::U64(42));
    values.insert("name".into(), Value::Str("user-42".into()));

    let mut g = c.benchmark_group("dynamic-tax");
    let rust_key = make_key(42).encode();
    let rust_payload = make_row(42).encode_payload();
    g.throughput(Throughput::Bytes(rust_key.len() as u64));
    g.bench_function("encode_key/dynamic", |b| {
        b.iter(|| black_box(encode_key(&schema, &values).unwrap()))
    });
    g.bench_function("encode_key/derive_reference", |b| {
        b.iter(|| black_box(make_key(42).encode()))
    });
    g.bench_function("encode_payload/dynamic", |b| {
        b.iter(|| black_box(encode_payload(&schema, &values).unwrap()))
    });
    g.bench_function("encode_payload/derive_reference", |b| {
        b.iter(|| black_box(make_row(42).encode_payload()))
    });
    // silence unused warnings for the payload-bytes pair (used above)
    let _ = (rust_key, rust_payload);
    g.finish()
}

criterion_group!(
    benches,
    bench_key_encode,
    bench_payload,
    bench_scan,
    bench_write_mock,
    bench_dynamic
);
criterion_main!(benches);
