//! Table — the row assembly point (ADR-0006): binds an engine instance, a
//! key type, and a row type. `put` writes the primary key (slot 0) and one
//! entry per access method in the same store instance, so atomicity holds
//! within a single engine; cross-ns atomicity is the store instance's
//! boundary, never the Table's. Index entries derive from the row payload
//! (indexed + includes fields live there), so put and delete are both
//! row-shaped.

use crate::engine::KvEngine;
use crate::index::{KvIndex, Row};
use crate::key::{KeyEncode, PrefixKey};

pub struct Table<S, K: KeyEncode, R: Row<Key = K>> {
    store: S,
    /// Raw ns segment (no direction bit) — comes from the Table's
    /// `#[kv_ns]` declaration on the key struct.
    ns: u16,
    /// Monotonic write-batch counter (in-process only, never persisted):
    /// bumped on every put/delete, stamped on emitted subscribe events so
    /// consumers can fold to exact same-table batch boundaries. Resets on
    /// restart — meaningful only within one process lifetime.
    epoch: u64,
    _marker: std::marker::PhantomData<(K, R)>,
}

impl<S: KvEngine, K: KeyEncode, R: Row<Key = K>> Table<S, K, R> {
    /// ns here is the table's segment ID; the assembly site passes it
    /// explicitly (it is declared once on the key struct's `#[kv_ns]` and
    /// threaded through by the user's binding code).
    pub fn new(store: S, ns: u16) -> Self {
        Self {
            store,
            ns,
            epoch: 0,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Primary key entry (slot 0): `[ns 2B][slot 0][key payload]`, value
    /// = TLV payload of the row. The slot byte keeps the header uniform
    /// with index entries (`[ns 2B][slot 1B]`); slot 0 = primary, per
    /// ADR-0005.
    pub fn primary_key(&self, key: &K) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + K::KEY_LEN);
        buf.extend_from_slice(&self.ns.to_be_bytes());
        buf.push(crate::index::PRIMARY_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Write a row: primary key + one (key, value) index entry per access
    /// method, all derived from this one row.
    ///
    /// Overwrite semantics (ADR-0008 fold/unfold discipline): when the
    /// key already holds a row, the stored row is unfolded from every
    /// reduce group before the new row is folded in — otherwise the
    /// second write double-counts. This costs one primary-table point
    /// read per overwrite (inserts skip it: slot-0 miss = no fold to
    /// undo). Index entries need no counterpart: they are derived
    /// per-row and the entry key encodes the indexed fields, so an
    /// overwrite with different indexed values lands at a different key
    /// — the stale entry dangles, which is why `delete` (and
    /// `delete_by_pkey`) exist; the reduce, by contrast, is a mutable
    /// aggregate under one group key and MUST compensate.
    pub fn put(&mut self, key: &K, row: &R) {
        // Overwrite detection doubles as the unfold source: the stored
        // row (if any) is exactly what the reduce groups currently
        // include for this key.
        let pkey = self.primary_key(key);
        let prev = self
            .store
            .get(&pkey)
            .map(|v| R::decode_payload(&v));
        if let Some(old) = &prev {
            R::__okm_apply_reduces(&mut self.store, key, old, self.ns, false);
        }
        self.store.put(pkey, row.encode_payload());
        for (ek, ev) in R::index_entries(key, row, self.ns) {
            self.store.put(ek, ev);
        }
        // Cross-row reduces: fold this row into each declared group.
        // Same store instance, so the RMW shares the engine's atomicity
        // boundary with the row + index writes.
        R::__okm_apply_reduces(&mut self.store, key, row, self.ns, true);
        // Subscribe: write-path event into the declared channel
        // (best-effort try_send — full channel drops, never blocks).
        // The event carries the table's post-write epoch (monotonic
        // write-batch boundary, ADR-0008 §5).
        self.epoch += 1;
        R::__okm_emit_event(crate::subscribe::Op::Put, self.epoch, key, row);
        let _ = prev; // kept alive for the unfold above; dropped here
    }

    /// Commanded RMW: `get` → `f(old)` → `put(key, new)` through the
    /// normal write path, so index maintenance, reduce hooks and channel
    /// emission all fire without special-casing. Returns the written row.
    /// `f` receives `None` when the key has no row yet (insert path).
    ///
    /// Single-writer only: OKM is an in-process library with a serial
    /// write order (`&mut self`), so get→f→put cannot interleave — no
    /// CAS needed. Same constraint that backs reduce's exactly-once.
    pub fn upsert_with(&mut self, key: &K, f: impl FnOnce(Option<R>) -> R) -> R {
        let new = f(self.get(key));
        self.put(key, &new);
        new
    }

    /// Encode this row's full write set (primary entry + one entry per
    /// access method) into an externally owned batch — no write happens
    /// until the batch commits. The cross-collection atomic path
    /// (ADR-0003): a row table and an edge table (or two rows tables)
    /// each `save_into` the same batch, then one `commit_batch` makes
    /// them live or die together.
    ///
    /// Note: save_into bypasses put's higher write-path hooks (reduce
    /// folds, subscribe emission, overwrite unfold) — it is the encoding
    /// surface, not the semantic one. Rows that participate in reduce or
    /// subscriptions must go through `put`/`upsert_with`; save_into is
    /// for batch-aligned bulk loads where the consumer settles those
    /// folds itself.
    pub fn save_into(&self, batch: &mut impl crate::engine::KvBatch, key: &K, row: &R) {
        batch.put(self.primary_key(key), row.encode_payload());
        for (ek, ev) in R::index_entries(key, row, self.ns) {
            batch.put(ek, ev);
        }
    }

    /// Index entry key for access method `I` derived from `key` + `row`.
    pub fn index_key<I: KvIndex<Key = K, Row = R>>(&self, key: &K, row: &R) -> Vec<u8> {
        I::entry_key(self.ns, key, row)
    }

    /// Delete a row: primary key + all declared index entries, all
    /// derived from the row being removed (the declaration IS the
    /// registry).
    ///
    /// Contract: `row` MUST be the row currently stored under `key` —
    /// the index entries are derived from `row`'s field values while the
    /// primary entry is derived from `key` alone, and a mismatched pair
    /// silently leaves dangling index entries (the KV layer's `del` is a
    /// no-op on absent keys, so nothing errors). The typical sound
    /// source is the `row` just fetched for `key` (get / scan 回表).
    /// When the row is not in hand or its provenance is doubtful, use
    /// [`Self::delete_by_pkey`], which derives both halves from the same
    /// read.
    pub fn delete(&mut self, key: &K, row: &R) {
        self.store.del(&self.primary_key(key));
        for (ek, _) in R::index_entries(key, row, self.ns) {
            self.store.del(&ek);
        }
        // Unfold from every declared reduce group (single call site —
        // delete_by_pkey reaches here after its internal get).
        R::__okm_apply_reduces(&mut self.store, key, row, self.ns, false);
        // Subscribe: deletion event (same best-effort contract as put),
        // stamped with the table's post-write epoch.
        self.epoch += 1;
        R::__okm_emit_event(crate::subscribe::Op::Delete, self.epoch, key, row);
    }

    /// Delete by primary key only: fetch the row from the primary table
    /// first, then delegate to [`Self::delete`]. Both halves of the
    /// removal (primary entry + index entries) derive from the same
    /// fetched row, so a mismatched key/row pair is structurally
    /// impossible. No-op (returns false) when the key has no row.
    ///
    /// The fetch costs one primary-table point read; when the caller
    /// already holds the row from a prior get/scan, prefer
    /// [`Self::delete`] with it.
    ///
    /// Not atomic: a concurrent writer between the internal get and the
    /// deletes could still orphan index entries. Sound under the
    /// current single-writer engine; revisit with transactional
    /// backends.
    pub fn delete_by_pkey(&mut self, key: &K) -> bool {
        match self.get(key) {
            Some(row) => {
                self.delete(key, &row);
                true
            }
            None => false,
        }
    }

    /// Point lookup: decode key payload + TLV payload.
    pub fn get(&self, key: &K) -> Option<R> {
        let k = self.primary_key(key);
        let v = self.store.get(&k)?;
        Some(R::decode_payload(&v))
    }

    /// Leftmost-prefix scan over access method `I`, then fetch-back
    /// (回表): decode each entry's key prefix and load its row payload.
    /// With a truncated `key(...)` prefix the decoded keys are partial
    /// (`PrefixKey.taken < KEY_LEN`) — use the trustworthy prefix fields
    /// to continue scanning the main table.
    pub fn scan<I: KvIndex<Key = K, Row = R>>(&self, encoded: &[u8]) -> Vec<(PrefixKey<K>, Option<R>)> {
        crate::scan_index::<S, I>(&self.store, self.ns, encoded)
            .into_iter()
            .map(|pk| {
                let row = if pk.taken == K::KEY_LEN {
                    self.get(&pk.decoded)
                } else {
                    None
                };
                (pk, row)
            })
            .collect()
    }

    /// Covered scan over access method `I`: the includes segment lives in
    /// the entry value, so a full-covering index answers without going
    /// back to the primary table (materialized view, ADR-0006). Returns
    /// `(key prefix, entry value bytes)`.
    pub fn scan_covered<I: KvIndex<Key = K, Row = R>>(
        &self,
        encoded: &[u8],
    ) -> Vec<(PrefixKey<K>, Vec<u8>)> {
        let p = I::entry_prefix(self.ns, encoded);
        let taken = I::key_prefix_width();
        let kl = K::KEY_LEN;
        self.store
            .scan_suffix_kv(&p)
            .into_iter()
            .map(|(suffix, v)| {
                assert!(suffix.len() >= taken, "index entry shorter than key prefix");
                let start = suffix.len() - taken;
                let decoded = if taken == kl {
                    K::decode(&suffix[start..])
                } else {
                    let mut buf = vec![0u8; kl];
                    buf[..taken].copy_from_slice(&suffix[start..]);
                    K::decode(&buf)
                };
                (PrefixKey { decoded, taken }, v)
            })
            .collect()
    }

    /// Raw rows: `(key encoding suffix, TLV payload)` per primary entry,
    /// in key order — the byte-level scan surface the Arrow bridge and
    /// snapshot exporter consume without struct materialization. Slot-0
    /// only: the ns segment also holds index entries (slots 1+), which
    /// are derived state excluded from export (rebuilt deterministically
    /// on import via put).
    pub fn scan_rows_raw(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut prefix = self.ns.to_be_bytes().to_vec();
        prefix.push(crate::index::PRIMARY_SLOT);
        self.store
            .scan_suffix(&prefix)
            .into_iter()
            .filter_map(|suffix| {
                let full = [prefix.as_slice(), suffix.as_slice()].concat();
                let v = self.store.get(&full)?;
                Some((suffix, v))
            })
            .collect()
    }

    /// Full-ns scan of primary keys (slot-0 entries only — the same
    /// slot-0 discipline as `scan_rows_raw`; index entries are slots 1+).
    pub fn scan_keys(&self) -> Vec<K> {
        let mut p = self.ns.to_be_bytes().to_vec();
        p.push(crate::index::PRIMARY_SLOT);
        self.store
            .scan_suffix(&p)
            .iter()
            .map(|suffix| {
                assert!(suffix.len() >= K::KEY_LEN, "primary entry shorter than key");
                K::decode(&suffix[suffix.len() - K::KEY_LEN..])
            })
            .collect()
    }
}
