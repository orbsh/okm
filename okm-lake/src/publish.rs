//! Publishing a table's current state as the lake's newest version.
//!
//! A service that already holds its own state does not need the change-log path: it has one
//! answer per table, now, and wants it in the lake. That is a REPLACEMENT, not an append —
//! the previous version's files stop being reachable in the same commit that adds the new
//! one — which is what [`crate::delta::commit_compaction`] expresses and what a reader sees
//! as a table whose current state is one file.
//!
//! A publisher that lands a version per interval makes a LOG that grows by a version per
//! interval while the table stays one file. [`PublishOptions::checkpoint_every`] is the answer
//! Delta gives for that shape: every `N` versions the writer states the log's whole state in
//! one artifact, so a reader starts there instead of at version 0 (ADR-0065).
//!
//! Nothing here is local: the store may be a directory or a bucket (see [`crate::store`]).

use crate::delta::{self, Addition, Commit};
use crate::store::{LakeStore, table_key};

/// What one publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub commit: Commit,
    /// Files the new version replaced.
    pub replaced: usize,
    /// Bytes of the file it added.
    pub bytes: u64,
    /// The version a checkpoint was written for, when this publish was one that needed it.
    pub checkpoint: Option<u64>,
}

/// How a publisher keeps its log readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PublishOptions {
    /// Write a checkpoint every N versions (`0`, the default: never).
    ///
    /// `N` counts VERSIONS, not time: it is the length of log a reader may have to replay
    /// between checkpoints, and it is the caller's cost trade — the checkpoint is written
    /// once per N publishes and read once per reader.
    pub checkpoint_every: usize,
}

impl PublishOptions {
    /// A checkpoint every `n` versions.
    pub fn every(n: usize) -> Self {
        Self {
            checkpoint_every: n,
        }
    }

    /// Is a checkpoint due after the version just committed?
    fn due(&self, version: u64) -> bool {
        self.checkpoint_every > 0 && (version + 1).is_multiple_of(self.checkpoint_every as u64)
    }
}

/// Put `bytes` (a Parquet file) as the table's current state, in one Delta version.
///
/// The file is written first and the commit names it, so a failure between the two leaves an
/// unreferenced object and no half-committed version — the next publish overwrites the same
/// name. A table with no version yet takes [`delta::commit_files`]; a table that already has
/// one takes [`delta::commit_compaction`], which is the same writer saying "these files leave
/// as this one arrives".
///
/// The replaced files are deleted once the commit is in place: a publisher is mirroring a
/// live service, not keeping a history, and a lake that grows a file per table per interval
/// is a leak with a version number. A reader that wants history keeps the versions before the
/// delete — that is its own copy, not this call's business.
pub fn replace_table(
    store: &dyn LakeStore,
    table: &str,
    file_name: &str,
    bytes: &[u8],
    keys: &[String],
) -> Result<Published, String> {
    replace_table_with(
        store,
        table,
        file_name,
        bytes,
        keys,
        &PublishOptions::default(),
    )
}

/// [`replace_table`], with the log's own maintenance turned on.
pub fn replace_table_with(
    store: &dyn LakeStore,
    table: &str,
    file_name: &str,
    bytes: &[u8],
    keys: &[String],
    options: &PublishOptions,
) -> Result<Published, String> {
    let name = file_name.trim_start_matches("data/").to_string();
    if name.is_empty() {
        return Err("a published file needs a name".to_string());
    }
    let relative = format!("data/{name}");
    let live = delta::files_in_order(store, table)?;
    store.put(&table_key(table, &relative), bytes)?;
    let addition = Addition::plain(relative.clone(), bytes.len() as u64);
    let commit = if live.is_empty() {
        delta::commit_files(store, table, &[addition], keys)?
    } else {
        delta::commit_compaction(store, table, &live, &[addition], keys)?
    };
    for old in &live {
        if old != &relative {
            store.delete(&table_key(table, old))?;
        }
    }
    // The checkpoint is written AFTER the commit and its deletes: it states the log, and a
    // log that names files the store no longer has would make the next read fail rather than
    // save it work.
    let checkpoint = if options.due(commit.version) {
        Some(delta::write_checkpoint(store, table)?.version)
    } else {
        None
    };
    Ok(Published {
        commit,
        replaced: live.len(),
        bytes: bytes.len() as u64,
        checkpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LocalFs;

    /// A minimal Parquet file: one `Int64` column, so the Delta schema is computable.
    fn parquet(rows: i64) -> Vec<u8> {
        use arrow::array::{Int64Array, RecordBatch};
        use arrow::datatypes::{DataType, Field, Schema};
        let schema =
            std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![std::sync::Arc::new(Int64Array::from(vec![
                1i64;
                rows as usize
            ]))],
        )
        .unwrap();
        crate::snapshot::parquet_io::batch_bytes(&batch).unwrap()
    }

    #[test]
    fn a_publish_replaces_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let lake = LocalFs::new(dir.path());
        let keys = vec!["id".to_string()];

        let first = replace_table(&lake, "log", "log-1.parquet", &parquet(3), &keys).unwrap();
        assert_eq!(first.commit.version, 0);
        assert_eq!(first.replaced, 0);

        let second = replace_table(&lake, "log", "log-2.parquet", &parquet(5), &keys).unwrap();
        assert_eq!(second.commit.version, 1);
        assert_eq!(second.replaced, 1);

        // One live file, and the one it replaced is gone from the store.
        assert_eq!(
            delta::files_in_order(&lake, "log").unwrap(),
            vec!["data/log-2.parquet".to_string()]
        );
        assert!(lake.head("log/data/log-1.parquet").unwrap().is_none());
        assert!(lake.head("log/data/log-2.parquet").unwrap().is_some());
        // No policy asked for checkpoints, so the log is only versions.
        assert_eq!(first.checkpoint, None);
        assert_eq!(delta::checkpoint_at(&lake, "log").unwrap(), None);
    }

    #[test]
    fn a_checkpoint_lands_on_the_interval_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let lake = LocalFs::new(dir.path());
        let keys = vec!["id".to_string()];
        let options = PublishOptions::every(3);

        let mut written = Vec::new();
        for round in 1..=7 {
            let published = replace_table_with(
                &lake,
                "log",
                &format!("log-{round}.parquet"),
                &parquet(round),
                &keys,
                &options,
            )
            .unwrap();
            assert_eq!(published.commit.version, round as u64 - 1);
            written.push(published.checkpoint);
        }
        // Versions 0..=6: a checkpoint after the 3rd and the 6th, which are versions 2 and 5.
        assert_eq!(
            written,
            vec![None, None, Some(2), None, None, Some(5), None]
        );
        assert_eq!(delta::checkpoint_at(&lake, "log").unwrap(), Some(5));

        // Version 6 landed after the checkpoint at 5, so a checkpoint now states v6; asking
        // again for the same version answers with it instead of writing a second.
        let at_six = delta::write_checkpoint(&lake, "log").unwrap();
        assert_eq!(at_six.version, 6);
        let again = delta::write_checkpoint(&lake, "log").unwrap();
        assert_eq!(again.version, 6);
        assert_eq!(again.files, 1, "a publisher keeps one current-state file");
        assert_eq!(delta::checkpoint_at(&lake, "log").unwrap(), Some(6));
    }
}
