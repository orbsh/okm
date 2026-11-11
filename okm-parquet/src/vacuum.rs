//! Reclaiming the bytes a fold leaves behind.
//!
//! A `remove` action takes a file out of the TABLE; the file stays in `data/`, because a
//! reader asking for an older version still needs it (`ADR-0058` keeps history addressable).
//! Vacuum is the other half of that decision, and it is a RETENTION decision before it is a
//! verb: it deletes a file only when no version a caller asked to keep can see it.
//!
//! `keep_versions = N` means "everything the last N+1 versions reference stays". So `0` keeps
//! only the current version and deletes every file it does not read — the aggressive setting,
//! spelled out rather than guessed — and `7` keeps a week of folds' worth of files if a fold
//! lands daily.

use std::path::Path;

use crate::delta;

/// What one vacuum did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VacuumReport {
    /// Files deleted from `data/` (or that would be, under `--dry-run`).
    pub deleted: usize,
    /// Bytes those files took.
    pub bytes: u64,
    /// Files the retention window still covers, left alone.
    pub kept: usize,
    /// The oldest version the retention window covers (`0` when the table has one version).
    pub floor: u64,
}

/// Delete the files no kept version references.
///
/// `keep_versions` counts VERSIONS, not days: the log is the clock here, and a table that
/// folds once a week has a different calendar from one that folds hourly.
pub fn vacuum(table_dir: &Path, keep_versions: u64, dry_run: bool) -> Result<VacuumReport, String> {
    let (gone, latest) = delta::removed_files(table_dir)?;
    let floor = latest.saturating_sub(keep_versions);
    let live = delta::files_in_order(table_dir)?.len();
    let mut report = VacuumReport {
        kept: live,
        floor,
        ..VacuumReport::default()
    };
    for (rel, removed_at) in gone {
        // Still visible to a version inside the window: the cheapest way to keep a reader's
        // promise is to keep the file.
        if removed_at > floor {
            report.kept += 1;
            continue;
        }
        let path = table_dir.join(&rel);
        // Already gone (an earlier sweep): the log keeps naming it, the disk does not, and a
        // second vacuum must not report work it did not do.
        let bytes = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if !dry_run {
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            // The manifest the drain wrote beside it describes a file that is gone; leaving
            // it would describe a window nobody can read.
            let manifest = path.with_extension("manifest.json");
            if manifest.exists() {
                std::fs::remove_file(&manifest)
                    .map_err(|e| format!("{}: {e}", manifest.display()))?;
            }
        }
        report.deleted += 1;
        report.bytes += bytes;
    }
    Ok(report)
}
