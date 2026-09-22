//! DynamicCollection — a runtime-typed table facade over VirtualStorage.
//!
//! The typed `Collection<S, K, R>` binds document/key types at compile time; this
//! facade binds them at runtime through a `CollectionSchema` + declared
//! `AccessMethod`s. Put/get/delete/scan produce and consume the same
//! bytes as the derive (codec shared with the typed path, byte equality
//! locked by the cross tests).
//!
//! Single-writer only (same contract as `Collection`): `&mut self` puts, the
//! engine arbitrates cross-process exclusion.
//!
//! Capability scope (ADR-0022): subscribe stays excluded; reduce and
//! function/partial indexes are binding-implementable via host-language
//! callables under the deployment-shape contract (embedded: in-process
//! calling discipline; remote: operation payloads carry semantic
//! results, single-writer-per-group).

use okm_core::storage::VirtualStorage;

use crate::index::{delete_entries, index_entries, scan_access_method, AccessMethod};
use crate::reduce::{BoundReduce, ReduceSpec};
use crate::{decode_payload, encode_payload, CollectionSchema, ValueMap};

/// Codec errors surfaced as strings (dynamic callers are host-language
/// bridges — error values, not typed hierarchies).
fn codec<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// A runtime-declared table: schema + access methods + a namespaced
/// slot of storage. `ns` MUST be unique within the store instance (the
/// two-instance data/meta model guarantees that across planes).
pub struct DynamicCollection<S: VirtualStorage> {
    store: S,
    schema: CollectionSchema,
    ns: Vec<u8>,
    indexes: Vec<AccessMethod>,
    reduces: Vec<BoundReduce>,
}

impl<S: VirtualStorage> DynamicCollection<S> {
    /// Declare a dynamic table. `ns` is the 2-byte BE namespace segment
    /// (the table's own allocation; access methods share it, slot bytes
    /// discriminate within). Slots on the access methods are caller-
    /// allocated (1-based, unique per table) — mirroring declaration
    /// order in the typed path.
    pub fn new(store: S, ns: u16, schema: CollectionSchema, indexes: Vec<AccessMethod>) -> Self {
        Self::with_reduces(store, ns, schema, indexes, Vec::new())
    }

    /// Declare a dynamic table with reduce groups (ADR-0022). Slots on
    /// the reduces are caller-allocated (unique per table, mirroring the
    /// derive's declaration-order rule in the reduce segment).
    pub fn with_reduces(
        store: S,
        ns: u16,
        schema: CollectionSchema,
        indexes: Vec<AccessMethod>,
        reduces: Vec<ReduceSpec>,
    ) -> Self {
        let slots: Vec<u16> = indexes.iter().map(|i| i.slot).collect();
        debug_assert!(
            slots.iter().all(|s| *s > 0) && {
                let mut sorted = slots.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.len() == slots.len()
            },
            "access method slots must be unique and 1-based"
        );
        let rslots: Vec<u16> = reduces.iter().map(|r| r.slot).collect();
        debug_assert!(
            rslots.iter().all(|s| *s > 0) && {
                let mut sorted = rslots.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.len() == rslots.len()
            },
            "reduce slots must be unique and 1-based"
        );
        Self {
            store,
            schema,
            ns: ns.to_be_bytes().to_vec(),
            indexes,
            reduces: reduces.into_iter().map(BoundReduce::new).collect(),
        }
    }

    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Direct store access (tests, maintenance sweeps).
    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn indexes(&self) -> &[AccessMethod] {
        &self.indexes
    }

    /// Declare an access method at runtime (binding-time registration,
    /// ADR-0022: the host language registers its callables after the
    /// table exists). The slot must be unique and non-zero — a collision
    /// is a caller bug surfaced as an error, not silently merged.
    pub fn declare_index(&mut self, index: AccessMethod) -> Result<(), String> {
        if index.slot == 0 || self.indexes.iter().any(|i| i.slot == index.slot) {
            return Err(format!(
                "access method slot {} must be unique and non-zero",
                index.slot
            ));
        }
        self.indexes.push(index);
        Ok(())
    }

    /// Declare a reduce group at runtime (binding-time registration).
    /// Slot rule mirrors `declare_index`.
    pub fn declare_reduce(&mut self, spec: crate::ReduceSpec) -> Result<(), String> {
        if spec.slot == 0 || self.reduces.iter().any(|r| r.spec.slot == spec.slot) {
            return Err(format!(
                "reduce slot {} must be unique and non-zero",
                spec.slot
            ));
        }
        self.reduces.push(crate::BoundReduce::new(spec));
        Ok(())
    }

    /// A reduce group's entry key for a group-field value map (read-side
    /// probe: the caller names the GROUP fields, same rule as the
    /// Rust-side `reduce_get` probe).
    pub fn reduce_entry_key(
        &self,
        _ns: u16,
        group_values: &ValueMap,
    ) -> Result<Vec<u8>, String> {
        // The first registered reduce owns this probe shape (single-reduce
        // tables; multi-reduce probes go through BoundReduce directly).
        let reduce = self
            .reduces
            .first()
            .ok_or_else(|| "no reduce registered".to_string())?;
        reduce.entry_key(&self.schema, &self.ns, group_values)
    }

    fn primary_key(&self, pkey: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(3 + self.schema.key_len);
        buf.extend_from_slice(&self.ns);
        buf.extend_from_slice(&0u16.to_be_bytes()); // PRIMARY_SLOT
        buf.extend_from_slice(pkey);
        buf
    }

    /// Write one document: primary entry + one index entry per access method
    /// + the reduce calling discipline (ADR-0022). Overwrite first removes
    /// the old document's index entries (they are keyed by indexed values —
    /// a changed value would otherwise leave a dangling entry), unfolds the
    /// old document from every reduce group, then folds the new one. Document
    /// write and acc updates share the same engine instance — atomicity
    /// holds within one engine (same boundary as index entries).
    pub fn put(&mut self, pkey: &[u8], document: &ValueMap) -> Result<(), String> {
        if pkey.len() != self.schema.key_len {
            return Err(format!(
                "key width mismatch: got {}, schema declares {}",
                pkey.len(),
                self.schema.key_len
            ));
        }
        let old_pkey = self.primary_key(pkey);
        // Sweep the old document's index entries before overwriting.
        if let Some(old_payload) = self.store.get(&old_pkey) {
            if let Ok(old_row) = decode_payload(&self.schema, &old_payload).map_err(codec) {
                let old_entries =
                    index_entries(&self.schema, &self.ns, &self.indexes, pkey, &old_row)?;
                delete_entries(&mut self.store, &old_entries);
                // Overwrite unfold: remove the stored document from every
                // group before the new document folds in.
                self.apply_reduces(&old_row, false)?;
            }
            // Undecodable old payload: the primary entry is overwritten
            // below anyway; a dangling index entry is the caller's schema
            // mismatch, surfaced by scan (missing primary on get).
        }
        let pkey_owned = pkey.to_vec();
        let entries = index_entries(&self.schema, &self.ns, &self.indexes, &pkey_owned, document)?;
        let payload = encode_payload(&self.schema, document).map_err(codec)?;
        self.apply_reduces(document, true)?;
        self.store.put(old_pkey, payload);
        for (ek, ev) in entries {
            self.store.put(ek, ev);
        }
        Ok(())
    }

    /// Point read by primary key (dynamic `Collection::get`).
    pub fn get(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
        if pkey.len() != self.schema.key_len {
            return Err(format!(
                "key width mismatch: got {}, schema declares {}",
                pkey.len(),
                self.schema.key_len
            ));
        }
        Ok(self
            .store
            .get(&self.primary_key(pkey))
            .map(|payload| decode_payload(&self.schema, &payload).map_err(codec))
            .transpose()?)
    }

    /// Delete a document: primary entry + every access method's entry for
    /// this key (entries are recomputed from the stored document — the delete
    /// path must see the same indexed values the write produced) + the
    /// unfold arm of the reduce calling discipline.
    pub fn delete(&mut self, pkey: &[u8]) -> Result<(), String> {
        let pk = self.primary_key(pkey);
        if let Some(payload) = self.store.get(&pk) {
            let document = decode_payload(&self.schema, &payload).map_err(codec)?;
            let entries = index_entries(&self.schema, &self.ns, &self.indexes, pkey, &document)?;
            delete_entries(&mut self.store, &entries);
            self.apply_reduces(&document, false)?;
            self.store.del(&pk);
        }
        Ok(())
    }

    /// The reduce calling discipline, one direction per call site: fold
    /// (add = true, a document entering the groups) or unfold (add = false,
    /// a stored document leaving them). Each group is a full
    /// get → callable → put against the same engine instance. A missing
    /// group entry is seeded from the logic (`ReduceLogic::seed`, the
    /// `Default::default()` counterpart) before the fold.
    fn apply_reduces(&mut self, document: &ValueMap, add: bool) -> Result<(), String> {
        for reduce in &self.reduces {
            let ek = reduce.entry_key(&self.schema, &self.ns, document)?;
            let mut acc = match self.store.get(&ek) {
                Some(bytes) => bytes,
                // Seed only on the fold arm: an unfold hitting a missing
                // entry is a discipline violation (nothing was folded),
                // not a zero group — surfaced by the callable, not here.
                None if add => reduce.spec.logic.seed(),
                None => return Ok(()),
            };
            if add {
                reduce.spec.logic.fold(&mut acc, document)?;
            } else {
                reduce.spec.logic.unfold(&mut acc, document)?;
            }
            self.store.put(ek, acc);
        }
        Ok(())
    }

    /// Access-method scan (leftmost prefix over the indexed fields,
    /// caller-encoded): returns the matching documents' primary keys, decoded.
    /// THE routing primitive — an event resolves its targets here.
    pub fn scan(
        &self,
        index_slot: u16,
        encoded_prefix: &[u8],
    ) -> Result<Vec<ValueMap>, String> {
        let index = self
            .indexes
            .iter()
            .find(|i| i.slot == index_slot)
            .ok_or_else(|| format!("no access method with slot {index_slot}"))?;
        scan_access_method(&self.store, &self.schema, &self.ns, index, encoded_prefix)
    }
}
