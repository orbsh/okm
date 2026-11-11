//! `okm-lake` — the exported lake: project the change log into Parquet, then keep it.
//!
//! ```text
//! okm-lake         --root <log dir> --out <lake dir> [--consumer NAME] [--max-segments N]
//!                  [--table <hex header>] [--partition COLUMN] [--max-rows-per-file N]
//!                  [--fold-after-files N] [--fold-after-bytes B]
//! okm-lake condense --out <lake dir> [--table <hex header>]
//! okm-lake vacuum   --out <lake dir> --keep-versions N [--table <hex header>] [--dry-run]
//! okm-lake status   --out <lake dir> [--table <hex header>] [--rows]
//! ```
//!
//! Nothing here knows the application: `--root` is the directory a capturing store wrote
//! (`ChangelogStore::dir`), `--out` is where windows land, and `--consumer` names the reader
//! so two readers keep two positions (ADR-0056 §8). `drain` is the default verb; the other
//! three are about what a reader is handed — `condense` folds change files into current state,
//! `vacuum` deletes the files no kept version can see, `status` says what is there
//! (ADR-0058 § Amendment).
//!
//! The LOG's own tool is `okm-log`: status of the segments and the claim, compaction, and
//! replay into a store. Two artifacts, two tools — a migration never needs the columnar stack.

use okm_lake::{DrainConfig, Report, drain, drain_table};
use std::path::{Path, PathBuf};

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
    if args.first().is_some_and(|a| a == "status") {
        args.remove(0);
        return status(&args);
    }
    if args.first().is_some_and(|a| a == "condense") {
        args.remove(0);
        return condense(&args);
    }
    if args.first().is_some_and(|a| a == "vacuum") {
        args.remove(0);
        return vacuum(&args);
    }
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        println!(
            "okm-lake [--root <log dir> --out <lake dir>] | <verb>\n\
             \n\
             (no verb)  --root <log dir> --out <lake dir> [--consumer NAME] [--max-segments N]\n\
             \x20          [--table <hex header>] [--partition COLUMN] [--max-rows-per-file N]\n\
             \x20          [--fold-after-files N] [--fold-after-bytes B]\n\
             condense   --out <lake dir> [--table <hex header>]\n\
             vacuum     --out <lake dir> --keep-versions N [--table <hex header>] [--dry-run]\n\
             status     --out <lake dir> [--table <hex header>] [--rows]\n\
             \n\
             The lake, not the log: `okm-log` has status/compact/replay for the segments."
        );
        return Ok(0);
    }
    if args.first().is_some_and(|a| !a.starts_with('-')) {
        return Err(format!(
            "unknown verb `{}` (okm-lake: condense, vacuum, status; okm-log: status, compact, replay)",
            args[0]
        ));
    }
    drain_verb(&args)
}

/// The default verb: project every table's windows into the lake, with the options that say
/// how the lake is shaped (partitions, row limit) and when it folds itself.
fn drain_verb(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut consumer = String::from("default");
    let mut max_segments = 16usize;
    let mut table: Option<String> = None;
    let mut fold_after_files: usize = 0;
    let mut fold_after_bytes: u64 = 0;
    let mut partition: Option<String> = None;
    let mut max_rows_per_file: usize = 0;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
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
            "--fold-after-files" => {
                fold_after_files = value()?
                    .parse()
                    .map_err(|_| "--fold-after-files takes a number".to_string())?
            }
            "--partition" => partition = Some(value()?),
            "--max-rows-per-file" => {
                max_rows_per_file = value()?
                    .parse()
                    .map_err(|e| format!("--max-rows-per-file: {e}"))?
            }
            "--fold-after-bytes" => {
                fold_after_bytes = value()?
                    .parse()
                    .map_err(|_| "--fold-after-bytes takes a number".to_string())?
            }
            "-h" | "--help" => {
                println!(
                    "okm-lake --root <log dir> --out <lake dir> [--consumer NAME] \
                     [--max-segments N] [--table <hex header>] \
                     [--partition COLUMN] [--max-rows-per-file N] [--fold-after-files N]
                     [--fold-after-bytes B]"
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
    cfg.partition = partition;
    cfg.max_rows_per_file = max_rows_per_file;
    if fold_after_files > 0 || fold_after_bytes > 0 {
        cfg.fold = Some(okm_lake::FoldPolicy {
            after_files: fold_after_files,
            after_bytes: fold_after_bytes,
        });
    }

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

/// The lake side of `status`: one line per table, from the log and the manifests alone.
fn status(args: &[String]) -> Result<i32, String> {
    let mut out: Option<PathBuf> = None;
    let mut count_rows = false;
    let mut table: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" => out = Some(PathBuf::from(value()?)),
            "--rows" => count_rows = true,
            "--table" => table = Some(value()?),
            "-h" | "--help" => {
                println!("okm-lake status --out <lake dir> [--table <hex header>] [--rows]");
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let Some(out) = out else {
        return Err(
            "--out is required (the exported lake; `okm-log status --root` is the log's side)"
                .to_string(),
        );
    };
    lake_status(&out, table.as_deref(), count_rows)
}

fn lake_status(out: &Path, only: Option<&str>, count_rows: bool) -> Result<i32, String> {
    for dir in lake_tables(out, only)? {
        let status = okm_lake::status::status(&dir, count_rows)?;
        let rows = status
            .change_rows
            .map(|n| match status.deletes {
                Some(deletes) => format!("{n} change row(s), {deletes} removal(s)"),
                None => format!("{n} change row(s)"),
            })
            .unwrap_or_else(|| "change rows unknown (a file has no manifest)".to_string());
        println!("{}", dir.display());
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

/// `vacuum --out <lake dir> --keep-versions N [--table <hex header>] [--dry-run]`
///
/// Delete the files NO kept version references — the bytes a fold leaves behind. `N` counts
/// versions, not days, and `0` keeps only the current version.
fn vacuum(args: &[String]) -> Result<i32, String> {
    let mut out: Option<PathBuf> = None;
    let mut table: Option<String> = None;
    let mut keep: Option<u64> = None;
    let mut dry_run = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" => out = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "--keep-versions" => {
                keep = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--keep-versions takes a number".to_string())?,
                )
            }
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!(
                    "okm-lake vacuum --out <lake dir> --keep-versions N \
                     [--table <hex header>] [--dry-run]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let out = out.ok_or("--out is required (the directory windows were drained into)")?;
    // Deleting files is the one verb that must be told how much history to keep: a default
    // here would be a guess about someone else's readers.
    let keep =
        keep.ok_or("--keep-versions is required: the number of versions to keep readable")?;
    let mut deleted = 0usize;
    let mut bytes = 0u64;
    let mut kept = 0usize;
    for dir in lake_tables(&out, table.as_deref())? {
        let report = okm_lake::vacuum::vacuum(&dir, keep, dry_run)?;
        println!(
            "{}: {} file(s), {} byte(s) {}; {} kept from v{} on",
            dir.display(),
            report.deleted,
            report.bytes,
            if dry_run { "would go" } else { "deleted" },
            report.kept,
            report.floor
        );
        deleted += report.deleted;
        bytes += report.bytes;
        kept += report.kept;
    }
    println!(
        "{} {deleted} file(s), {bytes} byte(s); {kept} kept",
        if dry_run { "would delete" } else { "deleted" }
    );
    Ok(0)
}

/// `condense --out <lake dir> [--table <hex header>]`
///
/// Fold each exported table into one current-state file. Idempotent in meaning, not in
/// commits: a table with one file is already its own current state and is left alone.
fn condense(args: &[String]) -> Result<i32, String> {
    let mut out: Option<PathBuf> = None;
    let mut table: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" => out = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "-h" | "--help" => {
                println!("okm-lake condense --out <lake dir> [--table <hex header>]");
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let out = out.ok_or("--out is required (the directory windows were drained into)")?;
    let mut folded = 0usize;
    let mut skipped = 0usize;
    for dir in lake_tables(&out, table.as_deref())? {
        match okm_lake::read::condense(&dir) {
            Ok(report) => {
                println!(
                    "{}: v{} holds {} row(s); replaced {} file(s), left {} alone",
                    dir.display(),
                    report.version,
                    report.rows,
                    report.replaced,
                    report.untouched
                );
                folded += 1;
            }
            // A table with one file has nothing to fold — that is a state, not a failure.
            Err(e) if e.what.contains("nothing to fold") => skipped += 1,
            Err(e) => eprintln!("{}: {e}", dir.display()),
        }
    }
    println!("folded {folded}, already current {skipped}");
    Ok(0)
}

/// The exported tables under a lake directory: a subdirectory that has a Delta log.
fn lake_tables(out: &Path, only: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(out).map_err(|e| format!("{}: {e}", out.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path.join("_delta_log").is_dir() {
            continue;
        }
        if let Some(name) = only
            && path.file_name().and_then(|n| n.to_str()) != Some(name)
        {
            continue;
        }
        dirs.push(path);
    }
    dirs.sort();
    Ok(dirs)
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
