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
    Direction, EDGE_IN_SLOT, EDGE_OUT_SLOT, EDGE_PREDICATE_IN_SLOT, EDGE_PREDICATE_OUT_SLOT,
    EdgeStep, HopHead, NodeRef,
};
use okm_core::obj_vocab::{FIELD_VOCAB, Faces, PREDICATE_VOCAB, Vocabulary};
use okm_core::schema::CollectionSchema;
use okm_core::storage::{Batch, ScanIter, VirtualStorage, prefix_end, scan_suffix_kv};
use okm_value::{Dict, decode_named};

/// One dynamic edge: endpoints by reference, predicate by name,
/// attributes as a name-keyed map (nTLV frames on the wire, ids allocated
/// through the payload's field vocabulary).
#[derive(Clone, Debug)]
pub struct DynEdge {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub predicate: String,
    pub attrs: Dict,
}

/// Engine = the dynamic form's operation surface. Symmetric with
/// `okm_core::Graph` (fixed-ontology): same faces, same wire, runtime
/// declarations. No registry — refs are self-describing.
pub struct Graph<S> {
    store: S,
    /// Predicate vocabulary (0x04/0x05) — per-handle state, allocation is
    /// single-writer, reads load on probe.
    predicates: Vocabulary<u16>,
    /// Payload field-name vocabulary (0x02/0x03) for attribute frames.
    fields: Vocabulary<u16>,
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
            predicates: Vocabulary::new(),
            fields: Vocabulary::new(),
            ns: ns_bytes.to_vec(),
            schema,
        }
    }

    /// The exported declaration this handle was assembled from.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// Diagnostic: vocabulary allocations written but not yet promoted. Zero
    /// after a successful `link` — its allocations commit immediately, and
    /// `write_into` promotes them; a non-zero value would mean the next write
    /// pays one engine probe per name.
    pub fn vocab_pending(&self) -> usize {
        self.predicates.pending_len() + self.fields.pending_len()
    }

    /// The engine handle. The cluster faces (`crate::cluster`) and the derived
    /// rebuild read through this, so a read verb needs no `&mut` on the graph.
    pub(crate) fn store_ref(&self) -> &S {
        &self.store
    }

    /// The ns as the two header bytes every key in this crate starts with —
    /// the shape core's key builders take (`cluster_key`, `edge_member_key`, …),
    /// so the dynamic form builds the same bytes through the same functions.
    pub(crate) fn ns_pair(&self) -> [u8; 2] {
        [self.ns[0], self.ns[1]]
    }

    /// One engine commit: the single boundary every write verb ends on, so
    /// `incorporate` and `retract` cannot drift apart on how a batch lands.
    pub(crate) fn commit(&self, batch: Batch) -> Result<(), String> {
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph commit failed: {e}"))
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
    fn predicate_vocab_id(&mut self, predicate: &str) -> Result<u16, String> {
        let Self {
            store,
            ns,
            predicates,
            ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, PREDICATE_VOCAB);
        predicates
            .ensure(&mut door, predicate)
            .map_err(|e| e.to_string())
    }

    /// The predicate's id if some edge ever carried it — no allocation.
    fn predicate_vocab_lookup(&mut self, predicate: &str) -> Result<Option<u16>, String> {
        let Self {
            store,
            ns,
            predicates,
            ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, PREDICATE_VOCAB);
        predicates
            .id_of(&mut door, predicate)
            .map_err(|e| e.to_string())
    }

    /// The predicate's label, or `None` when the vocabulary has no such id.
    fn predicate_label(&mut self, id: u16) -> Result<Option<String>, String> {
        let Self {
            store,
            ns,
            predicates,
            ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, PREDICATE_VOCAB);
        predicates.name_of(&mut door, id).map_err(|e| e.to_string())
    }

    /// The field id's name, or `None` when the vocabulary has no such id.
    fn field_label(&mut self, id: u16) -> Result<Option<String>, String> {
        let Self {
            store, ns, fields, ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, FIELD_VOCAB);
        fields.name_of(&mut door, id).map_err(|e| e.to_string())
    }

    /// Attribute map → nTLV frames (ids via the field vocabulary; values are
    /// self-describing typed frames — the dynamic segment's own wire).
    fn attrs_wire(&mut self, attrs: &Dict) -> Result<Vec<u8>, String> {
        // The segment codec speaks core's error type, so the closure hands it
        // the vocabulary's own result rather than this facade's string form.
        let Self {
            store, ns, fields, ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, FIELD_VOCAB);
        let mut resolver = |name: &str| fields.ensure(&mut door, name);
        okm_value::encode(attrs, &mut resolver).map_err(|e| e.to_string())
    }

    /// Attribute frames → name-keyed map (ids resolved via the field
    /// vocabulary; unknown ids dropped — the shared-engine degraded mode,
    /// the same rule the dynamic reader (`fetch`) follows).
    fn attrs_from(&mut self, wire: &[u8]) -> Result<Dict, String> {
        // Nested ids resolve through the same mirror; a store failure is parked
        // and raised after the walk (a nested decode has nowhere else to put it).
        let mut failed: Option<String> = None;
        let frames = decode_named(wire, &mut |id| match self.field_label(id) {
            Ok(name) => name,
            Err(e) => {
                failed = Some(e.to_string());
                None
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        let mut out = Dict::new();
        for f in frames {
            if let Some(name) = self.field_label(f.id)? {
                out.insert(name, f.value);
            }
        }
        Ok(out)
    }

    /// Build the primary value: `[src ref][dst ref][predicate_vocab_id][obj
    /// payload]`. The dynamic declaration has no payload fields, so the
    /// payload is ZERO bytes — the same thing the static form's empty
    /// declaration writes (no header: ADR-0042 as amended). Open attributes are
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

    /// Write one edge: primary + predicate index + four traversal faces + the
    /// dynamic segment + the predicate vocabulary, one engine batch.
    ///
    /// `pub(crate)` on the caller's-batch form because the write verb owns the
    /// commit: [`crate::cluster::DynFragment::write_into`] lands this edge's
    /// rows, its cluster membership rows and the cluster rows in ONE batch.
    pub fn link(&mut self, edge: &DynEdge, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        if self.store.get(&pk).map_err(|e| e.to_string())?.is_some() {
            return Err(format!("edge_id {edge_id} already live"));
        }
        let mut batch = Batch::default();
        self.write_into(&mut batch, edge, edge_id)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph link commit failed: {e}"))
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path.
    pub fn link_into(
        &mut self,
        batch: &mut Batch,
        edge: &DynEdge,
        edge_id: u64,
    ) -> Result<(), String> {
        self.write_into(batch, edge, edge_id)
    }

    /// The rows of one assertion, attributes included — the shape both the
    /// create path and a re-assertion of a live id write.
    ///
    /// The dynamic segment is REPLACED, not merged: the entry is deleted and
    /// rewritten with this edge's attributes, so an assertion that dropped an
    /// attribute cannot leave the old frame behind. (On a fresh id the delete
    /// is a no-op; on a re-assertion it is what makes the write idempotent
    /// instead of additive.)
    pub(crate) fn write_into(
        &mut self,
        batch: &mut Batch,
        edge: &DynEdge,
        edge_id: u64,
    ) -> Result<(), String> {
        self.check_members(&edge.src, &edge.dst)?;
        let predicate_vocab_id = self.predicate_vocab_id(&edge.predicate)?;
        let body = self.body_of(edge, predicate_vocab_id);
        let attr_wire = self.attrs_wire(&edge.attrs)?;
        let src_wire = edge.src.encode();
        let dst_wire = edge.dst.encode();
        let predicate_be = predicate_vocab_id.to_be_bytes();
        let id_be = edge_id.to_be_bytes();

        batch.put(self.primary_key(edge_id), body);
        // Open attributes ride the dynamic segment, one entry per edge. The put
        // is unconditional and therefore a REPLACE: the segment is keyed by the
        // edge alone, so a re-assertion overwrites the previous frame list
        // instead of merging into it, and no stale attribute can outlive the
        // assertion that wrote it.
        batch.put(self.fields_key(edge_id), attr_wire);
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
        // Vocabulary growth on THIS path commits immediately (`id_for` writes
        // through the real store, not the caller's batch), so every pending id
        // became durable the moment it returned Ok — promote, or the next write
        // pays a probe per name for the rest of the handle's life.
        self.predicates.landed_all();
        self.fields.landed_all();
        Ok(())
    }

    /// Remove one edge: the faces encode the edge's own values, so deletion
    /// re-derives them from the stored body (fetched first). Exact by
    /// construction — the dynamic form declares no attribute faces to sweep.
    pub fn unlink(&mut self, edge_id: u64) -> Result<(), String> {
        let mut batch = Batch::default();
        self.unlink_into(&mut batch, edge_id)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph unlink commit failed: {e}"))
    }

    /// The same delete, encoded into a caller-owned batch — the shape
    /// `retract` needs, because an edge's rows and the membership rows that
    /// name it leave in ONE commit.
    pub(crate) fn unlink_into(&mut self, batch: &mut Batch, edge_id: u64) -> Result<(), String> {
        let pk = self.primary_key(edge_id);
        let body = self
            .store
            .get(&pk)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unlink: edge_id {edge_id} not found"))?;
        let (src_wire, dst_wire, predicate_vocab_id) = self.parse_body_head(&body)?;
        let id_be = edge_id.to_be_bytes();
        let predicate_be = predicate_vocab_id.to_be_bytes();

        batch.del(&pk);
        batch.del(&self.fields_key(edge_id));
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
        Ok(())
    }

    /// The highest edge id this graph holds — `None` when it holds no edges.
    ///
    /// Primary entries are `[ns][0x00][edge_id 8B]` in big-endian order, so the
    /// maximum IS the last key of that range: one reverse step, which is what
    /// id allocation on the write path runs (the static form's
    /// `Graph::max_edge_id`, over the same bytes).
    pub fn max_edge_id(&self) -> Result<Option<u64>, String> {
        let mut begin = self.ns.clone();
        okm_core::write_slot(&mut begin, PRIMARY_SLOT);
        let end = prefix_end(&begin);
        let head = begin.len();
        let mut it = self.store.scan_range(&begin, end.as_deref());
        let Some(item) = it.next_back() else {
            return Ok(None);
        };
        let (key, _) = item.map_err(|e| e.to_string())?;
        // The suffix of a primary entry IS the id — a key of another width means
        // the store was written by a different declaration, which is reported
        // rather than truncated into a wrong number.
        if key.len() != head + 8 {
            return Err(format!(
                "an edge primary entry is {} byte(s) long, but this form makes it {}",
                key.len(),
                head + 8
            ));
        }
        Ok(<[u8; 8]>::try_from(&key[head..])
            .ok()
            .map(u64::from_be_bytes))
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
    pub fn get_edge(&mut self, edge_id: u64) -> Result<Option<DynEdge>, String> {
        let Some(body) = self
            .store
            .get(&self.primary_key(edge_id))
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let Some((src, n1)) = NodeRef::decode(&body) else {
            return Ok(None);
        };
        let Some((dst, n2)) = NodeRef::decode(&body[n1..]) else {
            return Ok(None);
        };
        if self.check_members(&src, &dst).is_err() {
            return Ok(None);
        }
        let after_dst = n1 + n2;
        let predicate_vocab_id = u16::from_be_bytes([body[after_dst], body[after_dst + 1]]);
        let Some(predicate) = self.predicate_label(predicate_vocab_id)? else {
            // An id whose label is gone: not this graph's edge to describe.
            return Ok(None);
        };
        // Open attributes live in the dynamic segment, not in the body.
        let attrs = match self
            .store
            .get(&self.fields_key(edge_id))
            .map_err(|e| e.to_string())?
        {
            Some(wire) => self.attrs_from(&wire)?,
            None => Dict::new(),
        };
        Ok(Some(DynEdge {
            src,
            dst,
            predicate,
            attrs,
        }))
    }

    /// Edge ids on one traversal face (full NodeRef leads — refs are
    /// self-describing, so the caller just encodes the node).
    fn traverse(&self, slot: u16, node: &NodeRef) -> Result<Vec<u64>, String> {
        let p = self.face_key(slot, &[&node.encode()]);
        Ok(ids_from_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// Out-edges of one node (0x06).
    pub fn out_edges(&self, src: &NodeRef) -> Result<Vec<u64>, String> {
        self.traverse(EDGE_OUT_SLOT, src)
    }

    /// In-edges of one node (0x07).
    pub fn in_edges(&self, dst: &NodeRef) -> Result<Vec<u64>, String> {
        self.traverse(EDGE_IN_SLOT, dst)
    }

    /// Predicate-qualified traversal (0x08/0x09): unknown predicates scan
    /// empty (no face).
    pub fn typed_edges(
        &mut self,
        slot: u16,
        predicate: &str,
        node: &NodeRef,
    ) -> Result<Vec<u64>, String> {
        let predicate_vocab_id = match self.predicate_vocab_lookup(predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let p = self.face_key(slot, &[&predicate_vocab_id.to_be_bytes(), &node.encode()]);
        Ok(ids_from_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// Predicate-qualified out-edges (0x09).
    pub fn typed_out(&mut self, predicate: &str, src: &NodeRef) -> Result<Vec<u64>, String> {
        self.typed_edges(EDGE_PREDICATE_OUT_SLOT, predicate, src)
    }

    /// Predicate-qualified in-edges (0x09).
    pub fn typed_in(&mut self, predicate: &str, dst: &NodeRef) -> Result<Vec<u64>, String> {
        self.typed_edges(EDGE_PREDICATE_IN_SLOT, predicate, dst)
    }

    /// Edges from one node to another, whatever the predicate (0x06 pair): the
    /// prefix names both endpoints, so the suffix is the id alone.
    pub fn out_edges_to(&self, src: &NodeRef, dst: &NodeRef) -> Result<Vec<u64>, String> {
        let p = self.face_key(EDGE_OUT_SLOT, &[&src.encode(), &dst.encode()]);
        Ok(ids_from_pair_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// The same lookup on the incoming face (0x07).
    pub fn in_edges_from(&self, dst: &NodeRef, src: &NodeRef) -> Result<Vec<u64>, String> {
        let p = self.face_key(EDGE_IN_SLOT, &[&dst.encode(), &src.encode()]);
        Ok(ids_from_pair_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// Predicate-qualified edge from `src` to `dst` (0x08): the assertion
    /// identity lookup, one seek whatever the source's out-degree.
    pub fn typed_out_to(
        &mut self,
        predicate: &str,
        src: &NodeRef,
        dst: &NodeRef,
    ) -> Result<Vec<u64>, String> {
        let predicate_vocab_id = match self.predicate_vocab_lookup(predicate)? {
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
        Ok(ids_from_pair_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// All edges of one predicate (0x06) — the `MATCH ()-[r:has]->()` entry.
    pub fn edges_of_predicate(&mut self, predicate: &str) -> Result<Vec<u64>, String> {
        let predicate_vocab_id = match self.predicate_vocab_lookup(predicate)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        // The bare `[predicate]` prefix of the predicate+out face IS the
        // predicate index; the old dedicated face held the same set under
        // shorter keys.
        let p = self.face_key(
            EDGE_PREDICATE_OUT_SLOT,
            &[&predicate_vocab_id.to_be_bytes()],
        );
        Ok(ids_from_suffixes(
            self.store.scan_suffix(&p).map_err(|e| e.to_string())?,
        ))
    }

    /// Every predicate this graph has, id-ordered: one 0x04 prefix scan.
    pub fn predicates(&mut self) -> Result<Vec<(u16, String)>, String> {
        let mut p = self.ns.clone();
        okm_core::write_slot(&mut p, okm_core::index::PREDICATE_VOCAB_ID_SLOT);
        let mut out: Vec<(u16, String)> = scan_suffix_kv(&self.store, &p)
            .map_err(|e| e.to_string())?
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

    /// Predicate id → name (diagnostics surface).
    pub fn predicate_vocab_name(&mut self, id: u16) -> Result<Option<String>, String> {
        self.predicate_label(id)
    }

    /// The predicate's vocabulary id, read-only — `None` when no edge ever
    /// carried it, so no face can hold one.
    ///
    /// The eager `typed_out` needs `&mut self` because it warms the handle's
    /// vocabulary cache; this is the same probe with a throwaway cache, which
    /// is what lets a stream (holding `&self`) push a predicate down to the
    /// 0x08/0x09 prefix when [`DynEdgeStream::by`] names one. One read of the
    /// predicate's 0x04 face, not a walk of the edges.
    pub fn predicate_id(&self, predicate: &str) -> Result<Option<u16>, String> {
        predicate_id_in(&self.store, &self.ns, predicate)
    }

    /// The out-edges of one node as a LAZY, ORDERED, repositionable stream
    /// (face 0x06) — one [`EdgeStep`] per `next()`, read straight off the entry.
    ///
    /// The eager verbs ([`out_edges`](Self::out_edges)) answer a `Vec` and are
    /// the right shape for "list them all"; this is the shape for "walk until
    /// you find". Nothing is collected, an abandoned stream stops reading, and
    /// `find`/`take`/`take_while` cost only the entries they touch.
    ///
    /// **The far endpoint comes from the entry's key.** Every traversal entry
    /// carries both endpoints (`[src][dst][edge_id]`), so a hop's destination is
    /// already in hand — this stream never reads the edge's primary row. What
    /// the step can say is therefore what the entry states: the endpoints and
    /// the id always, the predicate's id only when the scan went through a
    /// qualified face ([`by`](DynEdgeStream::by)) — 0x06/0x07 do not carry one.
    ///
    /// The direction is a PARAMETER, not a second API: [`inn`](Self::inn)
    /// answers the same stream over the other face, so the two compose with
    /// `chain`, `zip` and `flat_map` without an enum of shapes.
    ///
    /// A fault mid-walk is parked, not raised: [`DynEdgeStream::error`] answers
    /// it, because an [`Iterator`] has no place to put one and a corrupt face
    /// must not read as "no more neighbours".
    pub fn out(&self, node: &NodeRef) -> DynEdgeStream<'_, S> {
        DynEdgeStream::new(&self.store, self.ns_pair(), Direction::Out, node.clone())
    }

    /// The in-edges of one node as the same [`DynEdgeStream`] over face 0x07 —
    /// [`out`](Self::out) with the direction as its only difference.
    ///
    /// Its step's `far` is the SOURCE of the edge, and `head.dst` is the node it
    /// was started from: the mirror of the out walk, on the face the double
    /// write already materialized.
    pub fn inn(&self, node: &NodeRef) -> DynEdgeStream<'_, S> {
        DynEdgeStream::new(&self.store, self.ns_pair(), Direction::In, node.clone())
    }

    /// One hop out of `node` — the chainable spelling of [`out`](Self::out):
    /// a hop is `g.walk(n)`, two hops is
    /// `g.walk(n).flat_map(|s| g.walk(&s.far))`, and the depth lives in the
    /// caller's iterator chain rather than in a verb.
    pub fn walk(&self, node: &NodeRef) -> DynEdgeStream<'_, S> {
        self.out(node)
    }
}

/// One node's edges, one direction, as a LAZY ORDERED CURSOR over faces
/// 0x06/0x07 (or 0x08/0x09 once a predicate is pushed down).
///
/// The type is the dynamic counterpart of core's `EdgeStream`: same faces, same
/// entry key, same step (`okm_core::EdgeStep`), so a consumer written against
/// one form works against the other. What differs is only what the dynamic
/// form always differs in — nothing here names a declared type.
///
/// Three contracts hold it together:
///
/// - **One entry per step.** Construction puts the first entry on top (a
///   position has to exist before it can be read), and each `next` hands that
///   entry back and pulls at most one more — so a consumer reads at most one
///   entry ahead of what it was given, and dropping the stream stops the scan
///   where it stands.
/// - **Ordered by entry key.** The key is
///   `[ns][slot][scanned prefix][far ref][edge_id]`, i.e. the far endpoint's
///   byte order and then the id — the address order of the face, not an
///   insertion order. That is what makes it a
///   [`Seekable`](okm_core::seek::Seekable), and what makes two streams over
///   the same face alignable by `okm-query`'s operators.
/// - **Cycle-unaware.** The stream answers one node's edges and stops. "The
///   same node" depends on the caller's own filter, so a visited set here would
///   price every walk for a question only the caller can ask.
pub struct DynEdgeStream<'a, S> {
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
    /// item — a predicate nobody interned has no face to walk.
    scan: Option<ScanIter>,
    /// Bytes of key before the far endpoint: the scanned prefix's length.
    suffix_at: usize,
    /// The entry on top; `None` once spent.
    cur: Option<(Vec<u8>, EdgeStep)>,
    /// The fault that ended the walk, if one did.
    err: Option<String>,
}

impl<'a, S: VirtualStorage> DynEdgeStream<'a, S> {
    fn new(store: &'a S, ns: [u8; 2], dir: Direction, node: NodeRef) -> Self {
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

    /// Where this stream walks — the face family, the pushed-down filter and
    /// the node it was opened on.
    fn prefix(&self) -> Vec<u8> {
        edge_face_prefix(self.ns, self.dir, self.filter, &self.node)
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
    /// endpoint plus an id, a ref that does not parse) is SKIPPED rather than
    /// guessed at — the entry belongs to some other layout, and inventing a hop
    /// from it would be worse than not seeing it.
    fn fill(&mut self) {
        self.cur = None;
        loop {
            let next = match self.scan.as_mut() {
                Some(scan) => scan.next(),
                None => return,
            };
            match next {
                None => return,
                Some(Err(e)) => {
                    self.err = Some(e.to_string());
                    self.scan = None;
                    return;
                }
                Some(Ok((key, _))) => {
                    if let Some(step) =
                        edge_step(&key, self.suffix_at, self.dir, &self.node, self.filter)
                    {
                        self.cur = Some((key, step));
                        return;
                    }
                }
            }
        }
    }

    /// Narrow this hop to one predicate, PUSHED DOWN onto the 0x08/0x09 prefix
    /// — one scan of that predicate's entries for this node, not a scan of the
    /// node's whole face with discards.
    ///
    /// The predicate's id comes from the graph's own vocabulary, so an unknown
    /// name is an EMPTY stream (no edge can carry a predicate nobody wrote),
    /// and the `Result` carries only the vocabulary read's own fault.
    ///
    /// A filter change is a new face: the position restarts at that face's
    /// first entry, and whatever the old cursor had walked is dropped with it
    /// (it was a different key space, so its keys are not bounds in this one).
    pub fn by(mut self, predicate: &str) -> Result<Self, String> {
        match predicate_id_in(self.store, &self.ns, predicate)? {
            Some(id) => {
                self.reopen(Some(id));
                Ok(self)
            }
            None => {
                self.filter = None;
                self.scan = None;
                self.cur = None;
                Ok(self)
            }
        }
    }

    /// The fault that ended this walk, if an engine failed mid-scan. `None` for
    /// a stream that simply ran out of entries — the distinction [`Iterator`]
    /// cannot draw, and the one a caller needs before it trusts "no more
    /// neighbours".
    pub fn error(&self) -> Option<&String> {
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

impl<S: VirtualStorage> Iterator for DynEdgeStream<'_, S> {
    type Item = EdgeStep;

    fn next(&mut self) -> Option<EdgeStep> {
        let (_, step) = self.cur.take()?;
        self.fill();
        Some(step)
    }
}

impl<S: VirtualStorage> okm_core::seek::Seekable for DynEdgeStream<'_, S> {
    /// The face entry's RAW key — the byte string the scan's cursor speaks, so
    /// a position on one stream is directly usable as another's bound when both
    /// walk the same face family.
    type Key = Vec<u8>;

    fn key(&self) -> Option<&Vec<u8>> {
        self.cur.as_ref().map(|(key, _)| key)
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
            Some((key, _)) if key >= lower => return true,
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

/// Read one hop out of a face entry's key: `[far ref][edge_id]` after the
/// scanned prefix, turned into the edge's own direction with the node the
/// stream started from filling the near side.
///
/// Public because a cursor that OWNS its scan (a host binding cannot hold a
/// `&S` borrow across its own calls, so it cannot store a [`DynEdgeStream`])
/// still has to read a hop the way this face states it. Exposing the rule is
/// what keeps one implementation of it: a binding's cursor walks the entries
/// and asks this function what each one means.
pub fn edge_step(
    key: &[u8],
    suffix_at: usize,
    dir: Direction,
    node: &NodeRef,
    filter: Option<u16>,
) -> Option<EdgeStep> {
    let suffix = key.get(suffix_at..)?;
    let (far, taken) = NodeRef::decode(suffix)?;
    let id = <[u8; 8]>::try_from(suffix.get(taken..)?).ok()?;
    let (src, dst) = match dir {
        Direction::Out => (node.clone(), far.clone()),
        Direction::In => (far.clone(), node.clone()),
    };
    Some(EdgeStep {
        edge_id: u64::from_be_bytes(id),
        far,
        head: HopHead {
            src,
            dst,
            predicate: filter,
        },
    })
}

/// The scan prefix one node's walk opens on: `[ns][slot][predicate?][node ref]`,
/// where the slot is the face family the direction (and the pushed-down
/// predicate) selects. The counterpart of [`edge_step`] for the other half of
/// the layout: reading where a walk starts, not what an entry means.
pub fn edge_face_prefix(
    ns: [u8; 2],
    dir: Direction,
    filter: Option<u16>,
    node: &NodeRef,
) -> Vec<u8> {
    let mut prefix = ns.to_vec();
    okm_core::write_slot(&mut prefix, dir.slot(filter));
    if let Some(id) = filter {
        prefix.extend_from_slice(&id.to_be_bytes());
    }
    prefix.extend_from_slice(&node.encode());
    prefix
}

/// The predicate's id, read-only: a throwaway vocabulary cache over a shared
/// store handle.
///
/// Public for the same reason [`edge_step`] is: a cursor that owns its scan
/// reopens on another face when a predicate is pushed down, and the id that
/// names that face comes from the graph's own vocabulary — through this probe,
/// never through a re-derivation of the vocabulary's key layout.
///
/// The cache exists because the probe answers `Option<u16>` through a
/// [`Names`](okm_value::Names) door, which is the same door the write path
/// allocates through; a fresh one costs nothing on a cache miss and cannot go
/// stale, which is what a `&self` handle needs.
pub fn predicate_id_in<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8],
    predicate: &str,
) -> Result<Option<u16>, String> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, u16>::new(store, ns, PREDICATE_VOCAB);
    cache.id_of(&mut door, predicate).map_err(|e| e.to_string())
}

/// Face-suffix → edge ids: every traversal/predicate face ends in an 8-byte
/// BE edge id; shorter suffixes are foreign entries and are skipped.
/// The ids at the end of single-node entries (`other endpoint + edge_id`, so the
/// id is the trailing eight bytes), and — for the pair-qualified scans — the ids
/// that ARE the suffix. Two helpers because the two rules differ, and a shared
/// "take eight bytes" would silently mis-read one of them.
fn ids_from_suffixes(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
    suffixes
        .iter()
        .filter(|s| s.len() > 8)
        .filter_map(|s| s[s.len() - 8..].try_into().ok())
        .map(u64::from_be_bytes)
        .collect()
}

fn ids_from_pair_suffixes(suffixes: Vec<Vec<u8>>) -> Vec<u64> {
    suffixes
        .iter()
        .filter_map(|s| <[u8; 8]>::try_from(s.as_slice()).ok())
        .map(u64::from_be_bytes)
        .collect()
}
