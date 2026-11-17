//! Clusters on the dynamic form (ADR-0066 + ADR-0068), the counterpart of core's
//! `model::hyper` — same faces, same key shapes, same rules, `Dict` payloads
//! everywhere.
//!
//! The face numbers and widths come from `okm_core::model::layout` — one table
//! for the whole engine; this module mirrors the typed `hyper` layer for the
//! dynamic (schema-less) graph and never re-derives a number.
//!
//! A member is an EDGE (its ROW identity, ADR-0067: the rows hold
//! `[src_h][pred][dst_h]` — eighteen bytes, with no width table and no length
//! prefix) or an ENTITY the cluster declares (ADR-0068: one row per handle, eight
//! bytes). The public verbs resolve handles to `NodeRef`s at the boundary, so a
//! caller speaks tuples (`EdgeKey`) and references, and the faces never carry
//! either.
//!
//! # Why this exists as a second implementation and not a generic wrapper
//!
//! The static form cannot express what this one is for. `Fragment<G>` carries
//! `Decl::Node(G::Nodes, …)`, the conflict hooks are typed (`Fn(&G, &G) -> G`,
//! `Fn(&Person, &Person) -> Person`), and the edge payload is `E` — all of that
//! needs a declaration. A schema-as-data store has none: its endpoints are
//! `NodeRef`s, its edge payload is a `Dict`, its cluster payload is a `Dict`,
//! and its hooks are `Fn(&Dict, &Dict) -> Dict`. There is no node table to name,
//! so there is no node declaration and no node hook — a node here is an endpoint
//! and nothing else.
//!
//! Everything below that is shared is shared by the same BYTES, not by a
//! copied opinion: the key builders ([`cluster_key`], [`edge_member_key`],
//! [`child_key`], [`edge_cluster_key`], [`cluster_value`]) are core's own, and
//! the value layout (`[parent u64 BE][payload]`) is core's too. So a store
//! written through this form reads through the static one and vice versa; only
//! the declarations differ.
//!
//! # Two verbs, one direction each
//!
//! [`Graph::incorporate`] writes, [`Graph::retract`] removes — the names the
//! static form uses, so the vocabulary does not fork.
//!
//! - A [`DynFragment`] is the whole unit of work: the edge declarations, the
//!   clusters over them and the optional conflict hooks, landed in ONE engine
//!   batch.
//! - Membership is idempotent by construction: a member row is
//!   `(kind, cluster, edge)`, so writing the same triple twice is one row.
//! - Deleting a NODE ([`DynFragment::delete_node`]) takes every edge incident to
//!   it — both traversal faces, the predicate faces, the payload row, the
//!   dynamic segment and those edges' membership rows. A node cannot be left
//!   with a dangling edge.
//! - Deleting an EDGE ([`DynFragment::delete_edge`]) leaves both endpoints
//!   alone, even when they are then connected to nothing.
//!
//! A cluster outlives its members: removing the last one leaves the row and its
//! payload standing, with a count of zero. A cluster is a scope, created when
//! declared and deleted only when explicitly deleted, so membership changes never
//! take the row with them.

use std::collections::{BTreeMap, BTreeSet};

use okm_core::model::graph::{
    EdgeKey, Handle, HandleKey, NodeRef, in_prefix, out_prefix, parse_predicate_far,
};
use okm_core::model::handles::{HANDLE_WIDTH, Handles};
use okm_core::model::index::U64_WIDTH;
use okm_core::model::hyper::{
    CHILD_INDEX_SLOT, CLUSTER_SLOT, EDGE_CLUSTER_SLOT, EDGE_MEMBER_SLOT, NODE_CLUSTER_SLOT,
    NODE_MEMBER_SLOT, child_key, children_prefix, cluster_key, cluster_value, clusters_prefix,
    edge_cluster_key, edge_clusters_prefix, edge_member_key, members_prefix, node_cluster_key,
    node_clusters_prefix, node_member_key, node_members_prefix,
};
use okm_core::model::layout::{CLUSTER_KIND_WIDTH, ClusterKindId};
use okm_core::model::obj_vocab::{CLUSTER_KIND, FIELD_VOCAB, Faces, FieldNameId, Names, Vocabulary};
use okm_core::storage::{Batch, BatchStore, VirtualStorage, prefix_end, scan_suffix_kv};
use okm_value::{Dict, Variant, decode_named};

use crate::graph::{DynEdge, Graph};

/// An edge's tuple as bytes: `(src ns, src pkey, predicate id, dst ns, dst pkey)`.
/// The tuple IS the identity (ADR-0067), so this is what "the same edge" means to
/// a reader that has to dedupe one seen from both ends.
type EdgeIdentity = (u16, Vec<u8>, u16, u16, Vec<u8>);

/// One edge that crosses a subgraph's cut: the tuple as stored (which IS the
/// identity, so it is what a rewrite retracts), the member it touches, and the
/// end on the other side of the cut.
pub struct Crossing {
    pub key: EdgeKey,
    pub inside: NodeRef,
    pub outside: NodeRef,
}

/// One group of entities as a subgraph: its members, the edges with BOTH ends
/// inside, and the edges that cross the cut.
pub struct Subgraph {
    /// The declared members, in key order.
    pub nodes: Vec<NodeRef>,
    /// Edges with both ends in the selection: they die with it.
    pub internal: Vec<EdgeKey>,
    /// Edges with exactly one end in the selection: what a rewrite re-attaches.
    pub external: Vec<Crossing>,
}

impl Subgraph {
    /// The crossings the given entity holds — the ones a rewrite of THAT entity
    /// has to answer for. Order follows the read.
    pub fn crossings_of(&self, node: &NodeRef) -> Vec<&Crossing> {
        self.external
            .iter()
            .filter(|crossing| crossing.inside == *node)
            .collect()
    }
}


/// What the delete half of a batch decides: the edge rows it dooms, the entities
/// it retires, and the cluster plan those removals imply. Both verbs take one;
/// `replace` hands its plan to the write half so a cluster is counted once.
type Removal = (
    BTreeSet<HandleKey>,
    BTreeSet<NodeRef>,
    BTreeMap<(ClusterKindId, u64), Plan>,
);

/// The cluster vocabulary, re-exported so a consumer of this crate names one
/// package: `kind` is the same one-byte closed vocabulary the static form uses
/// (`COMMUNITY` / `EVENT` / `SUBGRAPH` / `SCOPE`), `hyper_id` the same run-time
/// allocated id, and `MEMBER_COUNT` the same reserved payload name the write
/// path stamps last.
pub use okm_core::model::hyper::{
    Cluster, ClusterRef, ClusterRow, DerivedReport, EdgeSet, NodeSet, Kind, MEMBER_COUNT,
    MemberEdges, NODE_COUNT,
    ReplaceReport, RetractReport,
    WriteReport,
};

// ---------------------------------------------------------------------------
// Keys and values: one header, four faces, the shapes core already builds
// ---------------------------------------------------------------------------

fn face_prefix(ns: &[u8; 2], slot: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(4);
    key.extend_from_slice(ns);
    // The fixed-face form of `write_slot`: one byte for 0x00–0x0D.
    okm_core::write_slot(&mut key, slot);
    key
}

fn be_vocab(bytes: &[u8]) -> Option<ClusterKindId> {
    bytes.try_into().ok().map(ClusterKindId::from_be_bytes)
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; U64_WIDTH]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

/// Intern the caller's label as a cluster kind (0x0A/0x0B): first seen claims the
/// next id, append-only, never renumbered — the dynamic twin of the typed face's
/// `kind_id`, and the door a binding needs, because on this form nothing declares
/// a kind up front (ADR-0068: `Kind` is an id, the word is the caller's).
pub fn intern_kind<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    label: &str,
) -> Result<Kind, String> {
    let mut door = Faces::<_, ClusterKindId>::new(store, ns, CLUSTER_KIND);
    let mut vocab = Vocabulary::<ClusterKindId>::new();
    Ok(Kind(vocab.ensure(&mut door, label).map_err(|e| e.to_string())?))
}

/// The label a kind was interned under — `None` for a number nobody minted.
pub fn interned_label<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    kind: Kind,
) -> Result<Option<String>, String> {
    let mut door = Faces::<_, ClusterKindId>::new(store, ns, CLUSTER_KIND);
    let mut vocab = Vocabulary::<ClusterKindId>::new();
    vocab.name_of(&mut door, kind.0).map_err(|e| e.to_string())
}

/// Every kind this namespace has interned, id-ordered: one prefix scan of 0x0A.
pub fn interned_kinds<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
) -> Result<Vec<(ClusterKindId, String)>, String> {
    let mut door = Faces::<_, ClusterKindId>::new(store, ns, CLUSTER_KIND);
    let mut pairs: Vec<(ClusterKindId, String)> = Names::scan(&mut door)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(label, id)| (id, label))
        .collect();
    pairs.sort_by_key(|(id, _)| *id);
    Ok(pairs)
}

fn count_of(payload: &Dict, name: &str) -> u64 {
    match payload.get(name) {
        Some(Variant::U8(v)) => u64::from(*v),
        Some(Variant::U16(v)) => u64::from(*v),
        Some(Variant::U32(v)) => u64::from(*v),
        Some(Variant::U64(v)) => *v,
        _ => 0,
    }
}

/// The cluster row's value, sliced rather than decoded: a fixed eight-byte
/// parent head, then the payload. `0` is the reserved "no parent" value, not a
/// nullable flag, so the head is always eight bytes.
fn parse_head(value: &[u8]) -> Result<(u64, &[u8]), String> {
    if value.len() < U64_WIDTH {
        return Err(format!(
            "a cluster row is {} byte(s); its parent head is 8",
            value.len()
        ));
    }
    let head = be_u64(&value[..8]).unwrap_or(0);
    Ok((head, &value[8..]))
}

/// Payload `Dict` → nTLV frames: names allocate in the namespace's field
/// vocabulary (0x02/0x03), the same ids an edge's attributes use. Encoding
/// through `store` — which is the caller's batch on the write path — is what
/// lets a refused fragment leave no vocabulary entry behind either.
fn encode_dict<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    dict: &Dict,
) -> Result<Vec<u8>, String> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, FieldNameId>::new(store, ns, FIELD_VOCAB);
    let mut resolver = |name: &str| cache.ensure(&mut door, name);
    okm_value::encode(dict, &mut resolver).map_err(|e| e.to_string())
}

/// The inverse. An id the vocabulary cannot resolve keeps no name, exactly as
/// the edge attribute path treats it — a reader never invents one.
fn decode_dict<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    raw: &[u8],
) -> Result<Dict, String> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, FieldNameId>::new(store, ns, FIELD_VOCAB);
    let mut failed: Option<String> = None;
    let frames = decode_named(raw, &mut |id| match cache.name_of(&mut door, id) {
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
    for frame in frames {
        if let Some(name) = cache
            .name_of(&mut door, frame.id)
            .map_err(|e| e.to_string())?
        {
            out.insert(name, frame.value);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// The member rows of one cluster, in key order: one prefix scan of 0x0B.
///
/// A member row's whole suffix IS the edge's row identity — eighteen bytes,
/// exactly what a traversal entry carries — so it parses by FIXED WIDTH. There
/// is no width table and no length prefix (ADR-0067), and nothing here resolves
/// a handle: rows are the unit the faces speak.
fn member_keys<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<HandleKey>, String> {
    let mut out = Vec::new();
    for (suffix, _) in
        scan_suffix_kv(store, &members_prefix(ns, cluster)).map_err(|e| e.to_string())?
    {
        if let Some(row) = HandleKey::parse(&suffix) {
            out.push(row);
        }
    }
    Ok(out)
}

/// One edge's stored ROW identity, read-only: `None` when either endpoint never
/// appeared in an edge, because then no face can name it. The boundary between
/// a caller's tuple and the face's handles, for the single-key verbs
/// ([`Graph::member_of`], [`Graph::edge_sets_of`]).
fn row_of_key<S: VirtualStorage>(
    store: &S,
    ns: &[u8; 2],
    key: &EdgeKey,
) -> Result<Option<HandleKey>, String> {
    let registry = Handles::new(store, *ns);
    let (Some(src), Some(dst)) = (
        registry.handle_of(&key.src).map_err(|e| e.to_string())?,
        registry.handle_of(&key.dst).map_err(|e| e.to_string())?,
    ) else {
        return Ok(None);
    };
    Ok(Some(HandleKey::new(src, key.predicate, dst)))
}

/// Resolve a set of rows to the tuples a caller speaks — ONE registry scan for
/// the whole answer (the handles are sorted by construction, which is what the
/// bulk resolver requires).
fn resolve_rows<S: VirtualStorage>(
    store: &S,
    ns: &[u8; 2],
    rows: Vec<HandleKey>,
) -> Result<Vec<EdgeKey>, String> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut wanted = BTreeSet::new();
    for row in &rows {
        wanted.insert(row.src);
        wanted.insert(row.dst);
    }
    let sorted: Vec<Handle> = wanted.iter().copied().collect();
    let refs = Handles::new(store, *ns)
        .nodes(&sorted)
        .map_err(|e| e.to_string())?;
    let nodes: BTreeMap<Handle, NodeRef> = sorted.into_iter().zip(refs).collect();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let (Some(src), Some(dst)) = (nodes.get(&row.src), nodes.get(&row.dst)) else {
            return Err("a member row names a handle the registry does not hold".to_string());
        };
        out.push(EdgeKey {
            src: src.clone(),
            predicate: row.predicate,
            dst: dst.clone(),
        });
    }
    Ok(out)
}

/// The node member rows of one cluster, in key order: one prefix scan of the
/// node member face. The suffix IS the handle, eight bytes.
fn node_member_keys<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<Handle>, String> {
    let mut out = Vec::new();
    for (suffix, _) in
        scan_suffix_kv(store, &node_members_prefix(ns, cluster)).map_err(|e| e.to_string())?
    {
        if let Some(handle) = Handle::parse(&suffix) {
            out.push(handle);
        }
    }
    Ok(out)
}

/// What one cluster write has to build on: the row and its member set, in ROW
/// space — the plan moves handles, never tuples.
struct State {
    exists: bool,
    parent: u64,
    payload: Dict,
    members: BTreeSet<HandleKey>,
    /// The stored node members, in handle space (ADR-0068).
    nodes: BTreeSet<Handle>,
}

fn read_cluster<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<State, String> {
    let mut state = State {
        exists: false,
        parent: 0,
        payload: Dict::new(),
        members: BTreeSet::new(),
        nodes: BTreeSet::new(),
    };
    if let Some(raw) = store
        .get(&cluster_key(ns, cluster.kind, cluster.id))
        .map_err(|e| e.to_string())?
    {
        state.exists = true;
        let (parent, body) = parse_head(&raw)?;
        state.parent = parent;
        if !body.is_empty() {
            state.payload = decode_dict(store, ns, body)?;
        }
    }
    state.members = member_keys(store, ns, cluster)?.into_iter().collect();
    state.nodes = node_member_keys(store, ns, cluster)?.into_iter().collect();
    Ok(state)
}

/// The next cluster id no row holds, for one kind: the highest `hyper_id` under
/// `[ns][0x0A][kind]` plus one. Ids start at **1**: `0` is the head's reserved
/// "no parent" value, so a cluster that could never be a parent would be the
/// one thing the numbering must not hand out.
fn next_free_cluster<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    kind: Kind,
) -> Result<u64, String> {
    let mut next = 1u64;
    for (suffix, _) in
        scan_suffix_kv(store, &clusters_prefix(ns, kind)).map_err(|e| e.to_string())?
    {
        if let Some(id) = be_u64(&suffix) {
            next = next.max(id.saturating_add(1));
        }
    }
    Ok(next)
}

impl<S: VirtualStorage> Graph<S> {
    /// Intern a kind label (0x0A/0x0B) and hand back its two-byte id — a WRITE,
    /// which is why it takes `&mut`: the vocabulary belongs to the collection,
    /// not to a batch.
    pub fn kind_id(&mut self, label: &str) -> Result<Kind, String> {
        let ns = self.ns_pair();
        intern_kind(self.store_ref(), &ns, label)
    }

    /// The label a kind was interned under; `None` for a number nobody minted.
    pub fn kind_label(&mut self, kind: Kind) -> Result<Option<String>, String> {
        let ns = self.ns_pair();
        interned_label(self.store_ref(), &ns, kind)
    }

    /// Every interned kind, id-ordered.
    pub fn kinds(&mut self) -> Result<Vec<(ClusterKindId, String)>, String> {
        let ns = self.ns_pair();
        interned_kinds(self.store_ref(), &ns)
    }

    /// One cluster row: its parent and payload. `None` = no such cluster.
    pub fn lookup(&self, kind: Kind, id: u64) -> Result<Option<ClusterRow>, String> {
        let ns = self.ns_pair();
        let Some(raw) = self
            .store_ref()
            .get(&cluster_key(&ns, kind, id))
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let (parent, body) = parse_head(&raw)?;
        let payload = if body.is_empty() {
            Dict::new()
        } else {
            decode_dict(self.store_ref(), &ns, body)?
        };
        Ok(Some(ClusterRow { parent, payload }))
    }

    /// Every cluster of one kind, id order: one scan of `[ns][0x0A][kind]`.
    /// No length test — the cluster row is the only row of that face.
    pub fn clusters(&self, kind: Kind) -> Result<Vec<u64>, String> {
        let ns = self.ns_pair();
        Ok(
            scan_suffix_kv(self.store_ref(), &clusters_prefix(&ns, kind))
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|(suffix, _)| be_u64(&suffix))
                .collect(),
        )
    }

    /// Every cluster of every kind, `(kind, id)` pairs in key order.
    pub fn all_clusters(&self) -> Result<Vec<ClusterRef>, String> {
        let ns = self.ns_pair();
        let prefix = face_prefix(&ns, CLUSTER_SLOT);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(self.store_ref(), &prefix).map_err(|e| e.to_string())? {
            if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
                continue;
            }
            if let (Some(kind), Some(id)) = (be_vocab(&suffix[..CLUSTER_KIND_WIDTH]), be_u64(&suffix[CLUSTER_KIND_WIDTH..]))
            {
                out.push(ClusterRef::new(Kind(kind), id));
            }
        }
        Ok(out)
    }

    /// The member edges of one cluster, in key order — the authority, resolved at
    /// the boundary: the member rows are handles, and this verb answers tuples
    /// (one registry scan for the whole set).
    pub fn edge_members(&self, kind: Kind, cluster: u64) -> Result<Vec<EdgeKey>, String> {
        let ns = self.ns_pair();
        let rows = member_keys(self.store_ref(), &ns, ClusterRef::new(kind, cluster))?;
        resolve_rows(self.store_ref(), &ns, rows)
    }

    /// The member NODES of one cluster, in handle order — the declared entity
    /// set (ADR-0068), resolved at the boundary in one registry batch. An entity
    /// no edge touches is a legal member here, which is the whole point.
    pub fn node_members(&self, kind: Kind, cluster: u64) -> Result<Vec<NodeRef>, String> {
        let ns = self.ns_pair();
        let handles = node_member_keys(self.store_ref(), &ns, ClusterRef::new(kind, cluster))?;
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        Handles::new(self.store_ref(), ns)
            .nodes(&handles)
            .map_err(|e| e.to_string())
    }

    /// The children of one cluster: one scan of `[ns][0x0C][kind][parent]`.
    pub fn children(&self, kind: Kind, parent: u64) -> Result<Vec<u64>, String> {
        let ns = self.ns_pair();
        Ok(
            scan_suffix_kv(self.store_ref(), &children_prefix(&ns, kind, parent))
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|(suffix, _)| be_u64(&suffix))
                .collect(),
        )
    }

    /// The parent of one cluster — one point read, because the head is in the
    /// row (the child index is the other direction only). `0` = no parent.
    pub fn parent(&self, kind: Kind, id: u64) -> Result<u64, String> {
        Ok(self.lookup(kind, id)?.map_or(0, |row| row.parent))
    }

    /// Is `edge` a member of cluster `(kind, cluster)`? One point read of the
    /// derived face `0x0D` — the read that makes a membership test a `.filter`
    /// predicate inside a traversal instead of a set built beforehand.
    ///
    /// Not a `bool`: a point read can fault, and a fault reported as "not a
    /// member" is how a broken store would read as an empty cluster. The
    /// traversal-filter spelling is
    /// `.filter(|step| graph.edge_member_of(kind, cluster, &step.key()).unwrap_or(false))`.
    pub fn edge_member_of(&self, kind: Kind, cluster: u64, edge: &EdgeKey) -> Result<bool, String> {
        let ns = self.ns_pair();
        let Some(row) = row_of_key(self.store_ref(), &ns, edge)? else {
            // An edge whose endpoints the graph never saw cannot be a member:
            // no row names it, so no derived row can either.
            return Ok(false);
        };
        Ok(self
            .store_ref()
            .get(&edge_cluster_key(&ns, &row, ClusterRef::new(kind, cluster)))
            .map_err(|e| e.to_string())?
            .is_some())
    }

    /// The member count the cluster row states (0 when the name is absent).
    pub fn edge_count(&self, kind: Kind, id: u64) -> Result<u64, String> {
        Ok(self
            .lookup(kind, id)?
            .map(|row| count_of(&row.payload, MEMBER_COUNT))
            .unwrap_or(0))
    }

    /// Is `node` a DECLARED member of this cluster? One point read of the node
    /// member face — the answer for an entity with no edge at all (ADR-0068).
    pub fn node_member_of(&self, kind: Kind, cluster: u64, node: &NodeRef) -> Result<bool, String> {
        let ns = self.ns_pair();
        let Some(handle) = Handles::new(self.store_ref(), ns)
            .handle_of(node)
            .map_err(|e| e.to_string())?
        else {
            // A node the registry never saw cannot be a member: no row names it.
            return Ok(false);
        };
        Ok(self
            .store_ref()
            .get(&node_member_key(&ns, ClusterRef::new(kind, cluster), handle))
            .map_err(|e| e.to_string())?
            .is_some())
    }

    /// The node member count the cluster row states (0 when the name is absent).
    pub fn node_count(&self, kind: Kind, id: u64) -> Result<u64, String> {
        Ok(self
            .lookup(kind, id)?
            .map(|row| count_of(&row.payload, NODE_COUNT))
            .unwrap_or(0))
    }

    /// Every cluster one edge is a member of, in `(kind, id)` key order — one
    /// scan of the DERIVED face `0x0D`, the edge-first mirror of
    /// [`members`](Self::members).
    ///
    /// This is the reverse question `members` answers, and it reads the mirror
    /// rather than the authority: the authority (edge member face) is keyed
    /// `[kind][cluster][edge]`, so an edge's clusters can only be found by
    /// scanning the whole face and matching a suffix. The write path maintains
    /// both faces in one commit, so the mirror is exact except across a
    /// [`rebuild_derived`](Self::rebuild_derived) window — the operator contract
    /// every derived read already carries.
    ///
    /// The NODE form is [`node_sets_of`](Self::node_sets_of): on this form a node
    /// has two ways into a cluster, and the two verbs keep them apart because
    /// they are two different facts.
    pub fn edge_sets_of(&self, edge: &EdgeKey) -> Result<Vec<EdgeSet>, String> {
        let ns = self.ns_pair();
        // The edge's row is the face's leading segment; an edge whose endpoints
        // never appeared has no run at all.
        let Some(row) = row_of_key(self.store_ref(), &ns, edge)? else {
            return Ok(Vec::new());
        };
        Ok(
            scan_suffix_kv(self.store_ref(), &edge_clusters_prefix(&ns, &row))
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|(suffix, _)| {
                    // `[kind][hyper_id]` — anything else is a foreign entry.
                    if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
                        return None;
                    }
                    let kind = be_vocab(&suffix[..CLUSTER_KIND_WIDTH])?;
                    be_u64(&suffix[CLUSTER_KIND_WIDTH..]).map(|id| EdgeSet::new(Kind(kind), id))
                })
                .collect(),
        )
    }

    /// Every cluster one NODE is a DECLARED member of, in `(kind, id)` key
    /// order — one scan of the node → cluster face (ADR-0068).
    ///
    /// This is the declared half only. An entity also reaches clusters through
    /// its incident edges, and that half is asked edge by edge with
    /// [`edge_sets_of`](Self::edge_sets_of): the two are separate questions on
    /// both forms, because they are separate facts.
    pub fn node_sets_of(&self, node: &NodeRef) -> Result<Vec<NodeSet>, String> {
        let ns = self.ns_pair();
        let Some(handle) = Handles::new(self.store_ref(), ns)
            .handle_of(node)
            .map_err(|e| e.to_string())?
        else {
            // A node the registry never saw is a member of nothing.
            return Ok(Vec::new());
        };
        Ok(
            scan_suffix_kv(self.store_ref(), &node_clusters_prefix(&ns, handle))
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|(suffix, _)| {
                    if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
                        return None;
                    }
                    let kind = be_vocab(&suffix[..CLUSTER_KIND_WIDTH])?;
                    be_u64(&suffix[CLUSTER_KIND_WIDTH..]).map(|id| NodeSet::new(Kind(kind), id))
                })
                .collect(),
        )
    }

    /// Rebuild the DERIVED faces (`0x0C`, `0x0D`) from the rows that carry the
    /// truth, and answer what was written.
    ///
    /// Sweep first, derive second — an entry's key encodes the value it was
    /// derived from, so a member that moved answers at a key no row produces
    /// any more, and re-deriving writes the new key without naming the old one.
    /// `chunk` bounds the ops per commit; a reader running DURING the rebuild
    /// sees a half-built face (operator window: do not read while it runs).
    ///
    /// The two faces are rebuilt from their OWN sources: the child index from
    /// `parent_id` in the cluster rows, the edge → cluster index from the member
    /// rows (`0x0B`) — never from each other, and never from the face being
    /// rebuilt. That is what makes `0x0D` rebuildable from `0x0B` ALONE, and it
    /// is the property the write path is checked against: the same input rows
    /// answer the same bytes either way.
    ///
    /// A fourth phase restates the handle registry's reverse half (`0x11`) from
    /// its forward half (`0x10`) — the same sweep-and-derive rule, for the one
    /// face an edge row depends on without naming it (ADR-0067).
    pub fn rebuild_derived(&self, chunk: usize) -> Result<DerivedReport, String> {
        let ns = self.ns_pair();
        let store = self.store_ref();
        let chunk = chunk.max(1);
        let mut report = DerivedReport::default();
        let mut batch = Batch::default();

        // Phase 1 — sweep both derived faces. One sweep per face because a key
        // from one face is not a key of the other; the loop keeps a half-built
        // store bounded by `chunk` either way.
        for slot in [CHILD_INDEX_SLOT, EDGE_CLUSTER_SLOT, NODE_CLUSTER_SLOT] {
            let prefix = face_prefix(&ns, slot);
            let end = prefix_end(&prefix);
            for item in store.scan_range(&prefix, end.as_deref()) {
                let (key, _) = item.map_err(|e| e.to_string())?;
                batch.del(&key);
                if batch.ops.len() >= chunk {
                    store
                        .commit_batch(std::mem::take(&mut batch))
                        .map_err(|e| e.to_string())?;
                    report.chunks += 1;
                }
            }
        }
        if !batch.ops.is_empty() {
            store
                .commit_batch(std::mem::take(&mut batch))
                .map_err(|e| e.to_string())?;
            report.chunks += 1;
        }

        // Phase 2 — the child index, from `parent_id` in the cluster rows.
        let prefix = face_prefix(&ns, CLUSTER_SLOT);
        let end = prefix_end(&prefix);
        for item in store.scan_range(&prefix, end.as_deref()) {
            let (key, value) = item.map_err(|e| e.to_string())?;
            let suffix = &key[prefix.len()..];
            if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
                continue;
            }
            let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                continue;
            };
            let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..]) else {
                continue;
            };
            let kind = Kind(kind);
            let (parent, _) = parse_head(&value)?;
            if parent != 0 {
                batch.put(child_key(&ns, kind, parent, id), Vec::new());
            }
            report.clusters += 1;
            if batch.ops.len() >= chunk {
                store
                    .commit_batch(std::mem::take(&mut batch))
                    .map_err(|e| e.to_string())?;
                report.chunks += 1;
            }
        }

        // Phase 3 — the edge → cluster index, from the MEMBER rows: `0x0B` is
        // `[kind][cluster][edge row]`, so each row states one `(row, kind,
        // cluster)` triple and the derived key is that triple in the face's own
        // order. Nothing here reads the face being written, which is what makes
        // this rebuild possible with `0x0D` wiped — and nothing is resolved:
        // both faces speak the same eighteen bytes.
        let prefix = face_prefix(&ns, EDGE_MEMBER_SLOT);
        let end = prefix_end(&prefix);
        for item in store.scan_range(&prefix, end.as_deref()) {
            let (key, _) = item.map_err(|e| e.to_string())?;
            let suffix = &key[prefix.len()..];
            // `[kind][hyper_id]` then the fixed-width row identity.
            if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH + HandleKey::WIDTH {
                continue;
            }
            let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                continue;
            };
            let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..CLUSTER_KIND_WIDTH + U64_WIDTH]) else {
                continue;
            };
            let Some(row) = HandleKey::parse(&suffix[CLUSTER_KIND_WIDTH + U64_WIDTH..]) else {
                continue;
            };
            let cluster = ClusterRef::new(Kind(kind), id);
            batch.put(edge_cluster_key(&ns, &row, cluster), Vec::new());
            report.members += 1;
            if batch.ops.len() >= chunk {
                store
                    .commit_batch(std::mem::take(&mut batch))
                    .map_err(|e| e.to_string())?;
                report.chunks += 1;
            }
        }

        // Phase 3b — the NODE → CLUSTER index, from the node MEMBER rows: the
        // node member face is `[kind][cluster][handle]`, so each row states one
        // `(handle, kind, cluster)` triple. The same sweep-and-derive rule as the
        // edge mirror, and nothing is resolved: a handle is the node's row
        // identity, so both node faces speak the same eight bytes.
        let prefix = face_prefix(&ns, NODE_MEMBER_SLOT);
        let end = prefix_end(&prefix);
        for item in store.scan_range(&prefix, end.as_deref()) {
            let (key, _) = item.map_err(|e| e.to_string())?;
            let suffix = &key[prefix.len()..];
            if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH + HANDLE_WIDTH {
                continue;
            }
            let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                continue;
            };
            let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..CLUSTER_KIND_WIDTH + U64_WIDTH])
            else {
                continue;
            };
            let Some(handle) = Handle::parse(&suffix[CLUSTER_KIND_WIDTH + U64_WIDTH..]) else {
                continue;
            };
            batch.put(
                node_cluster_key(&ns, handle, ClusterRef::new(Kind(kind), id)),
                Vec::new(),
            );
            report.node_members += 1;
            if batch.ops.len() >= chunk {
                store
                    .commit_batch(std::mem::take(&mut batch))
                    .map_err(|e| e.to_string())?;
                report.chunks += 1;
            }
        }
        if !batch.ops.is_empty() {
            store
                .commit_batch(std::mem::take(&mut batch))
                .map_err(|e| e.to_string())?;
            report.chunks += 1;
        }
        // Phase 4 — the handle registry's reverse half (0x11) from its forward
        // half (0x10), which is authority: it holds `[node_ns][pkey]`, and an
        // edge row names a handle rather than a reference (ADR-0067). A handle
        // no edge names any more is NOT reclaimed — that is compaction's
        // business, not a rebuild's — so the sweep-and-restate is the registry's
        // own verb rather than a second copy of it here.
        Handles::new(store, ns)
            .rebuild_reverse()
            .map_err(|e| e.to_string())?;
        report.chunks += 1;
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// The write fragment
// ---------------------------------------------------------------------------

/// One row a dynamic fragment declares — the same pair the static `Decl` has,
/// minus the node variant: on this form a node IS an endpoint, and a
/// `NodeRef` is self-describing, so there is nothing to declare.
#[derive(Clone, Debug)]
pub enum Decl {
    /// An edge this fragment asserts. The tuple IS the identity (ADR-0067): on a
    /// tuple no row holds it creates the edge, on one that is live it re-asserts
    /// that row, so this fragment's `on_edge_conflict` decides the attributes and
    /// a cluster declaration can adopt it as a member.
    Edge(DynEdge),
}

impl Decl {
    /// An edge declaration.
    pub fn edge(edge: DynEdge) -> Self {
        Decl::Edge(edge)
    }
}

/// One cluster write's intent, before the rows are derived from it.
#[derive(Default)]
struct Plan {
    /// The parent to write; `None` = keep whatever the row already says.
    parent: Option<u64>,
    payload: Dict,
    /// Whether the payload came from the caller (so a set's `on_*_set_conflict` runs on
    /// it), as opposed to being carried over from the stored row.
    from_caller: bool,
    /// Which spelling declared this row — `on_edge_set_conflict` and
    /// `on_node_set_conflict` are two registration points, so the entry carries
    /// the kind it was declared with. Only read when `from_caller`.
    node_set: bool,
    /// Whether a declaration asked for this cluster (as opposed to an op that
    /// only moves members).
    declared: bool,
    /// The cluster row is removed outright: an explicit delete, never a drop in
    /// membership.
    delete_row: bool,
    /// Membership is moved in ROW space (an edge's handle triple), because that
    /// is what the member and derived faces are keyed by.
    add: BTreeSet<HandleKey>,
    remove: BTreeSet<HandleKey>,
    /// The entities the caller DECLARES as members (ADR-0068), resolved to
    /// handles at write time because a node may not exist yet.
    add_nodes: BTreeSet<NodeRef>,
    remove_nodes: BTreeSet<NodeRef>,
}

/// One cluster write's outcome: the exact rows to put and delete.
struct Outcome {
    cluster: ClusterRef,
    delete_row: bool,
    parent: u64,
    old_parent: u64,
    existed: bool,
    payload: Dict,
    put_members: Vec<HandleKey>,
    del_members: Vec<HandleKey>,
    put_nodes: Vec<Handle>,
    del_nodes: Vec<Handle>,
}

/// A cluster operation that rides the same plan the declarations do.
enum ClusterOp {
    /// Drop this batch's edges from a cluster.
    RemoveEdges(ClusterRef),
    /// Drop these nodes from a cluster's DECLARED membership.
    RemoveNodes(ClusterRef, BTreeSet<NodeRef>),
    /// Remove a cluster outright: its row, its member rows and its derived rows.
    Delete(ClusterRef),
}

/// A conflict hook over the graph's edge payload — on this form the payload IS
/// a `Dict`, so the hook is uniform across edges. It takes `(stored, declared)`
/// and answers the attributes to write.
pub type EdgeConflict = Box<dyn Fn(&Dict, &Dict) -> Dict>;

/// A conflict hook over a cluster's payload; clusters are not declared types in
/// either form.
pub type ClusterConflict = Box<dyn Fn(&Dict, &Dict) -> Dict>;

/// The dynamic form's write fragment: the edge declarations, the clusters over
/// them and the two conflict hooks, all encoded into ONE engine batch by
/// [`Graph::incorporate`]; the delete declarations go the other way, through
/// [`Graph::retract`].
///
/// There are two hooks, not three: the static form's `on_node_conflict` resolves
/// a typed row the declaration names, and this form names no rows. A node here
/// is an endpoint of an edge — its document, if the store keeps one, is that
/// collection's business.
#[derive(Default)]
pub struct DynFragment {
    edges: Vec<Decl>,
    clusters: Vec<Cluster>,
    ops: Vec<ClusterOp>,
    edge_deletes: Vec<EdgeKey>,
    /// Assertions moved from one head to another: `(from, to)`. The stored
    /// attributes ride along, because on this form a declaration CARRIES its
    /// attributes — `DynEdge.attrs` — where the typed form needs a carrier.
    repoints: Vec<(DynEdge, DynEdge)>,
    node_deletes: Vec<NodeRef>,
    edge_conflict: Option<EdgeConflict>,
    edge_set_conflict: Option<ClusterConflict>,
    node_set_conflict: Option<ClusterConflict>,
}

impl DynFragment {
    /// The fragment as one value: its declarations are the only required part,
    /// and whatever a builder below attaches — a cluster, a conflict hook — is
    /// optional, so a fragment of declarations alone writes just its edges.
    pub fn new<I: IntoIterator<Item = Decl>>(decls: I) -> Self {
        let mut fragment = Self::default();
        fragment.edges.extend(decls);
        fragment
    }

    /// The cluster this fragment's edges belong to. `id: None` creates one and
    /// reports the id allocated for it; `Some(id)` incorporates into the
    /// cluster already there (and creates it there when no row holds that id).
    pub fn cluster(&mut self, cluster: Cluster) -> &mut Self {
        self.clusters.push(cluster);
        self
    }

    /// Resolve every edge this batch writes. One hook for every edge, because
    /// on this form every edge payload is the same shape — a `Dict` — and there
    /// is no declaration to tell them apart by.
    ///
    /// The two sides are the ones this form actually has: `stored` is the row's
    /// attributes as they stand (empty when the id is free) and `declared` is the
    /// declaration's own attributes. A re-assertion of a live id without a hook
    /// keeps the STORED ones — that is the whole update path, and the reason an
    /// update needs no verb of its own: the id, the faces and the membership rows
    /// stand, and only the frames change. A replacement of the head drops them
    /// (the old assertion goes with its attributes), so the declared ones land
    /// instead.
    ///
    /// The hook decides: `|_, declared| declared.clone()` overwrites,
    /// `|stored, _| stored.clone()` keeps what is there, and
    /// `|stored, declared| if declared["ts"] >= stored["ts"] { declared.clone() } else { stored.clone() }`
    /// is the last-writer-wins rule the old fold could not state.
    pub fn on_edge_conflict(&mut self, f: impl Fn(&Dict, &Dict) -> Dict + 'static) -> &mut Self {
        self.edge_conflict = Some(Box::new(f));
        self
    }

    /// Resolve every cluster payload this batch declares — `Dict` on both sides,
    /// because a cluster's payload is dynamic. `stored` is the row's map and
    /// `declared` is that map with this declaration's entries merged in; a
    /// cluster that does not exist yet is created by calling the hook with
    /// `(empty, declared)`.
    pub fn on_edge_set_conflict(
        &mut self,
        f: impl Fn(&Dict, &Dict) -> Dict + 'static,
    ) -> &mut Self {
        self.edge_set_conflict = Some(Box::new(f));
        self
    }

    /// The ENTITY half — the same two-payload shape, chosen by the spelling that
    /// declared the row.
    pub fn on_node_set_conflict(
        &mut self,
        f: impl Fn(&Dict, &Dict) -> Dict + 'static,
    ) -> &mut Self {
        self.node_set_conflict = Some(Box::new(f));
        self
    }

    /// Drop this batch's edges from `cluster`. The cluster row stays: a scope is
    /// not an alias for the edges inside it, so losing the last member leaves an
    /// empty cluster with an honest count of zero.
    pub fn remove_edges(&mut self, set: EdgeSet) -> &mut Self {
        self.ops.push(ClusterOp::RemoveEdges(set.into()));
        self
    }

    /// Retract a DECLARED membership: these entities leave `cluster`, and the
    /// entity itself stays in the graph untouched — the claim being withdrawn is
    /// about the group, not about the node. The declared half only: a member edge
    /// that still has the node as an endpoint keeps it in the union (ADR-0068),
    /// and a declaration in the same fragment wins over a removal.
    pub fn remove_nodes(
        &mut self,
        set: NodeSet,
        nodes: impl IntoIterator<Item = NodeRef>,
    ) -> &mut Self {
        self.ops
            .push(ClusterOp::RemoveNodes(set.into(), nodes.into_iter().collect()));
        self
    }

    /// Remove `cluster` outright: its row, its member rows and its derived
    /// rows, in this batch. Idempotent — a cluster that is not there has
    /// nothing to delete.
    pub fn delete_edges(&mut self, set: EdgeSet) -> &mut Self {
        self.ops.push(ClusterOp::Delete(set.into()));
        self
    }

    /// Remove an ENTITY group outright — the row and its declared members; the
    /// entities stay.
    pub fn delete_nodes(&mut self, set: NodeSet) -> &mut Self {
        self.ops.push(ClusterOp::Delete(set.into()));
        self
    }

    /// Delete one edge — and every membership row it owns — in the same batch
    /// as the rest of the fragment's deletes. Read by [`Graph::retract`].
    ///
    /// The clusters an edge belongs to are found by ONE pass over the member
    /// face (`0x0B` is the authority; `0x0D` is derived and may be stale, so it
    /// is not what a delete trusts). A tuple no row holds is nothing to do, not a
    /// refusal: the report says zero.
    ///
    /// It takes the identity, not a number, because there is no number
    /// (ADR-0067) — the same tuple the write used.
    pub fn delete_edge(&mut self, edge: EdgeKey) -> &mut Self {
        self.edge_deletes.push(edge);
        self
    }

    /// Assert `to` with the attributes the row at `from` holds, and retract `from`
    /// — one action, one batch.
    ///
    /// The dynamic form's counterpart of core's `repoint`, and smaller, because a
    /// declaration here carries its attributes (`DynEdge.attrs`) instead of
    /// needing a type to name them: the prepass reads the row being replaced and
    /// pushes it as a declaration at the new head, so the ordinary write path
    /// resolves it — a free target lands the moved map, a live one goes through
    /// `on_edge_conflict(stored, moved)` with the moved assertion as the declared
    /// side.
    ///
    /// Only [`Graph::replace`] takes one: `incorporate` counts it as a deletion
    /// and `retract` as a write, so each still refuses.
    pub fn repoint(&mut self, from: DynEdge, to: DynEdge) -> &mut Self {
        self.repoints.push((from, to));
        self
    }

    /// Delete one node AND every edge incident to it. A node here is a
    /// `NodeRef` — an ns and bare key bytes — so it is addressed exactly the way
    /// an edge's endpoint is; there is no type to name because the dynamic form
    /// has none.
    ///
    /// A node no edge names is not in the graph, so retracting it is nothing to
    /// do — the report says zero.
    pub fn delete_node(&mut self, node: NodeRef) -> &mut Self {
        self.node_deletes.push(node);
        self
    }

    /// A retract fragment removes, and only removes. Anything that declares
    /// edges, clusters or conflict hooks belongs to [`Graph::incorporate`] — a
    /// fragment carrying both would have two meanings in one batch.
    fn check_retractable(&self) -> Result<(), String> {
        let writes = self.edges.len()
            + self.repoints.len()
            + self.clusters.len()
            + usize::from(self.edge_conflict.is_some())
            + usize::from(self.edge_set_conflict.is_some())
            + usize::from(self.node_set_conflict.is_some())
            + self
                .ops
                .iter()
                .filter(|op| matches!(op, ClusterOp::RemoveEdges(_)))
                .count();
        if writes > 0 {
            return Err(format!(
                "retract removes; this fragment declares {writes} write(s) — that is incorporate's verb"
            ));
        }
        Ok(())
    }

    /// `incorporate` writes, and a fragment that declares deletions belongs to
    /// [`Graph::retract`] — two verbs with one meaning each.
    fn check_incorporable(&self) -> Result<(), String> {
        let deletions = self.edge_deletes.len() + self.node_deletes.len() + self.repoints.len();
        if deletions > 0 {
            return Err(format!(
                "incorporate writes; this fragment declares {deletions} deletion(s) — that is retract's verb"
            ));
        }
        Ok(())
    }

    /// Encode the whole write set into a caller-owned batch: the edges (with
    /// their dynamic segments), the cluster rows and both derived faces, in one
    /// op list. Nothing lands until the caller commits it.
    fn write_into<S: VirtualStorage>(
        mut self,
        graph: &mut Graph<S>,
        batch: &mut Batch,
        mut plan: BTreeMap<(ClusterKindId, u64), Plan>,
    ) -> Result<WriteReport, String> {
        let ns = graph.ns_pair();
        let mut report = WriteReport::default();

        // ---- 1. The edges: the tuple is the identity, then every face ----
        //
        // The declaration names endpoints and a predicate, and that tuple IS the
        // identity (ADR-0067). So the same declaration that creates an edge
        // re-asserts it: a row at that tuple means this fragment's
        // `on_edge_conflict` decides the attributes (the update path), and no row
        // means the declaration creates it.
        // ---- 1a. The repoints: carry the stored attributes to the new head ----
        //
        // Read BEFORE anything is written, and stated as an ordinary declaration
        // at the target, so the loop below resolves it exactly like any other
        // assertion: free target takes the moved map, live target merges through
        // the hook. The source joins `edge_deletes` and goes through the same
        // retraction the caller's own deletes do.
        for (from, to) in std::mem::take(&mut self.repoints) {
            let from_key = graph.key_of(&from)?;
            let Some(stored) = graph.get_edge(&from_key)? else {
                // No row: nothing to move. Absence is nothing to do.
                continue;
            };
            self.edges.push(Decl::Edge(DynEdge {
                attrs: stored.attrs,
                ..to
            }));
            self.edge_deletes.push(from_key);
        }

        let edges = std::mem::take(&mut self.edges);
        let mut batch_rows: Vec<HandleKey> = Vec::with_capacity(edges.len());
        for decl in &edges {
            let Decl::Edge(edge) = decl;
            // The vocabulary id is part of the key, so the predicate is interned
            // before the existence probe — the write would intern it anyway.
            let key = graph.key_of(edge)?;
            // The stored attributes and the declared ones go to the hook, whose
            // answer is what lands; with no hook the stored map stays, which is
            // the update path. A creation has no stored row, so the old side is
            // the empty map.
            let attrs = match graph.get_edge(&key)? {
                Some(stored) => match self.edge_conflict.as_ref() {
                    Some(resolve) => resolve(&stored.attrs, &edge.attrs),
                    None => stored.attrs,
                },
                None => {
                    let declared = edge.attrs.clone();
                    match self.edge_conflict.as_ref() {
                        Some(resolve) => resolve(&Dict::new(), &declared),
                        None => declared,
                    }
                }
            };
            let written = DynEdge {
                attrs,
                ..edge.clone()
            };
            // The rows are keyed by handles, so the write answers the ROW it
            // keyed: the membership plan moves those, never tuples.
            batch_rows.push(graph.write_row_into(batch, &written)?);
            report.edges.push(key);
        }

        // ---- 2. Cluster ids: allocated at run time, per kind ----
        // The declaration carries the caller's LABEL; this is where it becomes an
        // id, in the same batch that writes the row (ADR-0069's rule for a
        // vocabulary, the one an edge's predicate already follows).
        let mut declared_kinds: Vec<Kind> = Vec::with_capacity(self.clusters.len());
        for cluster in &self.clusters {
            declared_kinds.push(intern_kind(graph.store_ref(), &ns, &cluster.kind)?);
        }
        let mut kinds: BTreeSet<ClusterKindId> = BTreeSet::new();
        for kind in &declared_kinds {
            kinds.insert(kind.0);
        }
        for op in &self.ops {
            match op {
                ClusterOp::RemoveEdges(cluster)
                | ClusterOp::RemoveNodes(cluster, _)
                | ClusterOp::Delete(cluster) => {
                    kinds.insert(cluster.kind.0);
                }
            }
        }
        let mut next_id: BTreeMap<ClusterKindId, u64> = BTreeMap::new();
        for kind in kinds {
            next_id.insert(kind, next_free_cluster(graph.store_ref(), &ns, Kind(kind))?);
        }
        // An explicit id reserves the range above it, so a `None` declared
        // BEFORE it cannot take a number it has already claimed — and `0` is
        // reserved for "no parent", so it is nobody's id.
        for (cluster, kind) in self.clusters.iter().zip(&declared_kinds) {
            match cluster.id {
                Some(0) => {
                    return Err("hyper_id 0 is reserved: it is the head's \"no parent\"".into());
                }
                Some(id) => {
                    let slot = next_id.entry(kind.0).or_insert(1);
                    *slot = (*slot).max(id.saturating_add(1));
                }
                None => {}
            }
        }
        let mut assigned: Vec<u64> = Vec::with_capacity(self.clusters.len());
        for (cluster, kind) in self.clusters.iter().zip(&declared_kinds) {
            let slot = next_id.entry(kind.0).or_insert(1);
            match cluster.id {
                Some(id) => assigned.push(id),
                None => {
                    let id = *slot;
                    *slot = slot.saturating_add(1);
                    assigned.push(id);
                    if cluster.edges == MemberEdges::Fragment {
                        report.edge_sets.push(EdgeSet::new(*kind, id));
                    } else {
                        report.node_sets.push(NodeSet::new(*kind, id));
                    }
                }
            }
        }

        // ---- 3. The plan: one record per cluster this batch touches ----
        //
        // The plan ARRIVES seeded: `replace` hands the retract half's entries in,
        // so a cluster both halves touch is read and counted once. `incorporate`
        // hands an empty one.
        for ((cluster, kind), id) in self.clusters.iter().zip(&declared_kinds).zip(&assigned) {
            let entry = plan.entry((kind.0, *id)).or_default();
            entry.declared = true;
            entry.parent = Some(cluster.parent);
            entry.from_caller = true;
            entry.node_set = cluster.edges == MemberEdges::None;
            for (name, value) in cluster.payload.iter() {
                entry.payload.insert(name.clone(), value.clone());
            }
            if cluster.edges == MemberEdges::Fragment {
                entry.add.extend(batch_rows.iter().cloned());
            }
            entry.add_nodes.extend(cluster.nodes.iter().cloned());
        }
        for op in &self.ops {
            match op {
                ClusterOp::RemoveEdges(cluster) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove
                        .extend(batch_rows.iter().cloned());
                }
                ClusterOp::RemoveNodes(cluster, nodes) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove_nodes
                        .extend(nodes.iter().cloned());
                }
                ClusterOp::Delete(cluster) => {
                    // The row goes, and it is DECLARED: a delete names the
                    // cluster it wants gone, so a missing one is not an error.
                    let entry = plan.entry((cluster.kind.0, cluster.id)).or_default();
                    entry.delete_row = true;
                    entry.declared = true;
                }
            }
        }

        // ---- 4. The rows: outcomes, then the faces, one batch ----
        self.apply_plan(graph, &ns, plan, batch)?;
        Ok(report)
    }

    /// Steps 4 and 5 of a batch, for BOTH verbs: read every planned cluster's
    /// state, derive the rows it ends up with, then write the cluster faces into
    /// `batch`. Answers how many membership rows it deleted — the number
    /// `retract` reports.
    fn apply_plan<S: VirtualStorage>(
        &mut self,
        graph: &Graph<S>,
        ns: &[u8; 2],
        plan: BTreeMap<(ClusterKindId, u64), Plan>,
        batch: &mut Batch,
    ) -> Result<usize, String> {
        // A declared entity needs its handle before a row can be keyed, and a
        // cluster membership is the one place a node may appear before it is an
        // endpoint (ADR-0068 §4) — so the handle is issued here on first sight.
        let handles = Handles::new(graph.store_ref(), *ns);
        let mut node_handle: BTreeMap<(u16, Vec<u8>), Handle> = BTreeMap::new();
        for entry in plan.values() {
            for node in entry.add_nodes.iter() {
                let slot = (node.ns_id(), node.pkey.clone());
                if node_handle.contains_key(&slot) {
                    continue;
                }
                let handle = handles.issue_into(batch, node).map_err(|e| e.to_string())?;
                node_handle.insert(slot, handle);
            }
            for node in entry.remove_nodes.iter() {
                let slot = (node.ns_id(), node.pkey.clone());
                if node_handle.contains_key(&slot) {
                    continue;
                }
                if let Some(handle) = handles.handle_of(node).map_err(|e| e.to_string())? {
                    node_handle.insert(slot, handle);
                }
            }
        }
        let mut deleted_members = 0usize;
        let mut outcomes: Vec<Outcome> = Vec::with_capacity(plan.len());
        for ((kind, id), entry) in plan {
            let cluster = ClusterRef::new(Kind(kind), id);
            let state = read_cluster(graph.store_ref(), ns, cluster)?;
            if !state.exists && !entry.declared {
                // An op named a cluster that is not there and this batch does
                // not create it: there is nothing to move members in or out of.
                return Err(format!(
                    "cluster (kind {kind}, hyper_id {id}) is not there, and this batch does not create it"
                ));
            }
            // `add` wins over `remove` for the same edge: this write's claim is
            // the later one, and it is what makes a fragment that both adopts an
            // edge into a cluster and removes it from that same cluster a no-op
            // instead of a self-deletion.
            let mut members = state.members.clone();
            for gone in &entry.remove {
                members.remove(gone);
            }
            members.extend(entry.add.iter().cloned());
            // A cluster is a scope: it is created when declared and deleted only
            // when explicitly deleted. No membership count — not even zero —
            // takes the row away, so an empty or one-member cluster is a legal
            // answer rather than a dropped one.
            let delete_row = entry.delete_row;

            let mut payload = state.payload.clone();
            if !delete_row {
                // The two sides the hook compares: the stored map, and that map
                // with this declaration's entries merged in. Without a hook the
                // merged map lands, which is the plain write.
                let mut declared = state.payload.clone();
                for (name, value) in entry.payload.iter() {
                    declared.insert(name.clone(), value.clone());
                }
                let hook = if entry.node_set {
                    self.node_set_conflict.as_ref()
                } else {
                    self.edge_set_conflict.as_ref()
                };
                payload = match hook.filter(|_| entry.from_caller) {
                    Some(resolve) => resolve(&state.payload, &declared),
                    None => declared,
                };
            }

            let (put_members, del_members): (Vec<HandleKey>, Vec<HandleKey>) = if delete_row {
                let all: BTreeSet<HandleKey> = state.members.union(&entry.add).cloned().collect();
                (Vec::new(), all.into_iter().collect())
            } else {
                (
                    members.difference(&state.members).cloned().collect(),
                    state.members.difference(&members).cloned().collect(),
                )
            };

            // The same rule for the entity set: a declared member wins over a removal,
            // and the delta is what lands. Handles are already the row space.
            let mut node_members = state.nodes.clone();
            for gone in &entry.remove_nodes {
                if let Some(handle) = node_handle.get(&(gone.ns_id(), gone.pkey.clone())) {
                    node_members.remove(handle);
                }
            }
            for added in &entry.add_nodes {
                if let Some(handle) = node_handle.get(&(added.ns_id(), added.pkey.clone())) {
                    node_members.insert(*handle);
                }
            }
            let (put_nodes, del_nodes): (Vec<Handle>, Vec<Handle>) = if delete_row {
                (Vec::new(), state.nodes.iter().copied().collect())
            } else {
                (
                    node_members.difference(&state.nodes).copied().collect(),
                    state.nodes.difference(&node_members).copied().collect(),
                )
            };

            // The counts are stamped LAST, so an `on_*_set_conflict` that
            // writes the same name cannot make the row disagree with its rows.
            if !delete_row {
                payload.insert(MEMBER_COUNT.to_string(), Variant::U64(members.len() as u64));
                payload.insert(NODE_COUNT.to_string(), Variant::U64(node_members.len() as u64));
            }
            let parent = if delete_row {
                0
            } else {
                entry.parent.unwrap_or(state.parent)
            };
            deleted_members += del_members.len() + del_nodes.len();
            outcomes.push(Outcome {
                cluster,
                delete_row,
                parent,
                old_parent: state.parent,
                existed: state.exists,
                payload: if delete_row { Dict::new() } else { payload },
                put_members,
                del_members,
                put_nodes,
                del_nodes,
            });
        }

        // ---- 5. Write: the cluster faces, one batch ----
        //
        // A batch-shaped store, so the payload's field-name vocabulary commits
        // with the row instead of ahead of it: a refused fragment leaves no
        // vocabulary entry either.
        let adapter = BatchStore::new(graph.store_ref(), batch);
        for outcome in &outcomes {
            let cluster = outcome.cluster;
            // The child index follows the head: it is derived from `parent_id`,
            // so a moved cluster takes its own row with it.
            if outcome.old_parent != 0
                && (outcome.delete_row || outcome.old_parent != outcome.parent)
            {
                adapter
                    .del(&child_key(ns, cluster.kind, outcome.old_parent, cluster.id))
                    .map_err(|e| e.to_string())?;
            }
            if !outcome.delete_row
                && outcome.parent != 0
                && (!outcome.existed || outcome.old_parent != outcome.parent)
            {
                adapter
                    .put(
                        child_key(ns, cluster.kind, outcome.parent, cluster.id),
                        Vec::new(),
                    )
                    .map_err(|e| e.to_string())?;
            }
            for edge in &outcome.del_members {
                adapter
                    .del(&edge_member_key(ns, cluster, edge))
                    .map_err(|e| e.to_string())?;
                // The reverse direction is the SAME fact, so it leaves in the
                // same breath: a membership row and its edge-first mirror are
                // never allowed to disagree for longer than one commit.
                adapter
                    .del(&edge_cluster_key(ns, edge, cluster))
                    .map_err(|e| e.to_string())?;
            }
            for handle in &outcome.del_nodes {
                adapter
                    .del(&node_member_key(ns, cluster, *handle))
                    .map_err(|e| e.to_string())?;
                adapter
                    .del(&node_cluster_key(ns, *handle, cluster))
                    .map_err(|e| e.to_string())?;
            }
            if outcome.delete_row {
                adapter
                    .del(&cluster_key(ns, cluster.kind, cluster.id))
                    .map_err(|e| e.to_string())?;
                continue;
            }
            for edge in &outcome.put_members {
                adapter
                    .put(edge_member_key(ns, cluster, edge), Vec::new())
                    .map_err(|e| e.to_string())?;
                adapter
                    .put(edge_cluster_key(ns, edge, cluster), Vec::new())
                    .map_err(|e| e.to_string())?;
            }
            for handle in &outcome.put_nodes {
                adapter
                    .put(node_member_key(ns, cluster, *handle), Vec::new())
                    .map_err(|e| e.to_string())?;
                adapter
                    .put(node_cluster_key(ns, *handle, cluster), Vec::new())
                    .map_err(|e| e.to_string())?;
            }
            let body = encode_dict(&adapter, ns, &outcome.payload)?;
            adapter
                .put(
                    cluster_key(ns, cluster.kind, cluster.id),
                    cluster_value(outcome.parent, &body),
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(deleted_members)
    }
}

impl<S: VirtualStorage> Graph<S> {
    /// The write verb (ADR-0066 §7, dynamic form): consume `fragment` and land
    /// its declared edges, the dynamic segments they carry, its clusters and its
    /// two conflict hooks as ONE engine commit. A refused fragment writes nothing
    /// — every check runs before the commit — and the report names the ids it
    /// allocated.
    ///
    /// There is no update verb: an update is this verb with a conflict hook.
    /// Naming an edge at its own id ([`Decl::edge_at`], or `.at(..)` on a `Cluster::edges` declaration
    /// existing cluster) hands `on_edge_conflict` / `on_edge_set_conflict` / `on_node_set_conflict` that
    /// row's stored payload beside the declared one, so the payload changes while
    /// the id, its faces and its membership rows stand.
    pub fn incorporate(&mut self, fragment: DynFragment) -> Result<WriteReport, String> {
        fragment.check_incorporable()?;
        let mut batch = Batch::default();
        let report = fragment.write_into(self, &mut batch, BTreeMap::new())?;
        self.commit(batch)?;
        Ok(report)
    }

    /// The delete half of BOTH verbs: the edges the fragment dooms, the nodes it
    /// retires, and the cluster plan those removals imply.
    ///
    /// Split out so `replace` can plan the removal and the write against ONE
    /// plan: a cluster both halves touch is then read and counted once, instead
    /// of twice from a base that ignores the other half.
    fn retract_plan(&mut self, fragment: &DynFragment) -> Result<Removal, String> {
        let ns = self.ns_pair();
        // ---- 1. The doomed edges: a node's cascade first, then the declared
        //         identities, deduplicated — one edge is one removal whatever
        //         named it. The set lives in ROW space: the faces are keyed by
        //         handles, so a cascade reads them and resolves nothing.
        let mut doomed: BTreeSet<HandleKey> = BTreeSet::new();
        let mut nodes: BTreeSet<NodeRef> = BTreeSet::new();
        // Opened here because a node's cascade states two things: the edges to
        // drop, and the node's own memberships (ADR-0068).
        let mut plan: BTreeMap<(ClusterKindId, u64), Plan> = BTreeMap::new();
        for node in &fragment.node_deletes {
            let Some(near) = Handles::new(self.store_ref(), ns)
                .handle_of(node)
                .map_err(|e| e.to_string())?
            else {
                // No handle: neither an endpoint of any edge nor a declared
                // member, so nothing in the graph names it.
                continue;
            };
            let mut incident: Vec<HandleKey> = Vec::new();
            for (suffix, _) in scan_suffix_kv(self.store_ref(), &out_prefix(&ns, near))
                .map_err(|e| e.to_string())?
            {
                if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                    incident.push(HandleKey::new(near, predicate, far));
                }
            }
            for (suffix, _) in scan_suffix_kv(self.store_ref(), &in_prefix(&ns, near))
                .map_err(|e| e.to_string())?
            {
                if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                    incident.push(HandleKey::new(far, predicate, near));
                }
            }
            // The entity's own memberships: one prefix scan of the node →
            // cluster face, the precise question.
            let mut held: Vec<ClusterRef> = Vec::new();
            for (suffix, _) in scan_suffix_kv(self.store_ref(), &node_clusters_prefix(&ns, near))
                .map_err(|e| e.to_string())?
            {
                if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
                    continue;
                }
                let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                    continue;
                };
                let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..]) else {
                    continue;
                };
                held.push(ClusterRef::new(Kind(kind), id));
            }
            if incident.is_empty() && held.is_empty() {
                continue;
            }
            nodes.insert(node.clone());
            doomed.extend(incident);
            for cluster in held {
                plan.entry((cluster.kind.0, cluster.id))
                    .or_default()
                    .remove_nodes
                    .insert(node.clone());
            }
        }
        for edge in &fragment.edge_deletes {
            // A tuple naming an endpoint the graph never saw names no row:
            // absence is nothing to do, not a refusal.
            if let Some(row) = row_of_key(self.store_ref(), &ns, edge)? {
                doomed.insert(row);
            }
        }

        // ---- 2. Membership rows follow the edges: ONE pass over 0x0B ----
        //
        // A member row is `[kind][cluster][edge row]`, so an edge's clusters are
        // found by SUFFIX. The authority is the face that states membership;
        // `0x0D` is derived and may be stale, so a delete never consults it.
        // Both sides are rows, so the containment test is a byte comparison.
        if !doomed.is_empty() {
            let prefix = face_prefix(&ns, EDGE_MEMBER_SLOT);
            for (suffix, _) in
                scan_suffix_kv(self.store_ref(), &prefix).map_err(|e| e.to_string())?
            {
                if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH + HandleKey::WIDTH {
                    continue;
                }
                let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                    continue;
                };
                let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..CLUSTER_KIND_WIDTH + U64_WIDTH])
                else {
                    continue;
                };
                let Some(row) = HandleKey::parse(&suffix[CLUSTER_KIND_WIDTH + U64_WIDTH..]) else {
                    continue;
                };
                if doomed.contains(&row) {
                    plan.entry((kind, id)).or_default().remove.insert(row);
                }
            }
        }
        // A cluster `delete` in the fragment rides the same plan; `remove` is
        // already satisfied — every doomed edge is dropped from every cluster
        // that holds it — and anything else was refused above.
        for op in &fragment.ops {
            if let ClusterOp::Delete(cluster) = op {
                let entry = plan.entry((cluster.kind.0, cluster.id)).or_default();
                entry.delete_row = true;
                entry.declared = true;
            }
        }

        Ok((doomed, nodes, plan))
    }

    /// One group of entities as a subgraph: its members, the edges with BOTH ends
    /// inside, and the edges that cross the cut.
    ///
    /// The dynamic form's counterpart of core's `subgraph`, answering tuples
    /// instead of heads because a tuple is what this form's verbs take. A
    /// replacement is built from this and nothing else: `internal` is what dies
    /// with the selection, `external` is what has to be re-attached, and `nodes`
    /// says which entities the new shape may name.
    ///
    /// An internal edge is seen twice (once from each end), so it is deduped by
    /// its tuple; a crossing is seen once, from the member's side, and its
    /// `inside` is the end a rewrite replaces — the direction never has to be
    /// asked for.
    pub fn subgraph(&self, set: NodeSet) -> Result<Subgraph, String> {
        let members = self.node_members(set.kind, set.id)?;
        let inside: BTreeSet<([u8; 2], Vec<u8>)> = members
            .iter()
            .map(|node| (node.ns, node.pkey.clone()))
            .collect();
        let mut seen: BTreeSet<EdgeIdentity> = BTreeSet::new();
        let mut internal = Vec::new();
        let mut external = Vec::new();
        for member in &members {
            let outs = self.out_edges(member)?;
            let ins = self.in_edges(member)?;
            for (key, leaves) in outs
                .into_iter()
                .map(|key| (key, true))
                .chain(ins.into_iter().map(|key| (key, false)))
            {
                let far = if leaves { key.dst.clone() } else { key.src.clone() };
                if inside.contains(&(far.ns, far.pkey.clone())) {
                    let row = (
                        key.src.ns_id(),
                        key.src.pkey.clone(),
                        key.predicate,
                        key.dst.ns_id(),
                        key.dst.pkey.clone(),
                    );
                    if seen.insert(row) {
                        internal.push(key);
                    }
                    continue;
                }
                external.push(Crossing {
                    key,
                    inside: member.clone(),
                    outside: far,
                });
            }
        }
        Ok(Subgraph {
            nodes: members,
            internal,
            external,
        })
    }

    /// Replace a shape: ONE fragment may retract and declare, and the batch lands
    /// as a single state change.
    ///
    /// The dynamic form's counterpart of core's `replace`, with the same three
    /// rules: the two halves are planned together (retract's removals seed the
    /// write half's cluster plan), a moved assertion carries its attributes
    /// through [`DynFragment::repoint`], and a replacement that retires an entity
    /// must STATE every edge incident to it — repoint or delete — or the batch is
    /// refused.
    pub fn replace(&mut self, fragment: DynFragment) -> Result<ReplaceReport, String> {
        self.check_cut_is_stated(&fragment)?;
        let mut batch = Batch::default();
        let (doomed, nodes, plan) = self.retract_plan(&fragment)?;
        let mut edges = 0;
        for row in &doomed {
            if !self.has_row(row)? {
                continue;
            }
            self.purge_row_into(&mut batch, row);
            edges += 1;
        }
        let written = fragment.write_into(self, &mut batch, plan)?;
        self.commit(batch)?;
        Ok(ReplaceReport {
            nodes: nodes.len(),
            edges,
            written,
        })
    }

    /// Every half-edge a retired entity would cut has to be STATED in the
    /// fragment: repointed or deleted.
    ///
    /// The cascade is the right answer for a deletion ("the assertions are gone
    /// anyway") and the wrong answer for a replacement, whose claim is "this
    /// shape becomes that one" — a connection nobody decided about is exactly the
    /// loss it exists to prevent.
    fn check_cut_is_stated(&mut self, fragment: &DynFragment) -> Result<(), String> {
        if fragment.node_deletes.is_empty() {
            return Ok(());
        }
        let ns = self.ns_pair();
        let mut stated: BTreeSet<HandleKey> = BTreeSet::new();
        for edge in &fragment.edge_deletes {
            if let Some(row) = row_of_key(self.store_ref(), &ns, edge)? {
                stated.insert(row);
            }
        }
        for (from, _) in &fragment.repoints {
            let key = self.key_of(from)?;
            if let Some(row) = row_of_key(self.store_ref(), &ns, &key)? {
                stated.insert(row);
            }
        }
        let mut unaccounted = 0usize;
        for node in &fragment.node_deletes {
            let Some(near) = Handles::new(self.store_ref(), ns)
                .handle_of(node)
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            for (suffix, _) in scan_suffix_kv(self.store_ref(), &out_prefix(&ns, near))
                .map_err(|e| e.to_string())?
            {
                if let Some((predicate, far)) = parse_predicate_far(&suffix)
                    && !stated.contains(&HandleKey::new(near, predicate, far))
                {
                    unaccounted += 1;
                }
            }
            for (suffix, _) in scan_suffix_kv(self.store_ref(), &in_prefix(&ns, near))
                .map_err(|e| e.to_string())?
            {
                if let Some((predicate, far)) = parse_predicate_far(&suffix)
                    && !stated.contains(&HandleKey::new(far, predicate, near))
                {
                    unaccounted += 1;
                }
            }
        }
        if unaccounted > 0 {
            return Err(format!(
                "{unaccounted} half-edge(s) cut by a deleted entity are neither repointed nor \
                 deleted; state every one of them in this fragment"
            ));
        }
        Ok(())
    }

    /// The delete verb — the inverse of [`incorporate`](Self::incorporate).
    ///
    /// A fragment handed here carries delete declarations
    /// ([`DynFragment::delete_node`], [`DynFragment::delete_edge`], and the
    /// cluster [`DynFragment::delete`]); it may not also declare edges, clusters
    /// or conflict hooks, because those are what `incorporate` is for.
    ///
    /// Two rules, and they are not in tension:
    ///
    /// - deleting a NODE deletes every edge incident to it — both traversal
    ///   faces, the predicate faces, the payload row, the dynamic segment, and
    ///   those edges' cluster membership rows. A node cannot be left with a
    ///   dangling edge.
    /// - deleting an EDGE leaves both endpoints alone, even when they are then
    ///   connected to nothing. "A node exists only as an endpoint" is about
    ///   creation, not about what a delete may leave behind.
    ///
    /// Nothing here refuses for absence: an id no row holds, or a node no edge
    /// names, is nothing to do and reports zero. Everything runs in ONE batch,
    /// so a reader sees the whole removal or none of it.
    pub fn retract(&mut self, mut fragment: DynFragment) -> Result<RetractReport, String> {
        fragment.check_retractable()?;
        let ns = self.ns_pair();
        let mut report = RetractReport::default();
        let mut batch = Batch::default();

        let (doomed, nodes, plan) = self.retract_plan(&fragment)?;

        // ---- 3. The edges themselves: exactly their rows, in this batch ----
        for row in &doomed {
            if !self.has_row(row)? {
                continue;
            }
            // A delete takes the whole assertion, its open attributes included —
            // the segment is keyed by the same row, so it cannot outlive the
            // edge it describes.
            self.purge_row_into(&mut batch, row);
            report.edges += 1;
        }

        // ---- 4. The cluster faces, through the same path a write uses ----
        report.memberships = fragment.apply_plan(self, &ns, plan, &mut batch)?;
        self.commit(batch)?;
        report.nodes = nodes.len();
        Ok(report)
    }
}
