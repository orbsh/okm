//! What a series log costs: the bytes it keeps, the time to put them there, and the
//! time to read them back.
//!
//! Run with `cargo run -p okm-series --release --example series_tax`. (A criterion
//! benchmark hangs on this machine before any measurement starts, so the numbers come
//! from `Instant` around a fixed workload — same method as `changelog_tax`.)
//!
//! Bodies are built before the timer starts, so what is measured is the log and not
//! the benchmark's own allocation.

use std::error::Error;
use std::time::Instant;

use okm_series::{Body, Config, DictConfig, Log, Precision, Value};

const RECORDS: u64 = 50_000;
const VARIANTS: usize = 256;
const BASE_TS: u64 = 1_700_000_000_000;

fn main() -> Result<(), Box<dyn Error>> {
    println!("{RECORDS} records, 64 KiB blocks, one bucket, {VARIANTS} distinct bodies\n");
    for (workload, bodies) in [
        ("repetitive text", repetitive()),
        ("identity heavy", identity_heavy()),
        ("unique id per record", unique_per_record()),
    ] {
        println!("{workload}");
        println!(
            "  {:<26} {:>10} {:>9} {:>9} {:>10} {:>11}",
            "shape", "bytes", "B/rec", "x smaller", "w ns/rec", "r Mrec/s"
        );
        for (label, config) in [
            ("no compression, dictionary on", config(true, 1, false)),
            ("no compression, dictionary off", config(false, 1, false)),
            ("zstd 1, dictionary on", config(true, 1, true)),
            ("zstd 1, dictionary off", config(false, 1, true)),
            ("zstd 3, dictionary on", config(true, 3, true)),
        ] {
            let (bytes, raw, write_ns, read_mrec_s) = run(&config, &bodies)?;
            println!(
                "  {:<26} {:>10} {:>9.1} {:>9.1} {:>10} {:>11.2}",
                label,
                bytes,
                bytes as f64 / RECORDS as f64,
                raw as f64 / bytes as f64,
                write_ns / RECORDS,
                read_mrec_s
            );
        }
        println!();
    }

    // What one fsync per block costs: same records, larger blocks.
    println!("block size, unique id per record, zstd 1, dictionary on");
    let bodies = unique_per_record();
    for block_kib in [16usize, 64, 256] {
        let mut config = config(true, 1, true);
        config.block_bytes = block_kib * 1024;
        let (bytes, raw, write_ns, _read) = run(&config, &bodies)?;
        let blocks = raw.div_ceil(block_kib as u64 * 1024);
        println!(
            "  {block_kib:>4} KiB blocks: {:>6} ns/record, {blocks:>4} blocks, {bytes:>8} bytes",
            write_ns / RECORDS
        );
    }
    Ok(())
}

fn config(dict: bool, level: i32, compress: bool) -> Config {
    Config {
        precision: Precision::Ms,
        bucket_seconds: 3600,
        block_bytes: 64 * 1024,
        part_bytes: 256 * 1024 * 1024,
        zstd_level: level,
        block_codec: if compress {
            okm_series::Codec::Zstd
        } else {
            okm_series::Codec::Raw
        },
        dict: if dict {
            Some(DictConfig::default())
        } else {
            None
        },
        partition_tag: None,
        clock: Some(std::sync::Arc::new({
            let next = std::sync::atomic::AtomicU64::new(BASE_TS);
            move || next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        })),
    }
}

/// Long messages from a template: what an application log looks like.
fn repetitive() -> Vec<Body> {
    (0..VARIANTS)
        .map(|variant| {
            let mut body = Body::new();
            body.push(1, Value::Str("error".to_string()));
            body.push(
                2,
                Value::Str(format!(
                    "upstream timeout talking to billing-service after 3000ms, attempt {variant}"
                )),
            );
            body.push(3, Value::F64(0.35 + variant as f64 / 1000.0));
            body.push(4, Value::U64(1_000_000 + variant as u64));
            body
        })
        .collect()
}

/// Short strings from small closed sets: what a request record looks like, and where
/// a dictionary should earn its keep.
fn identity_heavy() -> Vec<Body> {
    const SERVICES: [&str; 8] = [
        "billing", "search", "identity", "gateway", "ledger", "mailer", "profile", "webhook",
    ];
    const REGIONS: [&str; 4] = ["eu-west", "us-east", "ap-south", "sa-east"];
    const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    (0..VARIANTS)
        .map(|variant| {
            let mut body = Body::new();
            body.push(1, Value::Str(METHODS[variant % METHODS.len()].to_string()));
            body.push(
                2,
                Value::Str(SERVICES[variant % SERVICES.len()].to_string()),
            );
            body.push(3, Value::Str(REGIONS[variant % REGIONS.len()].to_string()));
            body.push(4, Value::U64(200 + (variant % 5) as u64));
            body.push(5, Value::U64(1_700_000 + (variant as u64) * 7));
            body
        })
        .collect()
}

fn run(config: &Config, bodies: &[Body]) -> Result<(u64, u64, u64, f64), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let mut log = Log::open(dir.path(), config.clone())?;
    let raw: u64 = (0..RECORDS)
        .map(|index| bodies[index as usize % bodies.len()].encode().len() as u64)
        .sum();
    let start = Instant::now();
    for index in 0..RECORDS {
        log.append(&bodies[index as usize % bodies.len()])?;
    }
    log.seal()?;
    let write_ns = start.elapsed().as_nanos() as u64;

    let mut bytes = 0u64;
    for part in log.parts()? {
        bytes += part.bytes;
        for extension in ["dict", "idx"] {
            if let Ok(meta) = std::fs::metadata(part.path.with_extension(extension)) {
                bytes += meta.len();
            }
        }
    }

    let start = Instant::now();
    let rows = log.read(0, RECORDS as usize)?;
    let read_s = start.elapsed().as_secs_f64();
    if rows.len() != RECORDS as usize {
        return Err(format!("read back {} of {RECORDS}", rows.len()).into());
    }
    let read_mrec_s = RECORDS as f64 / read_s / 1_000_000.0;
    Ok((bytes, raw, write_ns, read_mrec_s))
}

/// One body per record, each carrying its own id: the shape a real application log has,
/// where whole-body deduplication cannot help and only field-level repetition can.
fn unique_per_record() -> Vec<Body> {
    (0..RECORDS)
        .map(|index| {
            let mut body = Body::new();
            body.push(1, Value::Str("info".to_string()));
            body.push(2, Value::Str("billing".to_string()));
            body.push(3, Value::Str("eu-west".to_string()));
            body.push(4, Value::U64(index));
            body.push(
                5,
                Value::Str(format!(
                    "order {} settled against invoice {}",
                    index * 3 + 7,
                    index * 11 + 1
                )),
            );
            body
        })
        .collect()
}
