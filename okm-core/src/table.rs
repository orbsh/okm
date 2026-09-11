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
    pub fn put(&mut self, key: &K, row: &R) {
        self.store.put(self.primary_key(key), row.encode_payload());
        for (ek, ev) in R::index_entries(key, row, self.ns) {
            self.store.put(ek, ev);
        }
        // Cross-row aggregates: fold this row into each declared group.
        // Same store instance, so the RMW shares the engine's atomicity
        // boundary with the row + index writes.
        R::__okm_apply_aggregates(&mut self.store, key, row, self.ns, true);
        // Subscribe: write-path event into the declared channel
        // (best-effort try_send — full channel drops, never blocks).
        R::__okm_emit_event(crate::subscribe::Op::Put, key, row);
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
        // Unfold from every declared aggregate group (single call site —
        // delete_by_pkey reaches here after its internal get).
        R::__okm_apply_aggregates(&mut self.store, key, row, self.ns, false);
        // Subscribe: deletion event (same best-effort contract as put).
        R::__okm_emit_event(crate::subscribe::Op::Delete, key, row);
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
    /// snapshot exporter consume without struct materialization.
    pub fn scan_rows_raw(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let prefix = self.ns.to_be_bytes().to_vec();
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

    /// Full-ns scan of primary keys (slot-0 entries only).
    pub fn scan_keys(&self) -> Vec<K> {
        let p = self.ns.to_be_bytes().to_vec();
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
