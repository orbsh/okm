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

use okm_parquet::{DrainConfig, drain, drain_table, Report};
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
    let mut root: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut consumer = String::from("default");
    let mut max_segments = 16usize;
    let mut table: Option<String> = None;

    let mut args = std::env::args().skip(1);
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