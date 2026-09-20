//! Fully dynamic graph edges (ADR-0017 §4, second form): a `KgNode` /
//! `KgEdge` pair with EMPTY declarations — identity only, attributes as
//! runtime data.
//!
//! Shares the fixed-ontology wire exactly: same eight entry kinds on
//! ADR-0016 segments, same self-describing `[ns 2B][len][pkey]` refs,
//! same kind dictionary (0x2/0x3). The differences are declaration-side
//! only:
//!
//! - endpoints need NO declaration at all — refs carry their own pkey
//!   width (varint len), so there is no registry to declare or maintain;
//! - edge attributes are nTLV frames (id + dictionary, self-describing
//!   value types) instead of compile-time fixed-width encodings —
//!   attribute names are runtime data, so no 0x1 declared-attribute
//!   faces exist structurally (kind filtering is fully covered by the
//!   0x4/0x7/0x8 faces);
//! - `edge_id` stays caller-chosen, verified non-colliding at link.
//!
//! Capability ceiling inherited from the dynamic layer: no
//! reduce/subscribe/function indexes.

use okm_core::index::PRIMARY_SLOT;
use okm_core::model::graph::{
    EDGE_IN_SLOT, EDGE_KIND_INDEX_SLOT, EDGE_KIND_IN_SLOT, EDGE_KIND_OUT_SLOT, EDGE_OUT_SLOT, NodeRef,
};
use okm_core::obj_dict::DictCache;
use okm_core::obj_dynamic::{decode_named, put_frame_named, DynamicValue};
use okm_core::storage::{KvBatch, VirtualStorage};

/// One dynamic edge fact: endpoints by reference, kind by name,
/// attributes as a name-keyed map (nTLV frames on the wire, ids
/// allocated through the edge collection's own dictionary).
#[derive(Clone, Debug)]
pub struct DynEdge {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind: String,
    pub attrs: std::collections::BTreeMap<String, DynamicValue>,
}

/// Engine = the dynamic form's operation surface. Symmetric with
/// `okm_core::Graph` (fixed-ontology): same faces, same wire, runtime
/// declarations. No registry — refs are self-describing.
pub struct Graph<S> {
    store: S,
    dict: DictCache,
    ns: Vec<u8>,
}

impl<S: VirtualStorage> Graph<S> {
    /// Assemble a dynamic graph's edge collection. `ns` MUST be unique
    /// within the store instance (same rule as `DynamicCollection`).
    pub fn new(store: S, ns: u16) -> Self {
        Self { store, dict: DictCache::default(), ns: ns.to_be_bytes().to_vec() }
    }

    fn primary_key(&self, edge_id: u64) -> Vec<u8> {
        let mut k = self.ns.clone();
        k.extend_from_slice(&PRIMARY_SLOT.to_be_bytes());
        k.extend_from_slice(&edge_id.to_be_bytes());
        k
    }

    fn face_key(&self, slot: u16, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = self.ns.clone();
        k.extend_from_slice(&slot.to_be_bytes());
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// Resolve (or allocate) the kind id through the edge collection's
    /// own dictionary (first-seen-claims-next, append-only).
    fn kind_id(&mut self, kind: &str) -> u16 {
        self.dict.id_for(&mut self.store, &self.ns, kind)
    }

    /// Attribute map → nTLV frames (ids via the dictionary; values are
    /// self-describing typed frames — the dynamic segment's own wire).
    fn attrs_wire(&mut self, attrs: &std::collections::BTreeMap<String, DynamicValue>) -> Vec<u8> {
        let mut body = Vec::new();
        let mut resolver = |name: &str| self.dict.id_for(&mut self.store, &self.ns, name);
        for (name, value) in attrs {
            put_frame_named(&mut body, name, value, &mut resolver);
        }
        body
    }

    /// Attribute frames → name-keyed map (ids resolved via dictionary;
    /// unknown ids dropped — the shared-engine degraded mode, same rule
    /// as `get_fields`).
    fn attrs_from(&mut self, wire: &[u8]) -> std::collections::BTreeMap<String, DynamicValue> {
        let mut out = std::collections::BTreeMap::new();
        let frames = decode_named(wire, &mut |id| {
            self.dict.name_for(&self.store, &self.ns, id)
        });
        for f in frames {
            if let Some(name) = self.dict.name_for(&self.store, &self.ns, f.id) {
                out.insert(name, f.value);
            }
        }
        out
    }

    /// Write one edge: primary + kind index + four traversal faces +
    /// the kind dictionary, one engine batch. Attributes ride the
    /// primary's value (nTLV frames after `[src][dst][kind_id]`).
    pub fn link(&mut self, edge: &DynEdge, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        if self.store.get(&pk).is_some() {
            return Err(format!("edge_id {edge_id} already live"));
        }
        let kind_id = self.kind_id(&edge.kind);
        let attr_wire = self.attrs_wire(&edge.attrs);
        let src_wire = edge.src.encode();
        let dst_wire = edge.dst.encode();
        let id_be = edge_id.to_be_bytes();

        let mut body = Vec::with_capacity(src_wire.len() + dst_wire.len() + 2 + attr_wire.len());
        body.extend_from_slice(&src_wire);
        body.extend_from_slice(&dst_wire);
        body.extend_from_slice(&kind_id.to_be_bytes());
        body.extend_from_slice(&attr_wire);

        let mut batch = self.store.batch();
        batch.put(pk, body);
        batch.put(
            self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes(), &id_be]),
            Vec::new(),
        );
        batch.put(self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]), Vec::new());
        batch.put(self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]), Vec::new());
        batch.put(
            self.face_key(EDGE_KIND_OUT_SLOT, &[&kind_id.to_be_bytes(), &src_wire, &id_be]),
            Vec::new(),
        );
        batch.put(
            self.face_key(EDGE_KIND_IN_SLOT, &[&kind_id.to_be_bytes(), &dst_wire, &id_be]),
            Vec::new(),
        );
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph link commit failed: {e}"))
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path.
    pub fn link_into(
        &mut self,
        batch: &mut impl KvBatch,
        edge: &DynEdge,
        edge_id: u64,
    ) -> Result<(), String> {
        let kind_id = self.kind_id(&edge.kind);
        let attr_wire = self.attrs_wire(&edge.attrs);
        let src_wire = edge.src.encode();
        let dst_wire = edge.dst.encode();
        let id_be = edge_id.to_be_bytes();

        let mut body = Vec::with_capacity(src_wire.len() + dst_wire.len() + 2 + attr_wire.len());
        body.extend_from_slice(&src_wire);
        body.extend_from_slice(&dst_wire);
        body.extend_from_slice(&kind_id.to_be_bytes());
        body.extend_from_slice(&attr_wire);

        batch.put(self.primary_key(edge_id), body);
        batch.put(
            self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes(), &id_be]),
            Vec::new(),
        );
        batch.put(self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]), Vec::new());
        batch.put(self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]), Vec::new());
        batch.put(
            self.face_key(EDGE_KIND_OUT_SLOT, &[&kind_id.to_be_bytes(), &src_wire, &id_be]),
            Vec::new(),
        );
        batch.put(
            self.face_key(EDGE_KIND_IN_SLOT, &[&kind_id.to_be_bytes(), &dst_wire, &id_be]),
            Vec::new(),
        );
        Ok(())
    }

    /// Remove one edge: the faces encode the edge's own values, so
    /// deletion re-derives them from the stored body (fetched first).
    pub fn unlink(&mut self, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        let body = self
            .store
            .get(&pk)
            .ok_or_else(|| format!("unlink: edge_id {edge_id} not found"))?;
        let (src_wire, dst_wire, kind_id) = self.parse_body_head(&body)?;
        let id_be = edge_id.to_be_bytes();

        let mut batch = self.store.batch();
        batch.del(&pk);
        batch.del(&self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes(), &id_be]));
        batch.del(&self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]));
        batch.del(&self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]));
        batch.del(&self.face_key(
            EDGE_KIND_OUT_SLOT,
            &[&kind_id.to_be_bytes(), &src_wire, &id_be],
        ));
        batch.del(&self.face_key(
            EDGE_KIND_IN_SLOT,
            &[&kind_id.to_be_bytes(), &dst_wire, &id_be],
        ));
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph unlink commit failed: {e}"))
    }

    /// Walk `[src ref][dst ref][kind_id]` from the body front; returns
    /// the wire forms of both refs (for face-key reconstruction) and
    /// the kind id. Attr frames start right after.
    fn parse_body_head(&mut self, body: &[u8]) -> Result<(Vec<u8>, Vec<u8>, u16), String> {
        let (src, n1) = okm_core::model::graph::NodeRef::decode(body)
            .ok_or("edge body: truncated src ref")?;
        let (dst, n2) = okm_core::model::graph::NodeRef::decode(&body[n1..])
            .ok_or("edge body: truncated dst ref")?;
        let after_dst = n1 + n2;
        if body.len() < after_dst + 2 {
            return Err("edge body: truncated kind_id".into());
        }
        Ok((
            src.encode(),
            dst.encode(),
            u16::from_be_bytes([body[after_dst], body[after_dst + 1]]),
        ))
    }

    /// Fetch one edge, decoded: endpoints, kind resolved to name,
    /// attributes as a name-keyed map.
    pub fn get_edge(&mut self, edge_id: u64) -> Option<DynEdge> {
        let body = self.store.get(&self.primary_key(edge_id))?;
        let (src, n1) = okm_core::model::graph::NodeRef::decode(&body)?;
        let (dst, n2) = okm_core::model::graph::NodeRef::decode(&body[n1..])?;
        let after_dst = n1 + n2;
        let kind_id = u16::from_be_bytes([body[after_dst], body[after_dst + 1]]);
        let kind = self.dict.name_for(&self.store, &self.ns, kind_id)?;
        let attrs = self.attrs_from(&body[after_dst + 2..]);
        Some(DynEdge { src, dst, kind, attrs })
    }

    /// Edge ids on one traversal face (full NodeRef leads — refs are
    /// self-describing, so the caller just encodes the node).
    fn traverse(&self, slot: u16, node: &NodeRef) -> Vec<u64> {
        let p = self.face_key(slot, &[&node.encode()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Out-edges of one node (0x5).
    pub fn out_edges(&self, src: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_OUT_SLOT, src)
    }

    /// In-edges of one node (0x6).
    pub fn in_edges(&self, dst: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_IN_SLOT, dst)
    }

    /// Typed traversal (0x7/0x8): unknown kinds scan empty (no face).
    pub fn typed_edges(&mut self, slot: u16, kind: &str, node: &NodeRef) -> Vec<u64> {
        let kind_id = match self.dict.by_name_get(kind) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(slot, &[&kind_id.to_be_bytes(), &node.encode()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Typed out-edges (0x7).
    pub fn typed_out(&mut self, kind: &str, src: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_KIND_OUT_SLOT, kind, src)
    }

    /// Typed in-edges (0x8).
    pub fn typed_in(&mut self, kind: &str, dst: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_KIND_IN_SLOT, kind, dst)
    }

    /// All edges of one kind (0x4) — the `MATCH ()-[r:has]->()` entry.
    pub fn edges_of_kind(&mut self, kind: &str) -> Vec<u64> {
        let kind_id = match self.dict.by_name_get(kind) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes()]);
        ids_from_suffixes(self.store.scan_suffix(&p))
    }

    /// Kind id → name (diagnostics surface).
    pub fn kind_name(&mut self, id: u16) -> Option<String> {
        self.dict.name_for(&self.store, &self.ns, id)
    }
}

/// Face-suffix → edge ids: every traversal/kind face ends in an 8-byte
/// BE edge id; shorter suffixes are foreign entries and are skipped.
fn ids_from_suffixes(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
    suffixes
        .iter()
        .filter(|s| s.len() == 8)
        .map(|s| u64::from_be_bytes(s.as_slice().try_into().unwrap()))
        .collect()
}
