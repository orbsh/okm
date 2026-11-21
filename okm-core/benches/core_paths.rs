//! Baseline benchmarks for the core paths (PLAN Phase 7 "Benchmarks"):
//! the numbers the abstractions claim, recorded once and re-run on
//! engine upgrades. No CI gates — variance in shared-CI runners makes
//! gates noisy; baselines are for relative comparisons across changes.
//!
//! Run: `cargo bench -p okm-core` — no engine feature: `TestStore` comes in
//! through the `okm-async` dev-dependency (ADR-0059). Name a group to run it
//! alone: `cargo bench -p okm-core -- --bench core_paths "index/entry_key"`.
//! The `query_cost` bench is the one that needs `--features fjall`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use okm_core::testing::TestStore;
use okm_core::{
    Collection, Document, DocumentEncode, KeyEncode, KvIndex, ReduceCodec, ReduceLogic, Reverse,
    Reversible, VarInt, VirtualStorage,
};

// ---------- declarations under test ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct BenchKey {
    pub org_id: u32,  // 4B
    pub user_id: u64, // 8B
    pub tag: [u8; 4], // 4B  → KEY_LEN = 16 ("typical" key)
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(BenchKey)]
#[ok_ns(9)]
#[ok_index(by_level { fields(level), includes(score) })]
#[ok_index(by_name { fields(level, name), includes(visits) })]
#[ok_reduce(TagTotals { group(level) })]
pub struct BenchRow {
    pub level: u32,          // hot 4B
    pub score: Reverse<u64>, // hot 8B (bit-flipped BE)
    pub visits: VarInt<u64>, // cold TLV
    pub name: String,        // cold TLV
}

use __OkmIndex_BenchRow_by_level as ByLevel;

// ---------- multi-entry (function index over a list) ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct TokKey {
    pub id: u64,
}

/// The token list is what a multi-entry fan-out produces one entry per element
/// from — the inverted-index shape. The tokens below are longer than one
/// allocator size class on purpose: a segment buffer that starts empty grows
/// (8 → 16 → 32) where a sized one allocates once, and only tokens that cross
/// the class boundary show that.
fn token_ids(row: &TokRow) -> Vec<String> {
    row.tags.split(',').map(|s| s.to_string()).collect()
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TokKey)]
#[ok_ns(10)]
#[ok_index(by_token { func(token_ids) })]
pub struct TokRow {
    pub tags: String,
}

use __OkmIndex_TokRow_by_token as ByToken;
use okm_core::parts;

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
    fn fold(acc: &mut TagTotals, _key: &BenchKey, item: &BenchRow) {
        acc.count += 1;
        acc.sum += item.score.0;
    }
    fn unfold(acc: &mut TagTotals, _key: &BenchKey, item: &BenchRow) {
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
    g.bench_function("encode_payload/hot+cold", |b| {
        b.iter(|| black_box(row.encode_payload()))
    });
    g.bench_function("decode_payload/hot+cold", |b| {
        b.iter(|| black_box(<BenchRow as Document>::decode_payload(black_box(&bytes))))
    });

    // Reverse<T> bit-flip (order-preserving descending index).
    g.throughput(Throughput::Bytes(8));
    g.bench_function("reverse/u64_bitflip", |b| {
        b.iter(|| black_box(Reverse(0x0123_4567_89AB_CDEFu64).encode()))
    });
    g.finish()
}

// ---------- index entry encoding ----------

fn bench_entry_encode(c: &mut Criterion) {
    use __OkmIndex_BenchRow_by_name as ByName;

    let mut g = c.benchmark_group("index");
    let row = make_row(42);
    let key = make_key(42);
    let ns = <BenchRow as Document>::NS_PREFIX;
    // `fields(level, name)` — one fixed-width field and one variable-width one,
    // i.e. the segment whose width is computable (4 + name.len()) but only from
    // the document. `includes(visits)` gives the value segment the same shape.
    g.bench_function("entry_key/fixed+variable", |b| {
        b.iter(|| black_box(ByName::entry_key(ns, &key, &row)))
    });
    g.finish()
}

// ---------- multi-entry fan-out ----------

fn bench_multi_entry(c: &mut Criterion) {
    let mut g = c.benchmark_group("index");
    let key = TokKey { id: 42 };
    // 24-byte tokens: past the allocator's first size class, so a buffer that
    // starts empty grows a ladder (8 → 16 → 32) where a sized one allocates once.
    let row = TokRow {
        tags: (0..8)
            .map(|i| format!("token-{i:02}-abcdefghijklm"))
            .collect::<Vec<_>>()
            .join(","),
    };
    let ns = <TokRow as Document>::NS_PREFIX;
    // Eight produced values = eight entries per document, each a key whose
    // segment is that token's bytes. This is the path where one buffer per
    // value is the normal case, not the exception.
    g.bench_function("entry_pairs/multi_entry_8", |b| {
        b.iter(|| black_box(ByToken::entry_pairs(ns, &key, &row).len()))
    });
    g.finish()
}

// ---------- index scan + fetch-back ----------

fn bench_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("index-scan");
    for &fanout in &[1usize, 100, 10_000] {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
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
            )
            .unwrap();
        }
        let probe = 7u32.to_be_bytes(); // level=7 for every seeded row

        g.throughput(Throughput::Elements(fanout as u64));
        g.bench_function(format!("scan_index+fetch_back/fanout_{fanout}"), |b| {
            b.iter(|| {
                black_box(
                    t.scan::<ByLevel>(&probe)
                        .collect::<okm_core::error::Result<Vec<_>>>()
                        .unwrap()
                        .len(),
                )
            })
        });
        g.bench_function(format!("scan_covered/fanout_{fanout}"), |b| {
            b.iter(|| {
                black_box(
                    t.scan::<ByLevel>(&probe)
                        .parts::<(parts::Fields, parts::Key, parts::Include)>()
                        .collect::<okm_core::error::Result<Vec<_>>>()
                        .unwrap()
                        .len(),
                )
            })
        });
    }
    g.finish()
}

// ---------- write path (TestStore) ----------

fn bench_write_mock(c: &mut Criterion) {
    let mut g = c.benchmark_group("write-mock");
    g.bench_function("put/row_with_index+reduce", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            t.put(&make_key(i), &make_row(i)).unwrap();
            i += 1;
            black_box(())
        })
    });
    g.bench_function("get/point", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        t.put(&make_key(1), &make_row(1)).unwrap();
        // Both slots plus the composition a document read pays: a point read
        // answers parts, the row converts to a value, the values merge.
        b.iter(|| {
            let (row, seg) = t
                .get::<(okm_core::parts::Row, okm_core::parts::Value)>(&make_key(1))
                .unwrap();
            black_box(
                row.map(|r| r.to_value())
                    .unwrap_or_default()
                    .merged(seg.unwrap_or_default()),
            )
        })
    });
    g.bench_function("batch_commit/100_ops", |b| {
        let store = TestStore::default();
        b.iter_batched(
            || {
                let store = store.clone();
                let mut batch = okm_core::Batch::default();
                for i in 0..100u64 {
                    let k = make_key(i);
                    batch.put(k.encode(), make_row(i).encode_payload());
                }
                (store, batch)
            },
            |(store, batch)| black_box(store.commit_batch(&batch).is_ok()),
            BatchSize::SmallInput,
        )
    });
    g.finish()
}

// ---------- the change log's tax (ADR-0045) ----------

/// A FIXED key and row, so every iteration is the same overwrite: the workload
/// is constant, the in-memory engine cannot grow under the measurement, and the
/// only difference between the two arms is whether a changelog is attached. (A
/// fresh key per iteration makes the store grow into the millions and turns the
/// run into an engine-compaction benchmark instead.)
fn bench_write_changelog(c: &mut Criterion) {
    let mut g = c.benchmark_group("write-changelog");
    g.sample_size(20);
    g.measurement_time(std::time::Duration::from_secs(2));

    g.bench_function("put/plain", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        let key = make_key(7);
        let row = make_row(7);
        b.iter(|| {
            t.put(&key, &row).unwrap();
            black_box(())
        })
    });
    g.bench_function("put/captured", |b| {
        // The directory lives in cargo's own tmp area (on disk) and is removed
        // after the run; it must outlive the collection, so it is declared first.
        let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("changelog-bench");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let t: Collection<_, BenchKey, BenchRow> =
            Collection::open(okm_core::changelog::ChangelogStore::new(
                TestStore::default(),
                okm_core::changelog::ChangelogConfig::new(&root),
            ))
            .unwrap();
        let key = make_key(7);
        let row = make_row(7);
        b.iter(|| {
            t.put(&key, &row).unwrap();
            black_box(())
        });
        drop(t);
        let _ = std::fs::remove_dir_all(&root);
    });
    g.finish()
}

// ---------- dynamic codec tax ----------

fn bench_dynamic(c: &mut Criterion) {
    use okm_core::model::encode::{encode_key, encode_payload};
    use okm_core::schema::CollectionSchema;
    use okm_value::{Dict, Variant as Value};

    let schema = CollectionSchema::of::<BenchKey, BenchRow>();
    let mut values = Dict::new();
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
    // The other direction: a row read back. The dynamic side is the same decoder the log and
    // the lake use (`okm_core::decode`), so this pair is the whole tax of "the reader learns
    // the layout from bytes" rather than from the type.
    let parsed_dynamic = okm_core::decode::decode_payload(&schema, &rust_payload).unwrap();
    g.bench_function("decode_payload/dynamic", |b| {
        b.iter(|| {
            black_box(okm_core::decode::decode_payload(&schema, &rust_payload).unwrap());
            black_box(&parsed_dynamic)
        })
    });
    g.bench_function("decode_payload/derive_reference", |b| {
        b.iter(|| black_box(BenchRow::decode_payload(&rust_payload)))
    });
    g.bench_function("decode_key/dynamic", |b| {
        b.iter(|| black_box(okm_core::decode::decode_key(&schema, &rust_key).unwrap()))
    });
    g.bench_function("decode_key/derive_reference", |b| {
        b.iter(|| black_box(BenchKey::decode(&rust_key)))
    });
    // Entry production, the pair ADR-0076 §1 puts against each other: the runtime producer
    // reads the document by NAME (a map lookup per field per entry, a fresh `Vec` for every
    // key and every includes segment), while the compiled walk slices the key and the payload
    // bytes a write already holds. The two faces here carry the same shapes as the typed ones
    // above — a hot segment, a cold `Str` tail, a covered field — minus the `VarInt` include,
    // which a dynamic index cannot address (`encode_variable` takes `Str`).
    {
        use okm_core::model::access_method::index_entries;
        use okm_core::model::entries::Entries;
        use okm_core::model::entry_layout::{EntryLayout, FaceDecl};

        let indexes = vec![
            okm_core::model::access_method::AccessMethod::plain(
                0x4001,
                vec!["level".into()],
                vec!["score".into()],
            ),
            okm_core::model::access_method::AccessMethod::plain(
                0x4002,
                vec!["level".into(), "name".into()],
                vec![],
            ),
        ];
        let faces = [
            FaceDecl {
                name: "0x4001",
                slot: 0x4001,
                fields: &["level"],
                includes: &["score"],
                key_prefix: &[],
                lead: false,
                func: false,
                partial: false,
            },
            FaceDecl {
                name: "0x4002",
                slot: 0x4002,
                fields: &["level", "name"],
                includes: &[],
                key_prefix: &[],
                lead: false,
                func: false,
                partial: false,
            },
        ];
        let layout = EntryLayout::compile(&schema, &faces).unwrap();
        let ns = [0u8, 9];
        let mut sink = Entries::new();
        g.bench_function("index_entries/plain_from_names", |b| {
            b.iter(|| {
                black_box(
                    index_entries(&schema, black_box(&ns), &indexes, &rust_key, &values).unwrap(),
                )
            })
        });
        g.bench_function("entry_layout/plain_from_bytes", |b| {
            b.iter(|| {
                sink.clear();
                layout
                    .produce_encoded(
                        black_box(&ns),
                        black_box(&rust_key),
                        black_box(&rust_payload),
                        &mut sink,
                    )
                    .unwrap();
                black_box(sink.as_bytes().len())
            })
        });
    }
    let _ = (rust_key, rust_payload);
    g.finish()
}

// ---------- the compiled plan, by hand ----------

/// A "typical" table's plan, written as literals — no schema, no names, no dictionary:
/// `BenchKey` is 4 + 8 + 4 bytes; `BenchRow`'s hot segment is 4 + 8 with two cold TLV
/// fields behind it.
///
/// The question this group answers: what does interpreting a plan that addresses bytes
/// by OFFSET cost, when the plan carries no field names and the reader builds no `Dict`?
/// The plan a real compiler would emit is exactly this shape, so the number is the tax
/// of the walk itself rather than of the dictionary around it.
mod compiled {
    use super::*;

    /// `(offset, width)` of every fixed field, in key order.
    pub const KEY: [(usize, usize); 3] = [(0, 4), (4, 8), (12, 4)];
    /// `(offset, width)` of every hot field, within the hot segment.
    pub const HOT: [(usize, usize); 2] = [(0, 4), (4, 8)];
    pub const HOT_WIDTH: usize = 12;
    pub const TAG_VISITS: u8 = 2;
    pub const TAG_NAME: u8 = 3;

    pub fn encode_key(k: &BenchKey) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[KEY[0].0..KEY[0].0 + KEY[0].1].copy_from_slice(&k.org_id.to_be_bytes());
        out[KEY[1].0..KEY[1].0 + KEY[1].1].copy_from_slice(&k.user_id.to_be_bytes());
        out[KEY[2].0..KEY[2].0 + KEY[2].1].copy_from_slice(&k.tag);
        out
    }

    pub fn decode_key(b: &[u8]) -> (u32, u64, [u8; 4]) {
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&b[KEY[2].0..KEY[2].0 + KEY[2].1]);
        (
            u32::from_be_bytes(b[KEY[0].0..KEY[0].0 + KEY[0].1].try_into().unwrap()),
            u64::from_be_bytes(b[KEY[1].0..KEY[1].0 + KEY[1].1].try_into().unwrap()),
            tag,
        )
    }

    /// Walk the payload: hot fields by offset into a fixed buffer, cold fields by tag
    /// into borrowed slices. No allocation, no key lookup, no enum match on a name.
    pub fn decode_borrowed(b: &[u8]) -> ([u32; 2], Option<&[u8]>, Option<&[u8]>) {
        let mut hot = [0u32; 2];
        for (i, (off, width)) in HOT.iter().enumerate() {
            let mut raw = [0u8; 8];
            raw[..*width].copy_from_slice(&b[*off..*off + *width]);
            hot[i] = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        }
        let mut visits = None;
        let mut name = None;
        let mut pos = HOT_WIDTH;
        while pos < b.len() {
            let tag = b[pos];
            pos += 1;
            let (len, n) = okm_core::take_len(&b[pos..]).unwrap();
            pos += n;
            let value = &b[pos..pos + len];
            pos += len;
            match tag {
                TAG_VISITS => visits = Some(value),
                TAG_NAME => name = Some(value),
                _ => {}
            }
        }
        (hot, visits, name)
    }

    /// The same walk, but producing what the derive's decoder produces: owned fields.
    pub fn decode_owned(b: &[u8]) -> (u32, u64, u64, String) {
        let (hot, visits, name) = decode_borrowed(b);
        // `Reverse<u64>` stores the bit-flipped big-endian form, which is the whole of
        // what the derive's decoder does to it.
        let score = !u64::from_be_bytes(b[4..12].try_into().unwrap());
        let visits = match visits {
            Some(v) => okm_core::take_len(v).map(|(n, _)| n as u64).unwrap_or(0),
            None => 0,
        };
        (
            hot[0],
            score,
            visits,
            std::str::from_utf8(name.unwrap_or(&[]))
                .unwrap()
                .to_string(),
        )
    }
}

fn bench_compiled_plan(c: &mut Criterion) {
    let row = make_row(42);
    let key = make_key(42);
    let payload = row.encode_payload();
    let key_bytes = key.encode();

    // The prototype must read the REAL bytes, or the number means nothing.
    assert_eq!(
        &compiled::encode_key(&key)[..],
        &key_bytes[..],
        "key layout"
    );
    let (level, score, visits, name) = compiled::decode_owned(&payload);
    assert_eq!(
        (level, visits, name.as_str()),
        (42, 42, "user-42"),
        "payload layout"
    );
    let _ = score;

    let mut g = c.benchmark_group("plan-tax");
    g.bench_function("key_encode/compiled", |b| {
        b.iter(|| black_box(compiled::encode_key(&make_key(42))))
    });
    g.bench_function("key_decode/compiled", |b| {
        b.iter(|| black_box(compiled::decode_key(&key_bytes)))
    });
    g.bench_function("payload_decode/compiled_borrowed", |b| {
        b.iter(|| black_box(compiled::decode_borrowed(&payload)))
    });
    g.bench_function("payload_decode/compiled_owned", |b| {
        b.iter(|| black_box(compiled::decode_owned(&payload)))
    });
    g.finish()
}

// ---------- what a lookup costs ----------

/// The route lookup a commit does, measured on its own: a two-byte key in the maps the store
/// actually uses. `BTreeMap<Vec<u8>, _>` is what the store keys by header; the `u16` pair is
/// what the declaration already gives us (a ns is two bytes), so the question "is the map
/// choice expensive" has a cheap, direct answer.
fn bench_route_maps(c: &mut Criterion) {
    use std::collections::{BTreeMap, HashMap};

    let mut g = c.benchmark_group("route-map");
    for n in [1usize, 8, 64] {
        let headers: Vec<Vec<u8>> = (0..n as u16).map(|i| i.to_be_bytes().to_vec()).collect();
        let tree: BTreeMap<Vec<u8>, usize> = headers.iter().cloned().zip(0..).collect();
        let map: HashMap<u16, usize> = (0..n as u16).map(|i| (i, i as usize)).collect();
        let probe_header = headers[n / 2].clone();
        let probe_ns = u16::from_be_bytes([probe_header[0], probe_header[1]]);

        g.bench_function(format!("btreemap_vec/{}", n), |b| {
            b.iter(|| black_box(tree.get(black_box(&probe_header))))
        });
        g.bench_function(format!("hashmap_u16/{}", n), |b| {
            b.iter(|| black_box(map.get(black_box(&probe_ns))))
        });
    }
    g.finish()
}

// ---------- what writing the declared access methods costs ----------

/// Two declared indexes (`fields(level) includes(score)`, `fields(level, name)
/// includes(visits)`) plus the reduce — the shape a "typical" table has. The number this
/// group reports is the ceiling an interpreter has to reach: whatever moves into `core`
/// must not add materially to it, and the bulk of it is per-entry allocation, not the field
/// reads a generated body performs.
fn bench_index_entries(c: &mut Criterion) {
    let key = make_key(42);
    let row = make_row(42);
    let ns = <BenchRow as Document>::NS_PREFIX;

    let mut g = c.benchmark_group("index-tax");
    g.bench_function("index_entries/two_declared", |b| {
        b.iter(|| {
            black_box(<BenchRow as Document>::index_entries(
                black_box(&key),
                black_box(&row),
                black_box(ns),
            ))
        })
    });
    // The form a write uses: one buffer, reused, so the append allocates nothing after the
    // first write. The gap to `index_entries/two_declared` is what the per-entry Vecs cost.
    let mut sink = okm_core::model::entries::Entries::new();
    g.bench_function("index_entries_into/reused", |b| {
        b.iter(|| {
            sink.clear();
            <BenchRow as Document>::index_entries_into(
                black_box(&key),
                black_box(&row),
                black_box(ns),
                &mut sink,
            );
            black_box(sink.as_bytes().len())
        })
    });
    // The compiled form (ADR-0076 §1): one walk over field references, fed from the bytes a
    // write already holds — the key it encoded and the payload it is about to store. Its
    // output is byte-identical to `index_entries_into` above (locked by
    // `tests/entry_layout_test.rs`); what this measures is the walk itself.
    let schema = okm_core::schema::CollectionSchema::of::<BenchKey, BenchRow>();
    let layout = okm_core::model::entry_layout::EntryLayout::compile(
        &schema,
        &[
            okm_core::model::entry_layout::FaceDecl {
                name: "by_level",
                slot: <ByLevel as KvIndex>::SLOT,
                fields: <ByLevel as KvIndex>::FIELDS,
                includes: <ByLevel as KvIndex>::INCLUDES,
                key_prefix: <ByLevel as KvIndex>::KEY_PREFIX,
                lead: false,
                func: false,
                partial: false,
            },
            okm_core::model::entry_layout::FaceDecl {
                name: "by_name",
                slot: <__OkmIndex_BenchRow_by_name as KvIndex>::SLOT,
                fields: <__OkmIndex_BenchRow_by_name as KvIndex>::FIELDS,
                includes: <__OkmIndex_BenchRow_by_name as KvIndex>::INCLUDES,
                key_prefix: <__OkmIndex_BenchRow_by_name as KvIndex>::KEY_PREFIX,
                lead: false,
                func: false,
                partial: false,
            },
        ],
    )
    .unwrap();
    let mut key_bytes = Vec::new();
    key.encode_into(&mut key_bytes);
    let payload = row.encode_payload();
    let mut walked = okm_core::model::entries::Entries::new();
    g.bench_function("entry_layout/compiled", |b| {
        b.iter(|| {
            walked.clear();
            layout
                .produce(
                    black_box(ns),
                    black_box(&key_bytes),
                    black_box(&payload),
                    &mut walked,
                )
                .unwrap();
            black_box(walked.as_bytes().len())
        })
    });
    g.bench_function("index_entries/one_declared", |b| {
        b.iter(|| {
            black_box(<ByLevel as KvIndex>::entry_pairs(
                black_box(ns),
                black_box(&key),
                black_box(&row),
            ))
        })
    });
    g.finish()
}

/// The same put over the engine that costs nothing but okm's own work (the ephemeral
/// default), because a slatedb commit is 40 µs and hides everything this crate does. This is
/// the number a change to the write path has to move.
fn bench_write_ephemeral(c: &mut Criterion) {
    let mut g = c.benchmark_group("write-ephemeral");
    g.bench_function("put/row_with_index+reduce", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            t.put(&make_key(i), &make_row(i)).unwrap();
            i += 1;
            black_box(())
        })
    });
    // The read-modify-write path: a map patch goes through merge, which decodes the row it
    // replaces, compares the two entry sets and re-encodes. Seeded once so every iteration
    // merges into an existing row — the case the path exists for.
    g.bench_function("merge/row_with_index+reduce", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        const SEED: u64 = 1 << 10;
        let mut patch = okm_core::okm_value::Dict::new();
        patch.insert("visits".to_string(), okm_core::okm_value::Variant::U64(7));
        let patch = okm_core::okm_value::Variant::Map(patch);
        // A patch carrying no declared name is not a merge of anything, so pick one the
        // declaration has: `visits` is a payload field on this row.
        for i in 0..SEED {
            t.put(&make_key(i), &make_row(i)).unwrap();
        }
        let mut i = 0u64;
        b.iter(|| {
            t.put(&make_key(i % SEED), &patch).unwrap();
            i += 1;
            black_box(())
        })
    });
    // Delete is measured with the write that keeps the row there: a purge of a missing key
    // is a cheaper path (no previous row to sweep), so measuring it alone would flatter it.
    // The number is purge + put, and the difference from put above is the purge.
    g.bench_function("purge+reput/row_with_index+reduce", |b| {
        let t: Collection<TestStore, BenchKey, BenchRow> =
            Collection::open(TestStore::default()).unwrap();
        const SEED: u64 = 1 << 10;
        for i in 0..SEED {
            t.put(&make_key(i), &make_row(i)).unwrap();
        }
        let mut i = 0u64;
        b.iter(|| {
            let k = i % SEED;
            t.purge(&make_key(k)).unwrap();
            t.put(&make_key(k), &make_row(k)).unwrap();
            i += 1;
            black_box(())
        })
    });
    g.finish()
}

criterion_group!(
    benches,
    bench_key_encode,
    bench_payload,
    bench_entry_encode,
    bench_multi_entry,
    bench_scan,
    bench_write_mock,
    bench_write_changelog,
    bench_dynamic,
    bench_compiled_plan,
    bench_route_maps,
    bench_index_entries,
    bench_write_ephemeral
);
criterion_main!(benches);
