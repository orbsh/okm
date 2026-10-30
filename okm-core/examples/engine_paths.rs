//! The same rows in three places: OKM over fjall, Redis over loopback, and a Parquet
//! file.
//!
//! `criterion` hangs on this machine before it measures anything (see PLAN), so this
//! times with `Instant` and prints what it measured.
//!
//! Run:
//!
//! ```text
//! cargo run -p okm-core --release --example engine_paths \
//!     --features test-engines,fjall
//! ```
//!
//! The Redis arm reads `REDIS_URL` (default `redis://127.0.0.1:6379`) and skips itself
//! when nothing answers. The Parquet arm is a file, not a store: it has no point get and
//! no secondary index, and the table says so where the cells would be.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::File;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{ArrayRef, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use okm_core::engine::fjall_backend::FjallStore;
use okm_core::{
    Batch, Collection, Document, DocumentEncode, KeyEncode, ReduceCodec, ReduceLogic, Reverse,
    Reversible, VarInt, VirtualStorage,
};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterVersion};

// ---------- the declarations under test ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct BenchKey {
    pub org_id: u32,
    pub user_id: u64,
    pub tag: [u8; 4],
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(BenchKey)]
#[ok_ns(9)]
#[ok_index(by_level { fields(level), includes(score) })]
#[ok_reduce(TagTotals { group(level) })]
pub struct BenchRow {
    pub level: u32,
    pub score: Reverse<u64>,
    pub visits: VarInt<u64>,
    pub name: String,
}

use __OkmIndex_BenchRow_by_level as ByLevel;

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

// ---------- one arm's result ----------

#[derive(Clone, Debug, Default)]
struct Line {
    put_ns: Option<f64>,
    get_ns: Option<f64>,
    batch_ns: Option<f64>,
    scan_fetch_ns: Option<f64>,
    scan_covered_ns: Option<f64>,
    bytes: Option<u64>,
    /// (fanout, fetch-back total ns, covered total ns) for the fanout sweep.
    sweep: Vec<(u64, f64, f64)>,
    notes: Vec<String>,
}

fn med_ns(total: std::time::Duration, ops: u64) -> f64 {
    total.as_nanos() as f64 / ops as f64
}

/// Median of three runs: a single run on a loaded machine drifts by more than the
/// differences this table is about.
fn median_of<F: FnMut() -> f64>(runs: usize, mut once: F) -> f64 {
    let mut xs: Vec<f64> = (0..runs).map(|_| once()).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

/// One index value's worth of rows: `level = i % 100`, so a probe on one level returns a
/// hundredth of the table — the fanout the data actually has, rather than a number picked
/// to look good.
fn fanout_for(rows: u64) -> u64 {
    (rows / 100).max(1)
}
const SCAN_LEVEL: u32 = 900;
/// Levels used by the fanout sweep, one per fanout (kept away from `SCAN_LEVEL`).
const SWEEP_LEVEL: u32 = 960;

// ---------- OKM over fjall ----------

fn measure_fjall(dir: &Path, rows: u64) -> Line {
    let store = FjallStore::open(dir, "okm").expect("open fjall");
    let t: Collection<FjallStore, BenchKey, BenchRow> = Collection::new(store.clone());

    for i in 0..2_000u64 {
        t.put(&make_key(i), &make_row(i)).expect("warm");
    }

    // One timed insert pass. Not a median of three: the second run would overwrite the
    // keys the first inserted, and an overwrite in an LSM is a tombstone plus a reduce
    // unfold — a different operation, several times slower, which is what a median over
    // three runs would actually have measured.
    let started = Instant::now();
    for i in 0..rows {
        t.put(&make_key(1_000_000 + i), &make_row(1_000_000 + i))
            .expect("put");
    }
    let put_ns = med_ns(started.elapsed(), rows);

    let key = make_key(1_000_000);
    let get_ns = median_of(3, || {
        let started = Instant::now();
        for _ in 0..rows {
            black_box(t.fetch(&key).expect("get"));
        }
        med_ns(started.elapsed(), rows)
    });

    // Also one pass, for the same reason as the put: a second pass would overwrite.
    let started = Instant::now();
    for round in 0..100u64 {
        let mut batch = Batch::default();
        for i in 0..100u64 {
            let k = make_key(2_000_000 + round * 100 + i);
            batch.put(k.encode(), make_row(i).encode_payload());
        }
        store.commit_batch(batch).expect("commit");
    }
    let batch_ns = med_ns(started.elapsed(), 100);

    // One index value with `fanout_for(rows)` rows behind it (seeded once, outside the
    // timed region).
    let fanout = fanout_for(rows);
    for i in 0..fanout {
        t.put(
            &make_key(3_000_000 + i),
            &BenchRow {
                level: SCAN_LEVEL,
                score: Reverse(i),
                visits: VarInt(i),
                name: format!("u{i}"),
            },
        )
        .expect("seed");
    }
    let probe = SCAN_LEVEL.to_be_bytes();
    let scan_fetch_ns = median_of(3, || {
        let started = Instant::now();
        black_box(t.scan::<ByLevel>(&probe).expect("scan").len());
        started.elapsed().as_nanos() as f64
    });
    let scan_covered_ns = median_of(3, || {
        let started = Instant::now();
        black_box(t.scan_covered::<ByLevel>(&probe).expect("covered").len());
        started.elapsed().as_nanos() as f64
    });

    // The same probe at every fanout worth asking about, one level each: does an index
    // value's cost stay flat as the number of rows behind it grows, and where does the
    // per-row part start to dominate.
    let mut sweep = Vec::new();
    for (slot, fanout) in [1u64, 10, 100, 1_000, 10_000, 100_000]
        .into_iter()
        .enumerate()
    {
        let level = SWEEP_LEVEL + slot as u32;
        let base = 500_000_000 + level as u64 * 1_000_000;
        for i in 0..fanout {
            t.put(
                &make_key(base + i),
                &BenchRow {
                    level,
                    score: Reverse(i),
                    visits: VarInt(i),
                    name: format!("s{i}"),
                },
            )
            .expect("seed sweep");
        }
        let probe = level.to_be_bytes();
        let fetch_total = median_of(3, || {
            let started = Instant::now();
            black_box(t.scan::<ByLevel>(&probe).expect("scan").len());
            started.elapsed().as_nanos() as f64
        });
        let covered_total = median_of(3, || {
            let started = Instant::now();
            black_box(t.scan_covered::<ByLevel>(&probe).expect("covered").len());
            started.elapsed().as_nanos() as f64
        });
        sweep.push((fanout, fetch_total, covered_total));
    }

    Line {
        sweep,
        put_ns: Some(put_ns),
        get_ns: Some(get_ns),
        batch_ns: Some(batch_ns),
        scan_fetch_ns: Some(scan_fetch_ns),
        scan_covered_ns: Some(scan_covered_ns),
        bytes: Some(dir_bytes(dir)),
        notes: vec![
            "Buffer persist mode: written to the page cache, fsynced on fjall's schedule"
                .to_string(),
        ],
    }
}

/// The same store with an fsync per write: the other end of the durability knob.
fn fjall_fsync_ns(dir: &Path) -> f64 {
    let store = FjallStore::open(dir, "durable").expect("open fjall");
    let t: Collection<FjallStore, BenchKey, BenchRow> = Collection::new(store.clone());
    let ops = 2_000u64;
    let started = Instant::now();
    for i in 0..ops {
        t.put(&make_key(4_000_000 + i), &make_row(i)).expect("put");
        store.persist().expect("persist");
    }
    med_ns(started.elapsed(), ops)
}

// ---------- fjall with no okm in the way ----------
//
// The same index value and the same documents, one keyspace each, driven by fjall's own
// iterator: no key struct, no document decode, no reduce, no `VirtualStorage` hop. This is
// the arm that separates "an LSM cannot do a batch scan" from "okm's scan costs something".

/// Index levels used by the raw sweep, away from `SCAN_LEVEL` and `SWEEP_LEVEL`.
const RAW_LEVEL: u32 = 970;

fn raw_index_key(level: u32, score: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&level.to_be_bytes());
    key.extend_from_slice(&score.to_be_bytes());
    key
}

fn measure_fjall_raw(dir: &Path, rows: u64) -> Line {
    let path = dir.join("raw");
    let db = fjall::Database::create_or_recover(fjall::Config::new(&path)).expect("open raw");
    let index = db
        .keyspace("raw_index", fjall::KeyspaceCreateOptions::default)
        .expect("index keyspace");
    let docs = db
        .keyspace("raw_docs", fjall::KeyspaceCreateOptions::default)
        .expect("doc keyspace");

    // A row is the document under an 8-byte primary key plus one index entry keyed by
    // (level, score) — the same two facts okm's put writes, minus the reduce fold.
    let write_row = |i: u64| {
        let row = make_row(i);
        docs.insert(make_key(i).user_id.to_be_bytes(), row.encode_payload())
            .expect("doc");
        index
            .insert(
                raw_index_key(row.level, row.score.0),
                row.score.0.to_be_bytes(),
            )
            .expect("index");
    };
    for i in 0..2_000u64 {
        write_row(i);
    }
    let started = Instant::now();
    for i in 0..rows {
        write_row(1_000_000 + i);
    }
    let put_ns = med_ns(started.elapsed(), rows);

    let one = make_key(1_000_000).user_id.to_be_bytes();
    let get_ns = median_of(3, || {
        let started = Instant::now();
        for _ in 0..rows {
            black_box(docs.get(one).expect("get"));
        }
        med_ns(started.elapsed(), rows)
    });

    let mut sweep = Vec::new();
    let mut scan_fetch_ns = None;
    let mut scan_covered_ns = None;
    for (slot, fanout) in [1u64, 10, 100, 1_000, 10_000, 100_000]
        .into_iter()
        .enumerate()
    {
        let level = RAW_LEVEL + slot as u32;
        for i in 0..fanout {
            docs.insert(
                (900_000_000 + level as u64 * 1_000_000 + i).to_be_bytes(),
                make_row(i).encode_payload(),
            )
            .expect("doc");
            index
                .insert(raw_index_key(level, i), i.to_be_bytes())
                .expect("index");
        }
        let prefix = level.to_be_bytes();
        let covered_total = median_of(3, || {
            let started = Instant::now();
            let mut sum = 0u64;
            for entry in index.prefix(prefix) {
                let value = entry.value().expect("value");
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&value[..8]);
                sum = sum.wrapping_add(u64::from_be_bytes(buf));
            }
            black_box(sum);
            started.elapsed().as_nanos() as f64
        });
        let fetch_total = median_of(3, || {
            let started = Instant::now();
            let mut sum = 0u64;
            for entry in index.prefix(prefix) {
                let key = entry.key().expect("key");
                let doc = docs.get(&key[4..12]).expect("doc");
                sum = sum.wrapping_add(doc.map(|v| v.len()).unwrap_or(0) as u64);
            }
            black_box(sum);
            started.elapsed().as_nanos() as f64
        });
        if fanout == fanout_for(rows) {
            scan_fetch_ns = Some(fetch_total);
            scan_covered_ns = Some(covered_total);
        }
        sweep.push((fanout, fetch_total, covered_total));
    }

    Line {
        put_ns: Some(put_ns),
        get_ns: Some(get_ns),
        batch_ns: None,
        scan_fetch_ns,
        scan_covered_ns,
        bytes: Some(dir_bytes(&path)),
        sweep,
        notes: vec![
            "no okm: fjall's own iterator over (level, score) keys, documents in a second keyspace"
                .to_string(),
            "the put writes the document and the index entry; there is no reduce here".to_string(),
        ],
    }
}

// ---------- Parquet ----------

fn parquet_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("org_id", DataType::UInt32, false),
        Field::new("user_id", DataType::UInt64, false),
        Field::new("tag", DataType::UInt32, false),
        Field::new("level", DataType::UInt32, false),
        Field::new("score", DataType::UInt64, false),
        Field::new("visits", DataType::UInt64, false),
        Field::new("name", DataType::Utf8, false),
    ]))
}

fn to_batch(schema: &Arc<Schema>, from: u64, to: u64) -> RecordBatch {
    let n = (to - from) as usize;
    let mut org = Vec::with_capacity(n);
    let mut user = Vec::with_capacity(n);
    let mut tag = Vec::with_capacity(n);
    let mut level = Vec::with_capacity(n);
    let mut score = Vec::with_capacity(n);
    let mut visits = Vec::with_capacity(n);
    let mut name: Vec<String> = Vec::with_capacity(n);
    for i in from..to {
        let key = make_key(i);
        let row = make_row(i);
        org.push(key.org_id);
        user.push(key.user_id);
        tag.push(u32::from_be_bytes(key.tag));
        level.push(row.level);
        score.push(row.score.0);
        visits.push(row.visits.0);
        name.push(row.name);
    }
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(org)),
        Arc::new(UInt64Array::from(user)),
        Arc::new(UInt32Array::from(tag)),
        Arc::new(UInt32Array::from(level)),
        Arc::new(UInt64Array::from(score)),
        Arc::new(UInt64Array::from(visits)),
        Arc::new(StringArray::from(name)),
    ];
    RecordBatch::try_new(schema.clone(), columns).expect("batch")
}

fn measure_parquet(path: &Path, rows: u64) -> Line {
    let _ = std::fs::remove_file(path);
    let schema = parquet_schema();
    let file = File::create(path).expect("create parquet");
    let props = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_1_0)
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(1).unwrap()))
        .build();
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props)).expect("writer");

    let started = Instant::now();
    let mut from = 0u64;
    while from < rows {
        let to = (from + 65_536).min(rows);
        writer.write(&to_batch(&schema, from, to)).expect("write");
        from = to;
    }
    writer.close().expect("close");
    let write_ns = med_ns(started.elapsed(), rows);

    let scan_ns = median_of(3, || {
        let file = File::open(path).expect("open");
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("builder")
            .with_batch_size(65_536)
            .build()
            .expect("build");
        let started = Instant::now();
        let mut seen = 0usize;
        for batch in reader {
            let batch = batch.expect("batch");
            seen += batch.num_rows();
            black_box(batch.column(4));
        }
        black_box(seen);
        started.elapsed().as_nanos() as f64
    });

    Line {
        put_ns: Some(write_ns),
        get_ns: None,
        batch_ns: None,
        scan_fetch_ns: Some(scan_ns),
        scan_covered_ns: None,
        bytes: std::fs::metadata(path).ok().map(|meta| meta.len()),
        sweep: Vec::new(),
        notes: vec![
            "file, not a store: no point get, no secondary index, no partial overwrite".to_string(),
            format!("the scan column is one full pass over all {rows} rows"),
        ],
    }
}

// ---------- a minimal RESP client ----------
//
// No dependency: the arm needs SET / GET / SADD / SMEMBERS / HINCRBY / MGET / MULTI /
// EXEC / DEL and nothing else.

struct Redis {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    prefix: String,
}

/// The replies this arm reads. `Simple`, `Error` and `Int` are parsed so the stream stays
/// in step; nothing in the measurements reads their payloads.
#[derive(Debug)]
#[allow(dead_code)]
enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Vec<Reply>),
}

impl Redis {
    fn connect(url: &str) -> std::io::Result<Self> {
        let authority = url
            .trim_start_matches("redis://")
            .trim_end_matches('/')
            .to_string();
        let stream = TcpStream::connect(&authority)?;
        stream.set_nodelay(true)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            prefix: "okmbench:".to_string(),
        })
    }

    fn write_command(&mut self, args: &[&[u8]]) -> std::io::Result<()> {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for arg in args {
            out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            out.extend_from_slice(arg);
            out.extend_from_slice(b"\r\n");
        }
        self.stream.write_all(&out)
    }

    fn read_reply(&mut self) -> std::io::Result<Reply> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        let mut chars = line.chars();
        let tag = chars.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty RESP line")
        })?;
        let body = line[1..].trim_end_matches("\r\n").to_string();
        Ok(match tag {
            '+' => Reply::Simple(body),
            '-' => Reply::Error(body),
            ':' => Reply::Int(body.parse().unwrap_or(0)),
            '$' => {
                let len: i64 = body.parse().unwrap_or(-1);
                if len < 0 {
                    Reply::Bulk(None)
                } else {
                    let mut buf = vec![0u8; len as usize + 2];
                    self.reader.read_exact(&mut buf)?;
                    buf.truncate(len as usize);
                    Reply::Bulk(Some(buf))
                }
            }
            '*' => {
                let len: i64 = body.parse().unwrap_or(0);
                let mut items = Vec::with_capacity(len.max(0) as usize);
                for _ in 0..len.max(0) {
                    items.push(self.read_reply()?);
                }
                Reply::Array(items)
            }
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown RESP tag {other}"),
                ));
            }
        })
    }

    fn call(&mut self, args: &[&[u8]]) -> Reply {
        self.write_command(args).expect("write");
        self.read_reply().expect("read")
    }

    fn key(&self, suffix: &str) -> Vec<u8> {
        format!("{}{suffix}", self.prefix).into_bytes()
    }

    fn flush(&mut self) {
        let pattern = format!("{}*", self.prefix);
        let keys = match self.call(&[b"KEYS", pattern.as_bytes()]) {
            Reply::Array(items) => items
                .into_iter()
                .filter_map(|item| match item {
                    Reply::Bulk(Some(bytes)) => Some(bytes),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        for chunk in keys.chunks(500) {
            let mut args: Vec<&[u8]> = vec![b"DEL"];
            for key in chunk {
                args.push(key);
            }
            self.call(&args);
        }
    }
}

fn measure_redis(url: &str, rows: u64) -> Option<Line> {
    let mut redis = match Redis::connect(url) {
        Ok(redis) => redis,
        Err(error) => {
            eprintln!("redis arm skipped: {url} unreachable ({error})");
            return None;
        }
    };
    redis.flush();

    // put = SET row + SADD index + HINCRBY reduce, atomically: what an application has to
    // write to get the one call OKM hands it.
    let put_once = |redis: &mut Redis, i: u64| {
        let level = i % 100;
        let value = make_row(i).encode_payload();
        let score = format!("{i:020}");
        let member = format!("{score}:row:{i}");
        redis.write_command(&[b"MULTI"]).expect("multi");
        redis
            .write_command(&[b"SET", redis.key(&format!("row:{i}")).as_slice(), &value])
            .expect("set");
        redis
            .write_command(&[
                b"SADD",
                redis.key(&format!("idx:{level}")).as_slice(),
                member.as_bytes(),
            ])
            .expect("sadd");
        redis
            .write_command(&[
                b"HINCRBY",
                redis.key(&format!("acc:{level}")).as_slice(),
                b"count",
                b"1",
            ])
            .expect("hincrby");
        redis
            .write_command(&[
                b"HINCRBY",
                redis.key(&format!("acc:{level}")).as_slice(),
                b"sum",
                score.as_bytes(),
            ])
            .expect("hincrby");
        redis.write_command(&[b"EXEC"]).expect("exec");
        for _ in 0..6 {
            redis.read_reply().expect("reply");
        }
    };

    for i in 0..500u64 {
        put_once(&mut redis, i);
    }
    // One pass, like the fjall arm: three would overwrite, and `HINCRBY` would then count
    // each row twice — a cheaper operation and a wrong number.
    let started = Instant::now();
    for i in 0..rows {
        put_once(&mut redis, 1_000_000 + i);
    }
    let put_ns = med_ns(started.elapsed(), rows);

    let one = redis.key("row:1000000");
    let get_ns = median_of(3, || {
        let started = Instant::now();
        for _ in 0..rows {
            black_box(redis.call(&[b"GET", &one]));
        }
        med_ns(started.elapsed(), rows)
    });

    let started = Instant::now();
    for round in 0..100u64 {
        redis.write_command(&[b"MULTI"]).expect("multi");
        for i in 0..100u64 {
            let n = 2_000_000 + round * 100 + i;
            let value = make_row(i).encode_payload();
            redis
                .write_command(&[b"SET", redis.key(&format!("row:{n}")).as_slice(), &value])
                .expect("set");
        }
        redis.write_command(&[b"EXEC"]).expect("exec");
        for _ in 0..102 {
            redis.read_reply().expect("reply");
        }
    }
    let batch_ns = med_ns(started.elapsed(), 100);

    // The same fanout: SMEMBERS + one MGET (fetch-back), SMEMBERS alone (covered).
    let idx = redis.key(&format!("idx:{SCAN_LEVEL}"));
    let fanout = fanout_for(rows);
    let members: Vec<String> = (0..fanout).map(|i| format!("{i:020}:row:{i}")).collect();
    for chunk in members.chunks(1_000) {
        let mut args: Vec<&[u8]> = vec![b"SADD", idx.as_slice()];
        for member in chunk {
            args.push(member.as_bytes());
        }
        redis.call(&args);
    }
    for i in 0..fanout {
        let value = BenchRow {
            level: SCAN_LEVEL,
            score: Reverse(i),
            visits: VarInt(i),
            name: format!("u{i}"),
        }
        .encode_payload();
        redis.call(&[b"SET", redis.key(&format!("row:{i}")).as_slice(), &value]);
    }

    let scan_fetch_ns = median_of(3, || {
        let started = Instant::now();
        let reply = redis.call(&[b"SMEMBERS", &idx]);
        let keys: Vec<Vec<u8>> = match reply {
            Reply::Array(items) => items
                .into_iter()
                .filter_map(|item| match item {
                    Reply::Bulk(Some(bytes)) => Some(bytes),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut args: Vec<&[u8]> = vec![b"MGET"];
        for key in &keys {
            args.push(key);
        }
        black_box(redis.call(&args));
        started.elapsed().as_nanos() as f64
    });
    let scan_covered_ns = median_of(3, || {
        let started = Instant::now();
        let reply = redis.call(&[b"SMEMBERS", &idx]);
        let mut sum = 0u64;
        if let Reply::Array(items) = reply {
            for item in items {
                if let Reply::Bulk(Some(bytes)) = item
                    && let Some((score, _)) = split_member(&bytes)
                {
                    sum = sum.wrapping_add(score);
                }
            }
        }
        black_box(sum);
        started.elapsed().as_nanos() as f64
    });

    // The same six-fanout sweep as the fjall arm, so the two can be read side by side.
    // Covered here is `SMEMBERS` alone with the score inside the member; fetch-back adds one
    // `MGET` for the whole set, which is one round trip however many rows it carries.
    let mut sweep = Vec::new();
    for (slot, fanout) in [1u64, 10, 100, 1_000, 10_000, 100_000]
        .into_iter()
        .enumerate()
    {
        let level = SWEEP_LEVEL + slot as u32;
        let idx = redis.key(&format!("idx:{level}"));
        let members: Vec<String> = (0..fanout)
            .map(|i| format!("{i:020}:srow:{level}:{i}"))
            .collect();
        for chunk in members.chunks(1_000) {
            let mut args: Vec<&[u8]> = vec![b"SADD", idx.as_slice()];
            for member in chunk {
                args.push(member.as_bytes());
            }
            redis.call(&args);
        }
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(fanout as usize);
        let mut values: Vec<Vec<u8>> = Vec::with_capacity(fanout as usize);
        for i in 0..fanout {
            keys.push(redis.key(&format!("srow:{level}:{i}")));
            values.push(
                BenchRow {
                    level,
                    score: Reverse(i),
                    visits: VarInt(i),
                    name: format!("s{i}"),
                }
                .encode_payload(),
            );
        }
        for start in (0..fanout as usize).step_by(500) {
            let end = (start + 500).min(fanout as usize);
            let mut args: Vec<&[u8]> = vec![b"MSET"];
            for j in start..end {
                args.push(&keys[j]);
                args.push(&values[j]);
            }
            redis.call(&args);
        }

        let fetch_total = median_of(3, || {
            let started = Instant::now();
            let reply = redis.call(&[b"SMEMBERS", &idx]);
            let fetched: Vec<Vec<u8>> = match reply {
                Reply::Array(items) => items
                    .into_iter()
                    .filter_map(|item| match item {
                        Reply::Bulk(Some(bytes)) => split_member(&bytes)
                            .map(|(_, rest)| redis.key(&String::from_utf8_lossy(rest))),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let mut args: Vec<&[u8]> = vec![b"MGET"];
            for key in &fetched {
                args.push(key);
            }
            black_box(redis.call(&args));
            started.elapsed().as_nanos() as f64
        });
        let covered_total = median_of(3, || {
            let started = Instant::now();
            let reply = redis.call(&[b"SMEMBERS", &idx]);
            let mut sum = 0u64;
            if let Reply::Array(items) = reply {
                for item in items {
                    if let Reply::Bulk(Some(bytes)) = item
                        && let Some((score, _)) = split_member(&bytes)
                    {
                        sum = sum.wrapping_add(score);
                    }
                }
            }
            black_box(sum);
            started.elapsed().as_nanos() as f64
        });
        sweep.push((fanout, fetch_total, covered_total));
    }

    // The server's own number; it counts the whole instance, not this key set.
    let mut used = 0u64;
    if let Reply::Bulk(Some(bytes)) = redis.call(&[b"INFO", b"memory"]) {
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(rest) = line.strip_prefix("used_memory:") {
                used = rest.trim().parse().unwrap_or(0);
            }
        }
    }

    redis.flush();
    Some(Line {
        put_ns: Some(put_ns),
        get_ns: Some(get_ns),
        batch_ns: Some(batch_ns),
        scan_fetch_ns: Some(scan_fetch_ns),
        scan_covered_ns: Some(scan_covered_ns),
        bytes: (used > 0).then_some(used),
        sweep,
        notes: vec![
            "each operation is a round trip on loopback; that is the protocol, not an overhead to subtract"
                .to_string(),
            format!("used_memory for the whole server: {:.0} MB", mb(used)),
        ],
    })
}

fn split_member(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (score, rest) = text.split_once(':')?;
    Some((score.parse().ok()?, rest.as_bytes()))
}

// ---------- printing ----------

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += dir_bytes(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

fn cell(value: Option<f64>) -> String {
    match value {
        Some(ns) if ns >= 1e6 => format!("{:.1} ms", ns / 1e6),
        Some(ns) if ns >= 1e3 => format!("{:.1} µs", ns / 1e3),
        Some(ns) => format!("{ns:.0} ns"),
        None => "—".to_string(),
    }
}

fn line(name: &str, line: &Line) {
    println!(
        "{:<24} {:>10} {:>10} {:>11} {:>11} {:>11}",
        name,
        cell(line.put_ns),
        cell(line.get_ns),
        cell(line.batch_ns),
        cell(line.scan_fetch_ns),
        cell(line.scan_covered_ns),
    );
    for note in &line.notes {
        println!("  {note}");
    }
}

fn main() {
    let rows: u64 = std::env::var("OKM_BENCH_PUTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50_000);
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

    println!(
        "{rows} rows, single thread. Writes are one timed pass (a second pass would overwrite, which is a different operation); reads are the median of three. The scan column is one index value holding 1/100 of the table, {} rows.\n",
        fanout_for(rows)
    );

    let dir = tempfile::TempDir::new().expect("tempdir");
    let fjall = measure_fjall(dir.path(), rows);
    let fsync_ns = fjall_fsync_ns(dir.path());
    let raw = measure_fjall_raw(dir.path(), rows);

    let parquet_path = std::env::temp_dir().join("okm-engine-paths.parquet");
    let parquet = measure_parquet(&parquet_path, rows);

    println!(
        "{:<24} {:>10} {:>10} {:>11} {:>11} {:>11}",
        "arm", "put", "get", "batch(100)", "scan(10k)", "covered"
    );
    line("okm (fjall, Buffer)", &fjall);
    line("fjall raw (no okm)", &raw);
    line("parquet (zstd 1)", &parquet);
    let redis = measure_redis(&redis_url, rows);
    if let Some(redis) = &redis {
        line("redis (loopback tcp)", redis);
    }

    if !fjall.sweep.is_empty() {
        println!("\nokm + fjall, one index value at six fanouts (fetch-back / covered):");
        println!(
            "  {:>8} {:>12} {:>14} {:>12} {:>14}",
            "fanout", "fetch total", "fetch per row", "covered", "covered per row"
        );
        for &(fanout, fetch, covered) in &fjall.sweep {
            println!(
                "  {:>8} {:>12} {:>14} {:>12} {:>14}",
                fanout,
                cell(Some(fetch)),
                cell(Some(fetch / fanout as f64)),
                cell(Some(covered)),
                cell(Some(covered / fanout as f64)),
            );
        }
    }

    if !raw.sweep.is_empty() {
        println!("\ncovered scan, one index value, per row — okm's scan / raw fjall / redis:");
        println!(
            "  {:>8} {:>11} {:>11} {:>11} {:>12}",
            "fanout", "okm scan", "raw fjall", "redis", "raw ÷ redis"
        );
        for (slot, &(fanout, _, okm_covered)) in fjall.sweep.iter().enumerate() {
            let (_, _, raw_covered) = raw.sweep[slot];
            let redis_cell = redis
                .as_ref()
                .and_then(|line| line.sweep.get(slot))
                .map(|&(_, _, covered)| covered);
            match redis_cell {
                Some(redis_covered) => println!(
                    "  {fanout:>8} {:>11} {:>11} {:>11} {:>11.1}x",
                    cell(Some(okm_covered / fanout as f64)),
                    cell(Some(raw_covered / fanout as f64)),
                    cell(Some(redis_covered / fanout as f64)),
                    raw_covered / redis_covered,
                ),
                None => println!(
                    "  {fanout:>8} {:>11} {:>11} {:>11} {:>12}",
                    cell(Some(okm_covered / fanout as f64)),
                    cell(Some(raw_covered / fanout as f64)),
                    "—",
                    "—",
                ),
            }
        }
    }

    if let Some(redis) = &redis
        && !redis.sweep.is_empty()
    {
        println!("\ncovered scan (no fetch-back), one index value, fjall against redis:");
        println!(
            "  {:>8} {:>11} {:>11} {:>11} {:>11} {:>9}",
            "fanout", "fjall", "fjall/row", "redis", "redis/row", "ratio"
        );
        for (&(fanout, _, fjall_covered), &(_, _, redis_covered)) in
            fjall.sweep.iter().zip(redis.sweep.iter())
        {
            println!(
                "  {fanout:>8} {:>11} {:>11} {:>11} {:>11} {:>9.1}x",
                cell(Some(fjall_covered)),
                cell(Some(fjall_covered / fanout as f64)),
                cell(Some(redis_covered)),
                cell(Some(redis_covered / fanout as f64)),
                fjall_covered / redis_covered,
            );
        }
        println!(
            "\n  a covered row costs {:.2} of a point get on fjall ({} against {}), and {:.2} on redis ({} against {})",
            fjall.scan_covered_ns.unwrap_or(1.0)
                / fanout_for(rows) as f64
                / fjall.get_ns.unwrap_or(1.0),
            cell(Some(
                fjall.scan_covered_ns.unwrap_or(0.0) / fanout_for(rows) as f64
            )),
            cell(fjall.get_ns),
            redis.scan_covered_ns.unwrap_or(1.0)
                / fanout_for(rows) as f64
                / redis.get_ns.unwrap_or(1.0),
            cell(Some(
                redis.scan_covered_ns.unwrap_or(0.0) / fanout_for(rows) as f64
            )),
            cell(redis.get_ns),
        );
    }

    println!("\nbytes, and the one durability knob:");
    println!(
        "  okm+fjall   {:.0} MB  ({:.0} B/row, journal file)",
        mb(fjall.bytes.unwrap_or(0)),
        fjall.bytes.unwrap_or(0) as f64 / rows as f64
    );
    println!(
        "  parquet     {:.1} MB  ({:.1} B/row)",
        mb(parquet.bytes.unwrap_or(0)),
        parquet.bytes.unwrap_or(0) as f64 / rows as f64
    );
    println!("  okm+fjall with fsync per write: {fsync_ns:.0} ns/row");

    let _ = std::fs::remove_dir_all(dir.path());
    let _ = std::fs::remove_file(&parquet_path);
}
