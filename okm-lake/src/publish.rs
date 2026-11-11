//! Publishing a table's current state as the lake's newest version.
//!
//! A service that already holds its own state does not need the change-log path: it has one
//! answer per table, now, and wants it in the lake. That is a REPLACEMENT, not an append —
//! the previous version's files stop being reachable in the same commit that adds the new
//! one — which is what [`crate::delta::commit_compaction`] expresses and what a reader sees
//! as a table whose current state is one file.
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
    Ok(Published {
        commit,
        replaced: live.len(),
        bytes: bytes.len() as u64,
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
    }
}
