//! redb engine adapter: `RedbStore` = Database + table wrapper,
//! implementing the sync `VirtualStorage`.
//!
//! redb is a single-file B-tree (COW, no WAL): read-deterministic, no
//! write amplification from log compaction — the complement to fjall's
//! LSM (write-heavy) profile (PLAN engine matrix: fjall for write-heavy
//! shards, redb for read-deterministic segments).
//!
//! Transaction mapping: redb only mutates inside write transactions, so
//! `put`/`del` each open+commit a transaction, and `commit_batch` opens
//! ONE transaction for the whole batch — redb's own atomicity carries
//! the batch mapping (same shape as fjall's native batch override).
//! `shared_handle` is an Arc-kernel clone (`Database` clones share the
//! file lock and keyspace).

use crate::engine::storage::{MemBatch, VirtualStorage};
use redb::{Database, ReadableDatabase, TableDefinition};

/// The single table holding the whole OKM keyspace (ns prefixes come
/// with the key encoding itself, so all tables/edges share one redb
/// table — same reasoning as fjall's single keyspace).
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("okm");

/// Clone is a handle clone: redb's `Database` is Arc-kernel internally
/// (`Arc<TransactionalMemory>` + `Arc<TransactionTracker>`) but does not
/// derive `Clone`, so `RedbStore` wraps it in an outer `Arc` — clones
/// share the file and its single-writer discipline. Reads are
/// concurrent (read transactions), writes serialize on redb's
/// transaction tracker.
#[derive(Clone)]
pub struct RedbStore {
    db: std::sync::Arc<Database>,
}

impl RedbStore {
    /// Open (or create) the database file at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self, redb::DatabaseError> {
        Ok(Self {
            db: std::sync::Arc::new(Database::create(path)?),
        })
    }

    /// Open over an existing `Arc<Database>` (the caller configured the
    /// builder; `Database::builder().create(path)` produces it).
    pub fn from_db(db: std::sync::Arc<Database>) -> Self {
        Self { db }
    }
}

impl VirtualStorage for RedbStore {
    /// redb note: the table is created on first write (`open_table`
    /// inside a write transaction creates it; read paths treat a
    /// missing table as an empty keyspace).
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        let txn = self.db.begin_write().expect("redb begin_write");
        {
            let mut table = txn.open_table(TABLE).expect("redb open_table");
            table
                .insert(key.as_slice(), value.as_slice())
                .expect("redb insert");
        }
        txn.commit().expect("redb commit");
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let txn = self.db.begin_read().expect("redb begin_read");
        // Missing table = empty keyspace (no writes yet).
        let Ok(table) = txn.open_table(TABLE) else {
            return None;
        };
        table.get(key).expect("redb get").map(|v| v.value().to_vec())
    }

    fn del(&mut self, key: &[u8]) {
        let txn = self.db.begin_write().expect("redb begin_write");
        {
            let mut table = txn.open_table(TABLE).expect("redb open_table");
            table.remove(key).expect("redb remove");
        }
        txn.commit().expect("redb commit");
    }

    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let txn = self.db.begin_read().expect("redb begin_read");
        let Ok(table) = txn.open_table(TABLE) else {
            return Vec::new(); // missing table = empty keyspace
        };
        // redb ranges are inclusive on the start bound; prefix scan =
        // range from `prefix` while keys start with it.
        table
            .range(prefix..)
            .expect("redb range")
            .take_while(|r| {
                r.as_ref()
                    .expect("redb range item")
                    .0
                    .value()
                    .starts_with(prefix)
            })
            .map(|r| {
                let (k, _) = r.expect("redb range item");
                k.value()[prefix.len()..].to_vec()
            })
            .collect()
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        let txn = self.db.begin_read().expect("redb begin_read");
        let Ok(table) = txn.open_table(TABLE) else {
            return Vec::new(); // missing table = empty keyspace
        };
        // redb's RangeBounds on `&[u8]` keys: begin inclusive, end
        // exclusive via `..end`; unbounded when no end.
        let iter = match end {
            Some(end) => table.range(begin..end),
            None => table.range(begin..),
        }
        .expect("redb range");
        iter.map(|r| {
                let (k, _) = r.expect("redb range item");
                k.value().to_vec()
            })
            .collect()
    }

    /// redb's `OwnedRange` is 'static (Arc'd transaction guard inside) —
    /// native streaming without holding the read txn borrow, both
    /// directions.
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> super::storage::ScanIter {
        let txn = self.db.begin_read().expect("redb begin_read");
        let Ok(table) = txn.open_table(TABLE) else {
            return super::storage::ScanIter::Buffered(std::iter::empty().collect::<Vec<_>>().into_iter());
        };
        let iter = match end {
            Some(end) => table.range_owned(begin..end),
            None => table.range_owned(begin..),
        }
        .expect("redb range");
        let _ = txn; // guard moved into the iterator via range_owned's Arc
        super::storage::ScanIter::Redb(iter)
    }

    fn batch(&mut self) -> MemBatch {
        MemBatch::default()
    }

    /// redb native: ONE write transaction over the whole batch — redb's
    /// own atomicity carries the mapping (the carrier stays
    /// engine-agnostic, MemBatch, same as fjall).
    fn commit_batch(&mut self, batch: MemBatch) -> Result<(), String> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| format!("redb begin_write: {e}"))?;
        {
            let mut table = txn
                .open_table(TABLE)
                .map_err(|e| format!("redb open_table: {e}"))?;
            for (key, value) in batch.ops {
                match value {
                    Some(v) => table
                        .insert(key.as_slice(), v.as_slice())
                        .map_err(|e| format!("redb insert: {e}"))?,
                    None => table
                        .remove(key.as_slice())
                        .map_err(|e| format!("redb remove: {e}"))?,
                };
            }
        }
        txn.commit().map_err(|e| format!("redb commit: {e}"))?;
        Ok(())
    }
}

impl crate::engine::storage::SharedVirtualStorage for RedbStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}
