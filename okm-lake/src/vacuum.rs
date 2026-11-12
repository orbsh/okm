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
//!
//! The store is asked for sizes and does the deleting: a bucket is a lake too, and a vacuum
//! that only knew how to `remove_file` would be a verb with a storage shape.

use std::path::Path;

use crate::delta;
use crate::store::{LakeStore, table_key};

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
pub fn vacuum(
    store: &dyn LakeStore,
    table: &str,
    keep_versions: u64,
    dry_run: bool,
) -> Result<VacuumReport, String> {
    let (gone, latest) = delta::removed_files(store, table)?;
    let floor = latest.saturating_sub(keep_versions);
    let live = delta::files_in_order(store, table)?.len();
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
        let key = table_key(table, &rel);
        // Already gone (an earlier sweep): the log keeps naming it, the store does not, and a
        // second vacuum must not report work it did not do.
        let Some(entry) = store.head(&key)? else {
            continue;
        };
        if !dry_run {
            store.delete(&key)?;
            // The manifest the drain wrote beside it describes a file that is gone; leaving
            // it would describe a window nobody can read.
            store.delete(&table_key(table, &crate::status::manifest_key(&rel)))?;
        }
        report.deleted += 1;
        report.bytes += entry.size;
    }
    Ok(report)
}

/// [`vacuum`] for a table inside a local lake directory.
pub fn vacuum_at(
    table_dir: &Path,
    keep_versions: u64,
    dry_run: bool,
) -> Result<VacuumReport, String> {
    let (store, table) = delta::lake_of(table_dir)?;
    vacuum(&store, &table, keep_versions, dry_run)
}
