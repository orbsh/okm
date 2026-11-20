//! The reader half of a lake baseline: put the lake's version back, then hand back the
//! number the tail starts above (ADR-0071 §1).
//!
//! A log directory whose compaction pointed at the lake holds no state of its own — that is
//! the trade. Restoring it is two steps, and this is the first: the pointer is verified, the
//! version it names is read AT that version, and its rows go through the collection's own
//! write path (so indexes, dynamic segments and reduce ledgers are rebuilt from the rows,
//! exactly as they are for any import).
//!
//! The second step is not this crate's: the tail is segments, and applying them is
//! `okm_core::changelog::replay::replay_after(dir, restored.covers, target)`. Splitting it
//! here is what keeps the medium (the lake) and the protocol (the segments) on their own
//! sides of the line — and what lets a caller hand the tail to whichever target it already
//! replays into.

use std::path::Path;

use okm_core::changelog::{LakeBaseline, lake_baseline};
use okm_core::{Collection, Document, KeyEncode, VirtualStorage};

use crate::baseline::verify_at;
use crate::delta;
use crate::read;
use crate::snapshot::parquet_io;

/// What a restore put back, and where the tail begins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// The lake version that was read.
    pub version: u64,
    /// The highest segment that version covers: the number `replay_after` starts above.
    pub covers: u64,
    /// Documents written into the collection.
    pub rows: usize,
}

/// Restore the table `log_dir` names, from the lake table at `lake_table_dir`.
///
/// `log_dir` is the table's own directory in the capturing store's log root — it is where the
/// reference lives, and it is the directory `replay_after` is then called on.
pub fn restore_at<S, K, R>(
    log_dir: &Path,
    lake_table_dir: &Path,
    app: &mut Collection<S, K, R>,
) -> Result<Restored, String>
where
    S: VirtualStorage,
    K: KeyEncode,
    R: Document<Key = K>,
{
    let header = R::NS_PREFIX;
    let root = log_dir.parent().ok_or_else(|| {
        format!(
            "{}: a table directory has a parent — the log root",
            log_dir.display()
        )
    })?;
    let pointer: LakeBaseline = lake_baseline(root, header)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{}: no lake reference, so the log has to be replayed from its own baseline",
                log_dir.display()
            )
        })?;
    let coverage = verify_at(lake_table_dir, &pointer)?;
    let (store, table) = delta::lake_of(lake_table_dir)?;
    let batch = read::current_state_at_version(&store, &table, pointer.version)
        .map_err(|e| e.to_string())?;
    let rows = parquet_io::import_parquet_batch(app, &batch).map_err(|e| e.to_string())?;
    Ok(Restored {
        version: pointer.version,
        covers: coverage.covers,
        rows,
    })
}
