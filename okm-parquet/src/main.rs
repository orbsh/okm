//! `okm-parquet` — project a change log into Parquet, with no link to the app.
//!
//! ```text
//! okm-parquet --root <log dir> --out <lake dir> [--consumer NAME]
//!             [--max-segments N] [--table <hex header>]
//! ```
//!
//! Nothing here knows the application: `--root` is the directory a capturing store
//! wrote (`ChangelogStore::dir`), `--out` is where windows land, and `--consumer`
//! names the reader so two readers keep two positions (ADR-0056 §8).

use okm_core::SharedVirtualStorage;
use okm_parquet::replay::{replay_all, replay_table};
use okm_parquet::{Report, DrainConfig, drain, drain_table};
use std::path::PathBuf;

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Two verbs, one binary: `parquet` (the default) projects windows, `replay`
    // writes the log back into a store built by this build. Both read the same table
    // directory and neither needs the application that wrote it (ADR-0056 §8).
    if args.first().is_some_and(|a| a == "replay") {
        args.remove(0);
        return replay(&args);
    }
    let mut root: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut consumer = String::from("default");
    let mut max_segments = 16usize;
    let mut table: Option<String> = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--root" | "--log" => root = Some(PathBuf::from(value()?)),
            "--out" | "--lake" => out = Some(PathBuf::from(value()?)),
            "--consumer" => consumer = value()?,
            "--table" => table = Some(value()?),
            "--max-segments" => {
                max_segments = value()?
                    .parse()
                    .map_err(|_| "--max-segments takes a number".to_string())?
            }
            "-h" | "--help" => {
                println!(
                    "okm-parquet --root <log dir> --out <lake dir> [--consumer NAME] \
                     [--max-segments N] [--table <hex header>]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    let mut cfg = DrainConfig::new(
        out.ok_or("--out is required (where window files land)")?,
        consumer,
    );
    cfg.max_segments = max_segments;

    let report = match table {
        // One table, named by the header in hex — the directory name, which IS the
        // table's identity on disk.
        Some(header) => {
            let mut report = Report::default();
            let dir = okm_core::changelog::window::dir_of(&root, &decode_hex(&header)?);
            drain_table(&dir, &cfg, &mut report).map_err(|e| e.to_string())?;
            report
        }
        None => drain(&root, &cfg).map_err(|e| e.to_string())?,
    };

    for file in &report.files {
        println!("{}", file.display());
    }
    println!(
        "tables {}, segments {}, rows {}, segment-only {}",
        report.tables, report.segments, report.rows, report.segment_only
    );
    Ok(0)
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("`{hex}`: odd-length hex header"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("`{hex}`: not a hex header"))
        })
        .collect()
}
/// `replay --root <log dir> --store <dir> [--table <hex header>]`
///
/// The target is a store this build opens — a fjall directory — and the tables are
/// declared from the sidecar beside each segment, so no Rust types are needed. The
/// rows go through the target's write path: index entries, dynamic segments and
/// ledgers are rebuilt, and vocabulary ids are the target's own (ADR-0056 §5).
fn replay(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut store_dir: Option<PathBuf> = None;
    let mut table: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--root" | "--log" => root = Some(PathBuf::from(value()?)),
            "--store" | "--target" => store_dir = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "-h" | "--help" => {
                println!("okm-parquet replay --root <log dir> --store <dir> [--table <hex header>]");
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    let store_dir = store_dir.ok_or("--store is required (the target store's directory)")?;
    std::fs::create_dir_all(&store_dir).map_err(|e| e.to_string())?;
    let store = okm_core::FjallStore::open(&store_dir, "okm").map_err(|e| e.to_string())?;

    let new_collection = |ns: u16, schema: &okm_core::schema::CollectionSchema| {
        okm_dynamic::DynamicCollection::new(store.shared_handle(), ns, schema.clone(), Vec::new())
    };

    let reports = match table {
        Some(header) => {
            let dir = okm_core::changelog::window::dir_of(&root, &decode_hex(&header)?);
            let schema = okm_core::changelog::window::load_sidecar(&dir).map_err(|e| e.to_string())?;
            let ns = u16::from_be_bytes(decode_hex(&header)?.as_slice().try_into().map_err(
                |_| format!("`{header}`: a header is one ns (two bytes)"),
            )?);
            let collection = new_collection(ns, &schema);
            vec![replay_table(&dir, &collection)?]
        }
        None => replay_all(&root, new_collection)?,
    };

    let mut puts = 0;
    let mut deletes = 0;
    for r in &reports {
        puts += r.puts;
        deletes += r.deletes;
    }
    println!(
        "tables {}, segments {}, puts {}, deletes {}, re-derived {}",
        reports.len(),
        reports.iter().map(|r| r.segments).sum::<usize>(),
        puts,
        deletes,
        reports.iter().map(|r| r.rederived).sum::<usize>(),
    );
    Ok(0)
}
