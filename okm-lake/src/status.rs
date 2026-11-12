//! What a lake holds, without reading a payload.
//!
//! Every number here comes from the log, the file sizes, and the manifests the drain writes
//! beside each window — so `status` is cheap on a table with a million rows. Rows of CURRENT
//! state are the one exception: they mean folding the table, which is why the caller has to
//! ask for them (`--rows`).
//!
//! Reads go through the store like every other verb: a lake may be a directory or a bucket,
//! and the numbers are the same either way.

use std::path::Path;

use crate::WindowManifest;
use crate::store::{LakeStore, table_key};
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
    /// Files the log removed that are still in the store: what a vacuum would reclaim.
    pub stale_files: usize,
    /// Bytes those files take.
    pub stale_bytes: u64,
}

/// Read one table's status. `count_rows` folds the table to answer "how many rows does it
/// have now"; without it nothing but the log and the manifests is read — no payload at all.
pub fn status(store: &dyn LakeStore, table: &str, count_rows: bool) -> Result<TableStatus, String> {
    let live = delta::files_in_order(store, table)?;
    let (gone, latest_version) = delta::removed_files(store, table)?;

    let mut status = TableStatus {
        latest_version,
        live_files: live.len(),
        ..TableStatus::default()
    };
    let mut rows = Some(0usize);
    let mut deletes = Some(0usize);
    for rel in &live {
        // The size comes from the store, not from a `stat`: on a bucket that is the only
        // question that has an answer.
        status.live_bytes += store
            .head(&table_key(table, rel))?
            .map(|entry| entry.size)
            .unwrap_or(0);
        match manifest_at(store, table, rel) {
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
        if let Some(entry) = store.head(&table_key(table, &rel))? {
            status.stale_files += 1;
            status.stale_bytes += entry.size;
        }
    }

    if count_rows {
        status.live_rows = Some(
            read::current_state(store, table)
                .map_err(|e| e.what)?
                .num_rows(),
        );
    }
    Ok(status)
}

/// [`status`] for a table inside a local lake directory.
pub fn status_at(table_dir: &Path, count_rows: bool) -> Result<TableStatus, String> {
    let (store, table) = delta::lake_of(table_dir)?;
    status(&store, &table, count_rows)
}

/// The key of the manifest that describes a data file: the same name with `.manifest.json`
/// where `.parquet` was.
pub fn manifest_key(relative: &str) -> String {
    Path::new(relative)
        .with_extension("manifest.json")
        .to_string_lossy()
        .replace('\\', "/")
}

/// The manifest for a live data file, if it has one and it parses.
pub fn manifest_at(store: &dyn LakeStore, table: &str, relative: &str) -> Option<WindowManifest> {
    let bytes = store.get(&table_key(table, &manifest_key(relative))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`manifest_at`] for a data file on a local disk.
pub fn manifest_of(data_file: &Path) -> Option<WindowManifest> {
    let text = std::fs::read_to_string(data_file.with_extension("manifest.json")).ok()?;
    serde_json::from_str(&text).ok()
}
