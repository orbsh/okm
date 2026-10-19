//! Fully dynamic graph edges (ADR-0017 §4, second form): a dynamic edge
//! with EMPTY declarations — identity only, attributes as runtime data.
//!
//! Shares the fixed-ontology wire exactly: the same entry kinds on
//! ADR-0030 fixed faces, the same self-describing `[ns 2B][len][pkey]` refs,
//! the same PREDICATE VOCABULARY (0x04/0x05) for predicates. The differences are
//! declaration-side only:
//!
//! - endpoints need NO declaration at all — refs carry their own pkey
//!   width (varint len), so there is no registry to declare or maintain;
//! - edge attributes are nTLV frames (id + vocabulary, self-describing
//!   value types) in the payload instead of compile-time fields — attribute
//!   names are runtime data, so their ids allocate through the payload's
//!   FIELD vocabulary (0x02/0x03), never through the predicate vocabulary
//!   (0x04/0x05): one id space per vocabulary, no collision;
//! - `edge_id` stays caller-chosen, verified non-colliding at link.
//!
//! The handle takes the ns and the schema as a PAIR (ADR-0025) — the
//! declaration's two halves, from the export side — and rejects a
//! disagreement instead of overriding one with the other.
//!
//! Capability scope inherited from the dynamic layer (ADR-0022):
//! subscribe stays excluded; func/partial indexes and reduce are
//! callable-implementable on the table side.

use okm_core::index::PRIMARY_SLOT;
use okm_core::model::graph::{
    EDGE_IN_SLOT, EDGE_OUT_SLOT, EDGE_PREDICATE_INDEX_SLOT, EDGE_PREDICATE_IN_SLOT,
    EDGE_PREDICATE_OUT_SLOT, NodeRef,
};
use okm_core::obj_vocab::{VocabCache, FIELD_VOCAB, PREDICATE_VOCAB};
use okm_core::obj_dynamic::{decode_named, put_frame_named, DynamicValue};
use okm_core::schema::CollectionSchema;
use okm_core::storage::{scan_suffix_kv, MemBatch, VirtualStorage};

/// One dynamic edge: endpoints by reference, predicate by name,
/// attributes as a name-keyed map (nTLV frames on the wire, ids allocated
/// through the payload's field vocabulary).
#[derive(Clone, Debug)]
pub struct DynEdge {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub predicate: String,
    pub attrs: std::collections::BTreeMap<String, DynamicValue>,
}

/// Engine = the dynamic form's operation surface. Symmetric with
/// `okm_core::Graph` (fixed-ontology): same faces, same wire, runtime
/// declarations. No registry — refs are self-describing.
pub struct Graph<S> {
    store: S,
    /// Predicate vocabulary (0x04/0x05) — per-handle state, allocation is
    /// single-writer, reads load on probe.
    predicates: VocabCache,
    /// Payload field-name vocabulary (0x02/0x03) for attribute frames.
    fields: VocabCache,
    ns: Vec<u8>,
    schema: CollectionSchema,
}

impl<S: VirtualStorage> Graph<S> {
    /// Assemble a dynamic graph's edge collection from the declaration's
    /// PAIR: the ns and the schema it yielded. `ns` MUST be unique within
    /// the store instance (same rule as `DynamicCollection`), and a schema
    /// that names a different ns is rejected — never overridden.
    pub fn new(store: S, ns: u16, schema: CollectionSchema) -> Self {
        let ns_bytes = ns.to_be_bytes();
        assert!(
            schema.ns_prefix.is_empty() || schema.ns_prefix == ns_bytes,
            "graph assembly: ns {ns} disagrees with the schema's declared ns {:?} — the pair comes from one declaration, never from two",
            schema.ns_prefix
        );
        Self {
            store,
            predicates: VocabCache::new(PREDICATE_VOCAB),
            fields: VocabCache::new(FIELD_VOCAB),
            ns: ns_bytes.to_vec(),
            schema,
        }
    }

    /// The exported declaration this handle was assembled from.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Membership (ADR-0017 §4): the declaration's member set rides the
    /// schema as data, and a dynamic writer enforces it against the refs' ns —
    /// the same check the static form makes through `E::Nodes`. An EMPTY set
    /// declares nothing (a plain writer without a graph declaration), so
    /// nothing is checked rather than guessed.
    fn check_members(&self, src: &NodeRef, dst: &NodeRef) -> Result<(), String> {
        if self.schema.members.is_empty() {
            return Ok(());
        }
        for (side, node) in [("src", src), ("dst", dst)] {
            if !self.schema.members.contains(&node.ns_id()) {
                return Err(format!(
                    "{side} ns {} is not a member of this graph",
                    node.ns_id()
                ));
            }
        }
        Ok(())
    }

    fn primary_key(&self, edge_id: u64) -> Vec<u8> {
        let mut k = self.ns.clone();
        okm_core::write_slot(&mut k, PRIMARY_SLOT);
        k.extend_from_slice(&edge_id.to_be_bytes());
        k
    }

    fn face_key(&self, slot: u16, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = self.ns.clone();
        okm_core::write_slot(&mut k, slot);
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// Resolve (or allocate) the predicate id through the graph's KIND
    /// VOCABULARY (0x04/0x05) — first-seen-claims-next, append-only.
    fn predicate_vocab_id(&mut self, predicate: &str) -> u16 {
        self.predicates.id_for(&mut self.store, &self.ns, predicate)
    }

    /// Attribute map → nTLV frames (ids via the field vocabulary; values are
    /// self-describing typed frames — the dynamic segment's own wire).
    fn attrs_wire(&mut self, attrs: &std::collections::BTreeMap<String, DynamicValue>) -> Vec<u8> {
        let mut body = Vec::new();
        let mut resolver = |name: &str| self.fields.id_for(&mut self.store, &self.ns, name);
        for (name, value) in attrs {
            put_frame_named(&mut body, name, value, &mut resolver);
        }
        body
    }

    /// Attribute frames → name-keyed map (ids resolved via the field
    /// vocabulary; unknown ids dropped — the shared-engine degraded mode,
    /// same rule as `get_fields`).
    fn attrs_from(&mut self, wire: &[u8]) -> std::collections::BTreeMap<String, DynamicValue> {
        let frames = decode_named(wire, &mut |id| self.fields.name_for(&self.store, &self.ns, id));
        let mut out = std::collections::BTreeMap::new();
        for f in frames {
            if let Some(name) = self.fields.name_for(&self.store, &self.ns, f.id) {
                out.insert(name, f.value);
            }
        }
        out
    }

    /// Build the primary value: `[src ref][dst ref][predicate_vocab_id][obj
    /// payload]`. The dynamic declaration has no payload fields, so the
    /// payload is the bare obj header (`[version 1][hot_len 0]`) — the same
    /// bytes the static form's empty declaration writes. Open attributes are
    /// NOT part of it: they live in the dynamic segment (fixed face 0x01),
    /// one entry per edge (ADR-0017 §3).
    fn body_of(&mut self, edge: &DynEdge, predicate_vocab_id: u16) -> Vec<u8> {
        let src_wire = edge.src.encode();
        let dst_wire = edge.dst.encode();
        let mut body = Vec::with_capacity(src_wire.len() + dst_wire.len() + 2 + 3);
        body.extend_from_slice(&src_wire);
        body.extend_from_slice(&dst_wire);
        body.extend_from_slice(&predicate_vocab_id.to_be_bytes());
        body.push(1);
        body.extend_from_slice(&0u16.to_be_bytes());
        body
    }

    /// The dynamic segment's entry: `[ns][0x01][edge_id]` → nTLV frames.
    fn fields_key(&self, edge_id: u64) -> Vec<u8> {
        self.face_key(okm_core::index::DYNAMIC_SLOT, &[&edge_id.to_be_bytes()])
    }

    /// Write one edge: primary + predicate index + four traversal faces +
    /// the predicate vocabulary, one engine batch. Attributes ride the
    /// primary's value (nTLV frames after `[src][dst][predicate_vocab_id]`).
    pub fn link(&mut self, edge: &DynEdge, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        if self.store.get(&pk).is_some() {
            return Err(format!("edge_id {edge_id} already live"));
        }
        let mut batch = MemBatch::default();
        self.write_into(&mut batch, edge, edge_id)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph link commit failed: {e}"))
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path.
    pub fn link_into(
        &mut self,
        batch: &mut MemBatch,
        edge: &DynEdge,
        edge_id: u64,
    ) -> Result<(), String> {
        self.write_into(batch, edge, edge_id)
    }

    fn write_into(
        &mut self,
        batch: &mut MemBatch,
        edge: &DynEdge,
        edge_id: u64,
    ) -> Result<(), String> {
        self.check_members(&edge.src, &edge.dst)?;
        let predicate_vocab_id = self.predicate_vocab_id(&edge.predicate);
        let body = self.body_of(edge, predicate_vocab_id);
        let attr_wire = self.attrs_wire(&edge.attrs);
        let src_wire = edge.src.encode();
        let dst_wire = edge.dst.encode();
        let predicate_be = predicate_vocab_id.to_be_bytes();
        let id_be = edge_id.to_be_bytes();

        batch.put(self.primary_key(edge_id), body);
        // Open attributes ride the dynamic segment, one entry per edge — a
        // zero-frame list is never written (absent = no attributes).
        if !attr_wire.is_empty() {
            batch.put(self.fields_key(edge_id), attr_wire);
        }
        batch.put(
            self.face_key(EDGE_PREDICATE_INDEX_SLOT, &[&predicate_be, &id_be]),
            Vec::new(),
        );
        batch.put(self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]), Vec::new());
        batch.put(self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]), Vec::new());
        batch.put(
            self.face_key(EDGE_PREDICATE_OUT_SLOT, &[&predicate_be, &src_wire, &id_be]),
            Vec::new(),
        );
        batch.put(
            self.face_key(EDGE_PREDICATE_IN_SLOT, &[&predicate_be, &dst_wire, &id_be]),
            Vec::new(),
        );
        Ok(())
    }

    /// Remove one edge: the faces encode the edge's own values, so deletion
    /// re-derives them from the stored body (fetched first). Exact by
    /// construction — the dynamic form declares no attribute faces to sweep.
    pub fn unlink(&mut self, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        let body = self
            .store
            .get(&pk)
            .ok_or_else(|| format!("unlink: edge_id {edge_id} not found"))?;
        let (src_wire, dst_wire, predicate_vocab_id) = self.parse_body_head(&body)?;
        let id_be = edge_id.to_be_bytes();
        let predicate_be = predicate_vocab_id.to_be_bytes();

        let mut batch = MemBatch::default();
        batch.del(&pk);
        batch.del(&self.fields_key(edge_id));
        batch.del(&self.face_key(EDGE_PREDICATE_INDEX_SLOT, &[&predicate_be, &id_be]));
        batch.del(&self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]));
        batch.del(&self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]));
        batch.del(&self.face_key(EDGE_PREDICATE_OUT_SLOT, &[&predicate_be, &src_wire, &id_be]));
        batch.del(&self.face_key(EDGE_PREDICATE_IN_SLOT, &[&predicate_be, &dst_wire, &id_be]));
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph unlink commit failed: {e}"))
    }

    /// Walk `[src ref][dst ref][predicate_vocab_id]` from the body front; returns
    /// the wire forms of both refs (for face-key reconstruction) and the
    /// predicate id. Attribute frames start right after.
    fn parse_body_head(&mut self, body: &[u8]) -> Result<(Vec<u8>, Vec<u8>, u16), String> {
        let (src, n1) = NodeRef::decode(body).ok_or("edge body: truncated src ref")?;
        let (dst, n2) = NodeRef::decode(&body[n1..]).ok_or("edge body: truncated dst ref")?;
        let after_dst = n1 + n2;
        if body.len() < after_dst + 2 {
            return Err("edge body: truncated predicate_vocab_id".into());
        }
        Ok((
            src.encode(),
            dst.encode(),
            u16::from_be_bytes([body[after_dst], body[after_dst + 1]]),
        ))
    }

    /// Fetch one edge, decoded: endpoints, predicate resolved to name,
    /// attributes as a name-keyed map.
    pub fn get_edge(&mut self, edge_id: u64) -> Option<DynEdge> {
        let body = self.store.get(&self.primary_key(edge_id))?;
        let (src, n1) = NodeRef::decode(&body)?;
        let (dst, n2) = NodeRef::decode(&body[n1..])?;
        self.check_members(&src, &dst).ok()?;
        let after_dst = n1 + n2;
        let predicate_vocab_id = u16::from_be_bytes([body[after_dst], body[after_dst + 1]]);
        let predicate = self
            .predicates
            .name_for(&self.store, &self.ns, predicate_vocab_id)?;
        // Open attributes live in the dynamic segment, not in the body.
        let attrs = match self.store.get(&self.fields_key(edge_id)) {
            Some(wire) => self.attrs_from(&wire),
            None => std::collections::BTreeMap::new(),
        };
        Some(DynEdge { src, dst, predicate, attrs })
    }

    /// Edge ids on one traversal face (full NodeRef leads — refs are
    /// self-describing, so the caller just encodes the node).
    fn traverse(&self, slot: u16, node: &NodeRef) -> Vec<u64> {
        let p = self.face_key(slot, &[&node.encode()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Out-edges of one node (0x07).
    pub fn out_edges(&self, src: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_OUT_SLOT, src)
    }

    /// In-edges of one node (0x08).
    pub fn in_edges(&self, dst: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_IN_SLOT, dst)
    }

    /// Predicate-qualified traversal (0x09/0x0A): unknown predicates scan
    /// empty (no face).
    pub fn typed_edges(&mut self, slot: u16, predicate: &str, node: &NodeRef) -> Vec<u64> {
        let predicate_vocab_id = match self.predicates.id_get(&self.store, &self.ns, predicate) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(slot, &[&predicate_vocab_id.to_be_bytes(), &node.encode()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Predicate-qualified out-edges (0x09).
    pub fn typed_out(&mut self, predicate: &str, src: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_PREDICATE_OUT_SLOT, predicate, src)
    }

    /// Predicate-qualified in-edges (0x0A).
    pub fn typed_in(&mut self, predicate: &str, dst: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_PREDICATE_IN_SLOT, predicate, dst)
    }

    /// All edges of one predicate (0x06) — the `MATCH ()-[r:has]->()` entry.
    pub fn edges_of_predicate(&mut self, predicate: &str) -> Vec<u64> {
        let predicate_vocab_id = match self.predicates.id_get(&self.store, &self.ns, predicate) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(EDGE_PREDICATE_INDEX_SLOT, &[&predicate_vocab_id.to_be_bytes()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Every predicate this graph has, id-ordered: one 0x04 prefix scan.
    pub fn predicates(&mut self) -> Vec<(u16, String)> {
        let mut p = self.ns.clone();
        okm_core::write_slot(&mut p, okm_core::index::PREDICATE_VOCAB_ID_SLOT);
        let mut out: Vec<(u16, String)> = scan_suffix_kv(&self.store, &p)
            .into_iter()
            .filter(|(suffix, _)| suffix.len() == 2)
            .filter_map(|(suffix, name)| {
                String::from_utf8(name)
                    .ok()
                    .map(|n| (u16::from_be_bytes([suffix[0], suffix[1]]), n))
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Predicate id → name (diagnostics surface).
    pub fn predicate_vocab_name(&mut self, id: u16) -> Option<String> {
        self.predicates.name_for(&self.store, &self.ns, id)
    }
}

/// Face-suffix → edge ids: every traversal/predicate face ends in an 8-byte
/// BE edge id; shorter suffixes are foreign entries and are skipped.
fn ids_from_suffixes(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
    suffixes
        .iter()
        .filter(|s| s.len() == 8)
        .map(|s| u64::from_be_bytes(s.as_slice().try_into().unwrap()))
        .collect()
}