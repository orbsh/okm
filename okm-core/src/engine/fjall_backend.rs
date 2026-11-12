//! fjall engine adapter: `FjallStore` = Database + keyspace wrapper,
//! implementing the sync `VirtualStorage`.
//!
//! One OKM Edge corresponds to one keyspace; ns prefixes come with the
//! key encoding itself, so different edge types sharing a keyspace do not
//! conflict.

use crate::engine::storage::{Batch, VirtualStorage};
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

/// Clone is a handle clone: Arc-inner in fjall, clones share the keyspace.
#[derive(Clone)]
pub struct FjallStore {
    db: Database,
    ks: Keyspace,
}

impl FjallStore {
    /// 打开（或创建）path 下的 keyspace `name`
    pub fn open(path: &std::path::Path, name: &str) -> fjall::Result<Self> {
        let db = Database::create_or_recover(fjall::Config::new(path))?;
        let ks = db.keyspace(name, KeyspaceCreateOptions::default)?;
        Ok(Self { db, ks })
    }

    /// 从已存在的 Database 挂一个 keyspace
    pub fn from_db(db: Database, name: &str) -> fjall::Result<Self> {
        Ok(Self {
            ks: db.keyspace(name, KeyspaceCreateOptions::default)?,
            db,
        })
    }

    /// 持久化（fjall 默认 Buffer 模式，需要时手动刷）
    pub fn persist(&self) -> fjall::Result<()> {
        self.db.persist(fjall::PersistMode::SyncData)
    }
}

/// Clone IS a shared handle (Arc-inner) — the NestStorage requirement.
impl crate::engine::storage::SharedVirtualStorage for FjallStore {
    fn shared_handle(&self) -> Self {
        self.clone()
    }
}

impl VirtualStorage for FjallStore {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> crate::error::Result<()> {
        self.ks
            .insert(key, value)
            .map_err(|e| crate::error::Error::Engine(e.to_string()))
    }
    fn get(&self, key: &[u8]) -> crate::error::Result<Option<Vec<u8>>> {
        self.ks
            .get(key)
            .map(|v| v.map(|v| v.to_vec()))
            .map_err(|e| crate::error::Error::Engine(e.to_string()))
    }
    fn del(&self, key: &[u8]) -> crate::error::Result<()> {
        self.ks
            .remove(key)
            .map_err(|e| crate::error::Error::Engine(e.to_string()))
    }
    /// fjall native: the op list executes in fjall's own cross-keyspace
    /// `Batch` — one `Batch::commit()` = one real WAL write; the carrier
    /// stays engine-agnostic (Batch), the atomicity is fjall's.
    fn commit_batch(&self, batch: Batch) -> crate::error::Result<()> {
        let mut wb = self.db.batch();
        for (k, v) in &batch.ops {
            match v {
                Some(v) => wb.insert(&self.ks, k.clone(), v.clone()),
                None => wb.remove(&self.ks, k.clone()),
            }
        }
        wb.commit()
            .map_err(|e| crate::error::Error::Engine(e.to_string()))
    }
    fn scan_suffix(&self, prefix: &[u8]) -> crate::error::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for g in self.ks.prefix(prefix) {
            let k = g
                .key()
                .map_err(|e| crate::error::Error::Engine(e.to_string()))?;
            out.push(k[prefix.len()..].to_vec());
        }
        Ok(out)
    }

    /// fjall's `Iter` is an owned, 'static lazy iterator (snapshot
    /// nonce held inside) — native streaming, no buffering, both
    /// directions. This is the trait's one ordered-read primitive, so the
    /// eager key list is the caller's `.collect()`.
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> super::storage::ScanIter {
        super::storage::ScanIter::Fjall(FjallScan::new(
            self.db.snapshot(),
            self.ks.clone(),
            begin,
            end,
        ))
    }
}

/// One fjall range walk that can be REPOSITIONED.
///
/// fjall 3.1 exposes no cursor seek: `fjall::Iter` is a boxed
/// `DoubleEndedIterator` (`iter.rs`, no `seek` anywhere in the crate). It does
/// expose the next rung down — a `Snapshot` whose `range` opens a fresh cursor
/// at the SAME nonce (`snapshot.rs`). Holding the snapshot beside the iterator
/// is therefore what makes `seek` consistent with the entries already returned:
/// a seek reopens the range on one snapshot, never on a newer view.
pub struct FjallScan {
    snapshot: fjall::Snapshot,
    keyspace: Keyspace,
    end: Option<Vec<u8>>,
    iter: fjall::Iter,
    /// One probed item, held so `seek` can answer "is anything left?" without
    /// consuming it. Drained before the cursor by both directions.
    pending: Option<Item>,
    /// The last key handed out by `next` (or the lower bound a seek put in
    /// force). This is the position a seek refuses to move behind; keeping it
    /// costs one small copy per entry, and it is what makes "backwards is a
    /// no-op" checkable without asking the engine.
    pos: Vec<u8>,
    /// Set once the forward walk has observed the end of the range.
    exhausted: bool,
}

type Item = crate::error::Result<(Vec<u8>, Vec<u8>)>;

impl FjallScan {
    fn new(
        snapshot: fjall::Snapshot,
        keyspace: Keyspace,
        begin: &[u8],
        end: Option<&[u8]>,
    ) -> Self {
        let end = end.map(<[u8]>::to_vec);
        let iter = open(&snapshot, &keyspace, begin, end.as_deref());
        Self {
            snapshot,
            keyspace,
            end,
            iter,
            pending: None,
            pos: begin.to_vec(),
            exhausted: false,
        }
    }

    fn set_pos(&mut self, key: &[u8]) {
        self.pos.clear();
        self.pos.extend_from_slice(key);
    }

    /// Reopen the range at `lower` on the snapshot this scan was born with.
    pub fn seek(&mut self, lower: &[u8]) -> bool {
        if lower <= self.pos.as_slice() {
            // Backwards (or same position): a no-op that keeps the cursor.
            return self.pending.is_some() || !self.exhausted;
        }
        self.set_pos(lower);
        self.iter = open(&self.snapshot, &self.keyspace, lower, self.end.as_deref());
        self.exhausted = false;
        // Probe so the answer is honest: `false` means nothing at or after
        // `lower`, and the probe item is not lost (it is the next item).
        match self.iter.next() {
            Some(item) => {
                self.pending = Some(guard_item(item));
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
    snapshot: &fjall::Snapshot,
    keyspace: &Keyspace,
    begin: &[u8],
    end: Option<&[u8]>,
) -> fjall::Iter {
    use fjall::Readable;
    match end {
        Some(end) => snapshot.range(keyspace, begin..end),
        None => snapshot.range(keyspace, begin..),
    }
}

fn guard_item(g: fjall::Guard) -> Item {
    match g.into_inner() {
        Ok((k, v)) => Ok((k.to_vec(), v.to_vec())),
        Err(e) => Err(crate::error::Error::Engine(e.to_string())),
    }
}

impl Iterator for FjallScan {
    type Item = Item;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(p) = self.pending.take() {
            return Some(p);
        }
        let item = self.iter.next().map(guard_item);
        if let Some(Ok((k, _))) = &item {
            self.set_pos(k);
        }
        if item.is_none() {
            self.exhausted = true;
        }
        item
    }
}

impl DoubleEndedIterator for FjallScan {
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.iter.next_back() {
            Some(g) => Some(guard_item(g)),
            // The probe item is the only one left when the cursor is spent.
            None => self.pending.take(),
        }
    }
}
