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

use crate::index::{scan_access_method, AccessMethod};
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
    pub(crate) store: S,
    pub(crate) schema: CollectionSchema,
    pub(crate) ns: Vec<u8>,
    pub(crate) indexes: Vec<AccessMethod>,
    pub(crate) reduces: Vec<BoundReduce>,
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

    pub(crate) fn primary_key(&self, pkey: &[u8]) -> Vec<u8> {
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
    /// old document from every reduce group, then folds the new one. The
    /// expansion is the SHARED plan surface (`plan::plan_put`) — the
    /// embedded path is plan + local engine replay, so remote plans land
    /// byte-identical by construction.
    pub fn put(&mut self, pkey: &[u8], document: &ValueMap) -> Result<(), String> {
        // The embedded path reads its own state: old document + accs.
        let old = self.stored_document(pkey)?;
        let store = &self.store;
        let plan = self.plan_put(pkey, document, old.as_ref(), &|ek| store.get(ek))?;
        self.replay(&plan.ops);
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
    /// unfold arm of the reduce calling discipline. Same shared plan core.
    pub fn delete(&mut self, pkey: &[u8]) -> Result<(), String> {
        let Some(old) = self.stored_document(pkey)? else {
            return Ok(()); // missing key: a no-op (mirrored by plan_delete)
        };
        let plan = self.plan_delete(pkey, &old, &|ek| self.store.get(ek))?;
        self.replay(&plan.ops);
        Ok(())
    }

    /// The stored document at `pkey`, decoded (None = absent). The
    /// embedded path's read its own state before planning.
    fn stored_document(&self, pkey: &[u8]) -> Result<Option<ValueMap>, String> {
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

    /// Replay a plan's ops against the local engine, in order (the
    /// embedded execution arm — one engine instance, same atomicity
    /// boundary as index entries).
    fn replay(&mut self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) {
        for (key, value) in ops {
            match value {
                Some(v) => self.store.put(key.clone(), v.clone()),
                None => self.store.del(key),
            }
        }
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
