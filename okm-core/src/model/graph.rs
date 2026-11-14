//! Graph Edge (ADR-0017): the third relation carrier — for DYNAMIC
//! graphs (knowledge graphs). Open endpoints (any collection's document,
//! any key shape), bidirectional, parallel edges, attributes.
//!
//! Division line vs the other two carriers (ADR-0015 §3, amended):
//! `Refs` = one-to-many (fixed child type, field position, one
//! direction); a relation table = many-to-many with compile-time typed
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
//! faces are ordinary `#[ok_index]` declarations — counting family 0x3nnn,
//! exactly the document machinery (`Document::index_entries`), because the
//! edge's payload IS an obj (ADR-0012). The node collection is a STANDARD
//! document collection: zero layout difference from any document
//! collection; a node collection carrying kind tokens declares its own
//! predicate vocabulary (the same 0x04/0x05 pair, per collection).
//!
//! Identity: the primary key is the bare `edge_id` (graph-global), so point
//! reads and a delete need only the id; the predicate is a VALUE segment in
//! the body and an index dimension on the faces.

use crate::engine::storage::{Batch, ScanIter, VirtualStorage, prefix_end, scan_suffix_kv};
use crate::model::index::{DYNAMIC_SLOT, Document, PRIMARY_SLOT, Slot, write_slot};
use crate::model::key::KeyEncode;
use crate::model::obj_vocab::{Faces, PREDICATE_VOCAB, Vocabulary};
use okm_value::{Dict, decode_named};

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
pub trait NodeSet: Copy {
    /// Every member collection's ns, one per variant.
    const NS_VALUES: &'static [u16];

    /// The variant's POSITION — its index into [`Self::NS_VALUES`], which the
    /// derive emits in declaration order. It exists because a write path is
    /// handed a variant and has to name its collection (which ns to write
    /// under), and the enum is fieldless: the discriminant IS the position.
    fn index(self) -> usize;

    /// The member collection's ns.
    fn ns(self) -> u16 {
        Self::NS_VALUES[self.index()]
    }

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
///
/// `pub(crate)` for the cluster faces (`model::hyper`, ADR-0066): a cluster's
/// node members are its member edges' endpoints, so deriving them reads an edge
/// body — the same decode, never a second one.
pub(crate) fn decode_body(bytes: &[u8]) -> EdgeBody {
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
/// (symmetric with a relation collection (ADR-0051)). One ns hosts every entry kind,
/// dispatched by segment. The `Vocabulary` is per-handle state over the
/// graph's PREDICATE VOCABULARY (0x04/0x05) — allocation is single-writer, and a
/// read-only handle still answers typed scans (the cache loads on probe).
pub struct Graph<S, E: GraphEncode> {
    store: S,
    vocab: Vocabulary<u16>,
    /// The ns the edge tables live under. `E::NS_PREFIX` unless a caller moved
    /// the whole graph — the restore's mapping (ADR-0062): an edge's endpoints
    /// carry the ns in their refs, so the graph moves as one.
    ns: [u8; 2],
    _pd: std::marker::PhantomData<E>,
}

impl<S: VirtualStorage, E: GraphEncode> Graph<S, E> {
    /// The handle takes the engine and nothing else: the ns comes from the
    /// declaration (`E::NS_PREFIX`), the member set from `E::Nodes`.
    pub fn new(store: S) -> Self {
        Self {
            store,
            vocab: Vocabulary::new(),
            ns: [E::NS_PREFIX[0], E::NS_PREFIX[1]],
            _pd: std::marker::PhantomData,
        }
    }

    /// The same graph rooted at ANOTHER ns — the restore's mapping (ADR-0062).
    ///
    /// One ns for the whole graph, not one per table: an edge's endpoints are
    /// `[ns 2B]` refs, so an edge restored next to nodes that stayed behind would
    /// point at nothing. What moves is the graph, and the node table moves with it
    /// because the caller builds that collection under the same ns.
    pub fn with_ns(store: S, ns: u16) -> Self {
        Self {
            store,
            vocab: Vocabulary::new(),
            ns: ns.to_be_bytes(),
            _pd: std::marker::PhantomData,
        }
    }

    /// Whether the edge table holds no edge at all — the restore's emptiness
    /// check for a graph (ADR-0062): nodes and edges share one ns, so "the
    /// namespace is empty" has to ask about both.
    pub fn has_edges(&self) -> crate::error::Result<bool> {
        let mut prefix = self.ns.to_vec();
        write_slot(&mut prefix, PRIMARY_SLOT);
        let end = crate::engine::storage::prefix_end(&prefix);
        Ok(self
            .store
            .scan_range(&prefix, end.as_deref())
            .next()
            .is_some())
    }

    /// The same graph rooted at another ns, over the same engine (ADR-0062).
    pub fn rebased(&self, ns: u16) -> Self
    where
        S: Clone,
    {
        Self {
            store: self.store.clone(),
            vocab: Vocabulary::new(),
            ns: ns.to_be_bytes(),
            _pd: std::marker::PhantomData,
        }
    }

    /// The ns this graph is rooted at.
    pub fn ns(&self) -> u16 {
        u16::from_be_bytes(self.ns)
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
        let mut k = self.ns.to_vec();
        write_slot(&mut k, PRIMARY_SLOT);
        k.extend_from_slice(&edge_id.to_be_bytes());
        k
    }

    fn face_key(&self, slot: Slot, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = self.ns.to_vec();
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
                    graph_ns: self.ns(),
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
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.ensure(&mut door, predicate)
    }

    /// The predicate's id if it was ever interned — no allocation. `None` means
    /// nobody wrote this predicate, so no edge can carry it.
    fn vocab_lookup(&mut self, predicate: &str) -> crate::error::Result<Option<u16>> {
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.id_of(&mut door, predicate)
    }

    /// Resolve predicate id → name for display/diagnostics.
    pub fn predicate_vocab_name(&mut self, id: u16) -> crate::error::Result<Option<String>> {
        let Self { store, vocab, .. } = self;
        let mut door = Faces::<_, u16>::new(&*store, &self.ns, PREDICATE_VOCAB);
        vocab.name_of(&mut door, id)
    }

    /// Every predicate this graph has, id-ordered: ONE prefix scan of 0x04.
    /// A listing, never an inference walked back from the index faces.
    pub fn predicates(&mut self) -> crate::error::Result<Vec<(u16, String)>> {
        let mut p = self.ns.to_vec();
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

    /// Encode one edge's full row set into the write fragment's batch: primary +
    /// four traversal faces + every DECLARED `#[ok_index]` face, and the
    /// vocabulary pair on first sight of a new predicate. `pub(crate)` because
    /// [`Graph::incorporate`] is the only way to write an edge — this is the row
    /// encoder underneath it, never a verb of its own.
    pub(crate) fn link_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
        edge_id: u64,
    ) -> crate::error::Result<()> {
        self.write_into(batch, head, payload, edge_id)
    }

    /// Re-assert an edge that is already there: every row of the assertion,
    /// WITHOUT the dynamic segment's delete — the open attributes (0x01) belong
    /// to the stored assertion and are no declaration's to clear.
    ///
    /// This is what `Decl::edge_at` on a live id uses, which is how an update
    /// (a reduce folded over a named id) keeps the id, the faces and the
    /// membership rows intact.
    pub(crate) fn assert_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
        edge_id: u64,
    ) -> crate::error::Result<()> {
        self.write_rows(batch, head, payload, edge_id)
    }

    /// Every edge id in this graph, in key order — the scan an export needs.
    /// Slot 0 only: the ns also holds faces, and an unfiltered scan would read
    /// them as rows (the same trap a document export hit).
    pub fn edge_ids(&self) -> crate::error::Result<Vec<u64>> {
        let mut prefix = self.ns.to_vec();
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

    /// The highest edge id this graph holds — `None` when it holds no edges.
    ///
    /// Primary entries are `[ns][0x00][edge_id 8B]` in big-endian order, so the
    /// maximum IS the last key of that range: one reverse step, against
    /// [`edge_ids`](Self::edge_ids) materializing every id. The reader is issuer
    /// recovery ("which id is free now"), which runs on open — the difference is
    /// a whole-table walk against one read.
    pub fn max_edge_id(&self) -> crate::error::Result<Option<u64>> {
        let mut begin = self.ns.to_vec();
        write_slot(&mut begin, PRIMARY_SLOT);
        let end = crate::engine::storage::prefix_end(&begin);
        let head = begin.len();
        let mut it = self.store.scan_range(&begin, end.as_deref());
        let Some(item) = it.next_back() else {
            return Ok(None);
        };
        let (key, _) = item?;
        // The suffix of a primary entry IS the id — a key of another width means
        // the store was written by a different declaration, which is reported
        // rather than truncated into a wrong number.
        if key.len() != head + <EdgeKey as KeyEncode>::KEY_LEN {
            return Err(crate::error::Error::Corrupt {
                what: format!(
                    "an edge primary entry is {} byte(s) long, but the declaration makes it {}",
                    key.len(),
                    head + <EdgeKey as KeyEncode>::KEY_LEN
                ),
            });
        }
        Ok(Some(EdgeKey::decode(&key[head..]).0))
    }

    /// The edge's DYNAMIC segment (face 0x01) — its open attributes, one entry
    /// per edge, keyed `[ns][0x01][edge_id]`.
    fn dynamic_key(&self, edge_id: u64) -> Vec<u8> {
        let mut key = self.ns.to_vec();
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
        fields: &Dict,
    ) -> crate::error::Result<()> {
        let key = self.dynamic_key(edge_id);
        batch.del(&key);
        if fields.is_empty() {
            return Ok(());
        }
        let body = {
            let mut dict = Vocabulary::<u16>::new();
            let mut door =
                Faces::<_, u16>::new(&self.store, &self.ns, crate::model::obj_vocab::FIELD_VOCAB);
            let mut resolver = |name: &str| dict.ensure(&mut door, name);
            okm_value::encode(fields, &mut resolver)?
        };
        batch.put(key, body);
        Ok(())
    }

    /// The edge's open attributes, when it has any — the dynamic half of an edge
    /// (its typed half is the payload, which is not a map). A name whose id the
    /// vocabulary cannot resolve is dropped rather than invented, and an empty
    /// segment answers `None` (no attributes) — the same rule the document path
    /// follows.
    pub fn attrs(&self, edge_id: u64) -> crate::error::Result<Option<Dict>> {
        let raw = match self.store.get(&self.dynamic_key(edge_id))? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let mut dict = Vocabulary::<u16>::new();
        let mut door =
            Faces::<_, u16>::new(&self.store, &self.ns, crate::model::obj_vocab::FIELD_VOCAB);
        let mut failed: Option<crate::error::Error> = None;
        let frames = decode_named(&raw, &mut |id| match dict.name_of(&mut door, id) {
            Ok(name) => name,
            Err(e) => {
                failed = Some(e);
                None
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        if frames.is_empty() {
            return Ok(None);
        }
        let mut out = Dict::new();
        for frame in frames {
            if let Some(name) = dict.name_of(&mut door, frame.id)? {
                out.insert(name, frame.value);
            }
        }
        Ok(Some(out))
    }

    /// A create: the assertion's rows, after the previous occupant's open
    /// attributes are dropped — a write at this id replaces the segment, so
    /// nothing inherits a stale prop.
    fn write_into(
        &mut self,
        batch: &mut Batch,
        head: &EdgeHead,
        payload: &E,
        edge_id: u64,
    ) -> crate::error::Result<()> {
        batch.del(&self.dynamic_key(edge_id));
        self.write_rows(batch, head, payload, edge_id)
    }

    /// The rows of one assertion, without touching the dynamic segment.
    fn write_rows(
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
        for (key, value) in E::index_entries(&EdgeKey(edge_id), payload, &self.ns) {
            batch.put(key, value);
        }
        Ok(())
    }

    /// Delete exactly the rows one edge wrote: decode the body, then delete the
    /// entries it produced. Declared faces are re-derived by decoding the payload
    /// into the declaration's type — no prefix sweep, so an unrelated edge with a
    /// shared attribute value cannot be touched.
    ///
    /// `pub(crate)` for the same reason [`link_into`](Self::link_into) is:
    /// [`Graph::retract`] owns the delete verb, and this is the row encoder it
    /// calls once per doomed edge.
    pub(crate) fn unlink_into(
        &mut self,
        batch: &mut Batch,
        edge_id: u64,
    ) -> crate::error::Result<()> {
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
        for (key, _) in E::index_entries(&EdgeKey(edge_id), &payload, &self.ns) {
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
    pub fn out_edges_to(&self, src: &NodeRef, dst: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.traverse_pair(EDGE_OUT_SLOT, src, dst)
    }

    /// Edges into one node from another, whatever the predicate (0x07 pair).
    pub fn in_edges_from(&self, dst: &NodeRef, src: &NodeRef) -> crate::error::Result<Vec<u64>> {
        self.traverse_pair(EDGE_IN_SLOT, dst, src)
    }

    /// Predicate-qualified traversal (0x08/0x09): all edge ids of one
    /// predicate from one node. An unknown predicate scans nothing — there
    /// is no face to match.
    pub fn typed_edges(
        &mut self,
        slot: Slot,
        predicate: &str,
        node: &NodeRef,
    ) -> crate::error::Result<Vec<u64>> {
        let predicate_vocab_id = match self.vocab_lookup(predicate)? {
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
        let predicate_vocab_id = match self.vocab_lookup(predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(
            EDGE_PREDICATE_OUT_SLOT,
            &[
                &predicate_vocab_id.to_be_bytes(),
                &src.encode(),
                &dst.encode(),
            ],
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
        let predicate_vocab_id = match self.vocab_lookup(predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(
            EDGE_PREDICATE_IN_SLOT,
            &[
                &predicate_vocab_id.to_be_bytes(),
                &dst.encode(),
                &src.encode(),
            ],
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
        let predicate_vocab_id = match self.vocab_lookup(predicate)? {
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
    /// this one's: a stale blob there decodes as garbage and is a cache miss, and
    /// `okm_vector::VectorIndex::wipe` is the verb that reclaims it.
    ///
    /// `chunk` bounds the ops per commit. A reader running DURING the rebuild
    /// sees a half-built face, which is why the operator contract is a window: do
    /// not read while it runs. `usize::MAX` restores the old single-commit shape
    /// (atomic, and one batch of every entry in the edge set).
    pub fn reindex(&mut self, chunk: usize) -> crate::error::Result<usize> {
        use crate::engine::storage::prefix_end;

        let chunk = chunk.max(1);
        let slots = [
            EDGE_OUT_SLOT,
            EDGE_IN_SLOT,
            EDGE_PREDICATE_OUT_SLOT,
            EDGE_PREDICATE_IN_SLOT,
        ];

        // Phase 1 — sweep. An edge id is the last segment of every face key, so a
        // face entry from another layout sits at a key this build never builds;
        // re-deriving cannot name it. Both phases commit every `chunk` ops, so a
        // table of edges costs one chunk of memory rather than one batch of all.
        let mut batch = Batch::default();
        for slot in slots {
            let prefix = self.face_key(slot, &[]);
            let end = prefix_end(&prefix);
            for item in self.store.scan_range(&prefix, end.as_deref()) {
                let (key, _) = item?;
                batch.del(&key);
                if batch.ops.len() >= chunk {
                    self.store.commit_batch(std::mem::take(&mut batch))?;
                }
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(std::mem::take(&mut batch))?;
        }

        // Phase 2 — derive from the primary, lazily: ids come from the walk, not
        // from a face we just emptied.
        let primary = self.face_key(PRIMARY_SLOT, &[]);
        let end = prefix_end(&primary);
        let mut rebuilt = 0usize;
        for item in self.store.scan_range(&primary, end.as_deref()) {
            let (key, bytes) = item?;
            let suffix = &key[primary.len()..];
            let Some(id) = <[u8; 8]>::try_from(suffix).ok().map(u64::from_be_bytes) else {
                continue;
            };
            let parsed = decode_body(&bytes);
            if self.check_members(&parsed.src, &parsed.dst).is_err() {
                continue;
            }
            let src_wire = parsed.src.encode();
            let dst_wire = parsed.dst.encode();
            let predicate_be = parsed.predicate_vocab_id.to_be_bytes();
            let id_be = id.to_be_bytes();
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
            rebuilt += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(batch)?;
        }
        Ok(rebuilt)
    }

    /// Filter by declared attribute (a counting-family 0x3nnn face): scan the
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

    /// Predicate-qualified neighbors (0x08/0x09): `typed_out`/`typed_in`
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

    /// Predicate-qualified in-neighbors (0x09) — the in-direction mirror of
    /// [`typed_out_nodes`](Self::typed_out_nodes).
    pub fn typed_in_nodes(
        &mut self,
        predicate: &str,
        dst: &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        let ids = self.typed_in(predicate, dst)?;
        self.endpoints(ids, |b| &b.src)
    }

    /// The out-edges of one node as a LAZY, ORDERED, repositionable stream
    /// (face 0x06) — one [`EdgeStep`] per `next()`, read straight off the entry.
    ///
    /// The eager verbs ([`out_edges`](Self::out_edges),
    /// [`out_nodes`](Self::out_nodes)) answer a `Vec` and are the right shape for
    /// "list them all". This is the shape for "walk until you find": nothing is
    /// collected, an abandoned stream stops reading, and
    /// [`find`](Iterator::find)/[`take`](Iterator::take)/
    /// [`take_while`](Iterator::take_while) cost only the entries they touch.
    ///
    /// **The far endpoint comes from the entry's key.** Every traversal entry
    /// carries both endpoints (`[src][dst][edge_id]`), so a hop's destination is
    /// already in hand — this stream never reads the edge's primary row, which
    /// is one read per hop saved against the `out_edges` + `get_edge` shape. The
    /// consequence is what the step can say: it knows the endpoints and the id,
    /// and (only when the scan went through a predicate-qualified face) the
    /// predicate's id. A hop on 0x06 does not know the predicate, because the
    /// entry does not carry it — that is exactly why
    /// [`by`](EdgeStream::by) switches faces instead of filtering afterwards.
    ///
    /// The direction is a PARAMETER, not a second API: [`inn`](Self::inn) answers
    /// the same [`EdgeStream`] over the other face, so the two compose with
    /// `chain`, `zip` and `flat_map` without an enum of shapes.
    ///
    /// **Cycle-unaware, deliberately.** A stream walks one node's face and
    /// stops; whether two paths reached "the same" node depends on the caller's
    /// filter (a tenant? a version? a kind?), so a visited set here would answer
    /// a question the stream cannot see. A hop chain is
    /// `flat_map`: `let mut here = vec![start]; for _ in 0..n { here = here.iter().flat_map(|node| g.walk(node)).map(|step| step.far).collect(); }`
    /// — and the caller that needs to stop on revisit owns that set, with its
    /// memory cost in plain sight.
    ///
    /// A fault mid-walk is parked, not raised: [`EdgeStream::error`] answers it,
    /// because an [`Iterator`] has no place to put one and a corrupt face must
    /// not read as "no more neighbours" (the rule
    /// [`SeekScan::error`](crate::model::document::SeekScan::error) states).
    pub fn out(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        EdgeStream::new(&self.store, self.ns, Direction::Out, node.clone())
    }

    /// The in-edges of one node as the same [`EdgeStream`] over face 0x07 —
    /// [`out`](Self::out) with the direction as its only difference.
    ///
    /// Its step's `far` is the SOURCE of the edge (`head.src`), and `head.dst`
    /// is the node it was started from: the mirror of the out walk, on the face
    /// the double write already materialized. No second index is involved, so
    /// "who points at this" costs exactly what "what does this point at" does.
    pub fn inn(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        EdgeStream::new(&self.store, self.ns, Direction::In, node.clone())
    }

    /// One hop out of `node` — the chainable spelling of [`out`](Self::out).
    ///
    /// It exists so a traversal reads as one: a hop is
    /// `g.walk(node)`, two hops is `g.walk(node).flat_map(|s| g.walk(&s.far))`,
    /// and the depth lives in the caller's iterator chain rather than in a
    /// verb. It is the same stream, not a shortcut with different semantics.
    pub fn walk(&self, node: &NodeRef) -> EdgeStream<'_, S> {
        self.out(node)
    }

    /// The predicate's vocabulary id, read-only — `None` when no edge ever
    /// carried it, so no face can hold one.
    ///
    /// [`typed_out`](Self::typed_out) needs a `&mut self` because it warms the
    /// handle's vocabulary cache on the way; this is the same probe with a
    /// throwaway cache, which is what lets a stream (holding `&self`) push a
    /// predicate down to the 0x08/0x09 prefix when [`by`](EdgeStream::by) names
    /// one. One `get` on the predicate's 0x04 face, not a walk of the edges.
    pub fn predicate_id(&self, predicate: &str) -> crate::error::Result<Option<u16>> {
        predicate_id_in(&self.store, &self.ns, predicate)
    }

    /// Shared resolve step of the *_nodes helpers: ids → bodies → far
    /// endpoint. A dead id (concurrent retract between scan and fetch)
    /// drops out silently — neighbors are a scan snapshot, not a
    /// transaction.
    fn endpoints(
        &self,
        ids: Vec<u64>,
        side: impl Fn(&EdgeBody) -> &NodeRef,
    ) -> crate::error::Result<Vec<NodeRef>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // A dead id (concurrent retract between scan and fetch) drops out:
            // neighbors are a scan snapshot, not a transaction.
            if let Some(body) = self.get_edge(id)? {
                out.push(side(&body).clone());
            }
        }
        Ok(out)
    }
}

/// Which face a hop walks: forward (0x06 / 0x08) or backward (0x07 / 0x09).
///
/// A direction is a VALUE, not a second type: [`Graph::out`] and
/// [`Graph::inn`] answer the same [`EdgeStream`], so the two directions
/// `chain`, `zip` and `flat_map` into one traversal without the consumer
/// matching on a shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Forward, source → destination: face 0x06, or 0x08 with a predicate.
    Out,
    /// Backward, destination → source: face 0x07, or 0x09 with a predicate.
    In,
}

impl Direction {
    /// The face this direction walks — predicate-qualified (0x08/0x09) when a
    /// predicate travelled with it, plain (0x06/0x07) when it did not.
    ///
    /// One decision, made in one place, so a stream cannot scan the untyped
    /// face with a predicate set (or the other way round): the two says the
    /// same thing about where the entries live.
    pub fn slot(self, predicate: Option<u16>) -> Slot {
        match (self, predicate) {
            (Direction::Out, None) => EDGE_OUT_SLOT,
            (Direction::In, None) => EDGE_IN_SLOT,
            (Direction::Out, Some(_)) => EDGE_PREDICATE_OUT_SLOT,
            (Direction::In, Some(_)) => EDGE_PREDICATE_IN_SLOT,
        }
    }
}

/// The head a traversal ENTRY states: the edge's two endpoints, always, and the
/// predicate's vocabulary id when the entry came off a qualified face.
///
/// It is deliberately not [`EdgeHead`]. The untyped faces 0x06/0x07 carry no
/// predicate at all — that is why filtering by one switches faces instead of
/// filtering rows — so a step decoded from them cannot answer a predicate
/// name, and `EdgeHead`'s `String` would have to be invented. `predicate: None`
/// means \"this entry did not say\", never \"the edge has no predicate\"; the
/// name behind an id is one vocabulary read away
/// ([`Graph::predicate_vocab_name`]), and the id itself came off the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HopHead {
    /// The edge's source endpoint.
    pub src: NodeRef,
    /// The edge's destination endpoint.
    pub dst: NodeRef,
    /// The predicate's vocabulary id, when the face carried one.
    pub predicate: Option<u16>,
}

/// One hop, as the traversal entry itself states it.
///
/// Everything here comes from the entry's KEY (`[src][dst][edge_id]`, or
/// `[predicate][…]` on the qualified faces): the far endpoint is the first ref
/// after the scanned prefix, the id is its trailing eight bytes. No row is
/// read to build one, so a `find` over a face costs the entries it walks and
/// nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeStep {
    /// The edge's identity — globally unique within the graph, and the id every
    /// other verb ([`Graph::get_edge`], [`Graph::attrs`], a retract) takes.
    pub edge_id: u64,
    /// The endpoint on the FAR side of this step: `head.dst` walking out,
    /// `head.src` walking in.
    pub far: NodeRef,
    /// The head the entry named — the edge's own direction, which is why an out
    /// step and an in step of ONE edge answer the same `head` and mirrored
    /// `far`s.
    pub head: HopHead,
}

/// The entry on top of an [`EdgeStream`]: its raw face key (the position
/// [`Seekable`] speaks) and the hop it decodes to.
struct FaceEntry {
    key: Vec<u8>,
    step: EdgeStep,
}

/// One node's edges, one direction, as a LAZY ORDERED CURSOR.
///
/// Built by [`Graph::out`] / [`Graph::inn`] / [`Graph::walk`]; the same type
/// answers both directions (`Direction` is a field), and
/// [`by`](Self::by) moves it onto the predicate-qualified face without
/// changing the type.
///
/// Three contracts hold it together:
///
/// - **One entry per step.** Construction puts the first entry on top (a
///   position has to exist before it can be read, the rule
///   [`SeekScan`](crate::model::document::SeekScan) follows), and each
///   [`next`](Iterator::next) hands that entry back and pulls at most one more.
///   So a consumer reads at most one entry ahead of what it was given, and
///   dropping the stream stops the scan where it stands.
/// - **Ordered by entry key.** The key is `[header][slot][scanned prefix][far
///   ref][edge_id]`, so the order is the far endpoint's byte order and then the
///   id — the address order of the face, not an insertion order. That is what
///   makes it a [`Seekable`](crate::seek::Seekable) and what makes two streams
///   over the same face alignable by [`okm-query`'s operators](crate::seek).
/// - **Cycle-unaware.** The stream answers one node's edges and stops. It does
///   not dedupe and it does not remember where it has been: \"the same node\"
///   depends on the caller's filter, and a visited set here would price every
///   walk for a question only the caller can ask.
pub struct EdgeStream<'a, S> {
    /// The engine the face lives in — re-read by [`by`](Self::by), which
    /// reopens the scan on another face.
    store: &'a S,
    /// The graph's namespace, as the two header bytes every key starts with.
    ns: [u8; 2],
    /// Which face family this stream walks.
    dir: Direction,
    /// The node the walk started from: the scanned prefix, and the `near` side
    /// of every head the stream reports.
    node: NodeRef,
    /// The pushed-down predicate id, or `None` for the untyped face.
    filter: Option<u16>,
    /// The face cursor. `None` means this stream is SPENT before its first
    /// item — a predicate nobody interned has no face to walk, and that is an
    /// empty answer rather than a scan that discards everything.
    scan: Option<ScanIter>,
    /// Bytes of key before the far endpoint: the scanned prefix's length, so
    /// decoding an entry is a slice, not a re-derived prefix.
    suffix_at: usize,
    /// The entry on top; `None` once spent.
    cur: Option<FaceEntry>,
    /// The fault that ended the walk, if one did.
    ///
    /// [`Iterator`] has no channel for it and [`Seekable`]'s verbs answer
    /// positions, so it is parked here instead of being reported as a clean
    /// end — the same rule [`SeekScan::error`](crate::model::document::SeekScan::error)
    /// states, and the reason a corrupt face cannot read as \"no neighbours\".
    err: Option<crate::error::Error>,
}

impl<'a, S: VirtualStorage> EdgeStream<'a, S> {
    /// Open one node's face and put its first entry on top.
    ///
    /// `pub(crate)` because the cluster layer's node form flattens the very same
    /// streams ([`crate::model::hyper::ClusterStream`]) rather than opening the
    /// faces a second time.
    pub(crate) fn new(store: &'a S, ns: [u8; 2], dir: Direction, node: NodeRef) -> Self {
        let mut stream = Self {
            store,
            ns,
            dir,
            node,
            filter: None,
            scan: None,
            suffix_at: 0,
            cur: None,
            err: None,
        };
        stream.reopen(None);
        stream
    }

    /// Where this stream walks — the same face family and node it was opened
    /// on, at whatever filter it carries now.
    fn prefix(&self) -> Vec<u8> {
        let mut prefix = self.ns.to_vec();
        write_slot(&mut prefix, self.dir.slot(self.filter));
        if let Some(id) = self.filter {
            prefix.extend_from_slice(&id.to_be_bytes());
        }
        prefix.extend_from_slice(&self.node.encode());
        prefix
    }

    /// Point the cursor at (possibly) another face and re-fill it.
    ///
    /// A filter change is a different FACE, not a narrower range on the same
    /// one, so the position does not survive: the old scan is dropped and the
    /// new one starts at its own first entry.
    fn reopen(&mut self, filter: Option<u16>) {
        self.filter = filter;
        let prefix = self.prefix();
        self.suffix_at = prefix.len();
        self.cur = None;
        self.err = None;
        let end = prefix_end(&prefix);
        self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
        self.fill();
    }

    /// Put the next decodable entry on top, or clear it when the face ends.
    ///
    /// An entry whose key this face cannot carry (a shorter suffix than an
    /// endpoint plus an id, a ref that does not parse) is skipped rather than
    /// guessed at — the same fail-closed direction
    /// [`ids_after_endpoint`](Graph::ids_after_endpoint) takes, since the entry
    /// belongs to some other layout and inventing a hop from it would be worse
    /// than not seeing it.
    fn fill(&mut self) {
        self.cur = None;
        loop {
            // The pull's borrow ends here, so decoding can read `self` freely.
            let next = match self.scan.as_mut() {
                Some(scan) => scan.next(),
                None => return,
            };
            match next {
                None => return,
                Some(Err(e)) => {
                    self.err = Some(e);
                    return;
                }
                Some(Ok((key, _))) => {
                    if let Some(step) = self.decode(&key) {
                        self.cur = Some(FaceEntry { key, step });
                        return;
                    }
                }
            }
        }
    }

    /// Read one hop out of a face entry's key: `[far ref][edge_id]` after the
    /// scanned prefix, turned into the edge's own direction with the node this
    /// stream started from filling the near side.
    fn decode(&self, key: &[u8]) -> Option<EdgeStep> {
        let suffix = key.get(self.suffix_at..)?;
        let (far, taken) = NodeRef::decode(suffix)?;
        let id_bytes = suffix.get(taken..)?;
        let id = <[u8; 8]>::try_from(id_bytes).ok()?;
        let (src, dst) = match self.dir {
            Direction::Out => (self.node.clone(), far.clone()),
            Direction::In => (far.clone(), self.node.clone()),
        };
        Some(EdgeStep {
            edge_id: u64::from_be_bytes(id),
            far,
            head: HopHead {
                src,
                dst,
                predicate: self.filter,
            },
        })
    }

    /// Narrow this hop to one predicate, PUSHED DOWN onto the 0x08/0x09 prefix
    /// — one scan of that predicate's entries for this node, not a scan of the
    /// node's whole face with discards.
    ///
    /// The pushed-down form is the same [`EdgeStream`], so `by` sits anywhere a
    /// stream does: `g.out(&n).by("employs")?.take(2)`. The predicate's id
    /// comes from the graph's own vocabulary, so an unknown name is an EMPTY
    /// stream — no edge can carry a predicate nobody ever wrote — and the
    /// `Result` carries only the vocabulary read's own fault.
    ///
    /// A filter change is a new face: the position restarts at that face's
    /// first entry, and whatever the old cursor had walked is dropped with it
    /// (it was a different key space, so its keys are not bounds in this one).
    pub fn by(mut self, predicate: &str) -> crate::error::Result<Self> {
        match self.predicate_id(predicate)? {
            Some(id) => {
                self.reopen(Some(id));
                Ok(self)
            }
            // No face can hold this predicate: spent before the first item,
            // with no scan opened to read and throw away.
            None => {
                self.filter = None;
                self.scan = None;
                self.cur = None;
                Ok(self)
            }
        }
    }

    /// The predicate's vocabulary id, without warming the graph handle's cache
    /// (this stream holds `&Graph`, not `&mut Graph`, which is what lets two
    /// streams from one graph be alive at once — and compared).
    fn predicate_id(&self, predicate: &str) -> crate::error::Result<Option<u16>> {
        predicate_id_in(self.store, &self.ns, predicate)
    }

    /// The fault that ended this walk, if an engine failed mid-scan.
    ///
    /// `None` for a stream that simply ran out of entries — the distinction
    /// [`Iterator`] cannot draw, and the one a caller needs before it trusts
    /// \"no more neighbours\".
    pub fn error(&self) -> Option<&crate::error::Error> {
        self.err.as_ref()
    }

    /// Which direction this stream walks.
    pub fn direction(&self) -> Direction {
        self.dir
    }

    /// The node the walk started from.
    pub fn node(&self) -> &NodeRef {
        &self.node
    }

    /// The pushed-down predicate id, when [`by`](Self::by) narrowed this
    /// stream.
    pub fn predicate(&self) -> Option<u16> {
        self.filter
    }
}

impl<S: VirtualStorage> Iterator for EdgeStream<'_, S> {
    type Item = EdgeStep;

    fn next(&mut self) -> Option<EdgeStep> {
        let entry = self.cur.take()?;
        self.fill();
        Some(entry.step)
    }
}

impl<S: VirtualStorage> crate::seek::Seekable for EdgeStream<'_, S> {
    /// The face entry's RAW key — the byte string the scan's cursor speaks, so
    /// a position on one stream is directly usable as another's bound when both
    /// walk the same face family.
    type Key = Vec<u8>;

    fn key(&self) -> Option<&Vec<u8>> {
        self.cur.as_ref().map(|entry| &entry.key)
    }

    fn step(&mut self) -> bool {
        if self.cur.is_none() {
            return false;
        }
        self.fill();
        self.cur.is_some()
    }

    fn seek(&mut self, lower: &Vec<u8>) -> bool {
        match &self.cur {
            // At or past the bound: forward-only, so the position is kept.
            Some(entry) if &entry.key >= lower => return true,
            // Spent (or never opened): nothing at or after any bound remains.
            None => return false,
            Some(_) => {}
        }
        let Some(scan) = self.scan.as_mut() else {
            return false;
        };
        // The cursor moves; the entry it lands on is not consumed by the seek,
        // so `fill` puts it on top (and skips anything undecodable after it).
        if !scan.seek(lower) {
            self.cur = None;
            return false;
        }
        self.fill();
        self.cur.is_some()
    }
}

/// The predicate's id, read-only: a throwaway vocabulary cache over a shared
/// store handle.
///
/// The cache exists because the probe answers `Option<u16>` through a
/// [`Names`](okm_value::Names) door, which is the same door the write path
/// allocates through; a fresh one costs nothing on a cache miss and cannot go
/// stale, which is what a `&self` handle needs.
fn predicate_id_in<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    predicate: &str,
) -> crate::error::Result<Option<u16>> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, u16>::new(store, ns, PREDICATE_VOCAB);
    cache.id_of(&mut door, predicate)
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
