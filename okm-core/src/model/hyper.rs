//! Clusters — a grouping of edges is a set of rows, not a node (ADR-0066).
//!
//! A community, an event's evidence, a subgraph, a permission boundary: each is
//! a **named set of assertions**, and the graph already carries the assertions.
//! Giving the set a node of its own would lose the set's identity, hide an edge
//! shared by two sets, and turn the fake node into a high-degree hub that
//! community detection reads as a core. So the set IS rows — one row per member,
//! in a face of its own — and the graph's topology is untouched.
//!
//! ```text
//! 0x0A  cluster row      [ns][0x0A][kind][hyper_id]                   → [parent u64 BE][dynamic payload]
//! 0x0B  edge member      [ns][0x0B][kind][hyper_id][edge_id]          → empty
//! 0x0C  node member      [ns][0x0C][kind][hyper_id][node ref][edge_id] → empty, derived
//! 0x0D  child cluster    [ns][0x0D][kind][parent_id][hyper_id]        → empty, derived
//! ```
//!
//! # What is authority and what is an index
//!
//! `0x0A` (the cluster) and `0x0B` (its edge members) are truth: a cluster's
//! existence and its membership are assertions, and a recompute writes a NEW
//! cluster id rather than overwriting one. `0x0C` and `0x0D` are derived and
//! rebuildable — `0x0C` from `0x0B` plus the edge rows, `0x0D` from `parent_id`
//! in the cluster rows — so a miss on either is a freshness problem, never a
//! correctness one ([`Hyperedges::rebuild_derived`] is the repair verb).
//!
//! # Why the node member face carries the node and the edge
//!
//! A cluster's NODES are its member edges' endpoints, deduplicated. `0x0C`
//! exists to make that projection **ordered and streamable**: the node segment
//! is the [`NodeRef`] wire (`[ns 2B][len varint][pkey]`, self-delimiting from
//! the left, so the trailing eight bytes are the edge id), and byte order over
//! that segment is node order — same ns, then same-width pkeys in numeric order.
//! Identical nodes from different edges therefore land ADJACENT, so
//! deduplication costs one comparison with the previous row instead of a set.
//!
//! The node COUNT is not obtained by counting `0x0C` rows: it lives in the
//! cluster row's payload under [`NODE_COUNT`], written by the same batch that
//! writes the rows, and [`ClusterRow::node_count`] is the reader for it.
//!
//! # Completeness rests on two write-time rules
//!
//! A node exists only as an endpoint of some edge — there is no "create a bare
//! node" path ([`Error::BareNode`]) — and a cluster exists only with two or
//! more members ([`Error::ClusterTooSmall`]). Under those two rules the
//! derivation is total, so no "member without an edge" shape is needed.
//!
//! # One write entry point
//!
//! [`WriteSet`] is the only entry: the declared rows (nodes and edges), the
//! clusters over them, and the three reduces all land in ONE engine batch, so a
//! reader sees the whole group or none of it.
//!
//! ```ignore
//! Vec::<Decl<Employment>>::new()
//!     .decls([
//!         Decl::edge(EdgeHead { src: alice(), dst: acme(), predicate: "works_at".into() }),
//!         Decl::edge(EdgeHead { src: bob(), dst: acme(), predicate: "works_at".into() }),
//!     ])
//!     .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("acme")))
//!     .reduce_node::<Person>(|p| p.seen += 1)
//!     .reduce_edge(|e| e.weight += 1)
//!     .reduce_hyper_edge(|h| h.insert("weight".into(), Variant::U64(2)))
//!     .commit(&mut graph)?;
//! ```
//!
//! Membership is idempotent by construction: a member row is `(kind, cluster,
//! edge)`, so writing the same triple twice is one row.

use std::collections::{BTreeMap, BTreeSet};

use okm_value::{Dict, Variant, decode_named};

use crate::engine::storage::{Batch, BatchStore, VirtualStorage, prefix_end, scan_suffix_kv};
use crate::error::{Error, Result};
use crate::model::graph::{EdgeHead, Graph, GraphEncode, NodeRef, NodeSet, decode_body};
use crate::model::index::{Document, PRIMARY_SLOT, Slot, write_slot};
use crate::model::obj_vocab::{FIELD_VOCAB, Faces, Vocabulary};

/// Fixed face 0x0A: the cluster row itself, one key SHORTER than its members —
/// `[ns][0x0A][kind][hyper_id]` → `[parent u64 BE][dynamic payload]`.
///
/// Nothing in this face shares a prefix below a cluster row (members live on
/// `0x0B`/`0x0C`), so a scan of `[ns][0x0A][kind]` needs no length test — the
/// structural difference the vocabulary cursor's face (0x02) cannot claim.
pub const CLUSTER_SLOT: Slot = 0x0A;
/// Fixed face 0x0B: one row per member EDGE — the authority for membership.
pub const EDGE_MEMBER_SLOT: Slot = 0x0B;
/// Fixed face 0x0C: one row per (member edge, endpoint) — the ordered, streamable
/// node projection. Derived, rebuildable from `0x0B` plus the edge rows.
pub const NODE_MEMBER_SLOT: Slot = 0x0C;
/// Fixed face 0x0D: the child index `[kind][parent][child]`, derived from
/// `parent_id` in the cluster rows so "list a cluster's children" is one clean
/// prefix scan instead of a walk over every cluster of the kind.
pub const CHILD_SLOT: Slot = 0x0D;

/// Payload name carrying the number of member EDGES, maintained by the write
/// path. Reserved: the writer stamps it last, after `reduce_hyper_edge` runs.
pub const MEMBER_COUNT: &str = "members";
/// Payload name carrying the number of distinct member NODES. This is the count
/// a reader trusts — never a count of `0x0C` rows.
pub const NODE_COUNT: &str = "nodes";

/// The cluster's kind: a one-byte CLOSED vocabulary (community / event /
/// subgraph / scope), kept in the key so one face serves every kind. Byte `0` is
/// the unnamed value the type defaults to; the constants below are the
/// vocabulary this build knows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Kind(pub u8);

impl Kind {
    /// Discovered grouping (label propagation, a community detector).
    pub const COMMUNITY: Kind = Kind(0x01);
    /// A moment's evidence: the assertions one event was read from.
    pub const EVENT: Kind = Kind(0x02);
    /// A declared subgraph: a named slice of the edge set.
    pub const SUBGRAPH: Kind = Kind(0x03);
    /// A boundary: the assertions one access scope covers.
    pub const SCOPE: Kind = Kind(0x04);

    /// The key byte.
    pub fn byte(self) -> u8 {
        self.0
    }
}

/// One cluster, named by kind and runtime id — the pair every row of the group
/// carries, and the identity `merge`/`split`/`reassign` speak in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClusterRef {
    pub kind: Kind,
    pub id: u64,
}

impl ClusterRef {
    pub const fn new(kind: Kind, id: u64) -> Self {
        Self { kind, id }
    }
}

/// A cluster row read back: the fixed parent head and the dynamic payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusterRow {
    /// The parent cluster's id, `0` = no parent. Fixed width and first, so a
    /// reader slices it without decoding anything.
    pub parent: u64,
    /// The dynamic tail: member count, weight, last write time — whatever the
    /// writer put there. [`MEMBER_COUNT`] and [`NODE_COUNT`] are maintained by
    /// the write path.
    pub payload: Dict,
}

impl ClusterRow {
    /// Member edges, from the payload (0 when the name is absent).
    pub fn member_count(&self) -> u64 {
        count_of(&self.payload, MEMBER_COUNT)
    }

    /// Distinct member nodes, from the payload (0 when the name is absent).
    pub fn node_count(&self) -> u64 {
        count_of(&self.payload, NODE_COUNT)
    }
}

/// A cluster declaration for one write: its kind, its id (`None` = create, and
/// the allocated id comes back in [`WriteReport::hyper_ids`]), and its payload.
///
/// `parent` nests the cluster through the row's own head, so no second face is
/// needed to express a hierarchy — `0` means "no parent".
#[derive(Clone, Debug, Default)]
pub struct HyperEdge {
    pub kind: Kind,
    /// `None` allocates one; `Some(id)` merges into the cluster already there.
    pub id: Option<u64>,
    pub payload: Dict,
    /// The parent cluster's id, `0` for none.
    pub parent: u64,
}

impl HyperEdge {
    /// A NEW cluster over this batch's edges.
    pub fn create(kind: Kind, payload: Dict) -> Self {
        Self {
            kind,
            id: None,
            payload,
            parent: 0,
        }
    }

    /// Merge into the cluster `id`: its members gain this batch's edges and its
    /// payload gains this declaration's names (a name both carry is overwritten).
    ///
    /// The id is also how a batch addresses a cluster it is creating in the same
    /// commit — an id the batch would allocate cannot be named before the call,
    /// so `Some(id)` on an id no row holds creates that cluster.
    pub fn merge_into(kind: Kind, id: u64, payload: Dict) -> Self {
        Self {
            kind,
            id: Some(id),
            payload,
            parent: 0,
        }
    }

    /// The same declaration, nested under `parent`.
    pub fn under(mut self, parent: u64) -> Self {
        self.parent = parent;
        self
    }
}

/// What one write set reports back: the ids it allocated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// The edges this write created, in declaration order.
    pub edge_ids: Vec<u64>,
    /// The clusters this write allocated (a `HyperEdge` with `id: None`, then any
    /// `split`'s new cluster), in that order.
    pub hyper_ids: Vec<u64>,
}

/// Member sets of two clusters, compared as sets: what "between two versions"
/// means (ADR-0066 §5 — a recompute writes a new id, and a diff is ordinary set
/// arithmetic over the two member scans).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterDiff {
    pub shared: Vec<u64>,
    pub only_a: Vec<u64>,
    pub only_b: Vec<u64>,
}

impl ClusterDiff {
    /// No member moved either way.
    pub fn is_empty(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty()
    }
}

/// What a derived-face rebuild did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DerivedReport {
    pub clusters: usize,
    pub members: usize,
    pub node_rows: usize,
    pub chunks: usize,
}

// ---------------------------------------------------------------------------
// Key layout
// ---------------------------------------------------------------------------

fn face_prefix(ns: &[u8; 2], slot: Slot) -> Vec<u8> {
    let mut key = Vec::with_capacity(4);
    key.extend_from_slice(ns);
    write_slot(&mut key, slot);
    key
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_be_bytes());
}

/// `[ns][0x0A][kind][hyper_id]` — the cluster row, one key shorter than its
/// members.
pub fn cluster_key(ns: &[u8; 2], kind: Kind, id: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CLUSTER_SLOT);
    key.push(kind.0);
    push_u64(&mut key, id);
    key
}

/// `[ns][0x0B][kind][hyper_id][edge_id]` — one member edge.
pub fn edge_member_key(ns: &[u8; 2], cluster: ClusterRef, edge_id: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, EDGE_MEMBER_SLOT);
    key.push(cluster.kind.0);
    push_u64(&mut key, cluster.id);
    push_u64(&mut key, edge_id);
    key
}

/// `[ns][0x0C][kind][hyper_id][node ref][edge_id]` — one endpoint of one member
/// edge. The node segment is the `NodeRef` wire, so it is self-delimiting and
/// the trailing eight bytes are always the edge id.
pub fn node_member_key(ns: &[u8; 2], cluster: ClusterRef, node: &NodeRef, edge_id: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, NODE_MEMBER_SLOT);
    key.push(cluster.kind.0);
    push_u64(&mut key, cluster.id);
    key.extend_from_slice(&node.encode());
    push_u64(&mut key, edge_id);
    key
}

/// `[ns][0x0D][kind][parent_id][hyper_id]` — the child index, one row per
/// (parent, child) pair.
pub fn child_key(ns: &[u8; 2], kind: Kind, parent: u64, child: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_SLOT);
    key.push(kind.0);
    push_u64(&mut key, parent);
    push_u64(&mut key, child);
    key
}

/// `[ns][0x0A][kind]` — every cluster of one kind.
pub fn clusters_prefix(ns: &[u8; 2], kind: Kind) -> Vec<u8> {
    let mut key = face_prefix(ns, CLUSTER_SLOT);
    key.push(kind.0);
    key
}

/// `[ns][0x0B][kind][hyper_id]` — every member edge of one cluster.
pub fn members_prefix(ns: &[u8; 2], cluster: ClusterRef) -> Vec<u8> {
    let mut key = face_prefix(ns, EDGE_MEMBER_SLOT);
    key.push(cluster.kind.0);
    push_u64(&mut key, cluster.id);
    key
}

/// `[ns][0x0C][kind][hyper_id]` — every node member row of one cluster.
pub fn node_members_prefix(ns: &[u8; 2], cluster: ClusterRef) -> Vec<u8> {
    let mut key = face_prefix(ns, NODE_MEMBER_SLOT);
    key.push(cluster.kind.0);
    push_u64(&mut key, cluster.id);
    key
}

/// `[ns][0x0D][kind][parent_id]` — every child of one cluster.
pub fn children_prefix(ns: &[u8; 2], kind: Kind, parent: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_SLOT);
    key.push(kind.0);
    push_u64(&mut key, parent);
    key
}

/// The cluster row's value: a fixed eight-byte parent head, then the dynamic
/// payload. The head is first and fixed width so a reader slices it without
/// decoding the tail.
pub fn cluster_value(parent: u64, payload: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(8 + payload.len());
    push_u64(&mut value, parent);
    value.extend_from_slice(payload);
    value
}

fn parse_head(value: &[u8]) -> Result<(u64, &[u8])> {
    if value.len() < 8 {
        return Err(Error::Corrupt {
            what: format!(
                "a cluster row is {} byte(s); its parent head is 8",
                value.len()
            ),
        });
    }
    let mut head = [0u8; 8];
    head.copy_from_slice(&value[..8]);
    Ok((u64::from_be_bytes(head), &value[8..]))
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_be_bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
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

// ---------------------------------------------------------------------------
// Payload codec
// ---------------------------------------------------------------------------

/// Encode a cluster payload as the dynamic segment's shape, allocating new field
/// names in the namespace's field vocabulary (0x02/0x03) — through `store`,
/// which is the caller's batch on the write path, so the vocabulary's growth
/// commits with the row.
fn encode_dict<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    dict: &Dict,
) -> Result<Vec<u8>> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, u16>::new(store, ns, FIELD_VOCAB);
    let mut resolver = |name: &str| cache.ensure(&mut door, name);
    Ok(okm_value::encode(dict, &mut resolver)?)
}

/// The inverse: names come back through the same vocabulary. An id the
/// vocabulary cannot resolve keeps no name, exactly as the edge attribute path
/// treats it — a reader never invents one.
fn decode_dict<S: VirtualStorage + ?Sized>(store: &S, ns: &[u8; 2], raw: &[u8]) -> Result<Dict> {
    let mut cache = Vocabulary::<u16>::new();
    let mut door = Faces::<_, u16>::new(store, ns, FIELD_VOCAB);
    let mut failed: Option<Error> = None;
    let frames = decode_named(raw, &mut |id| match cache.name_of(&mut door, id) {
        Ok(name) => name,
        Err(e) => {
            failed = Some(e);
            None
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let mut out = Dict::new();
    for frame in frames {
        if let Some(name) = cache.name_of(&mut door, frame.id)? {
            out.insert(name, frame.value);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// The member edges of one cluster, in edge-id order: one prefix scan of 0x0B.
/// No length test — member rows are the only rows under this prefix.
fn member_ids<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<u64>> {
    Ok(scan_suffix_kv(store, &members_prefix(ns, cluster))?
        .into_iter()
        .filter_map(|(suffix, _)| be_u64(&suffix))
        .collect())
}

/// The node member rows of one cluster, in key order: `(node, edge)` per row.
fn node_member_rows<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<(NodeRef, u64)>> {
    let mut out = Vec::new();
    for (suffix, _) in scan_suffix_kv(store, &node_members_prefix(ns, cluster))? {
        let Some((node, used)) = NodeRef::decode(&suffix) else {
            continue;
        };
        // The node segment is self-delimiting; what follows it is the edge id,
        // and a row of another width is not one of ours.
        let Some(edge) = be_u64(&suffix[used..]) else {
            continue;
        };
        if suffix.len() != used + 8 {
            continue;
        }
        out.push((node, edge));
    }
    Ok(out)
}

/// The state one cluster write has to build on: the row, its member set, and the
/// node rows the derived face already holds.
struct ClusterState {
    exists: bool,
    parent: u64,
    payload: Dict,
    members: BTreeSet<u64>,
    nodes: Vec<(NodeRef, u64)>,
}

fn read_cluster<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<ClusterState> {
    let mut state = ClusterState {
        exists: false,
        parent: 0,
        payload: Dict::new(),
        members: BTreeSet::new(),
        nodes: Vec::new(),
    };
    if let Some(raw) = store.get(&cluster_key(ns, cluster.kind, cluster.id))? {
        state.exists = true;
        let (parent, body) = parse_head(&raw)?;
        state.parent = parent;
        if !body.is_empty() {
            state.payload = decode_dict(store, ns, body)?;
        }
    }
    state.members = member_ids(store, ns, cluster)?.into_iter().collect();
    state.nodes = node_member_rows(store, ns, cluster)?;
    Ok(state)
}

/// One edge's endpoints, or `None` when the key holds no edge. An edge this same
/// write set is creating is answered from `batch` — its primary row is in the
/// batch, not in the store yet.
fn edge_endpoints<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    edge_id: u64,
    batch: &BTreeMap<u64, (NodeRef, NodeRef)>,
) -> Result<Option<(NodeRef, NodeRef)>> {
    if let Some(pair) = batch.get(&edge_id) {
        return Ok(Some(pair.clone()));
    }
    let mut key = face_prefix(ns, PRIMARY_SLOT);
    push_u64(&mut key, edge_id);
    let Some(body) = store.get(&key)? else {
        return Ok(None);
    };
    let parsed = decode_body(&body);
    Ok(Some((parsed.src, parsed.dst)))
}

/// The next cluster id no row holds, for one kind: the highest `hyper_id` under
/// `[ns][0x0A][kind]` plus one. Ids start at **1**: `0` is the head's reserved
/// "no parent" value (ADR-0066 §4), so a cluster that could never be a parent
/// would be the one thing the numbering must not hand out.
fn next_free_cluster<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    kind: Kind,
) -> Result<u64> {
    let mut next = 1u64;
    for (suffix, _) in scan_suffix_kv(store, &clusters_prefix(ns, kind))? {
        if let Some(id) = be_u64(&suffix) {
            next = next.max(id.saturating_add(1));
        }
    }
    Ok(next)
}

/// Engine + namespace = the read side of one graph's clusters.
///
/// Every verb is one prefix scan or one point read; nothing here counts a face
/// to learn a number the cluster row already states.
pub struct Hyperedges<S> {
    store: S,
    ns: [u8; 2],
}

impl<S: VirtualStorage> Hyperedges<S> {
    pub fn new(store: S, ns: u16) -> Self {
        Self {
            store,
            ns: ns.to_be_bytes(),
        }
    }

    /// The clusters of a graph declaration: the ns comes from the edge table, so
    /// a reader cannot name a namespace the edges do not live in.
    pub fn of<E: GraphEncode>(store: S) -> Self {
        Self::new(store, <E as Document>::NS)
    }

    pub fn ns(&self) -> u16 {
        u16::from_be_bytes(self.ns)
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// One cluster row: its parent and payload. `None` = no such cluster.
    pub fn lookup(&self, cluster: ClusterRef) -> Result<Option<ClusterRow>> {
        let Some(raw) = self
            .store
            .get(&cluster_key(&self.ns, cluster.kind, cluster.id))?
        else {
            return Ok(None);
        };
        let (parent, body) = parse_head(&raw)?;
        let payload = if body.is_empty() {
            Dict::new()
        } else {
            decode_dict(&self.store, &self.ns, body)?
        };
        Ok(Some(ClusterRow { parent, payload }))
    }

    /// Every cluster of one kind, id order: one scan of `[ns][0x0A][kind]`. No
    /// length test — the cluster row is the only row of this face.
    pub fn clusters(&self, kind: Kind) -> Result<Vec<u64>> {
        Ok(
            scan_suffix_kv(&self.store, &clusters_prefix(&self.ns, kind))?
                .into_iter()
                .filter_map(|(suffix, _)| be_u64(&suffix))
                .collect(),
        )
    }

    /// Every cluster of every kind, `(kind, id)` pairs in key order.
    pub fn all_clusters(&self) -> Result<Vec<ClusterRef>> {
        let prefix = face_prefix(&self.ns, CLUSTER_SLOT);
        let mut out = Vec::new();
        for (suffix, _) in scan_suffix_kv(&self.store, &prefix)? {
            if suffix.len() != 9 {
                continue;
            }
            if let Some(id) = be_u64(&suffix[1..]) {
                out.push(ClusterRef::new(Kind(suffix[0]), id));
            }
        }
        Ok(out)
    }

    /// The member edges of one cluster, in edge-id order — the authority.
    pub fn members(&self, cluster: ClusterRef) -> Result<Vec<u64>> {
        member_ids(&self.store, &self.ns, cluster)
    }

    /// The node member rows, in key order: `(node, edge)` per row, duplicates
    /// adjacent. This is the streaming form, not a set.
    pub fn node_rows(&self, cluster: ClusterRef) -> Result<Vec<(NodeRef, u64)>> {
        node_member_rows(&self.store, &self.ns, cluster)
    }

    /// The cluster's member NODES, deduplicated: one pass over `0x0C` comparing
    /// each row's node with the previous one (the face orders them so duplicates
    /// are adjacent), never a set.
    pub fn nodes(&self, cluster: ClusterRef) -> Result<Vec<NodeRef>> {
        let mut out: Vec<NodeRef> = Vec::new();
        for (node, _) in node_member_rows(&self.store, &self.ns, cluster)? {
            if out.last() != Some(&node) {
                out.push(node);
            }
        }
        Ok(out)
    }

    /// The children of one cluster: one scan of `[ns][0x0D][kind][parent]`.
    pub fn children(&self, kind: Kind, parent: u64) -> Result<Vec<u64>> {
        Ok(
            scan_suffix_kv(&self.store, &children_prefix(&self.ns, kind, parent))?
                .into_iter()
                .filter_map(|(suffix, _)| be_u64(&suffix))
                .collect(),
        )
    }

    /// The parent of one cluster — one point read, because the head is in the
    /// row (the child index is the other direction only).
    pub fn parent(&self, cluster: ClusterRef) -> Result<u64> {
        Ok(self.lookup(cluster)?.map_or(0, |row| row.parent))
    }

    /// Two clusters' member sets, as set arithmetic.
    pub fn diff(&self, a: ClusterRef, b: ClusterRef) -> Result<ClusterDiff> {
        let left: BTreeSet<u64> = self.members(a)?.into_iter().collect();
        let right: BTreeSet<u64> = self.members(b)?.into_iter().collect();
        Ok(ClusterDiff {
            shared: left.intersection(&right).copied().collect(),
            only_a: left.difference(&right).copied().collect(),
            only_b: right.difference(&left).copied().collect(),
        })
    }

    /// Rebuild the two DERIVED faces (`0x0C`, `0x0D`) from the rows that carry
    /// the truth, and answer what was written.
    ///
    /// Sweep first, derive second — an entry's key encodes the value it was
    /// derived from, so a member that moved answers at a key no row produces any
    /// more, and re-deriving writes the new key without naming the old one.
    /// `chunk` bounds the ops per commit; a reader running DURING the rebuild
    /// sees a half-built face (operator window: do not read while it runs).
    pub fn rebuild_derived(&self, chunk: usize) -> Result<DerivedReport> {
        let chunk = chunk.max(1);
        let mut report = DerivedReport::default();
        let mut batch = Batch::default();

        // Phase 1 — sweep both derived faces.
        for slot in [NODE_MEMBER_SLOT, CHILD_SLOT] {
            let prefix = face_prefix(&self.ns, slot);
            for item in self
                .store
                .scan_range(&prefix, prefix_end(&prefix).as_deref())
            {
                let (key, _) = item?;
                batch.del(&key);
                if batch.ops.len() >= chunk {
                    self.store.commit_batch(std::mem::take(&mut batch))?;
                    report.chunks += 1;
                }
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(std::mem::take(&mut batch))?;
            report.chunks += 1;
        }

        // Phase 2 — the child index, from `parent_id` in the cluster rows.
        let prefix = face_prefix(&self.ns, CLUSTER_SLOT);
        for item in self
            .store
            .scan_range(&prefix, prefix_end(&prefix).as_deref())
        {
            let (key, value) = item?;
            let suffix = &key[prefix.len()..];
            if suffix.len() != 9 {
                continue;
            }
            let kind = Kind(suffix[0]);
            let Some(id) = be_u64(&suffix[1..]) else {
                continue;
            };
            let (parent, _) = parse_head(&value)?;
            if parent != 0 {
                batch.put(child_key(&self.ns, kind, parent, id), Vec::new());
            }
            report.clusters += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
                report.chunks += 1;
            }
        }

        // Phase 3 — the node members, from `0x0B` plus the edge rows.
        let prefix = face_prefix(&self.ns, EDGE_MEMBER_SLOT);
        for item in self
            .store
            .scan_range(&prefix, prefix_end(&prefix).as_deref())
        {
            let (key, _) = item?;
            let suffix = &key[prefix.len()..];
            if suffix.len() != 17 {
                continue;
            }
            let (Some(id), Some(edge)) = (be_u64(&suffix[1..9]), be_u64(&suffix[9..])) else {
                continue;
            };
            let cluster = ClusterRef::new(Kind(suffix[0]), id);
            report.members += 1;
            if let Some((src, dst)) = edge_endpoints(&self.store, &self.ns, edge, &BTreeMap::new())?
            {
                for node in [src, dst] {
                    batch.put(node_member_key(&self.ns, cluster, &node, edge), Vec::new());
                    report.node_rows += 1;
                }
            }
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
                report.chunks += 1;
            }
        }
        if !batch.ops.is_empty() {
            self.store.commit_batch(batch)?;
            report.chunks += 1;
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// The write entry point
// ---------------------------------------------------------------------------

/// One row a write set declares. TWO variants, always: many node kinds live in
/// the enum `#[derive(GraphEncode)]` already generates for `#[ok_nodes(…)]` (one
/// variant per collection, ns from each table's own `#[ok_ns]`), so a new
/// participating table is one name in the graph's declaration and nothing here.
pub enum Decl<G: GraphEncode> {
    /// A node this batch touches, by collection and key encoding. It must be an
    /// endpoint of an edge this batch writes — the noded side has no other
    /// creation path.
    Node(G::Nodes, Vec<u8>),
    /// An edge this batch creates. Its identity is allocated at run time
    /// (`max + 1`), because parallel edges are separate edges and a declaration
    /// order is not an identity.
    Edge(EdgeHead),
}

impl<G: GraphEncode> Decl<G> {
    /// A node declaration: the collection variant plus the key's own encoding.
    pub fn node<K: crate::model::key::KeyEncode>(variant: G::Nodes, key: &K) -> Self {
        Decl::Node(variant, key.encode())
    }

    /// An edge declaration.
    pub fn edge(head: EdgeHead) -> Self {
        Decl::Edge(head)
    }
}

/// The batch as one value: `Vec::<Decl<G>>::new().decls([…])` starts it.
pub trait DeclSet<G: GraphEncode + Default>: Sized {
    /// Declare the rows this write set writes, then answer the write set.
    fn decls<I: IntoIterator<Item = Decl<G>>>(self, decls: I) -> WriteSet<G>;
}

impl<G: GraphEncode + Default> DeclSet<G> for Vec<Decl<G>> {
    fn decls<I: IntoIterator<Item = Decl<G>>>(self, decls: I) -> WriteSet<G> {
        let mut set = WriteSet::empty();
        set.rows.extend(self);
        set.rows.extend(decls);
        set
    }
}

/// One cluster write's intent, before the rows are derived from it.
#[derive(Default)]
struct ClusterPlan {
    /// The parent to write; `None` = keep whatever the row already says.
    parent: Option<u64>,
    payload: Dict,
    /// Whether the payload came from the caller (so `reduce_hyper_edge` runs on
    /// it), as opposed to being carried over from the stored row.
    from_caller: bool,
    /// Whether a declaration asked for this cluster (as opposed to an op that
    /// only moves members).
    declared: bool,
    /// The cluster is removed outright: a merge source, or a drop below two
    /// members.
    delete_row: bool,
    add: BTreeSet<u64>,
    remove: BTreeSet<u64>,
}

/// One cluster write's outcome: the exact rows to put and delete.
struct ClusterOutcome {
    cluster: ClusterRef,
    delete_row: bool,
    parent: u64,
    old_parent: u64,
    existed: bool,
    payload: Dict,
    put_members: Vec<u64>,
    del_members: Vec<u64>,
    put_nodes: Vec<(NodeRef, u64)>,
    del_nodes: Vec<(NodeRef, u64)>,
}

enum ClusterOp {
    /// Drop this batch's edges from a cluster.
    Remove(ClusterRef),
    /// Remove a cluster outright.
    Delete(ClusterRef),
    /// Union several clusters' members into one, deleting the sources.
    Merge {
        sources: Vec<ClusterRef>,
        into: ClusterRef,
    },
    /// Move `members` out of `cluster` into a new cluster.
    Split {
        cluster: ClusterRef,
        members: Vec<u64>,
    },
    /// Move one existing edge between clusters.
    Reassign {
        edge: u64,
        from: ClusterRef,
        to: ClusterRef,
    },
}

/// The per-collection reduce hook: node rows are heterogeneous, so the
/// registration is the type door.
trait NodeReduce {
    fn ns(&self) -> u16;
    /// The table's key header, `[0xFF][part] (if declared)[ns]`.
    fn header(&self, ns: &[u8; 2]) -> Vec<u8>;
    /// The row's new payload: the stored one (decoded, mutated) or the type's
    /// default when the endpoint does not exist yet.
    fn apply(
        &mut self,
        store: &mut dyn VirtualStorage,
        header: &[u8],
        existing: Option<&[u8]>,
    ) -> Result<Vec<u8>>;
}

struct NodeReducer<R: Document, F> {
    f: F,
    _pd: std::marker::PhantomData<R>,
}

impl<R, F> NodeReduce for NodeReducer<R, F>
where
    R: Document + Default,
    F: FnMut(&mut R),
{
    fn ns(&self) -> u16 {
        R::NS
    }

    fn header(&self, ns: &[u8; 2]) -> Vec<u8> {
        let mut header = Vec::with_capacity(R::PARTITION_PREFIX.len() + 2);
        header.extend_from_slice(R::PARTITION_PREFIX);
        header.extend_from_slice(ns);
        header
    }

    fn apply(
        &mut self,
        store: &mut dyn VirtualStorage,
        header: &[u8],
        existing: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let mut vocabs = R::Vocabs::default();
        let mut row = match existing {
            Some(bytes) => {
                let mut row = R::decode_payload(bytes);
                // A decoded payload carries vocabulary IDS; the mutate-and-write
                // cycle has to hold labels again or the re-encode would store ids
                // as labels (ADR-0032's two-door rule).
                row.__okm_deref_vocab(&mut vocabs, &*store)?;
                row
            }
            None => R::default(),
        };
        (self.f)(&mut row);
        if R::HAS_VOCAB {
            row.__okm_intern(&mut vocabs, store, header)?;
        }
        Ok(row.encode_payload())
    }
}

/// The one write entry point (ADR-0066 §7): declared rows, the clusters over
/// them, and the three reduces, all encoded into ONE batch.
/// A reduce over the graph's own edge payload — one type per graph, so one field.
pub type EdgeReduce<G> = Box<dyn FnMut(&mut G)>;

/// A reduce over a cluster's dynamic payload; clusters are not declared types.
pub type HyperReduce = Box<dyn FnMut(&mut Dict)>;

pub struct WriteSet<G: GraphEncode + Default> {
    rows: Vec<Decl<G>>,
    clusters: Vec<HyperEdge>,
    ops: Vec<ClusterOp>,
    deletes: Vec<u64>,
    node_reducers: Vec<Box<dyn NodeReduce>>,
    edge_reducer: Option<EdgeReduce<G>>,
    hyper_reducer: Option<HyperReduce>,
}

impl<G: GraphEncode + Default> WriteSet<G> {
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            clusters: Vec::new(),
            ops: Vec::new(),
            deletes: Vec::new(),
            node_reducers: Vec::new(),
            edge_reducer: None,
            hyper_reducer: None,
        }
    }

    /// Declare more rows (the entry verb is [`DeclSet::decls`] on the empty Vec).
    pub fn decls<I: IntoIterator<Item = Decl<G>>>(mut self, decls: I) -> Self {
        self.rows.extend(decls);
        self
    }

    /// The cluster this batch's edges belong to. `id: None` creates one and
    /// reports the id allocated for it; `Some(id)` merges into the cluster
    /// already there.
    pub fn hyper_edge(mut self, hyper: HyperEdge) -> Self {
        self.clusters.push(hyper);
        self
    }

    /// Fold every node of collection `R` that this batch touches — declared, or
    /// merely an endpoint of an edge it writes (endpoints are created on
    /// demand). One closure per collection, because node rows are
    /// heterogeneous.
    pub fn reduce_node<R>(mut self, f: impl FnMut(&mut R) + 'static) -> Self
    where
        R: Document + Default + 'static,
    {
        self.node_reducers.push(Box::new(NodeReducer::<R, _> {
            f,
            _pd: std::marker::PhantomData,
        }));
        self
    }

    /// Fold every edge this batch creates. One payload type per graph, so one
    /// closure.
    pub fn reduce_edge(mut self, f: impl FnMut(&mut G) + 'static) -> Self {
        self.edge_reducer = Some(Box::new(f));
        self
    }

    /// Fold every cluster payload this batch declares — `&mut Dict`, because a
    /// cluster's payload is dynamic (its `kind` is a run-time vocabulary, so no
    /// declared Rust type exists for it).
    pub fn reduce_hyper_edge(mut self, f: impl FnMut(&mut Dict) + 'static) -> Self {
        self.hyper_reducer = Some(Box::new(f));
        self
    }

    /// Drop this batch's edges from `cluster`. A cluster left with fewer than
    /// two members stops existing (ADR-0066 §3), so its rows go with them.
    pub fn remove(mut self, cluster: ClusterRef) -> Self {
        self.ops.push(ClusterOp::Remove(cluster));
        self
    }

    /// Remove `cluster` outright: its row, its member rows and its derived rows,
    /// in this batch. Idempotent — a cluster that is not there has nothing to
    /// delete, the same rule the engine's own `del` follows.
    pub fn delete(mut self, cluster: ClusterRef) -> Self {
        self.ops.push(ClusterOp::Delete(cluster));
        self
    }

    /// Union `sources`' members into `into` and remove the sources.
    pub fn merge<I: IntoIterator<Item = ClusterRef>>(
        mut self,
        sources: I,
        into: ClusterRef,
    ) -> Self {
        self.ops.push(ClusterOp::Merge {
            sources: sources.into_iter().collect(),
            into,
        });
        self
    }

    /// Move `members` out of `cluster` into a NEW cluster (its id comes back in
    /// the report).
    pub fn split(mut self, cluster: ClusterRef, members: Vec<u64>) -> Self {
        self.ops.push(ClusterOp::Split { cluster, members });
        self
    }

    /// Move one existing edge from one cluster to another. Writing the same
    /// `(kind, cluster, edge)` twice upserts one row, so this is idempotent.
    pub fn reassign(mut self, edge: u64, from: ClusterRef, to: ClusterRef) -> Self {
        self.ops.push(ClusterOp::Reassign { edge, from, to });
        self
    }

    /// Delete an edge — and every membership row it owns — in the same batch.
    ///
    /// Deletion is a WRITE-PATH contract, not a cascade (ADR-0066 §5): the edge's
    /// rows and its `0x0B`/`0x0C` rows leave together, and a cluster that drops
    /// below two members leaves with them. A miss would be a freshness problem
    /// rather than a correctness one (`0x0C` rebuilds), which is why nothing here
    /// needs a trigger or a sweeper.
    ///
    /// The clusters an edge belongs to are found by ONE pass over the member face
    /// (`0x0B` is the authority, and there is no reverse index — a member row is
    /// `[kind][cluster][edge]`, so "which clusters hold this edge" is a scan by
    /// suffix). One pass serves every edge this write set deletes.
    pub fn unlink(mut self, edge_id: u64) -> Self {
        self.deletes.push(edge_id);
        self
    }

    /// Encode the whole write set into `batch` and commit it as ONE engine
    /// commit. A refused write set writes nothing: every check runs before the
    /// commit, and the batch is dropped.
    pub fn commit<S: VirtualStorage>(self, graph: &mut Graph<S, G>) -> Result<WriteReport> {
        let mut batch = Batch::default();
        let report = self.write_into(graph, &mut batch)?;
        graph.store().commit_batch(batch)?;
        Ok(report)
    }

    /// The cross-collection atomic path: the same write set, into a batch the
    /// caller owns (documents and clusters commit together).
    pub fn write_into<S: VirtualStorage>(
        mut self,
        graph: &mut Graph<S, G>,
        batch: &mut Batch,
    ) -> Result<WriteReport> {
        let ns: [u8; 2] = graph.ns().to_be_bytes();
        let mut report = WriteReport::default();

        // ---- 1. The decls: members, endpoints, and the bare-node rule ----
        let mut heads: Vec<EdgeHead> = Vec::new();
        let mut endpoints: BTreeSet<(u16, Vec<u8>)> = BTreeSet::new();
        for decl in &self.rows {
            if let Decl::Edge(head) = decl {
                endpoints.insert((head.src.ns_id(), head.src.pkey.clone()));
                endpoints.insert((head.dst.ns_id(), head.dst.pkey.clone()));
                heads.push(head.clone());
            }
        }
        for decl in &self.rows {
            if let Decl::Node(variant, key) = decl {
                let node_ns = variant.ns();
                if !endpoints.contains(&(node_ns, key.clone())) {
                    return Err(Error::BareNode {
                        ns: node_ns,
                        key: hex(key),
                    });
                }
            }
        }

        // ---- 2. The edges: runtime ids, every face, in this batch ----
        let mut next_edge = match graph.max_edge_id()? {
            Some(max) => max.checked_add(1),
            None => Some(0),
        }
        .ok_or_else(|| Error::Corrupt {
            what: "edge ids are exhausted: u64::MAX is live".to_string(),
        })?;
        let mut bodies: BTreeMap<u64, (NodeRef, NodeRef)> = BTreeMap::new();
        for head in &heads {
            let id = next_edge;
            next_edge = next_edge.saturating_add(1);
            let mut payload = G::default();
            if let Some(reduce) = self.edge_reducer.as_mut() {
                reduce(&mut payload);
            }
            graph.link_into(batch, head, &payload, id)?;
            bodies.insert(id, (head.src.clone(), head.dst.clone()));
            report.edge_ids.push(id);
        }
        let batch_edges: Vec<u64> = report.edge_ids.clone();

        // ---- 3. Cluster ids: allocated at run time, per kind ----
        // Every kind this write set can allocate in: the ones it declares and the
        // ones its ops move members in or out of (a `split` creates a cluster in
        // the source's kind, so the floor has to be known before it allocates).
        let mut kinds: BTreeSet<u8> = BTreeSet::new();
        for hyper in &self.clusters {
            kinds.insert(hyper.kind.0);
        }
        for op in &self.ops {
            match op {
                ClusterOp::Remove(cluster) => {
                    kinds.insert(cluster.kind.0);
                }
                ClusterOp::Merge { sources, into } => {
                    kinds.insert(into.kind.0);
                    for source in sources {
                        kinds.insert(source.kind.0);
                    }
                }
                ClusterOp::Split { cluster, .. } => {
                    kinds.insert(cluster.kind.0);
                }
                ClusterOp::Delete(cluster) => {
                    kinds.insert(cluster.kind.0);
                }
                ClusterOp::Reassign { from, to, .. } => {
                    kinds.insert(from.kind.0);
                    kinds.insert(to.kind.0);
                }
            }
        }
        let mut next_id: BTreeMap<u8, u64> = BTreeMap::new();
        for kind in kinds {
            let base = next_free_cluster(graph.store(), &ns, Kind(kind))?;
            next_id.insert(kind, base);
        }
        // An explicit id reserves the range above it, so a `None` declared
        // BEFORE it cannot take a number it has already claimed — and `0` is
        // reserved for "no parent", so it is nobody's id.
        for hyper in &self.clusters {
            match hyper.id {
                Some(0) => return Err(Error::ReservedClusterId),
                Some(id) => {
                    let slot = next_id.entry(hyper.kind.0).or_insert(1);
                    *slot = (*slot).max(id.saturating_add(1));
                }
                None => {}
            }
        }
        let mut assigned: Vec<u64> = Vec::with_capacity(self.clusters.len());
        for hyper in &self.clusters {
            let slot = next_id.entry(hyper.kind.0).or_insert(1);
            match hyper.id {
                Some(id) => assigned.push(id),
                None => {
                    let id = *slot;
                    *slot = slot.saturating_add(1);
                    assigned.push(id);
                    report.hyper_ids.push(id);
                }
            }
        }

        // ---- 4. The plan: one record per cluster this batch touches ----
        let mut plan: BTreeMap<(u8, u64), ClusterPlan> = BTreeMap::new();
        for (hyper, id) in self.clusters.iter().zip(&assigned) {
            let entry = plan.entry((hyper.kind.0, *id)).or_default();
            entry.declared = true;
            entry.parent = Some(hyper.parent);
            entry.from_caller = true;
            for (name, value) in hyper.payload.iter() {
                entry.payload.insert(name.clone(), value.clone());
            }
            entry.add.extend(batch_edges.iter().copied());
        }
        for op in &self.ops {
            match op {
                ClusterOp::Remove(cluster) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove
                        .extend(batch_edges.iter().copied());
                }
                ClusterOp::Delete(cluster) => {
                    // The row goes, and it is DECLARED: a delete names the cluster
                    // it wants gone, so a missing one is not an error.
                    let entry = plan.entry((cluster.kind.0, cluster.id)).or_default();
                    entry.delete_row = true;
                    entry.declared = true;
                }
                ClusterOp::Reassign { edge, from, to } => {
                    plan.entry((from.kind.0, from.id))
                        .or_default()
                        .remove
                        .insert(*edge);
                    plan.entry((to.kind.0, to.id))
                        .or_default()
                        .add
                        .insert(*edge);
                }
                ClusterOp::Merge { sources, into } => {
                    // The target's row is rewritten (its counts change), but it
                    // is not DECLARED here: merging into a cluster that does not
                    // exist is a mistake, not a creation.
                    plan.entry((into.kind.0, into.id)).or_default();
                    for source in sources {
                        if source == into {
                            continue;
                        }
                        let state = read_cluster(graph.store(), &ns, *source)?;
                        if !state.exists {
                            return Err(Error::ClusterNotFound {
                                kind: source.kind.0,
                                id: source.id,
                            });
                        }
                        let target = plan.entry((into.kind.0, into.id)).or_default();
                        target.add.extend(state.members.iter().copied());
                        let gone = plan.entry((source.kind.0, source.id)).or_default();
                        gone.delete_row = true;
                        gone.remove.extend(state.members.iter().copied());
                    }
                }
                ClusterOp::Split { cluster, members } => {
                    let state = read_cluster(graph.store(), &ns, *cluster)?;
                    if !state.exists {
                        return Err(Error::ClusterNotFound {
                            kind: cluster.kind.0,
                            id: cluster.id,
                        });
                    }
                    let slot = next_id.entry(cluster.kind.0).or_insert(1);
                    let id = *slot;
                    *slot = slot.saturating_add(1);
                    let fresh = plan.entry((cluster.kind.0, id)).or_default();
                    fresh.declared = true;
                    fresh.parent = Some(state.parent);
                    fresh.payload = state.payload.clone();
                    fresh.add.extend(members.iter().copied());
                    let source = plan.entry((cluster.kind.0, cluster.id)).or_default();
                    source.remove.extend(members.iter().copied());
                    report.hyper_ids.push(id);
                }
            }
        }

        // ---- 4b. Edges leaving: membership rows go in the same batch ----
        //
        // One pass over the member face serves every deletion: a member row is
        // `[kind][cluster][edge]`, so an edge's clusters are found by SUFFIX.
        // There is no reverse index, and none is needed — this runs once per
        // write set that deletes something, not once per edge.
        if !self.deletes.is_empty() {
            let doomed: BTreeSet<u64> = self.deletes.iter().copied().collect();
            let prefix = face_prefix(&ns, EDGE_MEMBER_SLOT);
            for (suffix, _) in scan_suffix_kv(graph.store(), &prefix)? {
                if suffix.len() != 17 {
                    continue;
                }
                let (Some(id), Some(edge)) = (be_u64(&suffix[1..9]), be_u64(&suffix[9..])) else {
                    continue;
                };
                if doomed.contains(&edge) {
                    plan.entry((suffix[0], id)).or_default().remove.insert(edge);
                }
            }
            for edge in &self.deletes {
                graph.unlink_into(batch, *edge)?;
            }
        }

        // ---- 5. Outcomes: read the state, derive the rows ----
        let mut outcomes: Vec<ClusterOutcome> = Vec::with_capacity(plan.len());
        for ((kind, id), mut entry) in plan {
            let cluster = ClusterRef::new(Kind(kind), id);
            let state = read_cluster(graph.store(), &ns, cluster)?;
            if !state.exists && !entry.declared {
                // An op named a cluster that is not there and this batch does
                // not create it: there is nothing to move members in or out of.
                return Err(Error::ClusterNotFound { kind, id });
            }
            // `add` wins over `remove` for the same edge: this write's claim is
            // the later one, and it is what makes `reassign(e, c, c)` a no-op
            // instead of a self-deletion.
            let mut members = state.members.clone();
            for gone in &entry.remove {
                members.remove(gone);
            }
            members.extend(entry.add.iter().copied());
            if members.len() < 2 && !state.exists {
                return Err(Error::ClusterTooSmall {
                    kind,
                    id,
                    members: members.len(),
                });
            }
            // With fewer than two members a cluster stops existing (ADR-0066
            // §3), so the drop IS the deletion — no orphan row is left behind.
            let delete_row = entry.delete_row || members.len() < 2;

            let mut payload = state.payload.clone();
            if !delete_row {
                for (name, value) in entry.payload.iter() {
                    payload.insert(name.clone(), value.clone());
                }
                if let Some(reduce) = self.hyper_reducer.as_mut().filter(|_| entry.from_caller) {
                    reduce(&mut payload);
                }
            }

            // Node refs: the derived face's rows for members that stay, plus the
            // endpoints of the edges this write adds. A node count is the size
            // of THAT set — never the number of 0x0C rows.
            let mut refs: BTreeSet<NodeRef> = BTreeSet::new();
            for (node, edge) in &state.nodes {
                if members.contains(edge) {
                    refs.insert(node.clone());
                }
            }
            for edge in &entry.add {
                if !members.contains(edge) {
                    continue;
                }
                if let Some((src, dst)) = edge_endpoints(graph.store(), &ns, *edge, &bodies)? {
                    refs.insert(src);
                    refs.insert(dst);
                }
            }

            let (put_members, del_members): (Vec<u64>, Vec<u64>) = if delete_row {
                let all: BTreeSet<u64> = state.members.union(&entry.add).copied().collect();
                (Vec::new(), all.into_iter().collect())
            } else {
                (
                    members.difference(&state.members).copied().collect(),
                    state.members.difference(&members).copied().collect(),
                )
            };
            let mut put_nodes: Vec<(NodeRef, u64)> = Vec::new();
            for edge in &put_members {
                if let Some((src, dst)) = edge_endpoints(graph.store(), &ns, *edge, &bodies)? {
                    put_nodes.push((src, *edge));
                    put_nodes.push((dst, *edge));
                }
            }
            let mut del_nodes: Vec<(NodeRef, u64)> = Vec::new();
            for edge in &del_members {
                if let Some((src, dst)) = edge_endpoints(graph.store(), &ns, *edge, &bodies)? {
                    del_nodes.push((src, *edge));
                    del_nodes.push((dst, *edge));
                }
            }
            if delete_row {
                // The derived face's own rows go too, so nothing survives a
                // cluster's removal — including rows of edges that are gone.
                del_nodes.extend(state.nodes.iter().cloned());
            }

            if !delete_row {
                payload.insert(MEMBER_COUNT.to_string(), Variant::U64(members.len() as u64));
                payload.insert(NODE_COUNT.to_string(), Variant::U64(refs.len() as u64));
            }
            entry.parent = Some(entry.parent.unwrap_or(state.parent));
            outcomes.push(ClusterOutcome {
                cluster,
                delete_row,
                parent: if delete_row {
                    0
                } else {
                    entry.parent.unwrap_or(0)
                },
                old_parent: state.parent,
                existed: state.exists,
                payload: if delete_row { Dict::new() } else { payload },
                put_members,
                del_members,
                put_nodes,
                del_nodes,
            });
        }

        // ---- 6. Write: node rows, then the cluster faces, one batch ----
        let mut adapter = BatchStore::new(graph.store(), batch);
        for reduce in &mut self.node_reducers {
            let node_ns = reduce.ns();
            let header = reduce.header(&node_ns.to_be_bytes());
            for (_, pkey) in endpoints.iter().filter(|(ns, _)| *ns == node_ns) {
                let mut key = header.clone();
                write_slot(&mut key, PRIMARY_SLOT);
                key.extend_from_slice(pkey);
                let existing = adapter.get(&key)?;
                let bytes = reduce.apply(&mut adapter, &header, existing.as_deref())?;
                adapter.put(key, bytes)?;
            }
        }
        for outcome in &outcomes {
            let cluster = outcome.cluster;
            // The child index follows the head: it is derived from `parent_id`,
            // so a moved cluster takes its own row with it.
            if outcome.old_parent != 0
                && (outcome.delete_row || outcome.old_parent != outcome.parent)
            {
                adapter.del(&child_key(
                    &ns,
                    cluster.kind,
                    outcome.old_parent,
                    cluster.id,
                ))?;
            }
            if !outcome.delete_row
                && outcome.parent != 0
                && (!outcome.existed || outcome.old_parent != outcome.parent)
            {
                adapter.put(
                    child_key(&ns, cluster.kind, outcome.parent, cluster.id),
                    Vec::new(),
                )?;
            }
            for edge in &outcome.del_members {
                adapter.del(&edge_member_key(&ns, cluster, *edge))?;
            }
            for (node, edge) in &outcome.del_nodes {
                adapter.del(&node_member_key(&ns, cluster, node, *edge))?;
            }
            if outcome.delete_row {
                adapter.del(&cluster_key(&ns, cluster.kind, cluster.id))?;
                continue;
            }
            for edge in &outcome.put_members {
                adapter.put(edge_member_key(&ns, cluster, *edge), Vec::new())?;
            }
            for (node, edge) in &outcome.put_nodes {
                adapter.put(node_member_key(&ns, cluster, node, *edge), Vec::new())?;
            }
            let body = encode_dict(&adapter, &ns, &outcome.payload)?;
            adapter.put(
                cluster_key(&ns, cluster.kind, cluster.id),
                cluster_value(outcome.parent, &body),
            )?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_faces_are_the_fixed_band_the_adr_numbered() {
        assert_eq!(CLUSTER_SLOT, 0x0A);
        assert_eq!(EDGE_MEMBER_SLOT, 0x0B);
        assert_eq!(NODE_MEMBER_SLOT, 0x0C);
        assert_eq!(CHILD_SLOT, 0x0D);
    }

    #[test]
    fn a_cluster_key_is_one_segment_shorter_than_a_member_key() {
        let ns = [0x00u8, 0x21];
        let cluster = ClusterRef::new(Kind::COMMUNITY, 7);
        // [ns 2][0x0A][kind 1][id 8] = 12 bytes; a member adds the edge id.
        assert_eq!(cluster_key(&ns, Kind::COMMUNITY, 7).len(), 12);
        assert_eq!(edge_member_key(&ns, cluster, 9).len(), 20);
        assert!(!edge_member_key(&ns, cluster, 9).starts_with(&cluster_key(
            &ns,
            Kind::COMMUNITY,
            7
        )));
        // Every scan prefix is the whole key of its own row kind.
        assert_eq!(clusters_prefix(&ns, Kind::COMMUNITY).len(), 4);
        assert_eq!(members_prefix(&ns, cluster).len(), 12);
        assert_eq!(children_prefix(&ns, Kind::COMMUNITY, 1).len(), 12);
    }

    #[test]
    fn the_cluster_value_is_a_fixed_head_then_the_payload() {
        let value = cluster_value(0x0102, &[0xAA, 0xBB]);
        assert_eq!(&value[..8], &0x0102u64.to_be_bytes());
        assert_eq!(&value[8..], &[0xAA, 0xBB]);
        let (parent, body) = parse_head(&value).expect("head");
        assert_eq!(parent, 0x0102);
        assert_eq!(body, &[0xAA, 0xBB]);
        // `0` is the reserved "no parent" value, not a null flag: the head is
        // always eight bytes.
        assert!(
            parse_head(&0u64.to_be_bytes())
                .expect("bare head")
                .1
                .is_empty()
        );
        assert!(parse_head(&[0u8; 7]).is_err());
    }

    #[test]
    fn a_node_member_suffix_is_a_self_delimiting_ref_and_an_edge_id() {
        let ns = [0x10u8, 0x00];
        let cluster = ClusterRef::new(Kind::EVENT, 3);
        let node = NodeRef::new(&0x10u16.to_be_bytes(), &5u64.to_be_bytes());
        let key = node_member_key(&ns, cluster, &node, 42);
        let prefix = node_members_prefix(&ns, cluster);
        assert!(key.starts_with(&prefix));
        let suffix = &key[prefix.len()..];
        // [ns 2][len 1][pkey 8][edge 8]
        assert_eq!(suffix.len(), 19);
        let (back, used) = NodeRef::decode(suffix).expect("ref");
        assert_eq!(back, node);
        assert_eq!(be_u64(&suffix[used..]), Some(42));
    }
}
