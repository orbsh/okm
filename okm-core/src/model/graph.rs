//! Graph Edge (ADR-0017): the third relation carrier — for DYNAMIC
//! graphs (knowledge graphs). Open endpoints (any collection's document,
//! any key shape), bidirectional, parallel edges, attributes.
//!
//! Division line vs the other two carriers (ADR-0015 §3, amended):
//! `Refs` = one-to-many (fixed child type, field position, one
//! direction); `Junction` = many-to-many with compile-time typed
//! endpoints; **Graph Edge** = independent identity, attributes,
//! parallel edges, open endpoints, and a declaration that IS the graph.
//!
//! # Endpoint references and membership
//!
//! A reference is `[ns 2B BE][len varint][pkey]` — the ns acts as the type
//! marker and the pkey width rides inside the reference. The pkey boundary
//! is therefore wire data: no registry, no side table, mixed widths
//! coexist. What IS declared is the member SET (the `Node` enum of the
//! declaration), applied as a check at write and at decode — width is wire
//! data, membership is a declaration.
//!
//! # Slot layout (fixed faces within the edge ns, ADR-0030/ADR-0017 §3)
//!
//! ```text
//! 0x00    primary           [edge_id u64 BE]        → [src ref][dst ref][predicate_vocab_id][obj payload]
//! 0x01    dynamic segment   [edge_id u64 BE]        → the edge's open attributes (nTLV)
//! 0x02/0x03  field-name vocabulary (the payload's own names ↔ ids)
//! 0x04/0x05  PREDICATE VOCABULARY      (predicate name ↔ predicate_vocab_id)
//! 0x06    out-edges         [src NodeRef][dst NodeRef][edge_id u64 BE]
//! 0x07    in-edges          [dst NodeRef][src NodeRef][edge_id u64 BE]
//! 0x08    predicate+out     [predicate_vocab_id][src NodeRef][dst NodeRef][edge_id]
//! 0x09    predicate+in      [predicate_vocab_id][dst NodeRef][src NodeRef][edge_id]
//! ```
//!
//! **Every index carries BOTH endpoints.** A face entry states where an edge
//! came from and where it went, so the pair is addressable: `(src, dst)` on
//! 0x06, `(predicate, src, dst)` on 0x08. That is what makes "is this fact
//! already here" one seek instead of a scan over a hub's whole out-degree.
//!
//! **The suffix rule follows the prefix.** After ONE node the entry is
//! `other endpoint + edge_id`, so the id is the trailing eight bytes (the
//! `NodeRef` is self-delimiting — it cannot be confused with them). After a
//! PAIR — or a predicate and a pair — the prefix already consumed both
//! endpoints and the suffix IS the id, the fixed-width rule the old
//! single-node faces had.
//!
//! **There is no separate predicate index.** It used to be 0x06
//! (`[predicate][edge_id]`); scanning 0x08 by its `[predicate]` prefix yields
//! exactly the same edge set with longer keys, so the face was a denormalized
//! copy of information 0x08 already held — one fewer put, one fewer delete and
//! one fewer slot per edge.
//!
//! A vocabulary precedes the faces that read its ids. Declared attribute
//! faces are ordinary `#[ok_index]` declarations — counting family 0x1,
//! exactly the document machinery (`Document::index_entries`), because the
//! edge's payload IS an obj (ADR-0012). The node collection is a STANDARD
//! document collection: zero layout difference from any document
//! collection; a node collection carrying kind tokens declares its own
//! predicate vocabulary (the same 0x04/0x05 pair, per collection).
//!
//! Identity: the primary key is the bare `edge_id` (graph-global), so point
//! reads and `unlink` need only the id; the predicate is a VALUE segment in
//! the body and an index dimension on the faces.

use crate::engine::storage::{Batch, VirtualStorage, scan_suffix_kv};
use crate::model::index::{DYNAMIC_SLOT, Document, PRIMARY_SLOT, Slot, write_slot};
use crate::model::key::KeyEncode;
use crate::model::obj_dynamic::{DynamicValue, decode_named, put_frame_named};
use crate::model::obj_vocab::{PREDICATE_VOCAB, VocabCache};

/// Fixed face 0x06: out-edge traversal, pair-qualified
/// (`[src][dst][edge_id]`): prefix on `src` deliberately keeps the "all of a
/// node's out-edges" read the face has always served, and the pair in front of
/// the id adds the exact one.
pub const EDGE_OUT_SLOT: Slot = 0x06;
/// Fixed face 0x07: in-edge traversal, pair-qualified (`[dst][src][edge_id]`).
pub const EDGE_IN_SLOT: Slot = 0x07;
/// Fixed face 0x08: predicate-qualified out edges — `[predicate][src][dst][id]`.
/// Its bare `[predicate]` prefix is the predicate index the graph used to keep
/// in its own slot.
pub const EDGE_PREDICATE_OUT_SLOT: Slot = 0x08;
/// Fixed face 0x09: predicate-qualified in edges — `[predicate][dst][src][id]`.
pub const EDGE_PREDICATE_IN_SLOT: Slot = 0x09;

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
        Self {
            ns: [ns_prefix[0], ns_prefix[1]],
            pkey: pkey.to_vec(),
        }
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
            Self {
                ns: [wire[0], wire[1]],
                pkey: wire[2 + len_n..total].to_vec(),
            },
            total,
        ))
    }
}

/// The edge's identity as a key: `edge_id` u64 big-endian, 8 bytes — the
/// same bytes the pre-declaration layout used. It exists so the edge
/// payload can be an ordinary [`Document`] (its declared `#[ok_index]`
/// faces are keyed by it) without inventing a per-graph key struct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey(pub u64);

impl KeyEncode for EdgeKey {
    const KEY_LEN: usize = 8;
    const FIELD_WIDTHS: &'static [(&'static str, usize)] = &[("edge_id", 8)];
    /// The key's field list — the exported schema (`CollectionSchema::of`)
    /// walks it, so a graph declaration exports the same shape a document's
    /// key does.
    const FIELDS: &'static [crate::model::field::FieldDesc] = &[crate::model::field::FieldDesc {
        name: "edge_id",
        ty: crate::model::field::FieldType::U64,
        width: 8,
    }];

    fn encode(&self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Self {
        Self(u64::from_be_bytes(
            bytes[..8].try_into().expect("EdgeKey: 8 bytes"),
        ))
    }

    fn encode_prefix_named(&self, buf: &mut Vec<u8>, names: &[&str]) -> usize {
        assert!(
            names.len() <= 1,
            "EdgeKey has one field; a prefix cannot be longer"
        );
        if names.len() == 1 {
            assert_eq!(names[0], "edge_id", "EdgeKey's only field is `edge_id`");
            buf.extend_from_slice(&self.0.to_be_bytes());
            8
        } else {
            0
        }
    }

    fn prefix_width(names: &[&str]) -> usize {
        assert!(
            names.len() <= 1,
            "EdgeKey has one field; a prefix cannot be longer"
        );
        if names.len() == 1 {
            assert_eq!(names[0], "edge_id", "EdgeKey's only field is `edge_id`");
            8
        } else {
            0
        }
    }
}

/// The member set of a graph: one variant per participating COLLECTION
/// (each variant is one ns). A declaration, not storage — the set carries
/// no run-time information, so it never occupies a slot. Generated by
/// `#[derive(GraphEncode)]` from `#[ok_nodes(Table, …)]`, never written by
/// hand: the ns of a variant is read off the table's own `#[ok_ns]`.
pub trait NodeSet {
    /// Every member collection's ns, one per variant.
    const NS_VALUES: &'static [u16];

    /// Is `ns` a member of this graph?
    fn contains(ns: u16) -> bool {
        Self::NS_VALUES.contains(&ns)
    }
}

/// The graph declaration (ADR-0017 §4), generated by `#[derive(GraphEncode)]`:
/// `#[ok_ns(N)]` is the only ns source, the struct's fields are the edge's
/// payload (an ordinary document payload — both codec directions, its
/// `#[ok_index]` faces declared EXPLICITLY), and `#[ok_nodes(Table, …)]` lists
/// the member collections by their document type. `Graph<S, E>` is a handle
/// over this and declares nothing.
pub trait GraphEncode: Document<Key = EdgeKey> {
    /// The member set — generated from `#[ok_nodes]`: an enum over the listed
    /// node collections, its ns values taken from the tables themselves.
    type Nodes: NodeSet;
}

/// One edge's head: endpoints by reference, predicate by name. The payload
/// travels separately (the declaration's own type) and is encoded by the
/// graph at write time.
#[derive(Clone, Debug)]
pub struct EdgeHead {
    pub src: NodeRef,
    pub dst: NodeRef,
    /// The edge's type: resolved to `predicate_vocab_id` through the graph's kind
    /// vocabulary (0x04/0x05) at write time, so every key segment stays
    /// fixed-width. Never repeated in the payload.
    pub predicate: String,
}

/// The edge body stored at the primary (face 0x00):
/// `[src NodeRef][dst NodeRef][predicate_vocab_id u16 BE][obj payload]` — refs are
/// self-describing and the payload starts at a computable offset.
fn edge_body(head: &EdgeHead, predicate_vocab_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut v =
        Vec::with_capacity(3 + head.src.pkey.len() + 3 + head.dst.pkey.len() + 2 + payload.len());
    v.extend_from_slice(&head.src.encode());
    v.extend_from_slice(&head.dst.encode());
    v.extend_from_slice(&predicate_vocab_id.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Decoded edge body. `predicate_vocab_id` stays raw — name resolution needs the
/// vocabulary (a `&mut store` + cache), which the read paths own; `payload`
/// is the obj payload bytes, decoded by the declaration's type.
#[derive(Clone, Debug)]
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub predicate_vocab_id: u16,
    pub payload: Vec<u8>,
}

/// Walk `[src ref][dst ref][predicate_vocab_id][payload]` from the front — refs
/// are self-describing (len varint), so no side table is consulted.
fn decode_body(bytes: &[u8]) -> EdgeBody {
    let (src, n1) = NodeRef::decode(bytes).expect("edge body: truncated src ref");
    let (dst, n2) = NodeRef::decode(&bytes[n1..]).expect("edge body: truncated dst ref");
    let after_dst = n1 + n2;
    EdgeBody {
        src,
        dst,
        predicate_vocab_id: u16::from_be_bytes([bytes[after_dst], bytes[after_dst + 1]]),
        payload: bytes[after_dst + 2..].to_vec(),
    }
}

/// Engine + graph type = the operation surface of one graph's edge set
/// (symmetric with [`crate::Junction`]). One ns hosts every entry kind,
/// dispatched by segment. The `VocabCache` is per-handle state over the
/// graph's PREDICATE VOCABULARY (0x04/0x05) — allocation is single-writer, and a
/// read-only handle still answers typed scans (the cache loads on probe).
pub struct Graph<S, E: GraphEncode> {
    store: S,
    vocab: VocabCache,
    _pd: std::marker::PhantomData<E>,
}

impl<S: VirtualStorage, E: GraphEncode> Graph<S, E> {
    /// The handle takes the engine and nothing else: the ns comes from the
    /// declaration (`E::NS_PREFIX`), the member set from `E::Nodes`.
    pub fn new(store: S) -> Self {
        Self {
            store,
            vocab: VocabCache::new(PREDICATE_VOCAB),
            _pd: std::marker::PhantomData,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Mutable access to the underlying storage — the restore path's need:
    /// assembling a row's bytes interns vocabulary labels, which is a write,
    /// and the write happens before the row's own batch is committed.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    fn primary_key(&self, edge_id: u64) -> Vec<u8> {
        let mut k = E::NS_PREFIX.to_vec();
        write_slot(&mut k, PRIMARY_SLOT);
        k.extend_from_slice(&edge_id.to_be_bytes());
        k
    }

    fn face_key(&self, slot: Slot, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = E::NS_PREFIX.to_vec();
        write_slot(&mut k, slot);
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// Membership: the declared set is checked against the refs' ns at
    /// write and at decode (width stays wire data; membership does not).
    fn check_members(&self, src: &NodeRef, dst: &NodeRef) -> crate::error::Result<()> {
        for (side, node) in [("src", src), ("dst", dst)] {
            if !E::Nodes::contains(node.ns_id()) {
                return Err(crate::error::Error::EdgeNotMember {
                    side,
                    ns: node.ns_id(),
                    graph_ns: u16::from_be_bytes([E::NS_PREFIX[0], E::NS_PREFIX[1]]),
                });
            }
        }
        Ok(())
    }

    /// Resolve (or allocate) the predicate id through the graph's kind
    /// vocabulary (0x04/0x05) — first-seen-claims-next, append-only, the
    /// same discipline as every other vocabulary in OKM. Separate from the
    /// payload's field-name vocabulary (0x02/0x03): two allocators in one
    /// id space would hand the same id to two names.
    fn predicate_vocab_id(&mut self, predicate: &str) -> crate::error::Result<u16> {
        self.vocab.id_for(&mut self.store, E::NS_PREFIX, predicate)
    }

    /// Resolve predicate id → name for display/diagnostics.
    pub fn predicate_vocab_name(&mut self, id: u16) -> crate::error::Result<Option<String>> {
        self.vocab.name_for(&self.store, E::NS_PREFIX, id)
    }

    /// Every predicate this graph has, id-ordered: ONE prefix scan of 0x04.
    /// A listing, never an inference walked back from the index faces.
    pub fn predicates(&mut self) -> crate::error::Result<Vec<(u16, String)>> {
        let mut p = E::NS_PREFIX.to_vec();
        write_slot(&mut p, crate::model::index::PREDICATE_VOCAB_ID_SLOT);
        let mut out: Vec<(u16, String)> = scan_suffix_kv(&self.store, &p)?
            .into_iter()
            .filter(|(suffix, _)| suffix.len() == 2)
            .filter_map(|(suffix, name)| {
                String::from_utf8(name)
                    .ok()
                    .map(|n| (u16::from_be_bytes([suffix[0], suffix[1]]), n))
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    /// Write one edge: primary + predicate index + four traversal faces +
    /// every DECLARED `#[ok_index]` face (and the vocabulary pair on first
    /// sight of a new predicate), all in one engine batch. Parallel edges
    /// are separate edges — each call carries a caller-chosen `edge_id`
    /// (identity allocation stays with the caller; `link` verifies
    /// non-collision with one point read).
    pub fn link(&mut self, head: &EdgeHead, payload: &E, edge_id: u64) -> crate::error::Result<()> {
        let pk = self.primary_key(edge_id);
        if self.store.get(&pk)?.is_some() {
            return Err(crate::error::Error::EdgeAlreadyLive { edge_id });
        }
        let mut batch = Batch::default();
        self.write_into(&mut batch, head, payload, edge_id)?;
        self.store.commit_batch(batch)
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path (documents and edges share one batch,
    /// one commit covers them all). Vocabulary allocation cannot ride the
    /// caller's batch (it commits its own), so a first-sight predicate name
    /// may outlive a failed outer commit — an extra append-only entry is
    /// harmless by the same rule the obj vocabulary documents.
    pub fn link_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
        edge_id: u64,
    ) -> crate::error::Result<()> {
        self.write_into(batch, head, payload, edge_id)
    }

    /// The edge's DYNAMIC segment (face 0x01) — its open attributes, one entry
    /// per edge, keyed `[ns][0x01][edge_id]`.
    /// Every edge id in this graph, in key order — the scan an export needs.
    /// Slot 0 only: the ns also holds faces, and an unfiltered scan would read
    /// them as rows (the same trap a document export hit).
    pub fn edge_ids(&self) -> crate::error::Result<Vec<u64>> {
        let mut prefix = E::NS_PREFIX.to_vec();
        write_slot(&mut prefix, PRIMARY_SLOT);
        Ok(scan_suffix_kv(&self.store, &prefix)?
            .into_iter()
            .filter_map(|(suffix, _)| {
                <[u8; 8]>::try_from(suffix.as_slice())
                    .ok()
                    .map(u64::from_be_bytes)
            })
            .collect())
    }

    fn dynamic_key(&self, edge_id: u64) -> Vec<u8> {
        let mut key = E::NS_PREFIX.to_vec();
        write_slot(&mut key, DYNAMIC_SLOT);
        key.extend_from_slice(&edge_id.to_be_bytes());
        key
    }

    /// Replace the edge's open attributes, in the SAME batch as the row so one
    /// commit carries both. Names allocate in the edge ns's field-name
    /// vocabulary (0x02/0x03), which is separate from the predicate vocabulary
    /// the row's write touches.
    ///
    /// A rewrite replaces the segment outright: the previous assertion's
    /// attributes are deleted first, so nothing inherits a stale prop — this is
    /// `replace` semantics, not a per-name merge (an edge has no partial-update
    /// caller yet; add `merge_into` the day one appears).
    pub fn replace_into(
        &mut self,
        batch: &mut Batch,
        edge_id: u64,
        fields: &std::collections::BTreeMap<String, DynamicValue>,
    ) -> crate::error::Result<()> {
        let key = self.dynamic_key(edge_id);
        batch.del(&key);
        if fields.is_empty() {
            return Ok(());
        }
        let mut body = Vec::new();
        {
            let mut dict = VocabCache::new(crate::model::obj_vocab::FIELD_VOCAB);
            let store = &mut self.store;
            let mut resolver = |name: &str| dict.id_for(store, E::NS_PREFIX, name);
            for (name, value) in fields {
                put_frame_named(&mut body, name, value, &mut resolver)?;
            }
        }
        batch.put(key, body);
        Ok(())
    }

    /// The edge's open attributes, when it has any — the dynamic half of an edge
    /// (its typed half is the payload, which is not a map). A name whose id the
    /// vocabulary cannot resolve is dropped rather than invented, and an empty
    /// segment answers `None` (no attributes) — the same rule the document path
    /// follows.
    pub fn attrs(
        &self,
        edge_id: u64,
    ) -> crate::error::Result<Option<std::collections::BTreeMap<String, DynamicValue>>> {
        let raw = match self.store.get(&self.dynamic_key(edge_id))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let mut dict = VocabCache::new(crate::model::obj_vocab::FIELD_VOCAB);
        let mut failed: Option<crate::error::Error> = None;
        let frames = decode_named(&raw, &mut |id| {
            match dict.name_for(&self.store, E::NS_PREFIX, id) {
                Ok(name) => name,
                Err(e) => {
                    failed = Some(e);
                    None
                }
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if frames.is_empty() {
            return Ok(None);
        }
        let mut out = std::collections::BTreeMap::new();
        for frame in frames {
            if let Some(name) = dict.name_for(&self.store, E::NS_PREFIX, frame.id)? {
                out.insert(name, frame.value);
            }
        }
        Ok(Some(out))
    }

    fn write_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
        edge_id: u64,
    ) -> crate::error::Result<()> {
        self.check_members(&head.src, &head.dst)?;
        let predicate_vocab_id = self.predicate_vocab_id(&head.predicate)?;
        let predicate_be = predicate_vocab_id.to_be_bytes();
        let body = edge_body(head, predicate_vocab_id, &payload.encode_payload());
        let src_wire = head.src.encode();
        let dst_wire = head.dst.encode();
        let id_be = edge_id.to_be_bytes();

        // A rewrite replaces the edge outright, so the previous assertion's open
        // attributes must not survive it. The 0x01 entry is derived from the row
        // that is being overwritten — one delete, in the same batch.
        batch.del(&self.dynamic_key(edge_id));
        batch.put(self.primary_key(edge_id), body);
        // out (0x06): [src][dst][edge_id]
        batch.put(
            self.face_key(EDGE_OUT_SLOT, &[&src_wire, &dst_wire, &id_be]),
            Vec::new(),
        );
        // in (0x07): [dst][src][edge_id]
        batch.put(
            self.face_key(EDGE_IN_SLOT, &[&dst_wire, &src_wire, &id_be]),
            Vec::new(),
        );
        // predicate+out (0x08): [predicate_vocab_id][src][dst][edge_id]
        batch.put(
            self.face_key(
                EDGE_PREDICATE_OUT_SLOT,
                &[&predicate_be, &src_wire, &dst_wire, &id_be],
            ),
            Vec::new(),
        );
        // predicate+in (0x09): [predicate_vocab_id][dst][src][edge_id]
        batch.put(
            self.face_key(
                EDGE_PREDICATE_IN_SLOT,
                &[&predicate_be, &dst_wire, &src_wire, &id_be],
            ),
            Vec::new(),
        );
        // Declared faces: the payload's own #[ok_index] declarations, the
        // same entries a document collection writes for the same row.
        for (key, value) in E::index_entries(&EdgeKey(edge_id), payload, E::NS_PREFIX) {
            batch.put(key, value);
        }
        Ok(())
    }

    /// Remove one edge: decode the body, delete the exact entries the link
    /// wrote. Declared faces are re-derived by decoding the payload into
    /// the declaration's type — no prefix sweep, so an unrelated edge with
    /// a shared attribute value cannot be touched.
    pub fn unlink(&mut self, edge_id: u64) -> crate::error::Result<()> {
        let mut batch = Batch::default();
        self.unlink_into(&mut batch, edge_id)?;
        self.store.commit_batch(batch)
    }

    /// Encode the full delete set into an externally owned batch — the
    /// cross-collection atomic path, and the one that makes a REWRITE one
    /// commit instead of two (`unlink_into` + `link_into` into the same
    /// batch: the incumbent's primary is deleted and re-put in order, and
    /// every stale face goes with it).
    pub fn unlink_into(&mut self, batch: &mut Batch, edge_id: u64) -> crate::error::Result<()> {
        let pk = self.primary_key(edge_id);
        let bytes = self
            .store
            .get(&pk)?
            .ok_or(crate::error::Error::EdgeNotFound { edge_id })?;
        let parsed = decode_body(&bytes);
        self.check_members(&parsed.src, &parsed.dst)?;
        let src_wire = parsed.src.encode();
        let dst_wire = parsed.dst.encode();
        let predicate_be = parsed.predicate_vocab_id.to_be_bytes();
        let id_be = edge_id.to_be_bytes();
        let payload: E = E::decode_payload(&parsed.payload);

        batch.del(&pk);
        batch.del(&self.face_key(EDGE_OUT_SLOT, &[&src_wire, &dst_wire, &id_be]));
        batch.del(&self.face_key(EDGE_IN_SLOT, &[&dst_wire, &src_wire, &id_be]));
        batch.del(&self.face_key(
            EDGE_PREDICATE_OUT_SLOT,
            &[&predicate_be, &src_wire, &dst_wire, &id_be],
        ));
        batch.del(&self.face_key(
            EDGE_PREDICATE_IN_SLOT,
            &[&predicate_be, &dst_wire, &src_wire, &id_be],
        ));
        for (key, _) in E::index_entries(&EdgeKey(edge_id), &payload, E::NS_PREFIX) {
            batch.del(&key);
        }
        Ok(())
    }

    /// Decode an edge body by id (the fetch-back step of every face scan).
    /// Non-member refs (a corrupt or foreign row) answer `None` here rather
    /// than travelling as if they belonged to this graph.
    pub fn get_edge(&self, edge_id: u64) -> crate::error::Result<Option<EdgeBody>> {
        let body = match self.store.get(&self.primary_key(edge_id))? {
            Some(body) => body,
            None => return Ok(None),
        };
        let parsed = decode_body(&body);
        // A row whose endpoints are not members of this graph is not this
        // graph's edge: filtered out, exactly as before.
        if self.check_members(&parsed.src, &parsed.dst).is_err() {
            return Ok(None);
        }
        Ok(Some(parsed))
    }

    /// The edge's payload decoded into the declaration's own type.
    pub fn edge(&self, edge_id: u64) -> crate::error::Result<Option<E>> {
        Ok(self
            .get_edge(edge_id)?
            .map(|b| E::decode_payload(&b.payload)))
    }

    /// The ids at the end of single-node entries: `other endpoint + edge_id`,
    /// so the id is the trailing eight bytes. A suffix too short to hold an
    /// endpoint and an id is not one of ours and is skipped — the same
    /// fail-closed direction the fixed-width decoder used to give.
    fn ids_after_endpoint(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
        suffixes
            .iter()
            .filter(|sfx| sfx.len() > 8)
            .filter_map(|sfx| sfx[sfx.len() - 8..].try_into().ok())
            .map(u64::from_be_bytes)
            .collect()
    }

    /// The ids at the end of PAIR entries: the prefix consumed both endpoints,
    /// so the suffix IS the id (fixed width, exactly as before).
    fn ids_after_pair(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
        suffixes
            .iter()
            .filter_map(|sfx| <[u8; 8]>::try_from(sfx.as_slice()).ok())
            .map(u64::from_be_bytes)
            .collect()
    }

    /// All edge ids on one traversal face, decoded back through the
    /// primary. `slot` selects out/in; the prefix must be the full
    /// NodeRef wire (a full ref leads every traversal entry).
    fn traverse(&self, slot: Slot, node: &NodeRef) -> crate::error::Result<Vec<u64>> {
        let p = self.face_key(slot, &[&node.encode()]);
        Ok(Self::ids_after_endpoint(self.store.scan_suffix(&p)?))
    }

    /// One traversal face, PAIR-qualified: the prefix names both endpoints, so
    /// the suffix is the id alone.
    fn traverse_pair(
        &self,
        slot: Slot,
        from: &NodeRef,
        to: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        let p = self.face_key(slot, &[&from.encode(), &to.encode()]);
        Ok(Self::ids_after_pair(self.store.scan_suffix(&p)?))
    }

    /// Out-edges of one node (untyped, 0x06).
    pub fn out_edges(&self, src: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.traverse(EDGE_OUT_SLOT, src)
    }

    /// In-edges of one node (untyped, 0x07).
    pub fn in_edges(&self, dst: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.traverse(EDGE_IN_SLOT, dst)
    }

    /// Edges from one node to another, whatever the predicate (0x06 pair).
    pub fn out_edges_to(
        &self,
        src: &NodeRef,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        self.traverse_pair(EDGE_OUT_SLOT, src, dst)
    }

    /// Edges into one node from another, whatever the predicate (0x07 pair).
    pub fn in_edges_from(
        &self,
        dst: &NodeRef,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        self.traverse_pair(EDGE_IN_SLOT, dst, src)
    }

    /// Predicate-qualified traversal (0x09/0x0A): all edge ids of one
    /// predicate from one node. An unknown predicate scans nothing — there
    /// is no face to match.
    pub fn typed_edges(
        &mut self,
        slot: Slot,
        predicate: &str,
        node: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        let predicate_vocab_id = match self.vocab.id_get(&self.store, E::NS_PREFIX, predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(slot, &[&predicate_vocab_id.to_be_bytes(), &node.encode()]);
        Ok(Self::ids_after_endpoint(self.store.scan_suffix(&p)?))
    }

    /// Predicate-qualified out-edges (0x08).
    pub fn typed_out(&mut self, predicate: &str, src: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.typed_edges(EDGE_PREDICATE_OUT_SLOT, predicate, src)
    }

    /// Predicate-qualified in-edges (0x09).
    pub fn typed_in(&mut self, predicate: &str, dst: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.typed_edges(EDGE_PREDICATE_IN_SLOT, predicate, dst)
    }

    /// Predicate-qualified edges from `src` to `dst` (0x08, both endpoints and
    /// the predicate in the prefix) — the ASSERTION IDENTITY lookup: one seek,
    /// whatever the source node's out-degree.
    pub fn typed_out_to(
        &mut self,
        predicate: &str,
        src: &NodeRef,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        let predicate_vocab_id = match self.vocab.id_get(&self.store, E::NS_PREFIX, predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(
            EDGE_PREDICATE_OUT_SLOT,
            &[&predicate_vocab_id.to_be_bytes(), &src.encode(), &dst.encode()],
        );
        Ok(Self::ids_after_pair(self.store.scan_suffix(&p)?))
    }

    /// The same lookup on the incoming face (0x09).
    pub fn typed_in_from(
        &mut self,
        predicate: &str,
        dst: &NodeRef,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        let predicate_vocab_id = match self.vocab.id_get(&self.store, E::NS_PREFIX, predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(
            EDGE_PREDICATE_IN_SLOT,
            &[&predicate_vocab_id.to_be_bytes(), &dst.encode(), &src.encode()],
        );
        Ok(Self::ids_after_pair(self.store.scan_suffix(&p)?))
    }

    /// All edges of one predicate, regardless of endpoints — the
    /// `MATCH ()-[r:has]->()` entry point. The bare `[predicate]` prefix of
    /// 0x08 IS the predicate index: this face used to be a slot of its own,
    /// holding the same set under shorter keys.
    ///
    /// The cost is bytes, not entries: each suffix here carries two endpoints
    /// before the id, where the old face carried the id alone.
    pub fn edges_of_predicate(&mut self, predicate: &str) -> crate::error::Result<Vec<u64>> {
        let predicate_vocab_id = match self.vocab.id_get(&self.store, E::NS_PREFIX, predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(
            EDGE_PREDICATE_OUT_SLOT,
            &[&predicate_vocab_id.to_be_bytes()],
        );
        Ok(Self::ids_after_endpoint(self.store.scan_suffix(&p)?))
    }

    /// Rebuild every graph face from the primary rows.
    ///
    /// A store written before the faces carried both endpoints holds entries at
    /// keys this build never reads — and, because the slots were renumbered, at
    /// SLOT NUMBERS this build reads as something else (the retired predicate
    /// index sat exactly where out-edges live now). So the graph slots are
    /// WIPED and re-derived; the primary table is never touched, and neither is
    /// anything outside the edge namespace.
    ///
    /// Wiping the vector snapshot's slot is the vector layer's business, not
    /// this one's: a stale blob there decodes as garbage and is a cache miss.
    pub fn reindex(&mut self) -> crate::error::Result<usize> {
        let slots = [
            EDGE_OUT_SLOT,
            EDGE_IN_SLOT,
            EDGE_PREDICATE_OUT_SLOT,
            EDGE_PREDICATE_IN_SLOT,
        ];
        let mut batch = Batch::default();
        for slot in slots {
            for (key, _) in scan_suffix_kv(&self.store, &self.face_key(slot, &[]))? {
                batch.del(&key);
            }
        }
        // The edges themselves come from the primary: ids then, not scans of a
        // face we just emptied.
        let primary = self.face_key(PRIMARY_SLOT, &[]);
        let ids: Vec<u64> = self
            .store
            .scan_suffix(&primary)?
            .into_iter()
            .filter_map(|sfx| <[u8; 8]>::try_from(sfx.as_slice()).ok())
            .map(u64::from_be_bytes)
            .collect();
        for edge_id in &ids {
            let Some(bytes) = self.store.get(&self.primary_key(*edge_id))? else {
                continue;
            };
            let parsed = decode_body(&bytes);
            if self.check_members(&parsed.src, &parsed.dst).is_err() {
                continue;
            }
            let src_wire = parsed.src.encode();
            let dst_wire = parsed.dst.encode();
            let predicate_be = parsed.predicate_vocab_id.to_be_bytes();
            let id_be = edge_id.to_be_bytes();
            batch.put(
                self.face_key(EDGE_OUT_SLOT, &[&src_wire, &dst_wire, &id_be]),
                Vec::new(),
            );
            batch.put(
                self.face_key(EDGE_IN_SLOT, &[&dst_wire, &src_wire, &id_be]),
                Vec::new(),
            );
            batch.put(
                self.face_key(
                    EDGE_PREDICATE_OUT_SLOT,
                    &[&predicate_be, &src_wire, &dst_wire, &id_be],
                ),
                Vec::new(),
            );
            batch.put(
                self.face_key(
                    EDGE_PREDICATE_IN_SLOT,
                    &[&predicate_be, &dst_wire, &src_wire, &id_be],
                ),
                Vec::new(),
            );
        }
        self.store.commit_batch(batch)?;
        Ok(ids.len())
    }

    /// Filter by declared attribute (a counting-family 0x1 face): scan the
    /// face with the caller-encoded probe and return edge ids. The derive
    /// emits the per-field marker structs; this is the shared byte-level
    /// walk.
    pub fn by_attr_face(&self, slot: Slot, encoded: &[u8]) -> crate::error::Result<Vec<u64>> {
        let p = self.face_key(slot, &[encoded]);
        Ok(self
            .store
            .scan_suffix(&p)?
            .into_iter()
            .filter_map(|sfx| <[u8; 8]>::try_from(sfx.as_slice()).ok())
            .map(u64::from_be_bytes)
            .collect())
    }

    /// The NEIGHBOR nodes one hop out (untyped, 0x07): edge ids resolved
    /// to their far endpoints. Edge ids remain the identity currency —
    /// this is a convenience composition over `out_edges` + `get_edge`
    /// (zero new wire), for the traversal shape where only nodes
    /// matter. Parallel edges collapse into repeated refs; dedupe if
    /// the caller wants a set (`NodeRef` is Hash + Ord for exactly
    /// this), or keep the repeats as multiplicity.
    pub fn out_nodes(&self, src: &NodeRef) -> crate::error::Result<Vec<NodeRef>> {
        let ids = self.out_edges(src)?;
        self.endpoints(ids, |b| &b.dst)
    }

    /// Neighbor nodes one hop in (untyped, 0x08) — the in-direction
    /// mirror of [`out_nodes`](Self::out_nodes).
    pub fn in_nodes(&self, dst: &NodeRef) -> crate::error::Result<Vec<NodeRef>> {
        let ids = self.in_edges(dst)?;
        self.endpoints(ids, |b| &b.src)
    }

    /// Predicate-qualified neighbors (0x09/0x0A): `typed_out`/`typed_in`
    /// resolved to far endpoints. Unknown predicates scan empty, like their
    /// edge-id counterparts.
    pub fn typed_out_nodes(
        &mut self,
        predicate: &str,
        src: &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        let ids = self.typed_out(predicate, src)?;
        self.endpoints(ids, |b| &b.dst)
    }

    /// Predicate-qualified in-neighbors (0x0A) — the in-direction mirror of
    /// [`typed_out_nodes`](Self::typed_out_nodes).
    pub fn typed_in_nodes(
        &mut self,
        predicate: &str,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        let ids = self.typed_in(predicate, dst)?;
        self.endpoints(ids, |b| &b.src)
    }

    /// Shared resolve step of the *_nodes helpers: ids → bodies → far
    /// endpoint. A dead id (concurrent unlink between scan and fetch)
    /// drops out silently — neighbors are a scan snapshot, not a
    /// transaction.
    fn endpoints(
        &self,
        ids: Vec<u64>,
        side: impl Fn(&EdgeBody) -> &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // A dead id (concurrent unlink between scan and fetch) drops out:
            // neighbors are a scan snapshot, not a transaction.
            if let Some(body) = self.get_edge(id)? {
                out.push(side(&body).clone());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ns: u16, key: u64) -> NodeRef {
        NodeRef::new(&ns.to_be_bytes(), &key.to_be_bytes())
    }

    fn person(id: u64) -> NodeRef {
        node(0x10, id)
    }

    fn org(id: u32) -> NodeRef {
        node(0x11, id as u64)
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
    fn edge_key_is_the_bare_u64() {
        assert_eq!(EdgeKey(7).encode(), 7u64.to_be_bytes().to_vec());
        assert_eq!(EdgeKey::KEY_LEN, 8);
        assert_eq!(EdgeKey::decode(&7u64.to_be_bytes()), EdgeKey(7));
        let mut buf = Vec::new();
        assert_eq!(EdgeKey(9).encode_prefix_named(&mut buf, &["edge_id"]), 8);
        assert_eq!(buf, 9u64.to_be_bytes().to_vec());
        assert_eq!(EdgeKey::prefix_width(&[]), 0);
    }

    #[test]
    fn body_layout_is_ref_ref_predicate_payload() {
        let head = EdgeHead {
            src: person(1),
            dst: org(9),
            predicate: "has".into(),
        };
        let body = edge_body(&head, 0x0102, &[0xAA, 0xBB]);
        // [ns 2B][len 1B][pkey 8B] twice, then the predicate id, then payload.
        assert_eq!(&body[..2], &0x10u16.to_be_bytes());
        assert_eq!(body[2], 8, "pkey len varint (one byte for 8)");
        assert_eq!(&body[3..11], &1u64.to_be_bytes());
        assert_eq!(&body[11..13], &0x11u16.to_be_bytes());
        assert_eq!(body[13], 8);
        assert_eq!(&body[14..22], &9u64.to_be_bytes());
        assert_eq!(&body[22..24], &0x0102u16.to_be_bytes());
        assert_eq!(&body[24..], &[0xAA, 0xBB]);

        let back = decode_body(&body);
        assert_eq!(back.src, person(1));
        assert_eq!(back.dst, org(9));
        assert_eq!(back.predicate_vocab_id, 0x0102);
        assert_eq!(back.payload, vec![0xAA, 0xBB]);
    }
}
