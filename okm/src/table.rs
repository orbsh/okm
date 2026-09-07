//! Table — the row assembly point (ADR-0006): binds an engine instance, a
//! key type, and a row type. `put` writes the primary key (slot 0) and one
//! entry per access method in the same store instance, so atomicity holds
//! within a single engine; cross-ns atomicity is the store instance's
//! boundary, never the Table's.

use crate::engine::KvEngine;
use crate::index::{KvIndex, PRIMARY_SLOT, Row};
use crate::key::KeyEncode;

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

    /// Primary key entry (slot 0): `[ns 2B][0x00][key payload]`, value =
    /// TLV payload of the row.
    pub fn primary_key(&self, key: &K) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + K::KEY_LEN);
        buf.extend_from_slice(&self.ns.to_be_bytes());
        buf.push(PRIMARY_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Write a row: primary key + one index entry per access method.
    /// Callers add more index entries via [`Table::put_index`] only when
    /// the index is declared outside the row's `#[kv_index]` set (not
    /// possible today — put covers all declared access methods).
    pub fn put(&mut self, key: &K, row: &R) {
        self.store.put(self.primary_key(key), row.encode_payload());
        for e in R::index_entries(key, self.ns) {
            self.store.put(e, Vec::new());
        }
    }

    /// Index entry key for access method `I` (projection of the key only).
    pub fn index_key<I: KvIndex<Key = K>>(&self, key: &K) -> Vec<u8> {
        I::encode_entry(self.ns, key)
    }

    /// Write one index entry for `idx` derived from the row's key. The
    /// entry is a pure projection of the key — the row payload is not
    /// involved (value is empty; covering fields come from `includes`).
    pub fn put_index<I: KvIndex<Key = K>>(&mut self, key: &K) {
        self.store.put(I::encode_entry(self.ns, key), Vec::new());
    }

    /// Delete a row: primary key + all declared index entries must be
    /// removed by the caller (delete needs the index set, which lives at
    /// the row's declaration — see `Table::delete_with`).
    pub fn delete(&mut self, key: &K) {
        self.store.del(&self.primary_key(key));
        for e in R::index_entries(key, self.ns) {
            self.store.del(&e);
        }
    }

    pub fn delete_index<I: KvIndex<Key = K>>(&mut self, key: &K) {
        self.store.del(&I::encode_entry(self.ns, key));
    }

    /// Point lookup: decode key payload + TLV payload.
    pub fn get(&self, key: &K) -> Option<R> {
        let k = self.primary_key(key);
        let v = self.store.get(&k)?;
        Some(R::decode_payload(&v))
    }

    /// Leftmost-prefix scan over access method `I`, then fetch-back
    /// (回表): decode each primary key ID and load its row payload.
    pub fn scan<I: KvIndex<Key = K>>(&self, encoded: &[u8]) -> Vec<(K, Option<R>)> {
        crate::scan_index::<S, I>(&self.store, self.ns, encoded)
            .into_iter()
            .map(|k| {
                let row = self.get(&k);
                (k, row)
            })
            .collect()
    }

    /// Raw rows: `(key encoding suffix, TLV payload)` per primary entry,
    /// in key order — the byte-level scan surface the Arrow bridge and
    /// snapshot exporter consume without struct materialization.
    pub fn scan_rows_raw(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut prefix = self.ns.to_be_bytes().to_vec();
        prefix.push(PRIMARY_SLOT);
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
        let mut p = self.ns.to_be_bytes().to_vec();
        p.push(PRIMARY_SLOT);
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
