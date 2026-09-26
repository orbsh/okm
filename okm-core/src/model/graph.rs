//! Graph Edge (ADR-0017): the third relation carrier — for DYNAMIC
//! graphs (knowledge graphs). Open endpoints (any collection's document,
//! any key shape), bidirectional, parallel edges, attributes.
//!
//! Division line vs the other two carriers (ADR-0015 §3, amended):
//! `Refs` = one-to-many (fixed child type, field position, one
//! direction); `Junction` = many-to-many with compile-time typed
//! endpoints; **Graph Edge** = independent identity, attributes,
//! parallel edges, open endpoints.
//!
//! # Endpoint references and the registry
//!
//! A reference is `[ns 2B BE][pkey]` — the ns acts as the type marker.
//! The pkey boundary is NOT stored in the key: it resolves through the
//! **ns → KEY_LEN registry** (`EndpointRegistry`), the schema-fact
//! extension of the Ref discipline ("the reference is key bytes, ns
//! knowledge lives at the declaration point") from one bound endpoint
//! type to a registry of endpoint types. Compile-time form: the
//! `GraphEdgeEncode` derive generates the constant table. Runtime form:
//! the caller maintains the registry.
//!
//! # Slot layout (ADR-0016 segments, within the Edge ns)
//!
//! ```text
//! 0x0     primary       [edge_id u64 BE]        → value = edge body
//! 0x2/0x3 kind dictionary (name ↔ kind_id, DictCache reused)
//! 0x4     kind index    [kind_id u16 BE]        → edge_id set
//! 0x5     out-edges     [src NodeRef][edge_id u64 BE]
//! 0x6     in-edges      [dst NodeRef][edge_id u64 BE]
//! 0x7     kind+out      [kind_id][src NodeRef][edge_id]
//! 0x8     kind+in       [kind_id][dst NodeRef][edge_id]
//! ```
//!
//! Declared attribute fields (fixed-ontology form) add one face each in
//! the 0x1 segment — ordinary declared indexes, see the derive. The
//! node collection is a STANDARD document collection: its node-kind
//! face is its own declared index, its kinds live in its own 0x2/0x3
//! dictionary — zero layout difference from any document collection.
//!
//! Edge body value: `[src NodeRef][dst NodeRef][kind_id u16 BE]
//! [attrs payload]` — `attrs payload` is the edge's declared-attribute
//! encoding (empty for a fully dynamic edge; the dynamic segment slot-1
//! entry, built by `put_fields`-style callers, is not touched here).

use crate::engine::storage::{MemBatch, VirtualStorage};
use crate::model::index::{PRIMARY_SLOT, Slot};
use crate::model::obj_dict::DictCache;

/// Segment-0x4 kind-index slot (fixed-ontology form uses the first
/// counter; the face is an Edge-collection-internal fact, not a declared
/// document index — hence segment 0x4, not 0x1).
pub const EDGE_KIND_INDEX_SLOT: Slot = 0x4001;
/// Segment-0x5 out-edge traversal face.
pub const EDGE_OUT_SLOT: Slot = 0x5001;
/// Segment-0x6 in-edge traversal face.
pub const EDGE_IN_SLOT: Slot = 0x6001;
/// Segment-0x7 typed out-edge face (kind leads).
pub const EDGE_KIND_OUT_SLOT: Slot = 0x7001;
/// Segment-0x8 typed in-edge face (kind leads).
pub const EDGE_KIND_IN_SLOT: Slot = 0x8001;

/// Self-describing endpoint reference: `[ns 2B BE][len varint][pkey]`.
/// The ns is the type marker; the pkey width rides IN the ref as a
/// prefix-monotonic varint (the shared `put_len`/`take_len` codec) —
/// ADR-0017 update 2026-09-20: scheme b (self-describing ref) beats the
/// registry on every axis the original draft got wrong. len < 64 is ONE
/// byte, so the traversal faces pay ~1 byte per ref (pkey widths are
/// 4-16 bytes); the same node always encodes to the same bytes, so
/// prefix scans match exactly; and the boundary is wire data — no
/// registry to declare, maintain, or fail on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeRef {
    /// Big-endian `[ns 2B]` (the raw header bytes, same shape as
    /// `Document::NS_PREFIX`).
    pub ns: [u8; 2],
    /// Bare pkey bytes (no header, no length).
    pub pkey: Vec<u8>,
}

impl NodeRef {
    /// Assemble from a document's `NS_PREFIX` (or any raw `[ns 2B]`
    /// header) and its key encoding.
    pub fn new(ns_prefix: &[u8], pkey: &[u8]) -> Self {
        assert!(ns_prefix.len() == 2, "NodeRef ns header must be [ns 2B]");
        Self { ns: [ns_prefix[0], ns_prefix[1]], pkey: pkey.to_vec() }
    }

    /// The `[ns 2B]` header slice.
    pub fn ns_prefix(&self) -> &[u8] {
        &self.ns
    }

    /// u16 view of the ns.
    pub fn ns_id(&self) -> u16 {
        u16::from_be_bytes(self.ns)
    }

    /// Full wire form: `[ns 2B][len varint][pkey]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + 1 + self.pkey.len());
        buf.extend_from_slice(&self.ns);
        crate::put_len(&mut buf, self.pkey.len());
        buf.extend_from_slice(&self.pkey);
        buf
    }

    /// Decode a wire-form reference from the front of `wire`; returns
    /// the ref and the number of bytes consumed (the caller splices the
    /// next segment — edge_id on the traversal faces, the dst ref in
    /// the body).
    pub fn decode(wire: &[u8]) -> Option<(Self, usize)> {
        if wire.len() < 3 {
            return None;
        }
        let (len, len_n) = crate::take_len(&wire[2..])?;
        let total = 2 + len_n + len;
        if wire.len() < total {
            return None;
        }
        Some((
            Self { ns: [wire[0], wire[1]], pkey: wire[2 + len_n..total].to_vec() },
            total,
        ))
    }
}

/// One edge fact: endpoints by reference, kind by name, declared
/// attributes already encoded by the caller (the derive's payload
/// encoding for the fixed-ontology form; empty for dynamic edges).
#[derive(Clone, Debug)]
pub struct EdgeFact {
    pub src: NodeRef,
    pub dst: NodeRef,
    /// Kind name — resolved to `kind_id` through the kind dictionary
    /// (0x2/0x3) at write time, so every key segment stays fixed-width.
    pub kind: String,
    /// Declared-attribute payload bytes (the fixed-ontology form's field
    /// encoding; empty when the edge carries no declared attributes).
    pub attrs: Vec<u8>,
}

/// The edge body stored at the primary (slot 0x0):
/// `[src NodeRef][dst NodeRef][kind_id u16 BE][attrs]` — refs are
/// self-describing, so `attrs` starts at a computable offset.
fn edge_body(fact: &EdgeFact, kind_id: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(
        3 + fact.src.pkey.len() + 3 + fact.dst.pkey.len() + 2 + fact.attrs.len(),
    );
    v.extend_from_slice(&fact.src.encode());
    v.extend_from_slice(&fact.dst.encode());
    v.extend_from_slice(&kind_id.to_be_bytes());
    v.extend_from_slice(&fact.attrs);
    v
}

/// Decoded edge body. `kind_id` stays raw — name resolution needs the
/// dictionary (a `&mut store` + cache), which the read paths own.
#[derive(Clone, Debug)]
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind_id: u16,
    pub attrs: Vec<u8>,
}

/// Walk `[src ref][dst ref][kind_id][attrs]` from the front — refs are
/// self-describing (len varint), so no side table is consulted.
fn decode_body(bytes: &[u8]) -> EdgeBody {
    let (src, n1) = NodeRef::decode(bytes).expect("edge body: truncated src ref");
    let (dst, n2) = NodeRef::decode(&bytes[n1..]).expect("edge body: truncated dst ref");
    let after_dst = n1 + n2;
    EdgeBody {
        src,
        dst,
        kind_id: u16::from_be_bytes([bytes[after_dst], bytes[after_dst + 1]]),
        attrs: bytes[after_dst + 2..].to_vec(),
    }
}

/// Graph contract (fixed-ontology form; generated by the `EdgeEncode`
/// derive). `NS_PREFIX` = the edge collection's own ns (one ns per
/// graph, normal ns dictionary). Endpoints need NO declaration — refs
/// carry their own pkey width. Attribute accessors live on the concrete
/// edge type.
pub trait KvGraph: Sized {
    /// The graph's edge-collection ns (`[ns 2B BE]`, from `#[ok_edge(ns = N)]`).
    const NS_PREFIX: &'static [u8];

    /// Encode this edge's declared attributes (derive-generated; the
    /// typed path's payload segment inside the edge body).
    fn attrs(&self) -> Vec<u8>;

    /// One `(slot, value bytes)` pair per declared attribute field — the
    /// 0x1-segment faces `link`/`unlink` must maintain (empty for an
    /// edge declaration without attribute fields; the runtime form has
    /// none). The face key is `[ns][slot][value][edge_id]`, value bytes
    /// lead so the scan prefix can match.
    fn attr_faces(&self, edge_id: u64) -> Vec<(Slot, Vec<u8>)> {
        let _ = edge_id;
        Vec::new()
    }

    /// The 0x1 slots declared attribute faces occupy (declaration
    /// order). Derive-generated as a constant; drives unlink's stale-
    /// face sweep. Empty = no declared attribute fields.
    const ATTR_SLOTS: &'static [Slot] = &[];
}

/// Engine + graph type = the operation surface of one graph's edge set
/// (symmetric with [`crate::Junction`]). One ns hosts every entry kind,
/// dispatched by segment. The `DictCache` is per-handle state (the
/// single-writer discipline covers kind-name allocation).
pub struct Graph<S, E: KvGraph> {
    store: S,
    dict: DictCache,
    _pd: std::marker::PhantomData<E>,
}

impl<S: VirtualStorage, E: KvGraph> Graph<S, E> {
    pub fn new(store: S) -> Self {
        Self { store, dict: DictCache::default(), _pd: std::marker::PhantomData }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    fn primary_key(&self, edge_id: u64) -> Vec<u8> {
        let mut k = E::NS_PREFIX.to_vec();
        k.extend_from_slice(&PRIMARY_SLOT.to_be_bytes());
        k.extend_from_slice(&edge_id.to_be_bytes());
        k
    }

    fn face_key(&self, slot: Slot, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = E::NS_PREFIX.to_vec();
        k.extend_from_slice(&slot.to_be_bytes());
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// Resolve (or allocate) the kind id. Allocation rides the edge
    /// collection's own dictionary (slots 0x2/0x3) — the same
    /// first-seen-claims-next append-only discipline as every other
    /// vocabulary in OKM.
    fn kind_id(&mut self, kind: &str) -> u16 {
        self.dict.id_for(&mut self.store, E::NS_PREFIX, kind)
    }

    /// Resolve kind id → name for display/diagnostics.
    pub fn kind_name(&mut self, id: u16) -> Option<String> {
        self.dict.name_for(&self.store, E::NS_PREFIX, id)
    }

    /// Write one edge: primary + kind index + four traversal faces +
    /// declared-attribute faces (and the kind dictionary on first sight
    /// of a new name), all in one engine batch. Parallel edges are
    /// separate facts — each call carries a caller-chosen `edge_id`
    /// (the caller keeps identity allocation; `link` verifies
    /// non-collision with one point read). `edge` supplies the typed
    /// attribute faces (unit struct/`Default` for dynamic edges).
    pub fn link(
        &mut self,
        fact: &EdgeFact,
        edge: &E,
        edge_id: u64,
    ) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        if self.store.get(&pk).is_some() {
            return Err(format!("edge_id {edge_id} already live"));
        }
        let kind_id = self.kind_id(&fact.kind);
        let body = edge_body(fact, kind_id);
        let src_wire = fact.src.encode();
        let dst_wire = fact.dst.encode();
        let id_be = edge_id.to_be_bytes();

        let mut batch = MemBatch::default();
        batch.put(pk, body);
        // kind index (0x4): [kind_id][edge_id]
        batch.put(
            self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes(), &id_be]),
            Vec::new(),
        );
        // out (0x5): [src NodeRef][edge_id]
        batch.put(self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]), Vec::new());
        // in (0x6): [dst NodeRef][edge_id]
        batch.put(self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]), Vec::new());
        // kind+out (0x7): [kind_id][src NodeRef][edge_id]
        batch.put(
            self.face_key(EDGE_KIND_OUT_SLOT, &[&kind_id.to_be_bytes(), &src_wire, &id_be]),
            Vec::new(),
        );
        // kind+in (0x8): [kind_id][dst NodeRef][edge_id]
        batch.put(
            self.face_key(EDGE_KIND_IN_SLOT, &[&kind_id.to_be_bytes(), &dst_wire, &id_be]),
            Vec::new(),
        );
        // Declared-attribute faces (0x1 segment, one per field): the
        // typed edge enumerates them; each face is [value][edge_id].
        for (slot, value) in edge.attr_faces(edge_id) {
            batch.put(self.face_key(slot, &[&value, &id_be]), Vec::new());
        }
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("graph link commit failed: {e}"))
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path (documents and edges share one
    /// batch, one commit covers them all). Kind-name allocation cannot
    /// ride the caller's batch (the dictionary commits its own), so a
    /// first-sight kind name may outlive a failed outer commit — an
    /// extra append-only dictionary entry is harmless by the same rule
    /// the obj dictionary documents.
    pub fn link_into(
        &mut self,
        batch: &mut MemBatch,
        fact: &EdgeFact,
        edge: &E,
        edge_id: u64,
    ) -> Result<(), String> {
        let kind_id = self.kind_id(&fact.kind);
        let body = edge_body(fact, kind_id);
        let src_wire = fact.src.encode();
        let dst_wire = fact.dst.encode();
        let id_be = edge_id.to_be_bytes();
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
        for (slot, value) in edge.attr_faces(edge_id) {
            batch.put(self.face_key(slot, &[&value, &id_be]), Vec::new());
        }
        Ok(())
    }

    /// Remove one edge: the same six entries deleted (the faces encode
    /// the edge's own values, so deletion needs the fact — the body —
    /// not just the id).
    pub fn unlink(&mut self, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        let body = self
            .store
            .get(&pk)
            .ok_or_else(|| format!("unlink: edge_id {edge_id} not found"))?;
        let parsed = decode_body(&body);
        let src_wire = parsed.src.encode();
        let dst_wire = parsed.dst.encode();
        let id_be = edge_id.to_be_bytes();

        let mut batch = MemBatch::default();
        batch.del(&pk);
        batch.del(&self.face_key(EDGE_KIND_INDEX_SLOT, &[&parsed.kind_id.to_be_bytes(), &id_be]));
        batch.del(&self.face_key(EDGE_OUT_SLOT, &[&src_wire, &id_be]));
        batch.del(&self.face_key(EDGE_IN_SLOT, &[&dst_wire, &id_be]));
        batch.del(&self.face_key(
            EDGE_KIND_OUT_SLOT,
            &[&parsed.kind_id.to_be_bytes(), &src_wire, &id_be],
        ));
        batch.del(&self.face_key(
            EDGE_KIND_IN_SLOT,
            &[&parsed.kind_id.to_be_bytes(), &dst_wire, &id_be],
        ));
        // Stale attribute faces: the face keys encode the OLD values, so
        // they cannot be re-derived from the body without the typed
        // field widths. Instead each declared 0x1 slot is prefix-scanned
        // and every entry ending in this edge_id is deleted (per-slot
        // counts are tiny — one entry per edge).
        let attr_slots = E::ATTR_SLOTS;
        for slot in attr_slots {
            let p = self.face_key(*slot, &[]);
            for sfx in self.store.scan_suffix(&p) {
                if sfx.len() == 8 && sfx.as_slice() == &id_be {
                    let mut full = p.clone();
                    full.extend_from_slice(&sfx);
                    batch.del(&full);
                }
            }
        }
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("graph unlink commit failed: {e}"))
    }

    /// Decode an edge body by id (the fetch-back step of every face
    /// scan). `kind_name()` resolves the id when needed.
    pub fn get_edge(&self, edge_id: u64) -> Option<EdgeBody> {
        let body = self.store.get(&self.primary_key(edge_id))?;
        Some(decode_body(&body))
    }

    /// All edge ids on one traversal face, decoded back through the
    /// primary. `slot` selects out/in; the prefix must be the full
    /// NodeRef wire (a full ref leads every traversal entry).
    fn traverse(&self, slot: Slot, node: &NodeRef) -> Vec<u64> {
        let p = self.face_key(slot, &[&node.encode()]);
        self.store
            .scan_suffix(&p)
            .iter()
            .filter(|sfx| sfx.len() == 8)
            .map(|sfx| u64::from_be_bytes(sfx.as_slice().try_into().unwrap()))
            .collect()
    }

    /// Out-edges of one node (untyped, 0x5).
    pub fn out_edges(&self, src: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_OUT_SLOT, src)
    }

    /// In-edges of one node (untyped, 0x6).
    pub fn in_edges(&self, dst: &NodeRef) -> Vec<u64> {
        self.traverse(EDGE_IN_SLOT, dst)
    }

    /// Typed traversal (0x7/0x8): all edge ids of one kind from one
    /// node. The kind id must come from a prior write (unknown kinds
    /// scan nothing — there is no face to match).
    pub fn typed_edges(&mut self, slot: Slot, kind: &str, node: &NodeRef) -> Vec<u64> {
        let kind_id = match self.dict.by_name_get(kind) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(slot, &[&kind_id.to_be_bytes(), &node.encode()]);
        self.store
            .scan_suffix(&p)
            .iter()
            .filter(|sfx| sfx.len() == 8)
            .map(|sfx| u64::from_be_bytes(sfx.as_slice().try_into().unwrap()))
            .collect()
    }

    /// Typed out-edges (0x7).
    pub fn typed_out(&mut self, kind: &str, src: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_KIND_OUT_SLOT, kind, src)
    }

    /// Typed in-edges (0x8).
    pub fn typed_in(&mut self, kind: &str, dst: &NodeRef) -> Vec<u64> {
        self.typed_edges(EDGE_KIND_IN_SLOT, kind, dst)
    }

    /// All edges of one kind, regardless of endpoints (0x4) — the
    /// `MATCH ()-[r:has]->()` entry point.
    pub fn edges_of_kind(&mut self, kind: &str) -> Vec<u64> {
        let kind_id = match self.dict.by_name_get(kind) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let p = self.face_key(EDGE_KIND_INDEX_SLOT, &[&kind_id.to_be_bytes()]);
        self.store
            .scan_suffix(&p)
            .iter()
            .filter(|sfx| sfx.len() == 8)
            .map(|sfx| u64::from_be_bytes(sfx.as_slice().try_into().unwrap()))
            .collect()
    }

    /// Filter by declared attribute (0x1 face, fixed-ontology form):
    /// scan the face with the caller-encoded probe and return edge ids.
    /// The derive emits the per-field access methods on the concrete
    /// edge type; this is the shared byte-level walk.
    pub fn by_attr_face(&self, slot: Slot, encoded: &[u8]) -> Vec<u64> {
        let p = self.face_key(slot, &[encoded]);
        self.store
            .scan_suffix(&p)
            .iter()
            .filter(|sfx| sfx.len() == 8)
            .map(|sfx| u64::from_be_bytes(sfx.as_slice().try_into().unwrap()))
            .collect()
    }

    /// The NEIGHBOR nodes one hop out (untyped, 0x5): edge ids resolved
    /// to their far endpoints. Edge ids remain the identity currency —
    /// this is a convenience composition over `out_edges` + `get_edge`
    /// (zero new wire), for the traversal shape where only nodes
    /// matter. Parallel edges collapse into repeated refs; dedupe if
    /// the caller wants a set (`NodeRef` is Hash + Ord for exactly
    /// this), or keep the repeats as multiplicity.
    pub fn out_nodes(&self, src: &NodeRef) -> Vec<NodeRef> {
        self.out_edges(src)
            .iter()
            .filter_map(|id| self.get_edge(*id))
            .map(|b| b.dst)
            .collect()
    }

    /// Neighbor nodes one hop in (untyped, 0x6) — the in-direction
    /// mirror of [`out_nodes`](Self::out_nodes).
    pub fn in_nodes(&self, dst: &NodeRef) -> Vec<NodeRef> {
        self.in_edges(dst)
            .iter()
            .filter_map(|id| self.get_edge(*id))
            .map(|b| b.src)
            .collect()
    }

    /// Kind-qualified neighbors (0x7/0x8): `typed_out`/`typed_in`
    /// resolved to far endpoints. Unknown kinds scan empty, like their
    /// edge-id counterparts.
    pub fn typed_out_nodes(&mut self, kind: &str, src: &NodeRef) -> Vec<NodeRef> {
        let ids = self.typed_out(kind, src);
        self.endpoints(ids, |b| &b.dst)
    }

    /// Kind-qualified in-neighbors (0x8) — the in-direction mirror of
    /// [`typed_out_nodes`](Self::typed_out_nodes).
    pub fn typed_in_nodes(&mut self, kind: &str, dst: &NodeRef) -> Vec<NodeRef> {
        let ids = self.typed_in(kind, dst);
        self.endpoints(ids, |b| &b.src)
    }

    /// Shared resolve step of the *_nodes helpers: ids → bodies → far
    /// endpoint. A dead id (concurrent unlink between scan and fetch)
    /// drops out silently — neighbors are a scan snapshot, not a
    /// transaction.
    fn endpoints(&self, ids: Vec<u64>, side: impl Fn(&EdgeBody) -> &NodeRef) -> Vec<NodeRef> {
        ids.iter()
            .filter_map(|id| self.get_edge(*id))
            .map(|b| side(&b).clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_engine::TestStore;

    struct Follows;
    impl KvGraph for Follows {
        const NS_PREFIX: &'static [u8] = &[0x00, 0x64];
        fn attrs(&self) -> Vec<u8> {
            Vec::new()
        }
    }

    fn node(ns: u16, key: u64) -> NodeRef {
        NodeRef::new(&ns.to_be_bytes(), &key.to_be_bytes())
    }

    fn person(id: u64) -> NodeRef {
        node(0x10, id)
    }

    fn org(id: u32) -> NodeRef {
        node(0x11, id as u64)
    }

    fn fact(src: NodeRef, dst: NodeRef) -> EdgeFact {
        EdgeFact { src, dst, kind: "has".into(), attrs: Vec::new() }
    }

    #[test]
    fn noderef_wire_is_self_describing() {
        // [ns 2B][len 1B][pkey 8B]; len < 64 rides one byte.
        let wire = person(7).encode();
        assert_eq!(wire.len(), 2 + 1 + 8);
        let (back, n) = NodeRef::decode(&wire).unwrap();
        assert_eq!(n, wire.len());
        assert_eq!(back, person(7));
        // Trailing garbage belongs to the next segment (edge_id) — the
        // decode consumes exactly its own bytes.
        let mut w2 = wire.clone();
        w2.extend_from_slice(&0xEEu64.to_be_bytes());
        let (back2, n2) = NodeRef::decode(&w2).unwrap();
        assert_eq!(n2, wire.len());
        assert_eq!(back2.pkey, 7u64.to_be_bytes().to_vec());
        // Short wire rejected (None, never a guess).
        assert!(NodeRef::decode(&[0x00, 0x10]).is_none());
        // A 2-byte pkey ref encodes to 5 bytes total.
        let small = NodeRef::new(&0x11u16.to_be_bytes(), &[1, 2]);
        assert_eq!(small.encode().len(), 2 + 1 + 2);
    }

    #[test]
    fn link_writes_six_faces_and_kind_dict() {
        let store = TestStore::slatedb_mem();
        let mut g: Graph<_, Follows> = Graph::new(store.clone());
        let e = EdgeFact { src: person(1), dst: org(9), kind: "has".into(), attrs: Vec::new() };
        g.link(&e, &Follows, 100).unwrap();

        // Primary + the five faces + the two dictionary rows all live in
        // one ns. Kind id 0 for the first-seen name.
        let pk = {
            let mut k = vec![0x00, 0x64];
            k.extend_from_slice(&PRIMARY_SLOT.to_be_bytes());
            k.extend_from_slice(&100u64.to_be_bytes());
            k
        };
        let body = store.get(&pk).unwrap();
        // Refs are self-describing: [ns 2B][len 1B][pkey 8B] each.
        assert_eq!(&body[..2], &0x10u16.to_be_bytes());
        assert_eq!(body[2], 8, "pkey len varint (one byte for 8)");
        assert_eq!(&body[3..11], &1u64.to_be_bytes());
        assert_eq!(&body[11..13], &0x11u16.to_be_bytes());
        assert_eq!(body[13], 8);
        assert_eq!(&body[14..22], &9u64.to_be_bytes());
        assert_eq!(&body[22..24], &0u16.to_be_bytes(), "first-seen kind gets id 0");

        // Traversal faces answer.
        assert_eq!(g.out_edges(&person(1)), vec![100]);
        assert_eq!(g.in_edges(&org(9)), vec![100]);
        assert_eq!(g.typed_out("has", &person(1)), vec![100]);
        assert_eq!(g.typed_in("has", &org(9)), vec![100]);
        assert_eq!(g.edges_of_kind("has"), vec![100]);
        // Unknown kind scans nothing (no face, no panic).
        assert!(g.typed_out("owns", &person(1)).is_empty());
        assert!(g.edges_of_kind("owns").is_empty());
    }

    #[test]
    fn parallel_edges_are_separate_facts() {
        let store = TestStore::slatedb_mem();
        let mut g: Graph<_, Follows> = Graph::new(store.clone());
        g.link(&fact(person(1), org(9)), &Follows, 1).unwrap();
        g.link(&fact(person(1), org(9)), &Follows, 2).unwrap();
        assert_eq!(g.out_edges(&person(1)), vec![1, 2]);
        assert_eq!(g.in_edges(&org(9)), vec![1, 2]);
        assert_eq!(g.edges_of_kind("has"), vec![1, 2]);
    }

    #[test]
    fn unlink_removes_all_faces() {
        let store = TestStore::slatedb_mem();
        let mut g: Graph<_, Follows> = Graph::new(store.clone());
        g.link(&fact(person(1), org(9)), &Follows, 1).unwrap();
        g.link(&fact(person(2), org(9)), &Follows, 2).unwrap();
        g.unlink(1).unwrap();
        assert_eq!(g.out_edges(&person(1)), Vec::<u64>::new());
        assert_eq!(g.edges_of_kind("has"), vec![2]);
        assert!(g.get_edge(1).is_none());
        assert!(g.unlink(1).is_err(), "double unlink errors, not silently re-deletes");
    }

    #[test]
    fn link_rejects_live_edge_id() {
        let store = TestStore::slatedb_mem();
        let mut g: Graph<_, Follows> = Graph::new(store.clone());
        g.link(&fact(person(1), org(9)), &Follows, 1).unwrap();
        assert!(g.link(&fact(person(2), org(8)), &Follows, 1).is_err());
    }

    #[test]
    fn shared_ns_of_both_endpoints_still_scans() {
        // Same-ns endpoints (Person↔Person): both directions live in the
        // SAME edge ns (the edge collection's ns is separate from the
        // node's — the direction ambiguity that needed the Junction's
        // direction bit does not exist here, the two faces are distinct
        // slots).
        let store = TestStore::slatedb_mem();
        let mut g: Graph<_, Follows> = Graph::new(store.clone());
        g.link(&fact(person(1), person(2)), &Follows, 5).unwrap();
        assert_eq!(g.out_edges(&person(1)), vec![5]);
        assert_eq!(g.in_edges(&person(2)), vec![5]);
        assert_eq!(g.out_edges(&person(2)), Vec::<u64>::new());
    }
}
