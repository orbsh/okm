//! `okm-lake` — the exported lake: project the change log into Parquet, then keep it.
//!
//! ```text
//! okm-lake         --root <log dir> --out <lake> [--consumer NAME] [--max-segments N]
//!                  [--table <hex header>] [--partition COLUMN] [--max-rows-per-file N]
//!                  [--fold-after-files N] [--fold-after-bytes B]
//! okm-lake condense --out <lake> [--table <hex header>]
//! okm-lake vacuum   --out <lake> --keep-versions N [--table <hex header>] [--dry-run]
//! okm-lake status   --out <lake> [--table <hex header>] [--rows]
//! ```
//!
//! Nothing here knows the application: `--root` is the directory a capturing store wrote
//! (`ChangelogStore::dir`), `--out` is where windows land, and `--consumer` names the reader
//! so two readers keep two positions (ADR-0056 §8). `drain` is the default verb; the other
//! three are about what a reader is handed — `condense` folds change files into current state,
//! `vacuum` deletes the files no kept version can see, `status` says what is there
//! (ADR-0058 § Amendment).
//!
//! A lake is a directory, a `file://` path, or `s3://bucket/prefix`: the verbs write and read
//! through `okm_lake::store`, so a bucket is not a second implementation of anything
//! (ADR-0061). Credentials come from the environment (`AWS_*`, then `OKM_S3_*`).
//!
//! The LOG's own tool is `okm-log`: status of the segments and the claim, compaction, and
//! replay into a store. Two artifacts, two tools — a migration never needs the columnar stack.

use okm_lake::cli::{self, Verb};
use okm_lake::store::{self, LakeStore, S3Options};
use okm_lake::{DrainConfig, Report};
use std::collections::BTreeSet;

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args)? {
        Verb::Help => {
            println!("{}", cli::USAGE);
            Ok(0)
        }
        Verb::Drain(parsed) => drain_verb(&parsed),
        Verb::Condense(parsed) => condense(&parsed),
        Verb::Vacuum(parsed) => vacuum(&parsed),
        Verb::Status(parsed) => status(&parsed),
    }
}

/// Open the lake a verb was pointed at. Credentials are the environment's business, not the
/// arguments': a command line is a log line waiting to happen.
fn open(lake: &store::LakeUrl) -> Result<Box<dyn LakeStore>, String> {
    store::open_url(lake, Some(S3Options::from_env()))
}

/// The default verb: project every table's windows into the lake, with the options that say
/// how the lake is shaped (partitions, row limit) and when it folds itself.
fn drain_verb(parsed: &cli::DrainArgs) -> Result<i32, String> {
    let lake = open(&parsed.lake)?;
    // The path form's root, when the address is one: `drain_to` takes the lake from its
    // argument, so a bucket leaves this empty rather than pretending a url is a path.
    let out = match &parsed.lake {
        store::LakeUrl::Dir(path) => path.clone(),
        store::LakeUrl::S3 { .. } => std::path::PathBuf::new(),
    };
    let mut cfg = DrainConfig::new(out, &parsed.consumer);
    cfg.max_segments = parsed.max_segments;
    cfg.partition = parsed.partition.clone();
    cfg.max_rows_per_file = parsed.max_rows_per_file;
    cfg.fold = parsed.fold;

    let report = match parsed.table.as_deref() {
        // One table, named by the header in hex — the directory name, which IS the table's
        // identity on the log's side.
        Some(header) => {
            let mut report = Report::default();
            let dir = okm_core::changelog::window::dir_of(&parsed.root, &decode_hex(header)?);
            okm_lake::drain_table_to(&*lake, &dir, &cfg, &mut report).map_err(|e| e.to_string())?;
            report
        }
        None => okm_lake::drain_to(&*lake, &parsed.root, &cfg).map_err(|e| e.to_string())?,
    };

    // Keys, not paths: a bucket has no path to print, and a directory's key IS its path under
    // the lake root.
    for key in &report.keys {
        println!("{key}");
    }
    println!(
        "tables {}, segments {}, rows {}, deletes {}, segment-only {}, derived-deletes {}, \
         folds {}, fold-skipped {}",
        report.tables,
        report.segments,
        report.rows,
        report.deletes,
        report.open_fields,
        report.derived_deletes,
        report.folds,
        report.fold_skipped
    );
    Ok(0)
}

/// The lake side of `status`: one block per table, from the log and the manifests alone.
fn status(parsed: &cli::StatusArgs) -> Result<i32, String> {
    let lake = open(&parsed.lake)?;
    for table in lake_tables(&*lake, parsed.table.as_deref())? {
        let status = okm_lake::status::status(&*lake, &table, parsed.rows)?;
        let rows = status
            .change_rows
            .map(|n| match status.deletes {
                Some(deletes) => format!("{n} change row(s), {deletes} removal(s)"),
                None => format!("{n} change row(s)"),
            })
            .unwrap_or_else(|| "change rows unknown (a file has no manifest)".to_string());
        println!("{table}");
        println!(
            "  v{}: {} live file(s), {}, {}, {}",
            status.latest_version,
            status.live_files,
            human(status.live_bytes),
            rows,
            if status.folded {
                "folded"
            } else {
                "change files as drained"
            },
        );
        if let Some(live) = status.live_rows {
            println!("  current state: {live} row(s)");
        }
        if status.stale_files > 0 {
            println!(
                "  reclaimable: {} file(s), {} — a vacuum with the right --keep-versions takes them",
                status.stale_files,
                human(status.stale_bytes)
            );
        }
    }
    Ok(0)
}

/// `vacuum --out <lake> --keep-versions N [--table <hex header>] [--dry-run]`
///
/// Delete the files NO kept version references — the bytes a fold leaves behind. `N` counts
/// versions, not days, and `0` keeps only the current version.
fn vacuum(parsed: &cli::VacuumArgs) -> Result<i32, String> {
    let lake = open(&parsed.lake)?;
    let mut deleted = 0usize;
    let mut bytes = 0u64;
    let mut kept = 0usize;
    for table in lake_tables(&*lake, parsed.table.as_deref())? {
        let report =
            okm_lake::vacuum::vacuum(&*lake, &table, parsed.keep_versions, parsed.dry_run)?;
        println!(
            "{}: {} file(s), {} byte(s) {}; {} kept from v{} on",
            table,
            report.deleted,
            report.bytes,
            if parsed.dry_run {
                "would go"
            } else {
                "deleted"
            },
            report.kept,
            report.floor
        );
        deleted += report.deleted;
        bytes += report.bytes;
        kept += report.kept;
    }
    println!(
        "{} {deleted} file(s), {bytes} byte(s); {kept} kept",
        if parsed.dry_run {
            "would delete"
        } else {
            "deleted"
        }
    );
    Ok(0)
}

/// `condense --out <lake> [--table <hex header>]`
///
/// Fold each exported table into one current-state file. Idempotent in meaning, not in
/// commits: a table with one file is already its own current state and is left alone.
fn condense(parsed: &cli::LakeArgs) -> Result<i32, String> {
    let lake = open(&parsed.lake)?;
    let mut folded = 0usize;
    let mut skipped = 0usize;
    for table in lake_tables(&*lake, parsed.table.as_deref())? {
        match okm_lake::read::condense(&*lake, &table) {
            Ok(report) => {
                println!(
                    "{}: v{} holds {} row(s); replaced {} file(s), left {} alone",
                    table, report.version, report.rows, report.replaced, report.untouched
                );
                folded += 1;
            }
            // A table with one file has nothing to fold — that is a state, not a failure.
            Err(e) if e.what.contains("nothing to fold") => skipped += 1,
            Err(e) => eprintln!("{table}: {e}"),
        }
    }
    println!("folded {folded}, already current {skipped}");
    Ok(0)
}

/// The exporter's tables in a lake: a name whose keys include a Delta log.
///
/// Found by listing, not by walking a directory: on a bucket the keys are the only truth, and
/// a local directory lists the same way through [`LocalFs`](okm_lake::store::LocalFs).
fn lake_tables(store: &dyn LakeStore, only: Option<&str>) -> Result<Vec<String>, String> {
    let mut tables: BTreeSet<String> = BTreeSet::new();
    for entry in store.list("")? {
        if let Some((table, rest)) = entry.path.split_once('/')
            && rest.starts_with("_delta_log/")
        {
            tables.insert(table.to_string());
        }
    }
    Ok(tables
        .into_iter()
        .filter(|table| only.is_none_or(|wanted| wanted == table))
        .collect())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("`{hex}`: odd-length hex header"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| format!("`{hex}`: not a hex header"))
        })
        .collect()
}
