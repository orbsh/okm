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

use crate::engine::storage::{Batch, VirtualStorage};
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

/// redb raises a different error type per operation (transaction, table,
/// storage); one Display-based mapper keeps every read path reporting the
/// engine's own text.
fn eng<E: std::fmt::Display>(e: E) -> crate::error::Error {
    crate::error::Error::Engine(e.to_string())
}

/// An iterator whose ONE item is a failure: what a streaming constructor answers
/// when it cannot open the range (ADR-0038 keeps the error on the item).
fn one_error(e: crate::error::Error) -> super::storage::ScanIter {
    super::storage::ScanIter::Buffered(vec![Err(e)].into_iter())
}

impl VirtualStorage for RedbStore {
    /// redb note: the table is created on first write (`open_table`
    /// inside a write transaction creates it; read paths treat a
    /// missing table as an empty keyspace).
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        let txn = self.db.begin_write().map_err(eng)?;
        {
            let mut table = txn.open_table(TABLE).map_err(eng)?;
            table.insert(key.as_slice(), value.as_slice()).map_err(eng)?;
        }
        txn.commit().map_err(eng)?;
        Ok(())
    }

    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(eng)?;
        // Missing table = empty keyspace (no writes yet).
        let Ok(table) = txn.open_table(TABLE) else {
            return Ok(None);
        };
        Ok(table.get(key).map_err(eng)?.map(|v| v.value().to_vec()))
    }

    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        let txn = self.db.begin_write().map_err(eng)?;
        {
            let mut table = txn.open_table(TABLE).map_err(eng)?;
            table.remove(key).map_err(eng)?;
        }
        txn.commit().map_err(eng)?;
        Ok(())
    }

    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(eng)?;
        let Ok(table) = txn.open_table(TABLE) else {
            return Ok(Vec::new()); // missing table = empty keyspace
        };
        // redb ranges are inclusive on the start bound; prefix scan =
        // range from `prefix` while keys start with it.
        let mut out = Vec::new();
        for r in table.range(prefix..).map_err(eng)? {
            let (k, _) = r.map_err(eng)?;
            if !k.value().starts_with(prefix) {
                break;
            }
            out.push(k.value()[prefix.len()..].to_vec());
        }
        Ok(out)
    }

    fn scan_range(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> crate::error::Result<Vec<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(eng)?;
        let Ok(table) = txn.open_table(TABLE) else {
            return Ok(Vec::new()); // missing table = empty keyspace
        };
        // redb's RangeBounds on `&[u8]` keys: begin inclusive, end
        // exclusive via `..end`; unbounded when no end.
        let mut out = Vec::new();
        match end {
            Some(end) => {
                for r in table.range(begin..end).map_err(eng)? {
                    let (k, _) = r.map_err(eng)?;
                    out.push(k.value().to_vec());
                }
            }
            None => {
                for r in table.range(begin..).map_err(eng)? {
                    let (k, _) = r.map_err(eng)?;
                    out.push(k.value().to_vec());
                }
            }
        }
        Ok(out)
    }

    /// redb's `OwnedRange` is 'static (Arc'd transaction guard inside) —
    /// native streaming without holding the read txn borrow, both
    /// directions.
    fn scan_range_iter(
        &self,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> super::storage::ScanIter {
        // A failure to even OPEN the range rides the ITEM (ADR-0038): the
        // constructor cannot answer for it, and swallowing it is how "failed"
        // used to read as "empty".
        let txn = match self.db.begin_read() {
            Ok(txn) => txn,
            Err(e) => return one_error(eng(e)),
        };
        let Ok(table) = txn.open_table(TABLE) else {
            // Missing table = empty keyspace (no writes yet), not a fault.
            let empty: Vec<crate::error::Result<(Vec<u8>, Vec<u8>)>> = Vec::new();
            return super::storage::ScanIter::Buffered(empty.into_iter());
        };
        let iter = match end {
            Some(end) => table.range_owned(begin..end),
            None => table.range_owned(begin..),
        };
        let _ = txn; // guard moved into the iterator via range_owned's Arc
        match iter {
            Ok(iter) => super::storage::ScanIter::Redb(iter),
            Err(e) => one_error(eng(e)),
        }
    }

    /// redb native: ONE write transaction over the whole batch — redb's
    /// own atomicity carries the mapping (the carrier stays
    /// engine-agnostic, Batch, same as fjall).
    fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(eng)?;
        {
            let mut table = txn
                .open_table(TABLE)
                .map_err(eng)?;
            for (key, value) in batch.ops {
                match value {
                    Some(v) => table
                        .insert(key.as_slice(), v.as_slice())
                        .map_err(eng)?,
                    None => table
                        .remove(key.as_slice())
                        .map_err(eng)?,
                };
            }
        }
        txn.commit().map_err(eng)?;
        Ok(())
    }
}

impl crate::engine::storage::SharedVirtualStorage for RedbStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}
