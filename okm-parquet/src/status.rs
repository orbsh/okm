//! What a lake directory holds, without reading a payload.
//!
//! Every number here comes from the log, the file sizes, and the manifests the drain writes
//! beside each window — so `status` is cheap on a table with a million rows. Rows of CURRENT
//! state are the one exception: they mean folding the table, which is why the caller has to
//! ask for them (`--rows`).

use std::path::Path;

use crate::WindowManifest;
use crate::{delta, read};

/// One exported table, as an operator sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableStatus {
    /// The newest version in the log.
    pub latest_version: u64,
    /// Files the table reads now.
    pub live_files: usize,
    /// Bytes those files take.
    pub live_bytes: u64,
    /// Change rows across the live files, summed from their manifests — `None` when a file
    /// has none (an export written before manifests, or a fold from before they carried one).
    pub change_rows: Option<usize>,
    /// Change rows carrying `okm_op = del`, from the same manifests.
    pub deletes: Option<usize>,
    /// Rows of CURRENT state — only when the caller asked, since it folds the table.
    pub live_rows: Option<usize>,
    /// True when the table's newest file is the output of a fold.
    pub folded: bool,
    /// Files the log removed that are still on disk: what a vacuum would reclaim.
    pub stale_files: usize,
    /// Bytes those files take.
    pub stale_bytes: u64,
}

/// Read one table's status. `count_rows` folds the table to answer "how many rows does it
/// have now"; without it nothing but the key columns of nothing is read — no payload at all.
pub fn status(table_dir: &Path, count_rows: bool) -> Result<TableStatus, String> {
    let live = delta::files_in_order(table_dir)?;
    let (gone, latest_version) = delta::removed_files(table_dir)?;

    let mut status = TableStatus {
        latest_version,
        live_files: live.len(),
        ..TableStatus::default()
    };
    let mut rows = Some(0usize);
    let mut deletes = Some(0usize);
    for rel in &live {
        let path = table_dir.join(rel);
        status.live_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match manifest_of(&path) {
            Some(manifest) => {
                rows = rows.map(|n| n + manifest.rows);
                deletes = deletes.map(|n| n + manifest.deletes);
            }
            None => {
                // One file without a manifest makes the sum an undercount, and a number that
                // silently means "some of it" is worse than saying it is unknown.
                rows = None;
                deletes = None;
            }
        }
    }
    status.change_rows = rows;
    status.deletes = deletes;
    // The newest file is the one a fold wrote: a fold is incremental, so older files
    // can still be beside it and are none of this question's business.
    status.folded = live.last().is_some_and(|newest| newest.contains("-fold-"));

    for (rel, _) in gone {
        let path = table_dir.join(rel);
        if let Ok(meta) = std::fs::metadata(&path) {
            status.stale_files += 1;
            status.stale_bytes += meta.len();
        }
    }

    if count_rows {
        status.live_rows = Some(
            read::current_state(table_dir)
                .map_err(|e| e.what)?
                .num_rows(),
        );
    }
    Ok(status)
}

/// The manifest beside a data file, if it has one and it parses.
pub fn manifest_of(data_file: &Path) -> Option<WindowManifest> {
    let path = data_file.with_extension("manifest.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}
