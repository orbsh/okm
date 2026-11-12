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
            table
                .insert(key.as_slice(), value.as_slice())
                .map_err(eng)?;
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

    /// redb's `OwnedRange` is 'static (Arc'd transaction guard inside) —
    /// native streaming without holding the read txn borrow, both
    /// directions. The trait's one ordered-read primitive.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> super::storage::ScanIter {
        // A failure to even OPEN the range rides the ITEM (ADR-0038): the
        // constructor cannot answer for it, and swallowing it is how "failed"
        // used to read as "empty".
        let txn = match self.db.begin_read() {
            Ok(txn) => txn,
            Err(e) => return one_error(eng(e)),
        };
        match RedbScan::new(txn, begin, end) {
            Ok(scan) => super::storage::ScanIter::Redb(Box::new(scan)),
            // Missing table = empty keyspace (no writes yet), not a fault.
            Err(Missing::Table) => {
                let empty: Vec<crate::error::Result<(Vec<u8>, Vec<u8>)>> = Vec::new();
                super::storage::ScanIter::Buffered(empty.into_iter())
            }
            Err(Missing::Fault(e)) => one_error(e),
        }
    }

    /// redb native: ONE write transaction over the whole batch — redb's
    /// own atomicity carries the mapping (the carrier stays
    /// engine-agnostic, Batch, same as fjall).
    fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        let txn = self.db.begin_write().map_err(eng)?;
        {
            let mut table = txn.open_table(TABLE).map_err(eng)?;
            for (key, value) in batch.ops {
                match value {
                    Some(v) => table.insert(key.as_slice(), v.as_slice()).map_err(eng)?,
                    None => table.remove(key.as_slice()).map_err(eng)?,
                };
            }
        }
        txn.commit().map_err(eng)?;
        Ok(())
    }
}

/// One redb range walk that can be REPOSITIONED.
///
/// redb 4.2 has no seek on `OwnedRange`, and its cursor (`lower_bound`) is
/// behind the `experimental-api-5` feature, which this crate does not enable.
/// What `OwnedRange` DOES carry is an `Arc<TransactionGuard>`
/// (`table.rs`, `OwnedRange { inner, transaction_guard }`), and the read
/// transaction that produced it can open the table again. Holding that
/// transaction in the scan is what makes `seek` a cheap, CONSISTENT reopen:
/// the same snapshot as every entry already returned, instead of the fresh
/// `begin_read()` each `scan_range` call used to open.
pub struct RedbScan {
    txn: redb::ReadTransaction,
    end: Option<Vec<u8>>,
    iter: redb::OwnedRange<&'static [u8], &'static [u8]>,
    /// One probed item, held so `seek` can answer "is anything left?" without
    /// consuming it.
    pending: Option<crate::error::Result<(Vec<u8>, Vec<u8>)>>,
    /// The last key handed out by `next` (or the lower bound a seek put in
    /// force): the position a seek refuses to move behind.
    pos: Vec<u8>,
    /// Set once the forward walk has observed the end of the range.
    exhausted: bool,
}

/// Why the range could not be opened.
enum Missing {
    /// The table has never been written: an empty keyspace, not a fault.
    Table,
    /// The engine failed to open the range; the fault rides the first item.
    Fault(crate::error::Error),
}

impl RedbScan {
    fn new(txn: redb::ReadTransaction, begin: &[u8], end: Option<&[u8]>) -> Result<Self, Missing> {
        let end = end.map(<[u8]>::to_vec);
        let iter = open(&txn, begin, end.as_deref())?;
        Ok(Self {
            txn,
            end,
            iter,
            pending: None,
            pos: begin.to_vec(),
            exhausted: false,
        })
    }

    fn set_pos(&mut self, key: &[u8]) {
        self.pos.clear();
        self.pos.extend_from_slice(key);
    }

    /// Reopen the range at `lower` on the transaction this scan was born with.
    pub fn seek(&mut self, lower: &[u8]) -> bool {
        if lower <= self.pos.as_slice() {
            // Backwards (or same position): a no-op that keeps the cursor.
            return self.pending.is_some() || !self.exhausted;
        }
        self.set_pos(lower);
        match open(&self.txn, lower, self.end.as_deref()) {
            Ok(iter) => {
                self.iter = iter;
                self.exhausted = false;
            }
            // The transaction outlived its table: the fault is an item, and a
            // fault is not exhaustion, so the answer is `true`.
            Err(Missing::Fault(e)) => {
                self.pending = Some(Err(e));
                return true;
            }
            Err(Missing::Table) => {
                self.exhausted = true;
                return false;
            }
        }
        match self.iter.next() {
            Some(item) => {
                self.pending = Some(owned_item(item));
                true
            }
            None => {
                self.exhausted = true;
                false
            }
        }
    }
}

fn open(
    txn: &redb::ReadTransaction,
    begin: &[u8],
    end: Option<&[u8]>,
) -> Result<redb::OwnedRange<&'static [u8], &'static [u8]>, Missing> {
    let table = txn.open_table(TABLE).map_err(|e| match e {
        redb::TableError::TableDoesNotExist(_) => Missing::Table,
        other => Missing::Fault(eng(other)),
    })?;
    match end {
        Some(end) => table
            .range_owned(begin..end)
            .map_err(|e| Missing::Fault(eng(e))),
        None => table
            .range_owned(begin..)
            .map_err(|e| Missing::Fault(eng(e))),
    }
}

/// One entry as redb hands it out: the two owned guards, before the copy.
type OwnedEntry = (
    redb::OwnedAccessGuard<&'static [u8]>,
    redb::OwnedAccessGuard<&'static [u8]>,
);

fn owned_item(r: redb::Result<OwnedEntry>) -> crate::error::Result<(Vec<u8>, Vec<u8>)> {
    match r {
        Ok((k, v)) => Ok((k.value().to_vec(), v.value().to_vec())),
        Err(e) => Err(eng(e)),
    }
}

impl Iterator for RedbScan {
    type Item = crate::error::Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(p) = self.pending.take() {
            return Some(p);
        }
        let item = self.iter.next().map(owned_item);
        if let Some(Ok((k, _))) = &item {
            self.set_pos(k);
        }
        if item.is_none() {
            self.exhausted = true;
        }
        item
    }
}

impl DoubleEndedIterator for RedbScan {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.iter.next_back() {
            Some(item) => Some(owned_item(item)),
            // The probe item is the only one left when the cursor is spent.
            None => self.pending.take(),
        }
    }
}

impl crate::engine::storage::SharedVirtualStorage for RedbStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}
