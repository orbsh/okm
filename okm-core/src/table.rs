//! Table — the row assembly point (ADR-0006): binds an engine instance, a
//! key type, and a row type. `put` writes the primary key (slot 0) and one
//! entry per access method in the same store instance, so atomicity holds
//! within a single engine; cross-ns atomicity is the store instance's
//! boundary, never the Table's. Index entries derive from the row payload
//! (indexed + includes fields live there), so put and delete are both
//! row-shaped.

use crate::storage::VirtualStorage;
use crate::index::{KvIndex, Row};
use crate::key::{KeyEncode, PrefixKey};

pub struct Table<S, K: KeyEncode, R: Row<Key = K>> {
    store: S,
    /// Monotonic write-batch counter (in-process only, never persisted):
    /// bumped on every put/delete, stamped on emitted subscribe events so
    /// consumers can fold to exact same-table batch boundaries. Resets on
    /// restart — meaningful only within one process lifetime.
    epoch: u64,
    _marker: std::marker::PhantomData<(K, R)>,
}

impl<S: VirtualStorage, K: KeyEncode, R: Row<Key = K>> Table<S, K, R> {
    /// The ns prefix is NOT a constructor argument: it is declared once
    /// on the key struct (`#[ok_ns]`) and read at compile time via
    /// `R::NS_PREFIX` (ADR-0002: the ns dictionary is code; ADR-0010:
    /// engine choice is per-assembly-point, ns is not). The assembly
    /// site picks the engine; it never restates the ns.
    pub fn new(store: S) -> Self {
        Self {
            store,
            epoch: 0,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// The table's key header: `[0xFF][part 1B] (if declared)[ns 2B]` — every
    /// key byte sequence this table writes starts with it. ADR-0014 §5:
    /// the partition segment precedes the ns header (workload isolation
    /// lives outside ownership scope); absent when the row declares no
    /// `#[ok_partition]`.
    fn header(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3);
        buf.extend_from_slice(R::PARTITION_PREFIX);
        buf.extend_from_slice(R::NS_PREFIX);
        buf
    }

    /// Primary key entry (slot 0): `[0xFF][part 1B] (if declared)[ns 2B][slot 0]
    /// [key payload]`, value = TLV payload of the row. The slot byte keeps
    /// the header uniform with index entries (`[ns 2B][slot 1B]`); slot 0 =
    /// primary, per ADR-0005. The partition segment precedes the ns header
    /// (ADR-0014 §5): workload isolation lives outside ownership scope —
    /// a partition groups tables by compaction profile, a namespace groups
    /// them by owner; the 0xFF escape byte makes partitioned and
    /// unpartitioned keys structurally disjoint. Absent when the row
    /// declares no `#[ok_partition]`.
    pub fn primary_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
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
            let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, old, &header, false);
        }
        self.store.put(pkey, row.encode_payload());
        for (ek, ev) in R::index_entries(key, row, &self.header()) {
            self.store.put(ek, ev);
        }
        // Cross-row reduces: fold this row into each declared group.
        // Same store instance, so the RMW shares the engine's atomicity
        // boundary with the row + index writes.
        let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, row, &header, true);
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
    /// Cross-process exclusion is the engine's job, not OKM's: fjall
    /// takes a file lock on open (a second process fails to open the
    /// same directory), slatedb fences with a writer epoch in the
    /// manifest (a stale writer's commits are rejected). With the engine
    /// arbitrating, any moment has at most one live writer — the
    /// single-writer model holds end to end, and an OKM-level lock would
    /// only double-guard what the engine already enforces. The
    /// multi-writer future (optimistic CAS) triggers only if multiple
    /// live writers over one store become a real requirement.
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
    pub fn save_into(&self, batch: &mut impl crate::storage::KvBatch, key: &K, row: &R) {
        batch.put(self.primary_key(key), row.encode_payload());
        for (ek, ev) in R::index_entries(key, row, &self.header()) {
            batch.put(ek, ev);
        }
    }

    /// Index entry key for access method `I` derived from `key` + `row`.
    pub fn index_key<I: KvIndex<Key = K, Row = R>>(&self, key: &K, row: &R) -> Vec<u8> {
        I::entry_key(&self.header(), key, row)
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
        for (ek, _) in R::index_entries(key, row, &self.header()) {
            self.store.del(&ek);
        }
        // Unfold from every declared reduce group (single call site —
        // delete_by_pkey reaches here after its internal get).
        let header = self.header();
        R::__okm_apply_reduces(&mut self.store, key, row, &header, false);
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
        crate::scan_index::<S, I>(&self.store, &self.header(), encoded)
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
        let p = I::entry_prefix(&self.header(), encoded);
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
        let mut prefix = self.header();
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
    /// Clear stale entries under deprecated index slots (ADR-0005):
    /// a `#[ok_index(..., deprecated)]` declaration keeps its slot
    /// reserved but writes nothing; entries written before the
    /// deprecation remain until this method deletes them
    /// (`[ns][deprecated slot]` prefix scan, delete each). Returns the
    /// number of entries removed. Idempotent — a second call finds
    /// nothing. Does NOT touch live slots or the primary table.
    pub fn prune_deprecated_slots(&mut self) -> usize {
        let mut removed = 0;
        for slot in R::DEPRECATED_SLOTS {
            let mut p = self.header();
            p.push(*slot);
            for sfx in self.store.scan_suffix(&p) {
                let mut full = p.clone();
                full.extend_from_slice(&sfx);
                self.store.del(&full);
                removed += 1;
            }
        }
        removed
    }

    pub fn scan_keys(&self) -> Vec<K> {
        let mut p = self.header();
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

use std::collections::BTreeMap;

use crate::obj_dict::DictCache;
use crate::obj_dynamic::DynamicValue;

impl<S: VirtualStorage, K: KeyEncode, R: Row<Key = K>> Table<S, K, R> {
    /// Dynamic-segment key: `[header][DYNAMIC_SLOT][key payload]` — the
    /// per-row extension entry next to the primary (slot 0), same
    /// skeleton as an index entry with no field segment (ADR-0012).
    fn variants_key(&self, key: &K) -> Vec<u8> {
        let mut buf = self.header();
        buf.push(crate::index::DYNAMIC_SLOT);
        buf.extend_from_slice(&key.encode());
        buf
    }

    /// Read the obj's dynamic fields: slot 1 frames decoded to names via
    /// the field-name dictionary. `None` = the obj has no dynamic entry
    /// (distinct from an empty entry, which is also None here — a
    /// zero-frame list is never written).
    pub fn get_variants(&mut self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        let raw = self.store.get(&self.variants_key(key))?;
        let frames = crate::obj_dynamic::decode_variants(&raw);
        if frames.is_empty() {
            return None;
        }
        let mut d = DictCache::default();
        let header = self.header();
        let mut out = BTreeMap::new();
        for f in frames {
            if let Some(name) = d.name_for(&mut self.store, &header, f.id) {
                out.insert(name, f.value);
            }
            // Unknown id (dictionary entry absent): drop the field. Under
            // single-writer this cannot happen; with shared engines it is
            // the same degraded mode the cache documents.
        }
        Some(out)
    }

    /// Replace the obj's dynamic fields wholesale: the map becomes the
    /// entire slot-1 entry (fields absent from `variants` are removed —
    /// this is a whole-entry put, not a per-field merge). First-seen
    /// names allocate ids through the dictionary; one engine batch
    /// carries any dictionary growth plus the entry itself.
    pub fn set_variants(
        &mut self,
        key: &K,
        variants: &BTreeMap<String, DynamicValue>,
    ) {
        let mut d = DictCache::default();
        let header = self.header();
        let mut body = Vec::new();
        for (name, value) in variants {
            let id = d.id_for(&mut self.store, &header, name);
            crate::obj_dynamic::put_frame(&mut body, id, value);
        }
        self.epoch += 1;
        let k = self.variants_key(key);
        if body.is_empty() {
            self.store.del(&k);
        } else {
            self.store.put(k, body);
        }
    }

    /// The whole obj as a name-keyed map: declared fields (typed, from
    /// slot 0 via FieldDesc) merged over the dynamic segment (slot 1).
    /// Declared names win by construction — the two paths cannot collide
    /// because the typed side is compile-time and the dictionary only
    /// allocates names it has never seen (a dynamic name equal to a
    /// declared name would have to be allocated first; convention:
    /// callers don't, and `get_variants` exposes any violation).
    pub fn get_object(&mut self, key: &K) -> Option<BTreeMap<String, DynamicValue>> {
        // v1: dynamic fields only. Declared fields carry Rust types the
        // derive knows how to lift into DynamicValue — that helper lands
        // with the derive-side row↔map bridge (PLAN Phase 8); until then
        // the declared half is reachable through the typed `get`.
        let dynamic = self.get_variants(key)?;
        Some(dynamic)
    }

    /// Whole-obj write: fields whose names match the row struct go to
    /// the typed path (slot 0, `R::decode_payload` + field assignment),
    /// the rest to the dynamic segment. Unknown names allocate (this is
    /// the external-data entry point — unknown fields are normal input,
    /// unlike the typed decoder where they are caller bugs). One call,
    /// two slots, one epoch bump.
    pub fn set_object(
        &mut self,
        key: &K,
        object: &BTreeMap<String, DynamicValue>,
    ) {
        // Split by declared/undeclared.
        // v1: everything routes to the dynamic segment. Typed-path
        // assignment (declared names → slot 0 via a derive-generated
        // lift) lands with the row↔map bridge in PLAN Phase 8; until
        // then set_object is set_variants with a wider contract.
        self.set_variants(key, object);
    }

    /// Drop the dynamic entry (declared fields untouched). True if an
    /// entry existed.
    pub fn delete_variants(&mut self, key: &K) -> bool {
        let k = self.variants_key(key);
        if self.store.get(&k).is_some() {
            self.store.del(&k);
            self.epoch += 1;
            true
        } else {
            false
        }
    }
}
