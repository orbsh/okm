//! Fully dynamic graph edges (ADR-0017 §4, second form): a dynamic edge
//! with EMPTY declarations — identity only, attributes as runtime data.
//!
//! Shares the fixed-ontology wire exactly: the same entry kinds on
//! ADR-0030 fixed faces, the same `[ns 2B][pkey]` refs parsed by the
//! declaration's widths, the same PREDICATE VOCABULARY (0x04/0x05) for
//! predicates. The differences are declaration-side only:
//!
//! - endpoints are stored as the graph's own HANDLES: an edge row is
//!   `[src_h][pred][dst_h]`, eighteen bytes whatever width the member tables
//!   declare (ADR-0067's boundary-only handle). The schema's `member_widths` is
//!   what a caller-supplied `NodeRef` is checked against at write time; the
//!   faces themselves never carry a reference;
//! - edge attributes are nTLV frames (id + vocabulary, self-describing
//!   value types) in the dynamic segment instead of compile-time fields —
//!   attribute names are runtime data, so their ids allocate through the
//!   payload's FIELD vocabulary (0x02/0x03), never through the predicate
//!   vocabulary (0x04/0x05): one id space per vocabulary, no collision;
//! - an edge has no number here either: the tuple is the identity, and writing
//!   the same tuple twice is writing the same row.
//!
//! The handle takes the ns and the schema as a PAIR (ADR-0025) — the
//! declaration's two halves, from the export side — and rejects a
//! disagreement instead of overriding one with the other.
//!
//! Capability scope inherited from the dynamic layer (ADR-0022):
//! subscribe stays excluded; func/partial indexes and reduce are
//! callable-implementable on the table side.

use std::collections::{BTreeMap, BTreeSet};

use okm_core::model::graph::{
    Direction, EDGE_PREDICATE_OUT_SLOT, EdgeKey, EdgeStep, Handle, HandleKey, HopHead, Members,
    NodeRef, face_key, in_face_key, in_prefix, out_face_key, out_prefix, parse_predicate_far,
    predicate_in_face_key, predicate_in_prefix, predicate_out_face_key, predicate_out_prefix,
};
use okm_core::model::handles::Handles;
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
/// declarations.
pub struct Graph<S> {
    store: S,
    /// Predicate vocabulary (0x04/0x05) — per-handle state, allocation is
    /// single-writer, reads load on probe.
    predicates: Vocabulary<u16>,
    /// Payload field-name vocabulary (0x02/0x03) for attribute frames.
    fields: Vocabulary<u16>,
    ns: Vec<u8>,
    schema: CollectionSchema,
    /// The declaration's width table: every member collection's key width, which
    /// is what an edge key is parsed by (ADR-0067).
    members: Members,
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
        let members = Members::new(schema.member_widths.clone());
        Self {
            store,
            predicates: Vocabulary::new(),
            fields: Vocabulary::new(),
            ns: ns_bytes.to_vec(),
            schema,
            members,
        }
    }

    /// The exported declaration this handle was assembled from.
    pub fn schema(&self) -> &CollectionSchema {
        &self.schema
    }

    /// The declaration's width table (ns → key width).
    pub fn member_widths(&self) -> &Members {
        &self.members
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

    /// Membership and width (ADR-0067): the declaration's member set rides the
    /// schema as data, and a dynamic writer enforces it against the refs' ns —
    /// the same check the static form makes through `E::Nodes`. A member with no
    /// declared width could not be keyed at all, so an empty table is a refusal
    /// rather than "nothing to check": an edge key needs the declaration.
    fn check_endpoint(&self, side: &str, node: &NodeRef) -> Result<(), String> {
        let ns = node.ns_id();
        let Some(width) = self.members.key_len(ns) else {
            return Err(format!(
                "{side} ns {ns} is not a member of this graph — declare it in the schema's member \
                 set with its key width (ADR-0067)"
            ));
        };
        if node.pkey.len() != width {
            return Err(format!(
                "the {side} endpoint in ns {ns} is {} byte(s) wide, but the declaration fixes that \
                 table's key at {width} — a graph member must have a fixed-width key (ADR-0067)",
                node.pkey.len()
            ));
        }
        Ok(())
    }

    fn face_key(&self, slot: u16, segments: &[&[u8]]) -> Vec<u8> {
        let mut k = self.ns.clone();
        okm_core::write_slot(&mut k, slot);
        for s in segments {
            k.extend_from_slice(s);
        }
        k
    }

    /// The handle registry of THIS graph's namespace — the one place a write
    /// issues an identity and a read resolves one (ADR-0067).
    fn handles(&self) -> Handles<'_, S> {
        Handles::new(&self.store, self.ns_pair())
    }

    /// The stored row identity behind a caller's tuple: both endpoints resolved
    /// WITHOUT issuing (a read, a probe, or a delete must never allocate).
    /// `None` when either endpoint never appeared in an edge — then no row can
    /// name it.
    fn row_of(&self, key: &EdgeKey) -> Result<Option<HandleKey>, String> {
        let registry = self.handles();
        let (Some(src), Some(dst)) = (
            registry.handle_of(&key.src).map_err(|e| e.to_string())?,
            registry.handle_of(&key.dst).map_err(|e| e.to_string())?,
        ) else {
            return Ok(None);
        };
        Ok(Some(HandleKey::new(src, key.predicate, dst)))
    }

    /// Resolve a set of rows to the tuples a caller speaks — ONE registry scan
    /// for the whole answer, the boundary the design defers resolution to.
    fn edges_from_rows(&self, rows: Vec<HandleKey>) -> Result<Vec<EdgeKey>, String> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut wanted = BTreeSet::new();
        for row in &rows {
            wanted.insert(row.src);
            wanted.insert(row.dst);
        }
        let sorted: Vec<Handle> = wanted.iter().copied().collect();
        let refs = self.handles().nodes(&sorted).map_err(|e| e.to_string())?;
        let nodes: BTreeMap<Handle, NodeRef> = sorted.into_iter().zip(refs).collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let (Some(src), Some(dst)) = (nodes.get(&row.src), nodes.get(&row.dst)) else {
                return Err("an edge row names a handle the registry does not hold".to_string());
            };
            out.push(EdgeKey {
                src: src.clone(),
                predicate: row.predicate,
                dst: dst.clone(),
            });
        }
        Ok(out)
    }

    /// One handle's node — one point read, the boundary a stream consumer needs:
    /// [`DynEdgeStream`] reports HANDLES (ADR-0067), and a reference is this
    /// call, never a field on the step.
    pub fn node(&self, handle: Handle) -> Result<NodeRef, String> {
        self.handles()
            .node(handle)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "handle {} is named by an edge row but the registry has no node for it",
                    handle.id()
                )
            })
    }

    /// The tuple a traversal step names — its two handles resolved once, at the
    /// boundary; the walk itself resolves nothing.
    pub fn edge_key(&self, step: &EdgeStep) -> Result<EdgeKey, String> {
        self.edges_from_rows(vec![step.row()])?
            .into_iter()
            .next()
            .ok_or_else(|| "the handle registry answered no row for an edge step".to_string())
    }

    /// Does an edge sit at this row identity? One point read of 0x06 — the
    /// delete path's existence test, in handle space so nothing is resolved.
    pub(crate) fn has_row(&self, row: &HandleKey) -> Result<bool, String> {
        Ok(self
            .store
            .get(&self.out_key(row))
            .map_err(|e| e.to_string())?
            .is_some())
    }

    /// `[ns][0x06][src_h][pred][dst_h]` — the edge itself, payload in the value.
    fn out_key(&self, row: &HandleKey) -> Vec<u8> {
        out_face_key(&self.ns_pair(), row)
    }

    /// `[ns][0x07][dst_h][pred][src_h]`.
    fn in_key(&self, row: &HandleKey) -> Vec<u8> {
        in_face_key(&self.ns_pair(), row)
    }

    /// `[ns][0x08][pred][src_h][dst_h]`.
    fn predicate_out_key(&self, row: &HandleKey) -> Vec<u8> {
        predicate_out_face_key(&self.ns_pair(), row)
    }

    /// `[ns][0x09][pred][dst_h][src_h]`.
    fn predicate_in_key(&self, row: &HandleKey) -> Vec<u8> {
        predicate_in_face_key(&self.ns_pair(), row)
    }

    /// The dynamic segment's entry: `[ns][0x01][src_h][pred][dst_h]` → nTLV
    /// frames, keyed by the same row identity as the edge itself.
    fn fields_key(&self, row: &HandleKey) -> Vec<u8> {
        let mut k = self.face_key(okm_core::index::DYNAMIC_SLOT, &[]);
        row.write(&mut k);
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

    /// The refusal every delete states when a tuple names no row: the endpoints are
    /// named, because a caller's handle does not know the graph's numbering.
    fn not_found(key: &EdgeKey) -> String {
        format!(
            "unlink: edge (src ns {}, predicate {}, dst ns {}) not found",
            key.src.ns_id(),
            key.predicate,
            key.dst.ns_id()
        )
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
        let Self {
            store, ns, fields, ..
        } = self;
        let mut door = Faces::<_, u16>::new(&*store, ns, FIELD_VOCAB);
        let mut failed: Option<String> = None;
        let frames = decode_named(wire, &mut |id| match fields.name_of(&mut door, id) {
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
            if let Some(name) = fields.name_of(&mut door, f.id).map_err(|e| e.to_string())? {
                out.insert(name, f.value);
            }
        }
        Ok(out)
    }

    /// The dynamic declaration has no payload fields, so an edge's payload is the
    /// empty obj — the same bytes the static form's empty declaration writes
    /// (ADR-0042 as amended: no header beyond the obj's own version and count).
    fn empty_payload() -> Vec<u8> {
        vec![1, 0, 0]
    }

    /// The identity a write will use: endpoints checked against the declared
    /// member tables and the predicate interned (the vocabulary id is part of the
    /// key, so it comes first).
    pub fn key_of(&mut self, edge: &DynEdge) -> Result<EdgeKey, String> {
        self.check_endpoint("src", &edge.src)?;
        self.check_endpoint("dst", &edge.dst)?;
        let predicate = self.predicate_vocab_id(&edge.predicate)?;
        Ok(EdgeKey {
            src: edge.src.clone(),
            predicate,
            dst: edge.dst.clone(),
        })
    }

    /// Write one edge: the edge row (0x06, payload included) + the three derived
    /// faces + the dynamic segment + the predicate vocabulary, one engine batch.
    ///
    /// Idempotent by construction (ADR-0067): the tuple is the identity, so
    /// writing it twice is writing the same row.
    pub fn link(&mut self, edge: &DynEdge) -> Result<EdgeKey, String> {
        let mut batch = Batch::default();
        let key = self.write_into(&mut batch, edge)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph link commit failed: {e}"))?;
        Ok(key)
    }

    /// Encode the full write set into an externally owned batch — the
    /// cross-collection atomic path.
    pub fn link_into(&mut self, batch: &mut Batch, edge: &DynEdge) -> Result<EdgeKey, String> {
        self.write_into(batch, edge)
    }

    /// The rows of one assertion, attributes included — the shape both the
    /// create path and a re-assertion of a live identity write.
    ///
    /// The dynamic segment is REPLACED, not merged: the entry is written with
    /// this edge's attributes, so an assertion that dropped an attribute cannot
    /// leave the old frame behind.
    ///
    /// Answers the caller's tuple (endpoints as given), while the rows are keyed
    /// by the handles the write issued — [`write_row_into`](Self::write_row_into)
    /// is the row-shaped form a cluster plan needs.
    pub(crate) fn write_into(
        &mut self,
        batch: &mut Batch,
        edge: &DynEdge,
    ) -> Result<EdgeKey, String> {
        let row = self.write_row_into(batch, edge)?;
        Ok(EdgeKey {
            src: edge.src.clone(),
            predicate: row.predicate,
            dst: edge.dst.clone(),
        })
    }

    /// The same write, answering the ROW identity the faces were keyed by — both
    /// endpoints issued (or re-read) through the caller's batch, so the probe
    /// sees a handle this fragment already wrote (no second row) and a refused
    /// fragment leaves no handle behind (ADR-0067). Each endpoint costs one
    /// registry probe per write; a fragment that repeats a node pays that probe
    /// per edge, never a second allocation.
    pub(crate) fn write_row_into(
        &mut self,
        batch: &mut Batch,
        edge: &DynEdge,
    ) -> Result<HandleKey, String> {
        self.check_endpoint("src", &edge.src)?;
        self.check_endpoint("dst", &edge.dst)?;
        let predicate = self.predicate_vocab_id(&edge.predicate)?;
        let attr_wire = self.attrs_wire(&edge.attrs)?;
        let payload = Self::empty_payload();
        // Identity lives in the registry, and the issue rides THIS batch: the
        // probe reads through it, so the second endpoint of a repeated node is
        // the handle the first one already wrote.
        let registry = self.handles();
        let src = registry
            .issue_into(batch, &edge.src)
            .map_err(|e| e.to_string())?;
        let dst = registry
            .issue_into(batch, &edge.dst)
            .map_err(|e| e.to_string())?;
        let row = HandleKey::new(src, predicate, dst);

        // 0x06 IS the edge: its value is the payload, so the traversal row and
        // the row are one write (ADR-0067).
        batch.put(self.out_key(&row), payload);
        batch.put(self.in_key(&row), Vec::new());
        batch.put(self.predicate_out_key(&row), Vec::new());
        batch.put(self.predicate_in_key(&row), Vec::new());
        // Open attributes ride the dynamic segment, one entry per edge, keyed by
        // the same row identity: the put is unconditional and therefore a
        // REPLACE, so a re-assertion overwrites the previous frame list instead
        // of merging into it.
        if attr_wire.is_empty() {
            batch.del(&self.fields_key(&row));
        } else {
            batch.put(self.fields_key(&row), attr_wire);
        }
        // Vocabulary growth on THIS path commits immediately (`id_for` writes
        // through the real store, not the caller's batch), so every pending id
        // became durable the moment it returned Ok — promote, or the next write
        // pays a probe per name for the rest of the handle's life.
        self.predicates.landed_all();
        self.fields.landed_all();
        Ok(row)
    }

    /// Remove one edge: the identity is the key, so every face is re-derived
    /// from it — no body to read, and nothing of another edge's can be touched.
    pub fn unlink(&mut self, key: &EdgeKey) -> Result<(), String> {
        let mut batch = Batch::default();
        self.unlink_into(&mut batch, key)?;
        self.store
            .commit_batch(batch)
            .map_err(|e| format!("dynamic graph unlink commit failed: {e}"))
    }

    /// The same delete, encoded into a caller-owned batch — the shape
    /// `retract` needs, because an edge's rows and the membership rows that
    /// name it leave in ONE commit.
    pub(crate) fn unlink_into(&mut self, batch: &mut Batch, key: &EdgeKey) -> Result<(), String> {
        let Some(row) = self.row_of(key)? else {
            return Err(Self::not_found(key));
        };
        if !self.has_row(&row)? {
            return Err(Self::not_found(key));
        }
        self.purge_row_into(batch, &row);
        Ok(())
    }

    /// The delete stated as the ROW it is: every face re-derived from the
    /// eighteen bytes, nothing resolved. The encoding half of
    /// [`unlink_into`](Self::unlink_into) — `retract` holds rows (it read them
    /// in handle space), so this is the form that takes one.
    pub(crate) fn purge_row_into(&self, batch: &mut Batch, row: &HandleKey) {
        batch.del(&self.out_key(row));
        batch.del(&self.in_key(row));
        batch.del(&self.predicate_out_key(row));
        batch.del(&self.predicate_in_key(row));
        batch.del(&self.fields_key(row));
    }

    /// Fetch one edge, decoded: endpoints from the caller's tuple, predicate
    /// resolved to its name, attributes from the dynamic segment. The row is
    /// resolved once, at the boundary — a tuple naming an endpoint the graph
    /// never saw has no row to read.
    pub fn get_edge(&mut self, key: &EdgeKey) -> Result<Option<DynEdge>, String> {
        let Some(row) = self.row_of(key)? else {
            return Ok(None);
        };
        if self
            .store
            .get(&self.out_key(&row))
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Ok(None);
        }
        let predicate_vocab_id = key.predicate;
        let Some(predicate) = self.predicate_label(predicate_vocab_id)? else {
            // An id whose label is gone: not this graph's edge to describe.
            return Ok(None);
        };
        let attrs = match self
            .store
            .get(&self.fields_key(&row))
            .map_err(|e| e.to_string())?
        {
            Some(wire) => self.attrs_from(&wire)?,
            None => Dict::new(),
        };
        Ok(Some(DynEdge {
            src: key.src.clone(),
            dst: key.dst.clone(),
            predicate,
            attrs,
        }))
    }

    /// Out-edges of one node (0x06): each entry's suffix is `[predicate][dst_h]`,
    /// exactly ten bytes — the far endpoint comes off the key (ADR-0067). The
    /// rows are resolved at the boundary, in ONE registry scan.
    pub fn out_edges(&self, src: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        let Some(near) = self.handles().handle_of(src).map_err(|e| e.to_string())? else {
            // A node the graph never saw as an endpoint has no out face.
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &out_prefix(&self.ns_pair(), near))
            .map_err(|e| e.to_string())?
        {
            if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                rows.push(HandleKey::new(near, predicate, far));
            }
        }
        self.edges_from_rows(rows)
    }

    /// In-edges of one node (0x07): the face states `[predicate][src_h]`.
    pub fn in_edges(&self, dst: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        let Some(near) = self.handles().handle_of(dst).map_err(|e| e.to_string())? else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &in_prefix(&self.ns_pair(), near))
            .map_err(|e| e.to_string())?
        {
            if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                rows.push(HandleKey::new(far, predicate, near));
            }
        }
        self.edges_from_rows(rows)
    }

    /// Predicate-qualified traversal (0x08/0x09): unknown predicates scan
    /// empty (no face), and a node with no handle has no face either.
    fn typed_edges(
        &mut self,
        dir: Direction,
        predicate: &str,
        node: &NodeRef,
    ) -> Result<Vec<EdgeKey>, String> {
        let Some(predicate_id) = self.predicate_vocab_lookup(predicate)? else {
            return Ok(Vec::new());
        };
        let Some(near) = self.handles().handle_of(node).map_err(|e| e.to_string())? else {
            return Ok(Vec::new());
        };
        let ns = self.ns_pair();
        let prefix = match dir {
            Direction::Out => predicate_out_prefix(&ns, predicate_id, near),
            Direction::In => predicate_in_prefix(&ns, predicate_id, near),
        };
        let mut rows = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix).map_err(|e| e.to_string())? {
            let Some(far) = Handle::parse(&suffix) else {
                continue;
            };
            rows.push(match dir {
                Direction::Out => HandleKey::new(near, predicate_id, far),
                Direction::In => HandleKey::new(far, predicate_id, near),
            });
        }
        self.edges_from_rows(rows)
    }

    /// Predicate-qualified out-edges (0x08).
    pub fn typed_out(&mut self, predicate: &str, src: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        self.typed_edges(Direction::Out, predicate, src)
    }

    /// Predicate-qualified in-edges (0x09).
    pub fn typed_in(&mut self, predicate: &str, dst: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        self.typed_edges(Direction::In, predicate, dst)
    }

    /// Edges from one node to another, whatever the predicate. The predicate
    /// sits between the endpoints, so the pair is not a contiguous prefix: this
    /// filters the source's run.
    pub fn out_edges_to(&self, src: &NodeRef, dst: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        Ok(self
            .out_edges(src)?
            .into_iter()
            .filter(|key| key.dst == *dst)
            .collect())
    }

    /// The same lookup on the incoming face (0x07).
    pub fn in_edges_from(&self, dst: &NodeRef, src: &NodeRef) -> Result<Vec<EdgeKey>, String> {
        Ok(self
            .in_edges(dst)?
            .into_iter()
            .filter(|key| key.src == *src)
            .collect())
    }

    /// The assertion identity lookup: one exact key, whatever the source's
    /// out-degree. `None` when no edge carries the tuple.
    pub fn edge_at(
        &mut self,
        predicate: &str,
        src: &NodeRef,
        dst: &NodeRef,
    ) -> Result<Option<EdgeKey>, String> {
        let Some(predicate_id) = self.predicate_vocab_lookup(predicate)? else {
            return Ok(None);
        };
        let key = EdgeKey {
            src: src.clone(),
            predicate: predicate_id,
            dst: dst.clone(),
        };
        let Some(row) = self.row_of(&key)? else {
            // Neither endpoint ever appeared: no row can carry the tuple.
            return Ok(None);
        };
        Ok(self
            .store
            .get(&self.out_key(&row))
            .map_err(|e| e.to_string())?
            .map(|_| key))
    }

    /// All edges of one predicate (0x08's bare `[predicate]` prefix) — the
    /// `MATCH ()-[r:has]->()` entry. Each suffix is `[src_h][dst_h]`, sixteen
    /// bytes, so nothing is parsed by declaration width.
    pub fn edges_of_predicate(&mut self, predicate: &str) -> Result<Vec<EdgeKey>, String> {
        let Some(predicate_id) = self.predicate_vocab_lookup(predicate)? else {
            return Ok(Vec::new());
        };
        let mut prefix = face_key(&self.ns_pair(), EDGE_PREDICATE_OUT_SLOT);
        prefix.extend_from_slice(&predicate_id.to_be_bytes());
        let mut rows = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix).map_err(|e| e.to_string())? {
            if suffix.len() != 16 {
                continue;
            }
            let (Some(src), Some(dst)) = (Handle::parse(&suffix[..8]), Handle::parse(&suffix[8..]))
            else {
                continue;
            };
            rows.push(HandleKey::new(src, predicate_id, dst));
        }
        self.edges_from_rows(rows)
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
    /// 0x08/0x09 prefix when [`DynEdgeStream::by`] names one.
    pub fn predicate_id(&self, predicate: &str) -> Result<Option<u16>, String> {
        predicate_id_in(&self.store, &self.ns, predicate)
    }

    /// The out-edges of one node as a LAZY, ORDERED, repositionable stream
    /// (face 0x06) — one [`EdgeStep`] per `next()`, read straight off the entry.
    ///
    /// The eager verbs answer a `Vec` and are the right shape for "list them
    /// all"; this is the shape for "walk until you find". Nothing is collected,
    /// an abandoned stream stops reading, and `find`/`take`/`take_while` cost
    /// only the entries they touch.
    ///
    /// **A step is a whole identity.** The entry IS the edge, so a hop states
    /// both endpoints and the predicate — the far endpoint and the predicate come
    /// off the key, and no row is read (ADR-0067).
    ///
    /// The direction is a PARAMETER, not a second API: [`inn`](Self::inn)
    /// answers the same stream over the other face.
    ///
    /// A fault mid-walk is parked, not raised: [`DynEdgeStream::error`] answers
    /// it, because an [`Iterator`] has no place to put one and a corrupt face
    /// must not read as "no more neighbours".
    pub fn out(&self, node: &NodeRef) -> DynEdgeStream<'_, S> {
        DynEdgeStream::new(&self.store, self.ns_pair(), Direction::Out, node.clone())
    }

    /// The in-edges of one node as the same [`DynEdgeStream`] over face 0x07 —
    /// [`out`](Self::out) with the direction as its only difference.
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
/// - **Ordered by entry key.** The key is `[ns][slot][scanned prefix][far
///   handle]`, i.e. the far endpoint's handle order — the address order of the
///   face, not an insertion order (issue order is not a key order, ADR-0067).
///   That is what makes it a [`Seekable`](okm_core::seek::Seekable), and what
///   makes two streams over the same face alignable by `okm-query`'s operators.
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
    /// The node the walk started from: the `near` side of every head the stream
    /// reports.
    node: NodeRef,
    /// That node's HANDLE — the scanned prefix, resolved ONCE at the open
    /// (ADR-0067). [`Handle::NONE`] when the graph never saw the node as an
    /// endpoint, which is an empty walk, not a fault.
    handle: Handle,
    /// The pushed-down predicate id, or `None` for the untyped face.
    filter: Option<u16>,
    /// The face cursor. `None` means this stream is SPENT before its first
    /// item — a predicate nobody interned has no face to walk.
    scan: Option<ScanIter>,
    /// Bytes of key before the entry's own segment.
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
            handle: Handle::NONE,
            filter: None,
            scan: None,
            suffix_at: 0,
            cur: None,
            err: None,
        };
        // ONE resolution per walk, here at the open: the face is keyed by the
        // node's handle, and a node the graph never saw has no face at all (an
        // empty walk, not a fault — nothing names it).
        match Handles::new(store, ns).handle_of(&stream.node) {
            Ok(Some(handle)) => {
                stream.handle = handle;
                stream.reopen(None);
            }
            Ok(None) => {}
            Err(e) => stream.err = Some(e.to_string()),
        }
        stream
    }

    /// Where this stream walks — the face family, the pushed-down filter and
    /// the node it was opened on.
    fn prefix(&self) -> Vec<u8> {
        edge_face_prefix(self.ns, self.dir, self.filter, self.handle)
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
    /// An entry whose key this face cannot carry (a suffix that is not a
    /// predicate-plus-handle pair, or one of another width) is SKIPPED rather
    /// than guessed at — the entry belongs to some other layout.
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
                        edge_step(&key, self.suffix_at, self.dir, self.handle, self.filter)
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
    /// first entry, and whatever the old cursor had walked is dropped with it.
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

/// Read one hop out of a face entry's key: the far endpoint (and, on the
/// unqualified faces, the predicate) after the scanned prefix, turned into the
/// edge's own direction with the node the stream started from filling the near
/// side.
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
    handle: Handle,
    filter: Option<u16>,
) -> Option<EdgeStep> {
    let suffix = key.get(suffix_at..)?;
    let (predicate, far) = match filter {
        // A qualified face consumed the predicate in the scanned prefix.
        Some(id) => (id, Handle::parse(suffix)?),
        // The unqualified faces put it right after the near endpoint, and the
        // pair is exactly ten bytes.
        None => parse_predicate_far(suffix)?,
    };
    let (src_handle, dst_handle) = match dir {
        Direction::Out => (handle, far),
        Direction::In => (far, handle),
    };
    Some(EdgeStep {
        far_handle: far,
        head: HopHead {
            src_handle,
            dst_handle,
            predicate,
        },
    })
}

/// The scan prefix one node's walk opens on: `[ns][slot][predicate?][near_h]`,
/// where the slot is the face family the direction (and the pushed-down
/// predicate) selects. The counterpart of [`edge_step`] for the other half of
/// the layout: reading where a walk starts, not what an entry means.
///
/// The near side is a HANDLE, not a [`NodeRef`]: the faces are keyed by the
/// graph's own numbering (ADR-0067), and a caller that only holds one resolves
/// it through the registry before it opens a walk.
pub fn edge_face_prefix(
    ns: [u8; 2],
    dir: Direction,
    filter: Option<u16>,
    handle: Handle,
) -> Vec<u8> {
    match (dir, filter) {
        (Direction::Out, None) => out_prefix(&ns, handle),
        (Direction::In, None) => in_prefix(&ns, handle),
        (Direction::Out, Some(id)) => predicate_out_prefix(&ns, id, handle),
        (Direction::In, Some(id)) => predicate_in_prefix(&ns, id, handle),
    }
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
