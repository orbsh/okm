//! Publishing a table's current state as the lake's newest version.
//!
//! A service that already holds its own state does not need the change-log path: it has one
//! answer per table, now, and wants it in the lake. That is a REPLACEMENT, not an append —
//! the previous version's files stop being reachable in the same commit that adds the new
//! ones — which is what [`crate::delta::commit_compaction`] expresses and what a reader sees
//! as a table whose current state is the files of one version. One file, or one per partition:
//! a partitioned table states every partition it holds in ONE commit, so no version is ever
//! half old and half new.
//!
//! A publisher that lands a version per interval makes a LOG that grows by a version per
//! interval while the table stays the files of one version. [`PublishOptions::checkpoint_every`]
//! is the answer
//! Delta gives for that shape: every `N` versions the writer states the log's whole state in
//! one artifact, so a reader starts there instead of at version 0 (ADR-0065).
//!
//! Nothing here is local: the store may be a directory or a bucket (see [`crate::store`]).

use crate::delta::{self, Addition, Commit};
use crate::store::{LakeStore, table_key};
use crate::{WindowManifest, bounds, status};

/// What one publish did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub commit: Commit,
    /// Files the new version replaced.
    pub replaced: usize,
    /// Bytes the version added, summed over its files.
    pub bytes: u64,
    /// The lake keys the version added, in the order they were given.
    pub files: Vec<String>,
    /// The version a checkpoint was written for, when this publish was one that needed it.
    pub checkpoint: Option<u64>,
}

/// One file of a published version: the bytes, the name inside its partition directory, and the
/// partition values that place it.
///
/// The NAME does not carry a directory. Where a file lands is decided by its partition values
/// (`data/<column>=<value>/`, percent-encoded), because a caller that could spell the directory
/// itself could put a file in a partition the log does not say it is in — and then a fold, which
/// keeps to one partition, could not find it.
pub struct PublishedFile<'a> {
    /// The file's name inside its partition directory (`data/` when unpartitioned).
    pub name: &'a str,
    /// The Parquet bytes.
    pub bytes: &'a [u8],
    /// `(column, value)` for every partition column — empty for an unpartitioned table.
    pub partition: delta::Partition,
}

impl<'a> PublishedFile<'a> {
    /// A file of an unpartitioned table.
    pub fn plain(name: &'a str, bytes: &'a [u8]) -> Self {
        Self {
            name,
            bytes,
            partition: Vec::new(),
        }
    }

    /// A file that states one partition value.
    pub fn partitioned(name: &'a str, bytes: &'a [u8], column: &str, value: &str) -> Self {
        Self {
            name,
            bytes,
            partition: vec![(column.to_string(), value.to_string())],
        }
    }
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

/// Put one Parquet file as the table's whole current state, in one Delta version.
///
/// This is [`replace_files`] with a single unpartitioned file — the shape a table that is one
/// file per version has. A publisher that holds partitions states them all at once through
/// [`replace_files`], because one version that names every partition is the only version a
/// reader can see whole: two versions would be a window in which half the table is the old
/// answer.
///
/// The files are written first and the commit names them, so a failure between the two leaves
/// unreferenced objects and no half-committed version — the next publish overwrites the same
/// names. A table with no version yet takes [`delta::commit_files`]; a table that already has
/// one takes [`delta::commit_compaction`], which is the same writer saying "these files leave
/// as these arrive".
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
    replace_files_with(
        store,
        table,
        &[PublishedFile::plain(file_name, bytes)],
        keys,
        options,
    )
}

/// Put every file of the table's current state, as ONE Delta version.
///
/// A partitioned table is one file per partition value, so a publisher that holds partitions
/// hands all of them over in one call: the version that lands is complete, and the files of the
/// version it replaced leave in the same commit. [`replace_table`] is the single-file form.
///
/// The partition rules are the drain's (`DrainConfig::partition`), because a published table is
/// the same table a drain writes: the column must be a KEY column (a partition that does not
/// refine the key space lets one key live in two files), the directory name is percent-encoded
/// while the action keeps the raw text, and one file states one partition value.
pub fn replace_files(
    store: &dyn LakeStore,
    table: &str,
    files: &[PublishedFile<'_>],
    keys: &[String],
) -> Result<Published, String> {
    replace_files_with(store, table, files, keys, &PublishOptions::default())
}

/// [`replace_files`], with the log's own maintenance turned on.
pub fn replace_files_with(
    store: &dyn LakeStore,
    table: &str,
    files: &[PublishedFile<'_>],
    keys: &[String],
    options: &PublishOptions,
) -> Result<Published, String> {
    let placed = placed(files, keys)?;
    let live = delta::files_in_order(store, table)?;
    for file in &placed {
        store.put(&table_key(table, &file.relative), file.bytes)?;
        // The manifest goes down BEFORE the commit that names the file: a reader that sees the
        // version must find the description of what it points at, and `status` must never
        // report "unknown" for a file this writer just measured.
        write_manifest(store, table, &file.relative, file.name, file.bytes, keys)?;
    }
    let additions: Vec<Addition> = placed
        .iter()
        .map(|file| Addition {
            path: file.relative.clone(),
            partition: file.partition.clone(),
            size: file.bytes.len() as u64,
        })
        .collect();
    let commit = if live.is_empty() {
        delta::commit_files(store, table, &additions, keys)?
    } else {
        delta::commit_compaction(store, table, &live, &additions, keys)?
    };
    for old in &live {
        // A file the new version states again at the same path was overwritten in place.
        if !placed.iter().any(|file| &file.relative == old) {
            store.delete(&table_key(table, old))?;
            // The manifest described that file and nothing else, so it goes with it.
            store.delete(&table_key(table, &status::manifest_key(old)))?;
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
        bytes: placed.iter().map(|file| file.bytes.len() as u64).sum(),
        files: placed.into_iter().map(|file| file.relative).collect(),
        checkpoint,
    })
}

/// One file of a version, after the partition rules placed it at a lake key.
struct Placed<'a> {
    /// The name inside the partition directory (the Parquet file's own name).
    name: &'a str,
    /// `data/<column>=<encoded value>/<name>`, or `data/<name>`.
    relative: String,
    bytes: &'a [u8],
    partition: delta::Partition,
}

/// Place every file at the key its partition values name, or say which rule it broke.
fn placed<'a>(files: &[PublishedFile<'a>], keys: &[String]) -> Result<Vec<Placed<'a>>, String> {
    if files.is_empty() {
        return Err("a published version needs at least one file".to_string());
    }
    let columns: Vec<&str> = files[0]
        .partition
        .iter()
        .map(|(column, _)| column.as_str())
        .collect();
    // A published table is the table a drain writes, and that one partitions by ONE column
    // (`DrainConfig.partition` takes a single name): a second would produce a
    // `data/a=1/b=2/` shape the drain's own listing does not read.
    if columns.len() > 1 {
        return Err(format!(
            "a table partitions by one column, and this version names {columns:?}"
        ));
    }
    // A partition must refine the key space: partition by anything else and one key could live
    // in two files, where a fold can no longer see both halves of it.
    for column in &columns {
        if !keys.iter().any(|key| key == column) {
            return Err(format!(
                "`{column}` is not a key column of this table: a partition has to refine the \
                 key space, and {keys:?} is that space"
            ));
        }
    }
    let mut placed: Vec<Placed<'a>> = Vec::with_capacity(files.len());
    for file in files {
        let name = file.name.trim_start_matches("data/");
        if name.is_empty() {
            return Err("a published file needs a name".to_string());
        }
        if name.contains('/') {
            return Err(format!(
                "{name}: a published file is a name, not a path — its directory comes from its \
                 partition values"
            ));
        }
        if file
            .partition
            .iter()
            .map(|(column, _)| column.as_str())
            .ne(columns.iter().copied())
        {
            return Err(format!(
                "{name}: every file of one version states the same partition columns as the \
                 first, {columns:?}"
            ));
        }
        let relative = match file.partition.first() {
            Some((column, value)) => {
                format!("data/{column}={}/{}", crate::partition_dir(value), name)
            }
            None => format!("data/{name}"),
        };
        // One file per partition value: two files stating one value is one key in two places,
        // which is exactly what the partition rule exists to prevent.
        if let Some((_, value)) = file.partition.first() {
            let twice = placed
                .iter()
                .filter_map(|kept| kept.partition.first())
                .any(|(_, kept)| kept == value);
            if twice {
                return Err(format!(
                    "{relative}: one file per partition value, and `{value}` is named twice"
                ));
            }
        }
        if placed.iter().any(|kept| kept.relative == relative) {
            return Err(format!("{relative}: this version names the file twice"));
        }
        placed.push(Placed {
            name,
            relative,
            bytes: file.bytes,
            partition: file.partition.clone(),
        });
    }
    Ok(placed)
}

/// The manifest beside a published file: what it holds, and where its keys start and end.
///
/// A published file is not a window — no segment was projected into it — so `segments` is empty
/// and `layout_version` is 0 ("no layout behind this file"). What it DOES state is what the other
/// readers of a lake need: the row count `status` sums, and the `key_bounds` a fold prunes with.
/// Without them one published file makes `status` report "rows unknown" and makes every fold open
/// every file, because a file with no manifest is a file that could share any key.
fn write_manifest(
    store: &dyn LakeStore,
    table: &str,
    relative: &str,
    name: &str,
    bytes: &[u8],
    keys: &[String],
) -> Result<(), String> {
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(bytes),
    )
    .map_err(|e| format!("{relative}: {e}"))?;
    // The row count comes from the footer, like every other count here: the caller's file is
    // measured, never taken on trust.
    let rows = builder
        .metadata()
        .row_groups()
        .iter()
        .map(|group| group.num_rows() as usize)
        .sum();
    let columns = builder
        .schema()
        .fields()
        .iter()
        .map(|f| {
            (
                f.name().clone(),
                format!("{:?}", f.data_type()),
                f.is_nullable(),
            )
        })
        .collect();
    let manifest = WindowManifest {
        table: table.to_string(),
        layout_version: 0,
        segments: Vec::new(),
        rows,
        deletes: 0,
        columns,
        file: name.to_string(),
        key_bounds: bounds::to_manifest(&bounds::of_parquet(bytes, keys)?),
    };
    store.put(
        &table_key(table, &status::manifest_key(relative)),
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("manifest: {e}"))?
            .as_bytes(),
    )?;
    Ok(())
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
