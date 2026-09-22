//! Dynamic-side reduce (ADR-0022): the mutable-reduce entry driven by
//! host-language callables, byte-layout-aligned with the Rust-side
//! `ReduceLogic` path (okm-core/src/model/reduce.rs).
//!
//! Entry key: `[ns 2B][slot u16 BE][group segment]` — no key-prefix tail,
//! the group identity IS the entry. The accumulator travels as bytes
//! (byte-transparent form, the `Vec<u8>` escape hatch of
//! `ReduceCodec`): the host language owns the acc layout — okm-dynamic
//! only runs get → callable → put inside the calling discipline.
//!
//! Calling discipline (the correctness surface — ADR-0008's exactly-once
//! invariant re-derived as a deployment-shape contract, embedded mode:
//! single writer by construction):
//! - put (no old document): seed acc from empty, fold the new document;
//! - overwrite: unfold the stored document, then fold the new one;
//! - delete: unfold the stored document.
//! Document write and acc update share the same engine instance —
//! atomicity holds within one engine, same boundary as index entries.
//!
//! The accumulator has an independent lifetime: a deleted last document
//! leaves an empty group, not a deleted entry. No zero-garbage-collect —
//! a zero-detection policy is the caller's (same as the Rust side).

use okm_core::storage::VirtualStorage;
use crate::{Value, ValueMap};

/// Fold/unfold callable: mutates the accumulator bytes by one document.
/// The acc arrives as the empty slice on a group's first fold — the
/// callable owns the seed (e.g. zero for a BE u64 counter).
pub type AccOp = Box<dyn Fn(&mut Vec<u8>, &ValueMap) -> Result<(), String> + Send + Sync>;

/// One declared dynamic reduce: slot + group fields + the host callables.
pub struct ReduceSpec {
    /// Item-local slot in the reduce segment (the derive allocates the
    /// Rust side in declaration order; the caller mirrors that rule).
    pub slot: u16,
    /// Group-by fields, named document payload fields in declaration
    /// order — their encodings form the entry's group segment.
    pub group_fields: Vec<String>,
    /// Add one document into the group's accumulator.
    pub fold: AccOp,
    /// Remove one document from the group's accumulator. Reversibility
    /// `unfold(fold(a, x)) = a` is the implementor's obligation.
    pub unfold: AccOp,
}

impl std::fmt::Debug for ReduceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReduceSpec")
            .field("slot", &self.slot)
            .field("group_fields", &self.group_fields)
            .finish()
    }
}

/// A reduce bound to one collection (holds the group segment layout).
pub struct BoundReduce {
    pub spec: ReduceSpec,
}

impl BoundReduce {
    pub fn new(spec: ReduceSpec) -> Self {
        Self { spec }
    }

    /// The group segment: named fields' payload encodings in declaration
    /// order — the same encoders the index layer uses, so byte parity
    /// with the Rust-side `group_bytes` holds.
    pub fn group_bytes(
        &self,
        schema: &okm_core::schema::TableSchema,
        document: &ValueMap,
    ) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        for name in &self.spec.group_fields {
            if schema.key_fields.iter().any(|f| &f.name == name) {
                return Err(format!(
                    "group field `{name}` is a key field; groups take payload fields only"
                ));
            }
            let f = schema
                .hot_fields
                .iter()
                .chain(schema.cold_fields.iter())
                .find(|f| &f.name == name)
                .ok_or_else(|| format!("group field `{name}` not in schema"))?;
            let v = document
                .get(name)
                .ok_or_else(|| format!("group field `{name}` missing from document"))?;
            // Fixed-width BE storage form (the same rule as index
            // segments: group segments take fixed-width kinds, matching
            // the Rust-side walk).
            let bytes = match (f.ty, v) {
                (okm_core::field::FieldType::U8, Value::U8(n)) => vec![*n],
                (okm_core::field::FieldType::U16, Value::U16(n)) => n.to_be_bytes().to_vec(),
                (okm_core::field::FieldType::U32, Value::U32(n)) => n.to_be_bytes().to_vec(),
                (okm_core::field::FieldType::U64, Value::U64(n)) => n.to_be_bytes().to_vec(),
                (okm_core::field::FieldType::FixedBytes, Value::Bytes(b)) => b.clone(),
                _ => {
                    return Err(format!(
                        "group field `{name}`: fixed-width numeric/bytes kind required"
                    ))
                }
            };
            buf.extend_from_slice(&bytes);
        }
        Ok(buf)
    }

    /// Full entry key `[ns 2B][slot u16 BE][group segment]`.
    pub fn entry_key(
        &self,
        schema: &okm_core::schema::TableSchema,
        ns: &[u8],
        document: &ValueMap,
    ) -> Result<Vec<u8>, String> {
        let g = self.group_bytes(schema, document)?;
        let mut buf = Vec::with_capacity(ns.len() + 2 + g.len());
        buf.extend_from_slice(ns);
        buf.extend_from_slice(&self.spec.slot.to_be_bytes());
        buf.extend_from_slice(&g);
        Ok(buf)
    }
}

/// Read one group's current accumulator (None = group not yet created).
pub fn reduce_get<S: VirtualStorage>(
    store: &S,
    entry_key: &[u8],
) -> Option<Vec<u8>> {
    store.get(entry_key)
}

/// Scan every group of one reduce: group segment + acc bytes. Prefix
/// `[ns 2B][slot u16 BE]` — each suffix is the group segment.
pub fn scan_reduces<S: VirtualStorage>(
    store: &S,
    ns: &[u8],
    slot: u16,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut prefix = Vec::with_capacity(ns.len() + 2);
    prefix.extend_from_slice(ns);
    prefix.extend_from_slice(&slot.to_be_bytes());
    store
        .scan_suffix_kv(&prefix)
        .into_iter()
        .collect()
}
