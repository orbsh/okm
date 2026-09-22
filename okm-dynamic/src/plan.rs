//! The plan surface (ADR-0022 remote mode): the put/delete expansion as
//! a pure function over caller-held state — no engine access.
//!
//! Embedded mode's `DynamicCollection::put`/`delete` and the remote
//! mode's `plan_put`/`plan_delete` share ONE expansion: the embedded
//! path is plan + local engine replay, the remote path ships the plan's
//! ops as one wire frame (ADR-0010 single `commit_batch` — document
//! write + index entries + acc updates land atomically). Byte equality
//! between the modes is structural, not a test coincidence.
//!
//! The caller holds the remote state: the OLD document (actor cache or
//! a prior GET — the read the embedded path does against its engine is
//! the remote caller's responsibility) and the current accumulators
//! (the actor IS the authoritative acc holder — ADR-0022's
//! single-writer-per-group ownership rule; steady state costs zero
//! extra round trips, only restart recovery reads accs once).

use okm_core::storage::VirtualStorage;
use crate::index::index_entries;
use crate::{decode_payload, encode_payload, DynamicCollection, ValueMap};
use okm_core::schema::CollectionSchema;

/// One planned write: the engine-shaped op list (MemBatch's
/// `(key, Option<value>)` — None = delete) plus the new accumulator
/// values the caller must adopt into its local cache.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedPut {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Group entry key → the accumulator bytes that entry now holds.
    /// The actor updates its local acc cache from this receipt; the
    /// next plan reads the same keys through its `acc_of` closure.
    pub new_accs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// One planned delete: index-entry removals + acc unfold + primary
/// delete (empty when the caller says no old document exists — the
/// embedded path's delete of a missing key is a no-op, mirrored here).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedDelete {
    pub ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub new_accs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Current-accumulator lookup for one group entry key. `None` = the
/// actor's cache has no entry for the group (first fold seeds it).
pub type AccOf<'a> = dyn Fn(&[u8]) -> Option<Vec<u8>> + 'a;

/// The plan's own acc overlay: within ONE plan, a later arm reads the
/// acc a prior arm wrote (overwrite = unfold-then-fold on the same
/// group; the fold must see the unfolded value, not the engine's stale
/// one). The caller's cache adopts the overlay at the end (`new_accs`).
struct AccOverlay<'a> {
    base: &'a AccOf<'a>,
    written: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a> AccOverlay<'a> {
    fn new(base: &'a AccOf<'a>) -> Self {
        Self { base, written: std::collections::HashMap::new() }
    }
    fn get(&self, ek: &[u8]) -> Option<Vec<u8>> {
        match self.written.get(ek) {
            Some(v) => v.clone(),
            None => (self.base)(ek),
        }
    }
    fn put(&mut self, ek: Vec<u8>, acc: Vec<u8>) {
        self.written.insert(ek, Some(acc));
    }
}

impl<S: VirtualStorage> DynamicCollection<S> {
    /// Plan one put's full op list WITHOUT touching an engine. `old`
    /// is the currently-stored document at `pkey` (None = fresh key);
    /// `acc_of` resolves a group's current accumulator from the
    /// caller's cache.
    pub fn plan_put(
        &self,
        pkey: &[u8],
        document: &ValueMap,
        old: Option<&ValueMap>,
        acc_of: &AccOf,
    ) -> Result<PlannedPut, String> {
        self.check_key(pkey)?;
        let mut plan = PlannedPut { ops: Vec::new(), new_accs: Vec::new() };
        let mut overlay = AccOverlay::new(acc_of);
        // Overwrite: sweep the old document's index entries + unfold it
        // from every group (the embedded put's overwrite arm, verbatim).
        if let Some(old_row) = old {
            let old_entries =
                index_entries(self.schema(), &self.ns, &self.indexes, pkey, old_row)?;
            for (ek, _) in old_entries {
                plan.ops.push((ek, None));
            }
            self.plan_reduces(old_row, false, &mut plan, &mut overlay)?;
        }
        // New entries + primary write.
        let entries =
            index_entries(self.schema(), &self.ns, &self.indexes, pkey, document)?;
        let payload = encode_payload(self.schema(), document).map_err(|e| e.to_string())?;
        self.plan_reduces(document, true, &mut plan, &mut overlay)?;
        plan.ops.push((self.primary_key(pkey), Some(payload)));
        for (ek, ev) in entries {
            plan.ops.push((ek, Some(ev)));
        }
        Ok(plan)
    }

    /// Plan one delete's op list WITHOUT touching an engine. `old` is
    /// the stored document (None = nothing to remove: an empty plan,
    /// mirroring the embedded delete's no-op on a missing key).
    pub fn plan_delete(
        &self,
        pkey: &[u8],
        old: &ValueMap,
        acc_of: &AccOf,
    ) -> Result<PlannedDelete, String> {
        self.check_key(pkey)?;
        let mut plan = PlannedDelete { ops: Vec::new(), new_accs: Vec::new() };
        let mut overlay = AccOverlay::new(acc_of);
        let entries =
            index_entries(self.schema(), &self.ns, &self.indexes, pkey, old)?;
        for (ek, _) in entries {
            plan.ops.push((ek, None));
        }
        self.plan_reduces(old, false, &mut plan, &mut overlay)?;
        plan.ops.push((self.primary_key(pkey), None));
        Ok(plan)
    }

    /// The reduce calling discipline, planned form: same order and the
    /// same seed/unfold-missing rules as the embedded `apply_reduces`,
    /// reading accs through `acc_of` instead of the engine.
    fn plan_reduces(
        &self,
        document: &ValueMap,
        add: bool,
        plan: &mut impl PlanSink,
        overlay: &mut AccOverlay,
    ) -> Result<(), String> {
        for reduce in &self.reduces {
            let ek = reduce.entry_key(self.schema(), &self.ns, document)?;
            let mut acc = match overlay.get(&ek) {
                Some(bytes) => bytes,
                // Seed only on the fold arm (same rule as embedded):
                // an unfold hitting a missing entry is a discipline
                // violation, not a zero group.
                None if add => reduce.spec.logic.seed(),
                None => return Ok(()),
            };
            if add {
                reduce.spec.logic.fold(&mut acc, document)?;
            } else {
                reduce.spec.logic.unfold(&mut acc, document)?;
            }
            plan.push((ek.clone(), Some(acc.clone())));
            plan.push_acc((ek.clone(), acc.clone()));
            overlay.put(ek, acc);
        }
        Ok(())
    }

    fn check_key(&self, pkey: &[u8]) -> Result<(), String> {
        if pkey.len() != self.schema().key_len {
            return Err(format!(
                "key width mismatch: got {}, schema declares {}",
                pkey.len(),
                self.schema().key_len
            ));
        }
        Ok(())
    }
}

/// The two plan shapes share the sink's push form.
trait PlanSink {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>));
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>));
}

impl PlanSink for PlannedPut {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>)) {
        self.ops.push(op);
    }
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>)) {
        self.new_accs.push(entry);
    }
}

impl PlanSink for PlannedDelete {
    fn push(&mut self, op: (Vec<u8>, Option<Vec<u8>>)) {
        self.ops.push(op);
    }
    fn push_acc(&mut self, entry: (Vec<u8>, Vec<u8>)) {
        self.new_accs.push(entry);
    }
}

/// Decode a stored payload into the caller-side `old` document (the
/// remote actor's cache-refill helper — what the embedded path does
/// against its engine on overwrite, offered here so the binding wraps
/// it instead of re-implementing the decode).
pub fn decode_stored(schema: &CollectionSchema, payload: &[u8]) -> Result<ValueMap, String> {
    decode_payload(schema, payload).map_err(|e| e.to_string())
}
