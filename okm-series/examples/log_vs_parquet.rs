// A benchmark harness: a failure has nothing to clean up and nowhere to report, so
// panicking on it is the correct behaviour. The workspace lints forbid `expect`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! One million log records, written twice: into an `okm-series` log and into a Parquet
//! file, then measured for bytes on disk, write cost, full-scan read, and the cost of
//! fetching one record.
//!
//! `cargo run -p okm-series --release --example log_vs_parquet`
//!
//! The two formats are not trying to be the same thing, and the point of the numbers is
//! where the difference comes from: the log is row-major with a block index (one record
//! is one seek), Parquet is columnar with row groups (one record means decoding the
//! columns of its row group). Sizes are the part where Parquet has every advantage.

use std::fs::File;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use okm_series::body::{Body, Value};
use okm_series::{Codec, Config, DictConfig, Log, Precision};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterVersion};

const RECORDS: u64 = 1_000_000;
const POINT_READS: usize = 200;

const LEVELS: [&str; 5] = ["INFO", "DEBUG", "WARN", "ERROR", "TRACE"];
const SERVICES: [&str; 8] = [
    "checkout", "catalog", "search", "identity", "payments", "shipping", "media", "edge",
];
const HOSTS: [&str; 16] = [
    "ip-10-0-1-11",
    "ip-10-0-1-12",
    "ip-10-0-1-13",
    "ip-10-0-1-14",
    "ip-10-0-2-21",
    "ip-10-0-2-22",
    "ip-10-0-2-23",
    "ip-10-0-2-24",
    "ip-10-1-3-31",
    "ip-10-1-3-32",
    "ip-10-1-3-33",
    "ip-10-1-3-34",
    "ip-10-2-4-41",
    "ip-10-2-4-42",
    "ip-10-2-4-43",
    "ip-10-2-4-44",
];
const RESOURCES: [&str; 8] = [
    "orders",
    "carts",
    "products",
    "sessions",
    "invoices",
    "shipments",
    "assets",
    "users",
];

/// Tags in the log, columns in the Parquet file — the same eight fields.
const T_LEVEL: u16 = 1;
const T_SERVICE: u16 = 2;
const T_HOST: u16 = 3;
const T_MESSAGE: u16 = 4;
const T_TRACE: u16 = 5;
const T_LATENCY: u16 = 6;
const T_STATUS: u16 = 7;
const T_TENANT: u16 = 8;

/// xorshift64*, so the two writers see the same records without a rng dependency.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
}

struct Record {
    level: &'static str,
    service: &'static str,
    host: &'static str,
    message: String,
    trace: String,
    latency: f64,
    status: u64,
    tenant: u64,
}

fn record(rng: &mut Gen) -> Record {
    let level = *rng.pick(&LEVELS);
    let service = *rng.pick(&SERVICES);
    let host = *rng.pick(&HOSTS);
    let resource = *rng.pick(&RESOURCES);
    let id = rng.next() % 10_000_000;
    let status = *rng.pick(&[200u64, 200, 200, 201, 304, 400, 404, 500, 503]);
    let tenant = rng.next() % 4_096;
    let latency = (rng.next() % 50_000) as f64 / 100.0;
    let message = format!(
        "GET /api/v1/{resource}/{id} {status} in {latency:.2}ms tenant={tenant} host={host}"
    );
    let trace = format!("{:016x}{:016x}", rng.next(), rng.next());
    Record {
        level,
        service,
        host,
        message,
        trace,
        latency,
        status,
        tenant,
    }
}

fn body_of(record: &Record) -> Body {
    let mut body = Body::new();
    body.push(T_LEVEL, Value::Str(record.level.to_string()))
        .push(T_SERVICE, Value::Str(record.service.to_string()))
        .push(T_HOST, Value::Str(record.host.to_string()))
        .push(T_MESSAGE, Value::Str(record.message.clone()))
        .push(T_TRACE, Value::Str(record.trace.clone()))
        .push(T_LATENCY, Value::F64(record.latency))
        .push(T_STATUS, Value::U64(record.status))
        .push(T_TENANT, Value::U64(record.tenant));
    body
}

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let meta = entry.metadata();
        match meta {
            Ok(meta) if meta.is_dir() => total += dir_bytes(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

fn fresh(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("okm-series-vs-parquet-{name}"));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);
    path
}

/// Peak resident set of this process, from `/proc/self/status`.
fn peak_rss() -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().to_string();
        }
    }
    "unknown".to_string()
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    // Deterministic timestamps for the Parquet column: one millisecond per record, which
    // is also roughly the rate the log writes at when it stamps its own clock.
    let base_ts = 1_760_000_000_000u64;

    println!("{RECORDS} records, single thread, temp dir on XFS/NVMe\n");

    // What the record costs before any format sees it: the baseline both writers are
    // measured against, so neither is charged for the test's own arithmetic.
    let mut baseline = 0u64;
    let started = Instant::now();
    let mut rng = Gen(0x9E37_79B9_7F4A_7C15);
    for _ in 0..RECORDS {
        let record = record(&mut rng);
        baseline = baseline.wrapping_add(record.message.len() as u64);
    }
    let gen_ns = started.elapsed().as_nanos() as u64;
    let started = Instant::now();
    let mut rng = Gen(0x9E37_79B9_7F4A_7C15);
    for _ in 0..RECORDS {
        let record = record(&mut rng);
        baseline = baseline.wrapping_add(body_of(&record).len() as u64);
    }
    let gen_body_ns = started.elapsed().as_nanos() as u64;
    black_box(baseline);

    // ---- the log ------------------------------------------------------------------
    // Three shapes of the same append: the defaults, the dictionary off, and neither
    // dictionary nor compression (a caller that hands over already-compressed bodies).
    let shapes = [
        ("okm-series zstd 1, dictionary", true, true),
        ("okm-series zstd 1, no dictionary", false, true),
        ("okm-series raw, no dictionary", false, false),
    ];
    let mut runs: Vec<(String, u64, u64)> = Vec::new();
    let mut log: Option<Log> = None;
    let mut log_root = fresh("log");
    let mut checksum_log = 0u64;
    let mut sealed = 0usize;
    for (name, dictionary, compress) in shapes {
        let root = fresh(&format!("log-{}", name.replace([' ', ','], "-")));
        let mut opened = Log::open(
            &root,
            Config {
                precision: Precision::Ms,
                dict: dictionary.then(DictConfig::default),
                block_codec: if compress { Codec::Zstd } else { Codec::Raw },
                ..Config::default()
            },
        )
        .expect("open log");
        let primary = runs.is_empty();

        let mut rng = Gen(0x9E37_79B9_7F4A_7C15);
        let started = Instant::now();
        for _ in 0..RECORDS {
            let record = record(&mut rng);
            if primary {
                checksum_log = checksum_log
                    .wrapping_add(record.status)
                    .wrapping_add(record.message.len() as u64);
            }
            opened.append(&body_of(&record)).expect("append");
        }
        opened.flush().expect("flush");
        let write_ns = started.elapsed().as_nanos() as u64;
        let footers = opened.seal().expect("seal");
        let bytes = dir_bytes(&root);
        if primary {
            sealed = footers.len();
            log_root = root.clone();
            log = Some(opened);
        } else {
            let _ = std::fs::remove_dir_all(&root);
        }
        // Peak RSS is monotone, and this shape runs first, so here it is this shape's
        // own high-water mark.
        if primary {
            println!("{name}: peak RSS {}", peak_rss());
        }
        runs.push((name.to_string(), bytes, write_ns));
    }
    let log = log.expect("the first shape kept for reading");
    let log_parts = log.parts().expect("parts");
    let log_bytes = runs[0].1;

    // ---- parquet ------------------------------------------------------------------
    let schema = Arc::new(Schema::new(vec![
        Field::new("ts", DataType::UInt64, false),
        Field::new("level", DataType::Utf8, false),
        Field::new("service", DataType::Utf8, false),
        Field::new("host", DataType::Utf8, false),
        Field::new("message", DataType::Utf8, false),
        Field::new("trace", DataType::Utf8, false),
        Field::new("latency", DataType::Float64, false),
        Field::new("status", DataType::UInt64, false),
        Field::new("tenant", DataType::UInt64, false),
    ]));

    let mut rng;
    let mut checksum_parquet = 0u64;
    let mut variants = Vec::new();
    for row_group in [65_536usize, 4_096] {
        let path = fresh(&format!("rg{row_group}.parquet"));
        if path.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        }
        let file = File::create(&path).expect("create parquet");
        let props = WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(1).unwrap()))
            .set_max_row_group_row_count(Some(row_group))
            .build();
        let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props)).expect("writer");

        rng = Gen(0x9E37_79B9_7F4A_7C15);
        let mut checksum = 0u64;
        let started = Instant::now();
        let mut batch = Vec::with_capacity(row_group);
        for ts in base_ts..base_ts + RECORDS {
            let record = record(&mut rng);
            checksum = checksum
                .wrapping_add(record.status)
                .wrapping_add(record.message.len() as u64);
            batch.push((ts, record));
            if batch.len() == row_group {
                writer.write(&to_batch(&schema, &batch)).expect("write");
                batch.clear();
            }
        }
        if !batch.is_empty() {
            writer.write(&to_batch(&schema, &batch)).expect("write");
        }
        writer.close().expect("close");
        let write_ns = started.elapsed().as_nanos() as u64;
        checksum_parquet = checksum;
        variants.push((row_group, path, write_ns));
    }
    assert_eq!(
        checksum_log, checksum_parquet,
        "same records, same checksum"
    );

    // The part's own files, because the point-read cost below is dominated by opening
    // them, not by decoding a record.
    for part in &log_parts {
        let dir = part.path.parent().unwrap_or(Path::new("."));
        let mut files: Vec<(String, u64)> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let meta = entry.metadata().ok()?;
                        meta.is_file()
                            .then(|| (entry.file_name().to_string_lossy().into_owned(), meta.len()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        println!(
            "part {}: {} records, {} — {}",
            part.bucket,
            part.records,
            part.path.file_name().unwrap_or_default().to_string_lossy(),
            files
                .iter()
                .map(|(name, len)| format!("{name} {} B", len))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // ---- full scan ----------------------------------------------------------------
    let started = Instant::now();
    let mut scanned = 0u64;
    let mut from = 0u64;
    loop {
        let rows = log.read(from, 1_000_000).expect("read");
        if rows.is_empty() {
            break;
        }
        scanned += rows.len() as u64;
        for (_, offset, body) in &rows {
            black_box(body.get(T_STATUS));
            black_box(body.len());
            from = offset + 1;
        }
    }
    let scan_log_ns = started.elapsed().as_nanos() as u64;

    let mut scans = Vec::new();
    for (row_group, path, _) in &variants {
        let file = File::open(path).expect("open");
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("builder")
            .with_batch_size(65_536)
            .build()
            .expect("build");
        let started = Instant::now();
        let mut rows = 0u64;
        for batch in reader {
            let batch = batch.expect("batch");
            rows += batch.num_rows() as u64;
            black_box(batch.column(7));
        }
        scans.push((*row_group, started.elapsed().as_nanos() as u64, rows));
    }

    // ---- one record ---------------------------------------------------------------
    let mut offsets = Vec::with_capacity(POINT_READS);
    let mut rng = Gen(0x0123_4567_89AB_CDEF);
    for _ in 0..POINT_READS {
        offsets.push(rng.next() % RECORDS);
    }
    let started = Instant::now();
    for offset in &offsets {
        let rows = log.read(*offset, 1).expect("point read");
        black_box(rows.len());
    }
    let point_log_ns = started.elapsed().as_nanos() as u64;

    // Where the time goes in one point read, so the number above can be explained rather
    // than admired.
    let started = Instant::now();
    for _ in 0..POINT_READS {
        log.parts().expect("parts");
    }
    let breakdown_parts = started.elapsed().as_nanos() as u64 / POINT_READS as u64;
    let started = Instant::now();
    for _ in 0..POINT_READS {
        std::fs::File::open(&log_parts[0].path).ok();
    }
    let breakdown_open = started.elapsed().as_nanos() as u64 / POINT_READS as u64;

    let mut points = Vec::new();
    for (row_group, path, _) in &variants {
        let groups = RECORDS.div_ceil(*row_group as u64);
        let started = Instant::now();
        for offset in &offsets {
            let row = offset % RECORDS;
            let group = (row / *row_group as u64) as usize;
            debug_assert!((group as u64) < groups);
            let file = File::open(path).expect("open");
            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .expect("builder")
                .with_row_groups(vec![group])
                .with_batch_size(*row_group)
                .build()
                .expect("build");
            let batch = reader.into_iter().next().expect("batch").expect("batch");
            let within = (row - (group as u64 * *row_group as u64)) as usize;
            black_box(batch.column(4).as_any());
            black_box(within);
        }
        points.push((*row_group, started.elapsed().as_nanos() as u64));
    }

    // ---- the table ----------------------------------------------------------------
    let per_record = |ns: u64| ns as f64 / RECORDS as f64;
    println!(
        "{:<34} {:>12} {:>10} {:>9} {:>12}",
        "writer", "bytes", "B/record", "MB/s", "write ns/rec"
    );
    for (name, bytes, ns) in &runs {
        println!(
            "{:<34} {:>12} {:>10.1} {:>9.1} {:>12.0}",
            name,
            bytes,
            *bytes as f64 / RECORDS as f64,
            mb(*bytes) / (*ns as f64 / 1e9),
            per_record(*ns)
        );
    }
    for (row_group, path, ns) in &variants {
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!(
            "{:<34} {:>12} {:>10.1} {:>9.1} {:>12.0}",
            format!("parquet ({} rg rows)", row_group),
            bytes,
            bytes as f64 / RECORDS as f64,
            mb(bytes) / (*ns as f64 / 1e9),
            per_record(*ns)
        );
    }
    println!(
        "\nlog: {} part(s), {} sealed footer(s), {:.1} MB. Baseline: {} ns/record to generate, {} ns with the body built.",
        log_parts.len(),
        sealed,
        mb(log_bytes),
        gen_ns / RECORDS,
        gen_body_ns / RECORDS
    );

    println!(
        "\n{:<28} {:>12} {:>14}",
        "reader (all records)", "records", "M records/s"
    );
    println!(
        "{:<28} {:>12} {:>14.2}",
        "okm-series",
        scanned,
        scanned as f64 / (scan_log_ns as f64 / 1e9) / 1e6
    );
    for (row_group, ns, rows) in &scans {
        println!(
            "{:<28} {:>12} {:>14.2}",
            format!("parquet ({} rg rows)", row_group),
            rows,
            *rows as f64 / (*ns as f64 / 1e9) / 1e6
        );
    }

    println!("\n{:<28} {:>14}", "one record", "µs/read");
    println!(
        "{:<28} {:>14.1}",
        "okm-series read(offset, 1)",
        point_log_ns as f64 / POINT_READS as f64 / 1e3
    );
    for (row_group, ns) in &points {
        println!(
            "{:<28} {:>14.1}",
            format!("parquet ({} rg rows)", row_group),
            *ns as f64 / POINT_READS as f64 / 1e3
        );
    }

    println!(
        "\n{:<28} {:>14}\n{:<28} {:>14.1}\n{:<28} {:>14.1}",
        "breakdown, per call",
        "µs",
        "log.parts()",
        breakdown_parts as f64 / 1e3,
        "File::open(part)",
        breakdown_open as f64 / 1e3
    );

    let _ = std::fs::remove_dir_all(&log_root);
    for (_, path, _) in &variants {
        let _ = std::fs::remove_file(path);
    }
}

fn to_batch(schema: &Arc<Schema>, rows: &[(u64, Record)]) -> RecordBatch {
    let ts: Vec<u64> = rows.iter().map(|(ts, _)| *ts).collect();
    let level: Vec<&str> = rows.iter().map(|(_, r)| r.level).collect();
    let service: Vec<&str> = rows.iter().map(|(_, r)| r.service).collect();
    let host: Vec<&str> = rows.iter().map(|(_, r)| r.host).collect();
    let message: Vec<&str> = rows.iter().map(|(_, r)| r.message.as_str()).collect();
    let trace: Vec<&str> = rows.iter().map(|(_, r)| r.trace.as_str()).collect();
    let latency: Vec<f64> = rows.iter().map(|(_, r)| r.latency).collect();
    let status: Vec<u64> = rows.iter().map(|(_, r)| r.status).collect();
    let tenant: Vec<u64> = rows.iter().map(|(_, r)| r.tenant).collect();

    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ts)),
        Arc::new(StringArray::from(level)),
        Arc::new(StringArray::from(service)),
        Arc::new(StringArray::from(host)),
        Arc::new(StringArray::from(message)),
        Arc::new(StringArray::from(trace)),
        Arc::new(Float64Array::from(latency)),
        Arc::new(UInt64Array::from(status)),
        Arc::new(UInt64Array::from(tenant)),
    ];
    RecordBatch::try_new(schema.clone(), columns).expect("batch")
}
