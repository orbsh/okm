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
//!
//! Three maintenance verbs read the same directory and need no types either: `status`
//! reports what the log holds and who holds it, `compact` checkpoints it, and `replay`
//! writes it into a store built by this build. `compact` is the one verb that needs a
//! second argument — the STORE — because a baseline is a snapshot of state, and state
//! lives in the store, not in the log.

use okm_core::SharedVirtualStorage;
use okm_core::changelog::replay::{TableRun, replay_and_reconcile, replay_table};
use okm_parquet::{DrainConfig, Report, drain, drain_table};
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
    if args.first().is_some_and(|a| a == "status") {
        args.remove(0);
        return status(&args);
    }
    if args.first().is_some_and(|a| a == "compact") {
        args.remove(0);
        return compact(&args);
    }
    let mut root: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut consumer = String::from("default");
    let mut max_segments = 16usize;
    let mut table: Option<String> = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
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
        report.tables, report.segments, report.rows, report.open_fields
    );
    Ok(0)
}

/// `status --root <log dir> [--table <hex header>]`
///
/// What a log holds and who holds it, in the terms that decide anything: the segments,
/// the bytes the newest baseline does not cover, how old that covering point is, and the
/// claim (ADR-0056 §6). Read-only; it opens nothing and writes nothing.
fn status(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut table: Option<String> = None;
    let mut after_bytes = okm_core::changelog::CompactPolicy::default().after_bytes;
    let mut after_secs = okm_core::changelog::CompactPolicy::default().after_secs;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--root" | "--log" => root = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "--after-bytes" => {
                after_bytes = value()?
                    .parse()
                    .map_err(|_| "--after-bytes takes a number".to_string())?
            }
            "--after-secs" => {
                after_secs = value()?
                    .parse()
                    .map_err(|_| "--after-secs takes a number".to_string())?
            }
            "-h" | "--help" => {
                println!(
                    "okm-parquet status --root <log dir> [--table <hex header>] [--after-bytes N] [--after-secs N]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    for dir in selected_dirs(&root, table.as_deref())? {
        let header = okm_core::changelog::window::header_of(&dir).map_err(|e| e.to_string())?;
        print!("{} {}", hex_of(&header), dir.display());
        let shape = okm_core::changelog::log_shape(&dir).map_err(|e| e.to_string())?;
        println!();
        println!(
            "  segments {}, {}, baseline {}, uncovered {}, covering point {}",
            shape.segments,
            human(shape.bytes),
            shape.baseline.as_deref().unwrap_or("none"),
            human(shape.uncovered),
            shape
                .baseline_age_secs
                .map(|age| format!("{age} s old"))
                .unwrap_or_else(|| "nothing to cover yet".to_string()),
        );
        match okm_core::changelog::owner::read(&dir).map_err(|e| e.to_string())? {
            Some(claim) => println!(
                "  claim: {:?} by {} at epoch {} (store {}){}",
                claim.state,
                claim.holder,
                claim.epoch,
                claim.data,
                claim
                    .position
                    .map(|p| format!(", handed over at {p}"))
                    .unwrap_or_default()
            ),
            None => println!("  claim: none (nobody has claimed this log)"),
        }
        let due = shape.uncovered > after_bytes
            || shape.baseline_age_secs.is_some_and(|age| age >= after_secs);
        println!(
            "  policy {}/{} s: {}\n",
            human(after_bytes),
            after_secs,
            if due { "DUE" } else { "idle" }
        );
    }
    Ok(0)
}

/// `compact --root <log dir> --store <data dir> [--keep N] [--table <hex>] [--force]`
///
/// Write one baseline per table from the STORE's state and delete what it covers. Refuses
/// while a fresh claim says someone is writing: two builders must never compact the same
/// log, and a baseline taken from a store that keeps changing would miss rows.
fn compact(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut store_dir: Option<PathBuf> = None;
    let mut table: Option<String> = None;
    let mut keep = 0usize;
    let mut force = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--root" | "--log" => root = Some(PathBuf::from(value()?)),
            "--store" | "--data" => store_dir = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "--keep" => {
                keep = value()?
                    .parse()
                    .map_err(|_| "--keep takes a number".to_string())?
            }
            "--force" => force = true,
            "-h" | "--help" => {
                println!(
                    "okm-parquet compact --root <log dir> --store <data dir> [--keep N] \
                     [--table <hex header>] [--force]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    let store_dir =
        store_dir.ok_or("--store is required (the data directory the log's store lives in)")?;
    if overlapping(&root, &store_dir)? {
        return Err(separate_dir_error(&root, &store_dir));
    }
    let store = open_store(&store_dir)?;
    let cfg = okm_core::changelog::ChangelogConfig::new(&root);

    for dir in selected_dirs(&root, table.as_deref())? {
        let header = okm_core::changelog::window::header_of(&dir).map_err(|e| e.to_string())?;
        if !force {
            let fresh = std::fs::metadata(okm_core::changelog::owner::path(&dir))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age.as_secs() < 30);
            if let Some(claim) =
                okm_core::changelog::owner::read(&dir).map_err(|e| e.to_string())?
                && claim.state == okm_core::changelog::owner::State::Writing
                && fresh
            {
                return Err(format!(
                    "{}: `{}` is writing this log right now — stop the application, or \
                     pass --force if you know it is gone",
                    dir.display(),
                    claim.holder
                ));
            }
        }
        // The baseline's own declaration comes from a segment already there: same table,
        // same layout, same fingerprint, other kind.
        let Some(meta) = newest_segment(&dir)? else {
            println!("{}: no segments, nothing to compact", dir.display());
            continue;
        };
        let ops = okm_core::changelog::baseline_ops(&store, &header).map_err(|e| e.to_string())?;
        let Some((first, last)) = okm_core::changelog::write_baseline(
            &cfg,
            &header,
            meta.with_kind(okm_core::changelog::KIND_BASELINE),
            &ops,
        )
        .map_err(|e| e.to_string())?
        else {
            println!("{}: no baseline was written", dir.display());
            continue;
        };
        let (removed, freed) = okm_core::changelog::prune_below(&root, &header, &first, keep)
            .map_err(|e| e.to_string())?;
        println!(
            "{} {:?}: baseline {last}, removed {removed} segments, freed {}",
            hex_of(&header),
            dir.file_name().unwrap_or_default(),
            human(freed)
        );
    }
    Ok(0)
}

/// The table directories a verb works on, in name order.
fn selected_dirs(root: &std::path::Path, table: Option<&str>) -> Result<Vec<PathBuf>, String> {
    match table {
        Some(header) => Ok(vec![okm_core::changelog::window::dir_of(
            root,
            &decode_hex(header)?,
        )]),
        None => {
            let mut dirs =
                okm_core::changelog::window::table_dirs(root).map_err(|e| e.to_string())?;
            dirs.sort();
            Ok(dirs)
        }
    }
}

/// The newest sealed segment in a table directory — the one whose declaration a baseline
/// inherits.
fn newest_segment(
    dir: &std::path::Path,
) -> Result<Option<okm_core::changelog::SegmentMeta>, String> {
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".segment") {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(prev, _)| name.as_str() > prev.as_str())
        {
            best = Some((name, entry.path()));
        }
    }
    let Some((_, path)) = best else {
        return Ok(None);
    };
    let info = okm_core::changelog::segment_info(&path).map_err(|e| e.to_string())?;
    Ok(Some(info.meta))
}

/// Are these two directories the same tree, or one inside the other?
///
/// The log and the store are two lifecycles, and mixing them in one directory is a
/// deployment mistake nothing else catches: the names cannot collide (segments are
/// `NNNNNNNN.`-prefixed, the claim is `owner`, engine files are the engine's), so the
/// mistake stays silent until someone counts segments in a directory full of engine
/// tables. Checked BEFORE anything is created, so a refusal leaves no trace.
fn overlapping(a: &std::path::Path, b: &std::path::Path) -> Result<bool, String> {
    let a = resolved(a)?;
    let b = resolved(b)?;
    Ok(a == b || a.starts_with(&b) || b.starts_with(&a))
}

fn separate_dir_error(root: &std::path::Path, store: &std::path::Path) -> String {
    format!(
        "`{}` and `{}` are the same tree (or one inside the other): the log and the store \
         must be separate directories",
        root.display(),
        store.display()
    )
}

/// The path as the file system would name it: canonical when it exists, otherwise the
/// absolute form with the nearest existing ancestor resolved (so a directory that is
/// about to be created still compares against one that is already there).
fn resolved(path: &std::path::Path) -> Result<PathBuf, String> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    while let Some(name) = cursor.file_name().map(|n| n.to_os_string()) {
        rest.push(name);
        let Some(parent) = cursor.parent() else {
            break;
        };
        if let Ok(real) = std::fs::canonicalize(parent) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        cursor = parent.to_path_buf();
    }
    Ok(cwd.join(path))
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
/// `replay --root <log dir> --store <dir> [--table <hex header>]`
///
/// The target is a store this build opens — a fjall directory — and the tables are
/// declared from the sidecar beside each segment, so no Rust types are needed. The
/// rows go through the target's write path: index entries, dynamic segments and
/// ledgers are rebuilt, and vocabulary ids are the target's own (ADR-0056 §5).
fn replay(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut store_dir: Option<PathBuf> = None;
    let mut old_dir: Option<PathBuf> = None;
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
            // The store the log came from: with it, replay is followed by the
            // reconciliation scan that repairs appends the log never got.
            "--reconcile" | "--old" => old_dir = Some(PathBuf::from(value()?)),
            "--table" => table = Some(value()?),
            "-h" | "--help" => {
                println!(
                    "okm-parquet replay --root <log dir> --store <dir> \
                     [--reconcile <old store dir>] [--table <hex header>]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    let store_dir = store_dir.ok_or("--store is required (the target store's directory)")?;
    std::fs::create_dir_all(&store_dir).map_err(|e| e.to_string())?;
    if overlapping(&root, &store_dir)? {
        return Err(separate_dir_error(&root, &store_dir));
    }
    let store = okm_core::FjallStore::open(&store_dir, "okm").map_err(|e| e.to_string())?;

    let new_collection = |ns: u16, schema: &okm_core::schema::CollectionSchema| {
        okm_dynamic::DynamicCollection::new(store.shared_handle(), ns, schema.clone(), Vec::new())
    };

    let runs: Vec<TableRun> = match table {
        Some(header) => {
            let dir = okm_core::changelog::window::dir_of(&root, &decode_hex(&header)?);
            let schema =
                okm_core::changelog::window::load_sidecar(&dir).map_err(|e| e.to_string())?;
            let ns = u16::from_be_bytes(
                decode_hex(&header)?
                    .as_slice()
                    .try_into()
                    .map_err(|_| format!("`{header}`: a header is one ns (two bytes)"))?,
            );
            let collection = new_collection(ns, &schema);
            let replay = replay_table(&dir, &collection)?;
            let reconcile = match &old_dir {
                Some(path) => {
                    let old = open_store(path)?;
                    Some(okm_core::changelog::replay::reconcile(
                        &old,
                        &collection,
                        &decode_hex(&header)?,
                        &schema,
                    )?)
                }
                None => None,
            };
            vec![TableRun {
                table: okm_core::changelog::window::file_name(&dir),
                replay,
                reconcile,
            }]
        }
        None => match &old_dir {
            Some(path) => {
                let old = open_store(path)?;
                replay_and_reconcile(&root, new_collection, &old)?
            }
            None => okm_core::changelog::replay::replay_all(&root, new_collection)?,
        },
    };

    let mut puts = 0;
    let mut deletes = 0;
    for run in &runs {
        puts += run.replay.puts;
        deletes += run.replay.deletes;
    }
    println!(
        "tables {}, segments {}, puts {}, deletes {}, re-derived {}",
        runs.len(),
        runs.iter().map(|r| r.replay.segments).sum::<usize>(),
        puts,
        deletes,
        runs.iter().map(|r| r.replay.rederived).sum::<usize>(),
    );
    for run in &runs {
        if let Some(rec) = &run.reconcile {
            println!(
                "{}: reconciled — adopted {}, removed {}, unchanged {}",
                run.table, rec.adopted, rec.removed, rec.unchanged
            );
        }
    }
    Ok(0)
}

/// Open a store directory this build can read (the old store, for reconciliation).
fn open_store(path: &std::path::Path) -> Result<okm_core::FjallStore, String> {
    okm_core::FjallStore::open(path, "okm").map_err(|e| format!("{}: {e}", path.display()))
}
