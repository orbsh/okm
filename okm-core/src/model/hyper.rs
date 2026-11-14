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
//! 0x0A  cluster row      [ns][0x0A][kind][hyper_id]                    → [parent u64 BE][dynamic payload]
//! 0x0B  edge member      [ns][0x0B][kind][hyper_id][src][pred][dst]    → empty
//! 0x0C  child cluster    [ns][0x0C][kind][parent_id][hyper_id]         → empty, derived
//! 0x0D  edge → cluster   [ns][0x0D][src][pred][dst][kind][hyper_id]    → empty, derived
//! ```
//!
//! A member is an edge, and an edge is its tuple `(src, predicate, dst)` — there
//! is no edge number to carry (ADR-0067). The tuple's widths come from the
//! graph's `#[ok_nodes]` declaration, so a member row parses by width exactly as
//! a traversal entry does.
//!
//! # What is authority and what is an index
//!
//! `0x0A` (the cluster) and `0x0B` (its edge members) are truth: a cluster's
//! existence and its membership are assertions, and a recompute writes a NEW
//! cluster id rather than overwriting one. `0x0C` and `0x0D` are derived and
//! rebuildable — `0x0C` from `parent_id` in the cluster rows, `0x0D` from the
//! member rows themselves — so a miss on either is a freshness problem, never a
//! correctness one ([`Hyperedges::rebuild_derived`] is the repair verb).
//!
//! `0x0D` is the same fact as `0x0B` read the other way: `0x0B` is
//! `[kind][cluster][edge]` ("who is in this cluster"), `0x0D` is
//! `[edge][kind][cluster]` ("which clusters is this edge in"). It exists
//! because a **traversal is ordered by the tuple** and so is this face — the
//! edge leads, so both are in the same order and intersectable by key
//! (`intersect_seek` over `Graph::out` and `Hyperedges::members`), which is what
//! makes "the member edges of C that these nodes point at" one alignment instead
//! of a hash set. Writing it costs one more entry per member row, in the same
//! batch, on every add, remove and retract.
//!
//! # Why there is no node face
//!
//! A cluster's NODES are its member edges' endpoints, deduplicated — that fact
//! does not change, only where the projection lives: outside. A derived face
//! mirroring the endpoints would be a fourth row per grouped edge, and the node
//! kinds are heterogeneous, so keeping that mirror correct costs more than it
//! saves. A caller who wants the participants takes the member edges and sorts
//! or dedupes on its own side: inserting into a sorted list is log n, and no
//! HashMap is needed.
//!
//! # Completeness rests on two write-time rules
//!
//! A node exists only as an endpoint of some edge — there is no "create a bare
//! node" path ([`Error::BareNode`]) — and a cluster exists only with two or
//! more members ([`Error::ClusterTooSmall`]). Both are rules about CREATION: a
//! delete may leave a node that is an endpoint of nothing, because what a delete
//! removes is the assertion, not the endpoint.
//!
//! # Two verbs, one direction each
//!
//! [`Graph::incorporate`] writes, [`Graph::retract`] removes. There is no third
//! verb, and no separate edge-linking one: a [`Fragment`] is the whole unit of
//! work — the declared rows, the clusters over them and the three conflict
//! hooks — and it lands in ONE engine batch, so a reader sees the whole group or
//! none of it.
//!
//! ```ignore
//! let mut fragment = Fragment::<Employment>::new([
//!     Decl::edge(EdgeHead { src: alice(), dst: acme(), predicate: "works_at".into() }),
//!     Decl::edge(EdgeHead { src: bob(), dst: acme(), predicate: "works_at".into() }),
//! ]);
//! fragment
//!     .cluster(Cluster::new(Kind::COMMUNITY, attrs("acme")))
//!     .on_node_conflict::<Person>(|old, _declared| Person { seen: old.seen + 1 })
//!     .on_edge_conflict(|old, declared| Employment { weight: old.weight.max(declared.weight) })
//!     .on_cluster_conflict(|old, declared| {
//!         let mut row = declared.clone();
//!         row.insert("weight".into(), Variant::U64(2));
//!         row
//!     });
//! let report = graph.incorporate(fragment)?;
//!
//! // The declarations alone are a valid write: the cluster and the three hooks are optional.
//! graph.incorporate(Fragment::new([Decl::edge(EdgeHead { src: alice(), dst: acme(), predicate: "works_at".into() })]))?;
//! ```
//!
//! Membership is idempotent by construction: a member row is `(kind, cluster,
//! edge)`, so writing the same triple twice is one row.
//!
//! # There is no update verb
//!
//! An update IS `incorporate` with a conflict hook. Declaring an edge the graph
//! already holds hands the fragment's hooks that row's stored payload beside the
//! declared one, so the payload changes while the identity, its faces and its
//! membership rows stand — and the identity is the tuple, so "the edge already
//! held" is one exact key rather than a remembered number (ADR-0067).
//!
//! # What a delete takes
//!
//! [`Graph::retract`] is the inverse: a fragment whose declarations are
//! `delete_node` / `delete_edge`, plus the cluster `delete` it may carry. Nothing
//! that is merely absent is a refusal — an id no row holds is nothing to do, and
//! the report says zero.
//!
//! - Deleting a NODE deletes every edge incident to it: both traversal faces,
//!   the predicate faces, the payload row, and those edges' cluster membership
//!   rows. A node cannot be left with a dangling edge.
//! - Deleting an EDGE leaves both endpoints alone, even when they are then
//!   connected to nothing. "A node exists only as an endpoint" is about
//!   creation, not about what a delete may leave behind. The two rules do not
//!   contradict each other: one is the cascade a node's absence forces, the
//!   other is what an edge's absence forces (nothing).
//!
//! A cluster that drops below two members stops existing, so its remaining
//! membership rows leave too — and the report counts every membership row that
//! went, not only the doomed edges' own.

use std::collections::{BTreeMap, BTreeSet};

use okm_value::{Dict, Variant, decode_named};

use crate::engine::storage::{
    Batch, BatchStore, ScanIter, VirtualStorage, prefix_end, scan_suffix_kv,
};
use crate::error::{Error, Result};
use crate::model::graph::{
    Direction, EdgeHead, EdgeKey, EdgeStep, EdgeStream, Graph, GraphEncode, Members, NodeRef,
    NodeSet,
};
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
/// Fixed face 0x0C: the derived child index `[kind][parent][child]`, derived from
/// `parent_id` in the cluster rows so "list a cluster's children" is one clean
/// prefix scan instead of a walk over every cluster of the kind.
pub const CHILD_INDEX_SLOT: Slot = 0x0C;
/// Fixed face 0x0D: the derived EDGE → CLUSTER index
/// `[ns][0x0D][src][pred][dst][kind][hyper_id]` — the reverse direction of
/// `0x0B`, derived from the member rows so it rebuilds from `0x0B` alone.
///
/// The tuple leads on purpose: the face is then ordered by edge, which is the
/// order a traversal is in, so a perceptive caller can align the two with
/// `okm_query`'s set operators instead of materializing a set. `(kind,
/// hyper_id)` follow, so a membership test for a known
/// `(kind, cluster, edge)` is one exact key — [`member_of`] is that read.
pub const EDGE_CLUSTER_SLOT: Slot = 0x0D;

/// Payload name carrying the number of member EDGES, maintained by the write
/// path. Reserved: the writer stamps it last, after `on_cluster_conflict` runs.
pub const MEMBER_COUNT: &str = "members";

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
/// carries, and the identity `incorporate`/`split`/`reassign` speak in.
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
    /// writer put there. [`MEMBER_COUNT`] is maintained by the write path.
    pub payload: Dict,
}

impl ClusterRow {
    /// Member edges, from the payload (0 when the name is absent).
    pub fn member_count(&self) -> u64 {
        count_of(&self.payload, MEMBER_COUNT)
    }
}

/// A cluster declaration for one write: its kind, its id (`None` = create, and
/// the allocated id comes back in [`WriteReport::hyper_ids`]), and its payload.
///
/// `parent` nests the cluster through the row's own head, so no second face is
/// needed to express a hierarchy — `0` means "no parent".
#[derive(Clone, Debug, Default)]
pub struct Cluster {
    pub kind: Kind,
    /// `None` allocates one; `Some(id)` incorporates into the cluster already there.
    pub id: Option<u64>,
    pub payload: Dict,
    /// The parent cluster's id, `0` for none.
    pub parent: u64,
}

impl Cluster {
    /// A NEW cluster over this fragment's edges.
    pub fn new(kind: Kind, payload: Dict) -> Self {
        Self {
            kind,
            id: None,
            payload,
            parent: 0,
        }
    }

    /// Incorporate into the cluster `id`: its members gain this fragment's edges and its
    /// payload gains this declaration's names (a name both carry is overwritten).
    ///
    /// The id is also how a fragment addresses a cluster it is creating in the same
    /// commit — an id the fragment would allocate cannot be named before the call,
    /// so `Some(id)` on an id no row holds creates that cluster.
    pub fn into(kind: Kind, id: u64, payload: Dict) -> Self {
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

/// What one fragment reports back: the ids it allocated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// The edges this write asserted, in declaration order — their identities,
    /// because there is no number to hand back (ADR-0067). A caller that wants
    /// to keep referring to one takes the tuple.
    pub edges: Vec<EdgeKey>,
    /// The clusters this write allocated (a `Cluster` with `id: None`), in
    /// declaration order.
    pub hyper_ids: Vec<u64>,
}

/// What one `retract` removed — the inverse of [`WriteReport`], counted in rows
/// that are no longer there. A refusal is never "the id was not there": an
/// absent node or edge is nothing to do, and it contributes zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetractReport {
    /// Node deletions honored: a node counts when at least one edge named it, so
    /// the cascade removed something. A node no edge names is not in the graph
    /// (a node exists only as an endpoint), so retracting it reports zero.
    pub nodes: usize,
    /// Edges actually there and removed — including the ones a node's cascade
    /// pulled in.
    pub edges: usize,
    /// Membership rows removed: the doomed edges' own, plus every row a cluster
    /// took with it when it dropped below two members.
    pub memberships: usize,
}

/// What a derived-face rebuild did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DerivedReport {
    /// Cluster rows walked — the child index's derived-from.
    pub clusters: usize,
    /// Member rows walked — the edge → cluster index's derived-from.
    pub members: usize,
    /// Commits the rebuild took.
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

/// `[ns][0x0B][kind][hyper_id][src][pred][dst]` — one member edge.
pub fn edge_member_key(ns: &[u8; 2], cluster: ClusterRef, edge: &EdgeKey) -> Vec<u8> {
    let mut key = members_prefix(ns, cluster);
    edge.write(&mut key);
    key
}

/// `[ns][0x0C][kind][parent_id][hyper_id]` — the child index, one row per
/// (parent, child) pair.
pub fn child_key(ns: &[u8; 2], kind: Kind, parent: u64, child: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_INDEX_SLOT);
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

/// `[ns][0x0C][kind][parent_id]` — every child of one cluster.
pub fn children_prefix(ns: &[u8; 2], kind: Kind, parent: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_INDEX_SLOT);
    key.push(kind.0);
    push_u64(&mut key, parent);
    key
}

/// `[ns][0x0D][src][pred][dst][kind][hyper_id]` — one edge's membership in one
/// cluster, derived from the member row that says the same thing the other way
/// round.
///
/// The edge first: the face is ordered by edge, so it aligns with a traversal
/// stream. `(kind, hyper_id)` after it, so the triple is one exact key.
pub fn edge_cluster_key(ns: &[u8; 2], edge: &EdgeKey, cluster: ClusterRef) -> Vec<u8> {
    let mut key = edge_clusters_prefix(ns, edge);
    key.push(cluster.kind.0);
    push_u64(&mut key, cluster.id);
    key
}

/// `[ns][0x0D][src][pred][dst]` — every cluster one edge belongs to.
pub fn edge_clusters_prefix(ns: &[u8; 2], edge: &EdgeKey) -> Vec<u8> {
    let mut key = face_prefix(ns, EDGE_CLUSTER_SLOT);
    edge.write(&mut key);
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

/// The member edges of one cluster, in key order: one prefix scan of 0x0B.
///
/// A member row's whole suffix IS the edge tuple, so it parses by the
/// declaration's widths — the same rule a traversal entry follows, and the reason
/// a membership row carries no edge number (ADR-0067).
fn member_keys<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
    members: &Members,
) -> Result<Vec<EdgeKey>> {
    let mut out = Vec::new();
    for (suffix, _) in scan_suffix_kv(store, &members_prefix(ns, cluster))? {
        let Some((key, taken)) = EdgeKey::parse(&suffix, members) else {
            continue;
        };
        if taken == suffix.len() {
            out.push(key);
        }
    }
    Ok(out)
}

/// The state one cluster write has to build on: the row and its member set.
struct ClusterState {
    exists: bool,
    parent: u64,
    payload: Dict,
    members: BTreeSet<EdgeKey>,
}

fn read_cluster<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
    members: &Members,
) -> Result<ClusterState> {
    let mut state = ClusterState {
        exists: false,
        parent: 0,
        payload: Dict::new(),
        members: BTreeSet::new(),
    };
    if let Some(raw) = store.get(&cluster_key(ns, cluster.kind, cluster.id))? {
        state.exists = true;
        let (parent, body) = parse_head(&raw)?;
        state.parent = parent;
        if !body.is_empty() {
            state.payload = decode_dict(store, ns, body)?;
        }
    }
    state.members = member_keys(store, ns, cluster, members)?
        .into_iter()
        .collect();
    Ok(state)
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
    /// The graph's width table: a member row holds an edge tuple, and a tuple is
    /// read by the declaration's widths (ADR-0067).
    members: Members,
}

impl<S: VirtualStorage> Hyperedges<S> {
    /// A cluster reader over one ns, with the graph's member widths.
    pub fn new(store: S, ns: u16, members: Members) -> Self {
        Self {
            store,
            ns: ns.to_be_bytes(),
            members,
        }
    }

    /// The clusters of a graph declaration: the ns AND the widths come from the
    /// declaration, so a reader cannot name a namespace the edges do not live in
    /// and cannot read a tuple it has no widths for.
    pub fn of<E: GraphEncode>(store: S) -> Self {
        Self::new(store, <E as Document>::NS, Members::of::<E::Nodes>())
    }

    /// The width table this reader parses member rows with.
    pub fn members_table(&self) -> &Members {
        &self.members
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

    /// The member edges of one cluster, in key order — the authority.
    pub fn members(&self, cluster: ClusterRef) -> Result<Vec<EdgeKey>> {
        member_keys(&self.store, &self.ns, cluster, &self.members)
    }

    /// The children of one cluster: one scan of `[ns][0x0C][kind][parent]`.
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

    /// Rebuild the DERIVED faces (`0x0C`, `0x0D`) from the rows that carry the
    /// truth, and answer what was written.
    ///
    /// Sweep first, derive second — an entry's key encodes the value it was
    /// derived from, so a member that moved answers at a key no row produces any
    /// more, and re-deriving writes the new key without naming the old one.
    /// `chunk` bounds the ops per commit; a reader running DURING the rebuild
    /// sees a half-built face (operator window: do not read while it runs).
    ///
    /// The two faces are rebuilt from their OWN sources: the child index from
    /// `parent_id` in the cluster rows, the edge → cluster index from the member
    /// rows (`0x0B`) — never from each other, and never from the face being
    /// rebuilt. That is what makes `0x0D` rebuildable from `0x0B` ALONE, and it
    /// is the property the write path is checked against: the same input rows
    /// answer the same bytes either way.
    pub fn rebuild_derived(&self, chunk: usize) -> Result<DerivedReport> {
        let chunk = chunk.max(1);
        let mut report = DerivedReport::default();
        let mut batch = Batch::default();

        // Phase 1 — sweep both derived faces. One sweep per face because a key
        // from one face is not a key of the other; the loop keeps a half-built
        // store bounded by `chunk` either way.
        for slot in [CHILD_INDEX_SLOT, EDGE_CLUSTER_SLOT] {
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

        // Phase 3 — the edge → cluster index, from the MEMBER rows: `0x0B` is
        // `[kind][cluster][edge]`, so each row states one `(edge, kind,
        // cluster)` triple and the derived key is that triple in the face's own
        // order. Nothing here reads the face being written, which is what makes
        // this rebuild possible with `0x0D` wiped.
        let prefix = face_prefix(&self.ns, EDGE_MEMBER_SLOT);
        for item in self
            .store
            .scan_range(&prefix, prefix_end(&prefix).as_deref())
        {
            let (key, _) = item?;
            let suffix = &key[prefix.len()..];
            // `[kind][hyper_id]` then the edge tuple, whose width is the
            // declaration's — never a length in the row.
            if suffix.len() <= 9 {
                continue;
            }
            let Some(id) = be_u64(&suffix[1..9]) else {
                continue;
            };
            let cluster = ClusterRef::new(Kind(suffix[0]), id);
            let Some((edge, taken)) = EdgeKey::parse(&suffix[9..], &self.members) else {
                continue;
            };
            if taken + 9 != suffix.len() {
                continue;
            }
            batch.put(edge_cluster_key(&self.ns, &edge, cluster), Vec::new());
            report.members += 1;
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
// The reverse index as a stream (0x0D)
// ---------------------------------------------------------------------------

/// What a cluster stream is asked about: one EDGE (`EdgeKey`) or one NODE
/// (`NodeRef`, or a reference to one).
///
/// It exists because the two questions share one name and Rust has no
/// overloading: `graph.clusters_of(&key)` walks one edge's `0x0D` run, and
/// `graph.clusters_of(&node)` walks the node's incident edges and flattens their
/// runs. One name, two costs, one cursor type.
pub trait ClusterSubject {
    /// The cursor's open — the only thing the two subjects do differently.
    fn open<'a, S: VirtualStorage>(
        self,
        store: &'a S,
        ns: [u8; 2],
        members: &'a Members,
    ) -> ClusterStream<'a, S>;
}

impl ClusterSubject for EdgeKey {
    fn open<'a, S: VirtualStorage>(
        self,
        store: &'a S,
        ns: [u8; 2],
        members: &'a Members,
    ) -> ClusterStream<'a, S> {
        ClusterStream::of_edge(store, members, ns, self)
    }
}

impl ClusterSubject for &EdgeKey {
    fn open<'a, S: VirtualStorage>(
        self,
        store: &'a S,
        ns: [u8; 2],
        members: &'a Members,
    ) -> ClusterStream<'a, S> {
        ClusterStream::of_edge(store, members, ns, self.clone())
    }
}

impl ClusterSubject for NodeRef {
    fn open<'a, S: VirtualStorage>(
        self,
        store: &'a S,
        ns: [u8; 2],
        members: &'a Members,
    ) -> ClusterStream<'a, S> {
        ClusterStream::of_node(store, members, ns, &self)
    }
}

impl ClusterSubject for &NodeRef {
    fn open<'a, S: VirtualStorage>(
        self,
        store: &'a S,
        ns: [u8; 2],
        members: &'a Members,
    ) -> ClusterStream<'a, S> {
        ClusterStream::of_node(store, members, ns, self)
    }
}

/// The clusters some subject belongs to, as a LAZY ORDERED cursor over the
/// derived face `0x0D`.
///
/// One subject, two shapes — and the shape is a field, not a second type:
///
/// - **an edge** (`EdgeKey`): one prefix scan of `[ns][0x0D][src][pred][dst]`,
///   whose suffixes are `[kind][hyper_id]`. One entry per cluster the edge is
///   in.
/// - **a node** (`NodeRef`): its incident edges (`0x06` then `0x07`, the same
///   [`EdgeStream`] a traversal walks) `flat_map`ed through the edge form, with
///   adjacent duplicates collapsed.
///
/// # Cost, stated plainly
///
/// The edge form is one scan and reads exactly its own entries. The node form
/// opens one scan of the out face, then one of the in face, then ONE scan per
/// incident edge (`0x0D` is keyed by the tuple, so an edge's clusters are
/// contiguous and nothing else's are) — so its cost is
/// `out-degree + in-degree + 1` scans,
/// and each of them reads only the entries it answers. That is the price of
/// keying the face by the tuple: it is what makes the face alignable with a
/// traversal, and it is why a node degree of ten thousand is a
/// thousand-segment walk rather than one scan. A caller that needs one cluster's
/// members the other way round takes
/// [`members`](Hyperedges::members) — that face is keyed by cluster, and it is
/// one scan.
///
/// Duplicates: only ADJACENT ones collapse. Within one edge there are none (the
/// key is unique), and across edges the same cluster repeats only when two
/// incident edges are in it — which is not necessarily adjacent, because the
/// entries are ordered by edge id, not by `(kind, hyper_id)`. A caller that
/// needs a set collects into one; the stream deliberately does not, because the
/// set would have to be held for the whole walk and the walk is the point.
///
/// Derived, not truth: an entry here is a copy of a `0x0B` row, so a missing one
/// is a stale index (repair with [`Hyperedges::rebuild_derived`]) and never a
/// wrong answer about what the graph holds.
pub struct ClusterStream<'a, S> {
    store: &'a S,
    ns: [u8; 2],
    /// The graph's width table — the node form's incident edges come off the
    /// traversal faces, whose keys are read by it.
    members: &'a Members,
    /// The cursor over the CURRENT key space: one edge's `[kind][hyper_id]` run.
    scan: Option<ScanIter>,
    /// Bytes of key before the `[kind][hyper_id]` pair.
    suffix_at: usize,
    /// The entry on top: its raw key (the [`Seekable`] position) and the cluster
    /// it names.
    cur: Option<(Vec<u8>, ClusterRef)>,
    /// The edge this stream is about, in the edge form.
    edge: Option<EdgeKey>,
    /// The node this stream is about, in the node form.
    node: Option<NodeRef>,
    /// The node's out-edges, then (lazily) its in-edges — the two directions
    /// concatenated are still ascending by key, because `0x06` < `0x07`.
    out: Option<EdgeStream<'a, S>>,
    inn: Option<EdgeStream<'a, S>>,
    /// The last cluster answered, for adjacent-duplicate collapse.
    last: Option<ClusterRef>,
    /// The fault that ended the walk, if one did — [`Seekable`] has no channel
    /// for one, the same rule [`EdgeStream::error`] follows.
    err: Option<Error>,
}

impl<'a, S: VirtualStorage> ClusterStream<'a, S> {
    /// One edge's clusters.
    fn of_edge(store: &'a S, members: &'a Members, ns: [u8; 2], edge: EdgeKey) -> Self {
        let mut stream = Self {
            store,
            ns,
            members,
            scan: None,
            suffix_at: 0,
            cur: None,
            edge: Some(edge.clone()),
            node: None,
            out: None,
            inn: None,
            last: None,
            err: None,
        };
        stream.open_edge(&edge);
        stream.fill();
        stream
    }

    /// One node's clusters: its incident edges, flattened.
    fn of_node(store: &'a S, members: &'a Members, ns: [u8; 2], node: &NodeRef) -> Self {
        let mut stream = Self {
            store,
            ns,
            members,
            scan: None,
            suffix_at: 0,
            cur: None,
            edge: None,
            node: Some(node.clone()),
            out: Some(EdgeStream::new(
                store,
                members,
                ns,
                Direction::Out,
                node.clone(),
            )),
            inn: None,
            last: None,
            err: None,
        };
        stream.fill();
        stream
    }

    /// Point the cursor at one edge's run — the key space changes, so the
    /// position does too.
    fn open_edge(&mut self, edge: &EdgeKey) {
        let prefix = edge_clusters_prefix(&self.ns, edge);
        self.suffix_at = prefix.len();
        self.cur = None;
        let end = prefix_end(&prefix);
        self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
    }

    /// The next incident edge of the node form: the out face to exhaustion, then
    /// the in face. Opened lazily so a node with out-edges never pays for a scan
    /// of its in face it does not need.
    fn next_edge(&mut self) -> Option<EdgeStep> {
        self.node.as_ref()?;
        let done_with_out = match self.out.as_mut() {
            Some(out) => match out.next() {
                Some(step) => return Some(step),
                None => true,
            },
            None => false,
        };
        if done_with_out {
            self.out = None;
            let node = self.node.as_ref()?.clone();
            self.inn = Some(EdgeStream::new(
                self.store,
                self.members,
                self.ns,
                Direction::In,
                node,
            ));
        }
        self.inn.as_mut()?.next()
    }

    /// One entry's cluster: `[kind][hyper_id]` after the scanned prefix, nothing
    /// else. A suffix of another width is not this face's and is skipped.
    fn decode(&self, key: &[u8]) -> Option<ClusterRef> {
        let suffix = key.get(self.suffix_at..)?;
        if suffix.len() != 9 {
            return None;
        }
        Some(ClusterRef::new(Kind(suffix[0]), be_u64(&suffix[1..])?))
    }

    /// Put the next answer on top, or clear it when there is none left.
    ///
    /// A duplicate of the entry just answered is dropped here rather than handed
    /// out twice — the adjacent-run collapse of the node form.
    fn fill(&mut self) {
        loop {
            if self.cur.is_some() {
                return;
            }
            let pulled = self.scan.as_mut().map(|scan| scan.next());
            match pulled {
                Some(Some(Ok((key, _)))) => {
                    if let Some(cluster) = self.decode(&key)
                        && self.last != Some(cluster)
                    {
                        self.last = Some(cluster);
                        self.cur = Some((key, cluster));
                    }
                    continue;
                }
                Some(Some(Err(e))) => {
                    self.err = Some(e);
                    self.scan = None;
                    return;
                }
                // This key space is exhausted (or there is none yet).
                Some(None) | None => self.scan = None,
            }
            match self.next_edge() {
                Some(step) => self.open_edge(&step.key()),
                None => return,
            }
        }
    }

    /// The fault that ended this walk, if an engine failed mid-scan.
    pub fn error(&self) -> Option<&Error> {
        self.err.as_ref()
    }

    /// The edge this stream was opened on — `None` for the node form, whose key
    /// space changes as it walks.
    pub fn edge(&self) -> Option<&EdgeKey> {
        self.edge.as_ref()
    }

    /// Whether this stream walks one edge's run or a node's incident edges.
    pub fn is_edge_form(&self) -> bool {
        self.edge.is_some()
    }

    /// The node this stream was opened on — `None` for the edge form.
    pub fn node(&self) -> Option<&NodeRef> {
        self.node.as_ref()
    }
}

impl<S: VirtualStorage> Iterator for ClusterStream<'_, S> {
    type Item = ClusterRef;

    fn next(&mut self) -> Option<ClusterRef> {
        let (_, cluster) = self.cur.take()?;
        self.fill();
        Some(cluster)
    }
}

impl<S: VirtualStorage> crate::seek::Seekable for ClusterStream<'_, S> {
    /// The `0x0D` entry's raw key — the byte string the scan's cursor speaks.
    type Key = Vec<u8>;

    fn key(&self) -> Option<&Vec<u8>> {
        self.cur.as_ref().map(|(key, _)| key)
    }

    fn step(&mut self) -> bool {
        if self.cur.is_none() {
            return false;
        }
        self.cur = None;
        self.fill();
        self.cur.is_some()
    }

    fn seek(&mut self, lower: &Vec<u8>) -> bool {
        match &self.cur {
            // At or past the bound: forward-only, the position is kept.
            Some((key, _)) if key >= lower => return true,
            // The cursor is filled to exhaustion by `fill`, so no position means
            // no entries left.
            None => return false,
            Some(_) => {}
        }
        if self.node.is_none() {
            // Edge form: one key space, so the scan can REPOSITION instead of
            // walking the gap.
            let moved = match self.scan.as_mut() {
                Some(scan) => scan.seek(lower),
                None => false,
            };
            self.cur = None;
            if !moved {
                self.scan = None;
                return false;
            }
            self.fill();
            return self.cur.is_some();
        }
        // Node form: the bound may live in a LATER edge's run, and the runs are
        // separate scans — so this degrades to a drain, one item per step. The
        // trait documents that as the honest cost where no cursor exists.
        loop {
            if !self.step() {
                return false;
            }
            if let Some((key, _)) = &self.cur
                && key >= lower
            {
                return true;
            }
        }
    }
}

/// Is `edge` a member of `cluster`? One point read of the derived face — the
/// read that makes a membership test a `.filter` predicate inside a traversal
/// instead of a set built beforehand.
fn member_of_in<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
    edge: &EdgeKey,
) -> Result<bool> {
    Ok(store.get(&edge_cluster_key(ns, edge, cluster))?.is_some())
}

impl<S: VirtualStorage> Hyperedges<S> {
    /// The clusters one edge belongs to, in `(kind, hyper_id)` order — the
    /// derived face read by its leading segment.
    pub fn clusters_of(&self, edge: &EdgeKey) -> ClusterStream<'_, S> {
        ClusterStream::of_edge(&self.store, &self.members, self.ns, edge.clone())
    }

    /// Is this edge a member of this cluster? The derived face's exact key, one
    /// read.
    pub fn member_of(&self, cluster: ClusterRef, edge: &EdgeKey) -> Result<bool> {
        member_of_in(&self.store, &self.ns, cluster, edge)
    }
}

impl<S: VirtualStorage, G: GraphEncode> Graph<S, G> {
    /// The clusters one edge belongs to — [`ClusterStream`] over `0x0D`, or over
    /// a node's incident edges when given a [`NodeRef`] (see the type's cost
    /// note: the node form costs one scan per incident edge).
    ///
    /// ```ignore
    /// for cluster in graph.clusters_of(&key) { /* derived, ordered by edge */ }
    /// for cluster in graph.clusters_of(&node) { /* flat_map over its edges */ }
    /// ```
    pub fn clusters_of<Q: ClusterSubject>(&self, subject: Q) -> ClusterStream<'_, S> {
        subject.open(self.store(), self.ns().to_be_bytes(), self.member_widths())
    }

    /// Is `edge` a member of cluster `(kind, cluster)`? One point read of the
    /// derived face — `Ok(false)` for a member row that is not there.
    ///
    /// Not a `bool`: a point read can fault, and a fault reported as \"not a
    /// member\" is how a broken store would read as an empty cluster. The
    /// traversal-filter spelling is
    /// `.filter(|step| graph.member_of(kind, cluster, &step.key()).unwrap_or(false))`
    /// — the `unwrap_or` is the caller saying what a fault means to it.
    pub fn member_of(&self, kind: Kind, cluster: u64, edge: &EdgeKey) -> Result<bool> {
        member_of_in(
            self.store(),
            &self.ns().to_be_bytes(),
            ClusterRef::new(kind, cluster),
            edge,
        )
    }
}

// ---------------------------------------------------------------------------
// The write entry point
// ---------------------------------------------------------------------------

/// One row a fragment declares. Node kinds live in the enum
/// `#[derive(GraphEncode)]` already generates for `#[ok_nodes(…)]` (one variant
/// per collection, ns from each table's own `#[ok_ns]`), so a new participating
/// table is one name in the graph's declaration and nothing here.
pub enum Decl<G: GraphEncode> {
    /// A node this fragment touches, by collection and key encoding. It must be an
    /// endpoint of an edge this fragment writes — the noded side has no other
    /// creation path.
    Node(G::Nodes, Vec<u8>),
    /// An edge this fragment asserts. The head IS the identity (ADR-0067): on a
    /// tuple no row holds it creates the edge, on one that is live it re-asserts
    /// that row, so this fragment's `on_edge_conflict` decides its payload and a
    /// cluster declaration can adopt it as a member.
    ///
    /// Writing the same tuple twice is writing the same row, so there is nothing
    /// to allocate and no way to write a second edge by accident. Payload and
    /// open attributes survive a re-assertion of the SAME tuple; a replaced
    /// assertion takes its attributes with it.
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

/// One cluster write's intent, before the rows are derived from it.
#[derive(Default)]
struct ClusterPlan {
    /// The parent to write; `None` = keep whatever the row already says.
    parent: Option<u64>,
    payload: Dict,
    /// Whether the payload came from the caller (so `on_cluster_conflict` runs
    /// on it), as opposed to being carried over from the stored row.
    from_caller: bool,
    /// Whether a declaration asked for this cluster (as opposed to an op that
    /// only moves members).
    declared: bool,
    /// The cluster is removed outright: an incorporation source, or a drop below two
    /// members.
    delete_row: bool,
    add: BTreeSet<EdgeKey>,
    remove: BTreeSet<EdgeKey>,
}

/// One cluster write's outcome: the exact rows to put and delete.
struct ClusterOutcome {
    cluster: ClusterRef,
    delete_row: bool,
    parent: u64,
    old_parent: u64,
    existed: bool,
    payload: Dict,
    put_members: Vec<EdgeKey>,
    del_members: Vec<EdgeKey>,
}

enum ClusterOp {
    /// Drop this batch's edges from a cluster.
    Remove(ClusterRef),
    /// Remove a cluster outright.
    Delete(ClusterRef),
    /// Union several clusters' members into one, deleting the sources.
    Incorporate {
        sources: Vec<ClusterRef>,
        into: ClusterRef,
    },
}

/// The per-collection conflict hook: node rows are heterogeneous, so the
/// registration is the type door.
trait NodeConflict {
    fn ns(&self) -> u16;
    /// The table's key header, `[0xFF][part] (if declared)[ns]`.
    fn header(&self, ns: &[u8; 2]) -> Vec<u8>;
    /// The row's new payload: the hook's answer for the stored payload and the
    /// declared one, or the declared payload when nothing is stored yet.
    fn apply(
        &mut self,
        store: &mut dyn VirtualStorage,
        header: &[u8],
        existing: Option<&[u8]>,
    ) -> Result<Vec<u8>>;
}

struct NodeConflictHook<R: Document, F> {
    f: F,
    _pd: std::marker::PhantomData<R>,
}

impl<R, F> NodeConflict for NodeConflictHook<R, F>
where
    R: Document + Default,
    F: Fn(&R, &R) -> R,
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
        // A node declaration carries no payload — the key names the row and the
        // collection's mapping is the only writer — so the DECLARED side of the
        // comparison is the type's default. A creation therefore calls the hook
        // with (default, default): there is no stored row, and the old payload's
        // shape is still what lands.
        let declared = R::default();
        let mut row = match existing {
            Some(bytes) => {
                let mut stored = R::decode_payload(bytes);
                // A decoded payload carries vocabulary IDS; the fold-and-write
                // cycle has to hold labels again or the re-encode would store ids
                // as labels (ADR-0032's two-door rule).
                stored.__okm_deref_vocab(&mut vocabs, &*store)?;
                (self.f)(&stored, &declared)
            }
            None => (self.f)(&declared, &declared),
        };
        if R::HAS_VOCAB {
            row.__okm_intern(&mut vocabs, store, header)?;
        }
        Ok(row.encode_payload())
    }
}

/// A conflict hook over the graph's own edge payload — one type per graph, so
/// one field. It takes `(stored, declared)` and answers the payload to write.
pub type EdgeConflict<G> = Box<dyn Fn(&G, &G) -> G>;

/// A conflict hook over a cluster's dynamic payload; clusters are not declared
/// types, so both sides are a `Dict`.
pub type ClusterConflict = Box<dyn Fn(&Dict, &Dict) -> Dict>;

/// The write fragment (ADR-0066 §7): the declared rows, the clusters over them and
/// the three conflict hooks, all encoded into ONE engine batch by
/// [`Graph::incorporate`]; the delete declarations go the other way, through
/// [`Graph::retract`].
pub struct Fragment<G: GraphEncode + Default> {
    rows: Vec<Decl<G>>,
    clusters: Vec<Cluster>,
    ops: Vec<ClusterOp>,
    edge_deletes: Vec<EdgeHead>,
    node_deletes: Vec<NodeRef>,
    node_conflicts: Vec<Box<dyn NodeConflict>>,
    edge_conflict: Option<EdgeConflict<G>>,
    cluster_conflict: Option<ClusterConflict>,
}

impl<G: GraphEncode + Default> Fragment<G> {
    /// The fragment as one value: its declarations are the only required part, and whatever a builder below attaches — a cluster, a reducer — is optional, so a fragment of declarations alone writes just its nodes and edges.
    pub fn new<I: IntoIterator<Item = Decl<G>>>(decls: I) -> Self {
        let mut fragment = Self::empty();
        fragment.rows.extend(decls);
        fragment
    }

    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            clusters: Vec::new(),
            ops: Vec::new(),
            edge_deletes: Vec::new(),
            node_deletes: Vec::new(),
            node_conflicts: Vec::new(),
            edge_conflict: None,
            cluster_conflict: None,
        }
    }

    /// The cluster this fragment's edges belong to. `id: None` creates one and
    /// reports the id allocated for it; `Some(id)` incorporates into the cluster
    /// already there.
    pub fn cluster(&mut self, cluster: Cluster) -> &mut Self {
        self.clusters.push(cluster);
        self
    }

    /// Resolve every node of collection `R` that this fragment touches — declared, or
    /// merely an endpoint of an edge it writes (endpoints are created on
    /// demand). One hook per collection, because node rows are
    /// heterogeneous.
    ///
    /// The hook compares the STORED row with the DECLARED one and answers the row
    /// to write: `|old, declared| old.clone()` keeps what is there, `|_, declared|
    /// declared.clone()` overwrites, `|old, declared| Person { seen: old.seen.max(declared.seen), ..*old }`
    /// is the monotone merge. A node declaration carries no payload, so the
    /// declared side is the collection's default row; and a node that does not
    /// exist yet is created by calling the hook with `(default, default)`, so a
    /// counter hook still seeds the new row.
    pub fn on_node_conflict<R>(&mut self, f: impl Fn(&R, &R) -> R + 'static) -> &mut Self
    where
        R: Document + Default + 'static,
    {
        self.node_conflicts.push(Box::new(NodeConflictHook::<R, _> {
            f,
            _pd: std::marker::PhantomData,
        }));
        self
    }

    /// Resolve every edge this batch writes. One payload type per graph, so one
    /// hook, and the stored payload on a live id is what the hook compares the
    /// declared one against (`declared` is the payload type's default — an edge
    /// declaration names a head, not a payload).
    ///
    /// Called with `(default, default)` when the id is free: a creation has no
    /// stored row to compare against, and the hook's answer is the payload that
    /// lands.
    pub fn on_edge_conflict(&mut self, f: impl Fn(&G, &G) -> G + 'static) -> &mut Self {
        self.edge_conflict = Some(Box::new(f));
        self
    }

    /// Resolve every cluster payload this batch declares — `Dict` on both sides,
    /// because a cluster's payload is dynamic (its `kind` is a run-time
    /// vocabulary, so no declared Rust type exists for it).
    ///
    /// This is the one hook whose two sides are both real payloads: `old` is the
    /// stored map and `declared` is that map with this declaration's entries
    /// merged in, so a hook can decide between them (`|old, declared| if
    /// declared["ts"] >= old["ts"] { declared.clone() } else { old.clone() }`).
    /// A cluster that does not exist yet is created by calling the hook with
    /// `(empty, declared)`.
    ///
    /// Optional, like every builder here: a batch with only declarations writes
    /// just the nodes and edges.
    pub fn on_cluster_conflict(&mut self, f: impl Fn(&Dict, &Dict) -> Dict + 'static) -> &mut Self {
        self.cluster_conflict = Some(Box::new(f));
        self
    }

    /// Drop this batch's edges from `cluster`. A cluster left with fewer than
    /// two members stops existing (ADR-0066 §3), so its rows go with them.
    pub fn remove(&mut self, cluster: ClusterRef) -> &mut Self {
        self.ops.push(ClusterOp::Remove(cluster));
        self
    }

    /// Remove `cluster` outright: its row, its member rows and its derived rows,
    /// in this batch. Idempotent — a cluster that is not there has nothing to
    /// delete, the same rule the engine's own `del` follows.
    pub fn delete(&mut self, cluster: ClusterRef) -> &mut Self {
        self.ops.push(ClusterOp::Delete(cluster));
        self
    }

    /// Union `sources`' members into `into` and remove the sources.
    pub fn incorporate<I: IntoIterator<Item = ClusterRef>>(
        &mut self,
        sources: I,
        into: ClusterRef,
    ) -> &mut Self {
        self.ops.push(ClusterOp::Incorporate {
            sources: sources.into_iter().collect(),
            into,
        });
        self
    }

    /// Delete one edge — and every membership row it owns — in the same batch
    /// as the rest of the fragment's deletes. Read by [`Graph::retract`].
    ///
    /// The head IS the identity (ADR-0067), so this takes what a declaration
    /// takes: no number to remember, and the same tuple the write used. A
    /// predicate no edge ever carried is nothing to do, the same tolerant reading
    /// an absent edge gets.
    ///
    /// Deletion is a WRITE-PATH contract, not a cascade (ADR-0066 §5): the edge's
    /// rows and its `0x0B` rows leave together, and a cluster that drops below
    /// two members leaves with them. A miss would be a freshness problem rather
    /// than a correctness one (the child index rebuilds), which is why nothing
    /// here needs a trigger or a sweeper.
    ///
    /// The clusters an edge belongs to are found by ONE pass over the member face
    /// (`0x0B` is the authority, and there is no reverse index — a member row is
    /// `[kind][cluster][edge]`, so "which clusters hold this edge" is a scan by
    /// suffix). One pass serves every edge this fragment deletes.
    ///
    /// A tuple no row holds is nothing to do, not a refusal: the report says zero.
    pub fn delete_edge(&mut self, head: EdgeHead) -> &mut Self {
        self.edge_deletes.push(head);
        self
    }

    /// Delete one node AND every edge incident to it — both traversal faces, the
    /// predicate faces, the payload row, and those edges' cluster membership
    /// rows. A node cannot be left with a dangling edge.
    ///
    /// The node's own document row is not this verb's to remove: a `NodeRef`
    /// names a table's ns and a key, not that table's key header (`[partition]
    /// [ns]`, ADR-0014 §5), so the row a reducer materialized leaves through its
    /// collection's own delete. What changes here is the graph: after this
    /// fragment, no edge names the node, which is the only sense in which a node
    /// exists at all (ADR-0066 §3).
    ///
    /// A node no edge names is not in the graph, so retracting it is nothing to
    /// do — the report says zero, the same tolerant reading an absent id gets.
    pub fn delete_node(&mut self, node: NodeRef) -> &mut Self {
        self.node_deletes.push(node);
        self
    }

    /// A retract fragment removes, and only removes. Anything that declares rows,
    /// clusters or reduces belongs to [`Graph::incorporate`] — a fragment
    /// carrying both would have two meanings in one batch.
    ///
    /// The cluster `delete` is a removal, so it rides this verb; `remove` is
    /// already satisfied (every doomed edge is dropped from every cluster that
    /// holds it), and a cluster merge is a write, so it does not.
    fn check_retractable(&self) -> Result<()> {
        let writes = self.rows.len()
            + self.clusters.len()
            + self.node_conflicts.len()
            + usize::from(self.edge_conflict.is_some())
            + usize::from(self.cluster_conflict.is_some())
            + self
                .ops
                .iter()
                .filter(|op| matches!(op, ClusterOp::Incorporate { .. }))
                .count();
        if writes > 0 {
            return Err(Error::RetractWithWrites {
                declarations: writes,
            });
        }
        Ok(())
    }

    /// `incorporate` writes, and a fragment that declares deletions belongs to
    /// [`Graph::retract`] — two verbs with one meaning each.
    fn check_incorporable(&self) -> Result<()> {
        let deletions = self.edge_deletes.len() + self.node_deletes.len();
        if deletions > 0 {
            return Err(Error::IncorporateWithDeletes { deletions });
        }
        Ok(())
    }

    /// The cross-collection atomic path: the same batch, encoded into an engine
    /// batch the caller owns (documents and clusters commit together). Nothing is
    /// written until the caller commits `batch` — [`Graph::incorporate`] is the
    /// verb that commits it for you.
    fn write_into<S: VirtualStorage>(
        mut self,
        graph: &mut Graph<S, G>,
        batch: &mut Batch,
    ) -> Result<WriteReport> {
        let ns: [u8; 2] = graph.ns().to_be_bytes();
        let mut report = WriteReport::default();

        // ---- 1. The decls: members, endpoints, and the bare-node rule ----
        let mut endpoints: BTreeSet<(u16, Vec<u8>)> = BTreeSet::new();
        for decl in &self.rows {
            if let Decl::Edge(head) = decl {
                endpoints.insert((head.src.ns_id(), head.src.pkey.clone()));
                endpoints.insert((head.dst.ns_id(), head.dst.pkey.clone()));
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

        // ---- 2. The edges: the tuple is the identity, then every face ----
        //
        // A declaration names a head, and the head IS the identity (ADR-0067).
        // So the same declaration that creates an edge re-asserts it: a row at
        // that tuple means this fragment's `on_edge_conflict` decides the payload
        // (the update path), and no row means the declaration creates it.
        for decl in &self.rows {
            let Decl::Edge(head) = decl else {
                continue;
            };
            // The vocabulary id is part of the key, so the predicate is interned
            // before the existence probe — the write would intern it anyway.
            let key = graph.write_key(head)?;
            match graph.get_edge(&key)? {
                Some(stored) => {
                    // The stored payload and the declared one go to the hook,
                    // whose answer is what lands: an update is a write, and it
                    // keeps the identity. An edge declaration names a head, not
                    // a payload, so the declared side is the type's default.
                    let stored_payload = G::decode_payload(&stored.payload);
                    let declared = G::default();
                    let payload = match self.edge_conflict.as_ref() {
                        Some(resolve) => resolve(&stored_payload, &declared),
                        None => stored_payload,
                    };
                    // The entries the OLD payload wrote into the `#[ok_index]`
                    // faces are only re-derivable from the old payload, so the
                    // previous assertion's rows leave first and the re-assertion
                    // lands in the same batch: the edge is never absent at
                    // commit.
                    //
                    // `unlink_into` is not a delete: it takes the row, the three
                    // derived faces and the declared faces, and leaves the
                    // dynamic segment (0x01) alone — the open attributes are a
                    // segment of their own and no declaration's to clear.
                    graph.unlink_into(batch, &key)?;
                    graph.assert_into(batch, head, &payload)?;
                }
                None => {
                    // A creation: no row to compare against, so the hook sees
                    // (declared, declared) — its answer is the payload.
                    let declared = G::default();
                    let payload = match self.edge_conflict.as_ref() {
                        Some(resolve) => resolve(&declared, &declared),
                        None => declared,
                    };
                    graph.link_into(batch, head, &payload)?;
                }
            }
            report.edges.push(key);
        }
        let batch_edges: Vec<EdgeKey> = report.edges.clone();

        // ---- 3. Cluster ids: allocated at run time, per kind ----
        // Every kind this fragment can allocate in: the ones it declares and the
        // ones its ops move members in or out of.
        let mut kinds: BTreeSet<u8> = BTreeSet::new();
        for cluster in &self.clusters {
            kinds.insert(cluster.kind.0);
        }
        for op in &self.ops {
            match op {
                ClusterOp::Remove(cluster) => {
                    kinds.insert(cluster.kind.0);
                }
                ClusterOp::Incorporate { sources, into } => {
                    kinds.insert(into.kind.0);
                    for source in sources {
                        kinds.insert(source.kind.0);
                    }
                }
                ClusterOp::Delete(cluster) => {
                    kinds.insert(cluster.kind.0);
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
        for cluster in &self.clusters {
            match cluster.id {
                Some(0) => return Err(Error::ReservedClusterId),
                Some(id) => {
                    let slot = next_id.entry(cluster.kind.0).or_insert(1);
                    *slot = (*slot).max(id.saturating_add(1));
                }
                None => {}
            }
        }
        let mut assigned: Vec<u64> = Vec::with_capacity(self.clusters.len());
        for cluster in &self.clusters {
            let slot = next_id.entry(cluster.kind.0).or_insert(1);
            match cluster.id {
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
        for (cluster, id) in self.clusters.iter().zip(&assigned) {
            let entry = plan.entry((cluster.kind.0, *id)).or_default();
            entry.declared = true;
            entry.parent = Some(cluster.parent);
            entry.from_caller = true;
            for (name, value) in cluster.payload.iter() {
                entry.payload.insert(name.clone(), value.clone());
            }
            entry.add.extend(batch_edges.iter().cloned());
        }
        for op in &self.ops {
            match op {
                ClusterOp::Remove(cluster) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove
                        .extend(batch_edges.iter().cloned());
                }
                ClusterOp::Delete(cluster) => {
                    // The row goes, and it is DECLARED: a delete names the cluster
                    // it wants gone, so a missing one is not an error.
                    let entry = plan.entry((cluster.kind.0, cluster.id)).or_default();
                    entry.delete_row = true;
                    entry.declared = true;
                }
                ClusterOp::Incorporate { sources, into } => {
                    // The target's row is rewritten (its counts change), but it
                    // is not DECLARED here: merging into a cluster that does not
                    // exist is a mistake, not a creation.
                    plan.entry((into.kind.0, into.id)).or_default();
                    for source in sources {
                        if source == into {
                            continue;
                        }
                        let state =
                            read_cluster(graph.store(), &ns, *source, graph.member_widths())?;
                        if !state.exists {
                            return Err(Error::ClusterNotFound {
                                kind: source.kind.0,
                                id: source.id,
                            });
                        }
                        let target = plan.entry((into.kind.0, into.id)).or_default();
                        target.add.extend(state.members.iter().cloned());
                        let gone = plan.entry((source.kind.0, source.id)).or_default();
                        gone.delete_row = true;
                        gone.remove.extend(state.members.iter().cloned());
                    }
                }
            }
        }

        // ---- 5. The rows: outcomes, then the faces, one batch ----
        self.apply_plan(graph, &ns, &endpoints, plan, batch)?;
        Ok(report)
    }

    /// Steps 5 and 6 of a batch, for BOTH verbs: read every planned cluster's
    /// state, derive the rows it ends up with, then write the node rows and the
    /// cluster faces into `batch`. Answers how many membership rows it deleted —
    /// the number `retract` reports.
    fn apply_plan<S: VirtualStorage>(
        &mut self,
        graph: &Graph<S, G>,
        ns: &[u8; 2],
        endpoints: &BTreeSet<(u16, Vec<u8>)>,
        plan: BTreeMap<(u8, u64), ClusterPlan>,
        batch: &mut Batch,
    ) -> Result<usize> {
        let mut deleted_members = 0usize;
        let mut outcomes: Vec<ClusterOutcome> = Vec::with_capacity(plan.len());
        for ((kind, id), mut entry) in plan {
            let cluster = ClusterRef::new(Kind(kind), id);
            let state = read_cluster(graph.store(), ns, cluster, graph.member_widths())?;
            if !state.exists && !entry.declared {
                // An op named a cluster that is not there and this batch does
                // not create it: there is nothing to move members in or out of.
                return Err(Error::ClusterNotFound { kind, id });
            }
            // `add` wins over `remove` for the same edge: this write's claim is
            // the later one, and it is what makes a fragment that both adopts
            // an edge into a cluster and removes it from that same cluster a
            // no-op instead of a self-deletion.
            let mut members = state.members.clone();
            for gone in &entry.remove {
                members.remove(gone);
            }
            members.extend(entry.add.iter().cloned());
            // A cluster below two members may not be CREATED — but a delete is
            // not a creation, so an id that was never there is nothing to do.
            if members.len() < 2 && !state.exists && !entry.delete_row {
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
                // The two sides the hook compares: the stored map, and that map
                // with this declaration's entries merged in. Without a hook the
                // merged map is what lands, which is the plain write.
                let mut declared = state.payload.clone();
                for (name, value) in entry.payload.iter() {
                    declared.insert(name.clone(), value.clone());
                }
                payload = match self.cluster_conflict.as_ref().filter(|_| entry.from_caller) {
                    Some(resolve) => resolve(&state.payload, &declared),
                    None => declared,
                };
            }

            let (put_members, del_members): (Vec<EdgeKey>, Vec<EdgeKey>) = if delete_row {
                let all: BTreeSet<EdgeKey> = state.members.union(&entry.add).cloned().collect();
                (Vec::new(), all.into_iter().collect())
            } else {
                (
                    members.difference(&state.members).cloned().collect(),
                    state.members.difference(&members).cloned().collect(),
                )
            };

            if !delete_row {
                payload.insert(MEMBER_COUNT.to_string(), Variant::U64(members.len() as u64));
            }
            entry.parent = Some(entry.parent.unwrap_or(state.parent));
            deleted_members += del_members.len();
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
            });
        }

        // ---- 6. Write: node rows, then the cluster faces, one batch ----
        let mut adapter = BatchStore::new(graph.store(), batch);
        for reduce in &mut self.node_conflicts {
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
                adapter.del(&child_key(ns, cluster.kind, outcome.old_parent, cluster.id))?;
            }
            if !outcome.delete_row
                && outcome.parent != 0
                && (!outcome.existed || outcome.old_parent != outcome.parent)
            {
                adapter.put(
                    child_key(ns, cluster.kind, outcome.parent, cluster.id),
                    Vec::new(),
                )?;
            }
            for edge in &outcome.del_members {
                adapter.del(&edge_member_key(ns, cluster, edge))?;
                // The reverse direction is the SAME fact, so it leaves in the
                // same breath: a membership row and its edge-first mirror are
                // never allowed to disagree for longer than one commit.
                adapter.del(&edge_cluster_key(ns, edge, cluster))?;
            }
            if outcome.delete_row {
                adapter.del(&cluster_key(ns, cluster.kind, cluster.id))?;
                continue;
            }
            for edge in &outcome.put_members {
                adapter.put(edge_member_key(ns, cluster, edge), Vec::new())?;
                adapter.put(edge_cluster_key(ns, edge, cluster), Vec::new())?;
            }
            let body = encode_dict(&adapter, ns, &outcome.payload)?;
            adapter.put(
                cluster_key(ns, cluster.kind, cluster.id),
                cluster_value(outcome.parent, &body),
            )?;
        }
        Ok(deleted_members)
    }
}

impl<S: VirtualStorage, G: GraphEncode + Default> Graph<S, G> {
    /// The write verb (ADR-0066 §7): consume `fragment` and land its declared
    /// rows, its clusters and its three conflict hooks as ONE engine commit. A
    /// refused fragment writes nothing — every check runs before the commit — and
    /// the report names the ids it allocated.
    ///
    /// There is no update verb: an update is this verb with a conflict hook.
    /// Naming an edge at its own id ([`Decl::edge_at`]) hands `on_edge_conflict`
    /// that row's stored payload beside the declared one, so its payload changes
    /// while its id, faces and memberships stand.
    pub fn incorporate(&mut self, fragment: Fragment<G>) -> Result<WriteReport> {
        fragment.check_incorporable()?;
        let mut rows = Batch::default();
        let report = fragment.write_into(self, &mut rows)?;
        self.store().commit_batch(rows)?;
        Ok(report)
    }

    /// The delete verb — the inverse of [`incorporate`](Self::incorporate).
    ///
    /// A fragment handed here carries delete declarations
    /// ([`Fragment::delete_node`], [`Fragment::delete_edge`], and the cluster
    /// `delete`); it may not also declare rows, clusters or reduces, because
    /// those are what `incorporate` is for ([`Error::RetractWithWrites`]).
    ///
    /// Two rules, and they are not in tension:
    ///
    /// - deleting a NODE deletes every edge incident to it — both traversal
    ///   faces, the predicate faces, the payload row, and those edges' cluster
    ///   membership rows. A node cannot be left with a dangling edge.
    /// - deleting an EDGE leaves both endpoints alone, even when they are then
    ///   connected to nothing. "A node exists only as an endpoint" is about
    ///   creation, not about what a delete may leave behind.
    ///
    /// Refusals are the caller's to ignore, and nothing here refuses for
    /// absence: an id no row holds, or a node no edge names, is nothing to do
    /// and reports zero. Everything runs in ONE batch, so a reader sees the
    /// whole removal or none of it.
    pub fn retract(&mut self, mut fragment: Fragment<G>) -> Result<RetractReport> {
        fragment.check_retractable()?;
        let ns: [u8; 2] = self.ns().to_be_bytes();
        let mut report = RetractReport::default();
        let mut batch = Batch::default();

        // ---- 1. The doomed edges: a node's cascade first, then the declared
        //         heads, deduplicated — one edge is one removal whatever named it.
        //
        // A declaration names a head and the head IS the identity (ADR-0067), so
        // a doomed edge is stated, never looked up by number.
        let mut doomed: BTreeSet<EdgeKey> = BTreeSet::new();
        let mut nodes: BTreeSet<NodeRef> = BTreeSet::new();
        for node in &fragment.node_deletes {
            let mut incident = self.out_edges(node)?;
            incident.extend(self.in_edges(node)?);
            if incident.is_empty() {
                // A node exists only as an endpoint, so one no edge names is not
                // in the graph: nothing to remove, and the report says zero.
                continue;
            }
            nodes.insert(node.clone());
            doomed.extend(incident);
        }
        for head in &fragment.edge_deletes {
            // A predicate nobody interned names no row: absence is nothing to do.
            if let Some(key) = self.key_of(head)? {
                doomed.insert(key);
            }
        }

        // ---- 2. Membership rows follow the edges: ONE pass over 0x0B ----
        //
        // A member row is `[kind][cluster][edge]`, so an edge's clusters are
        // found by SUFFIX. There is no reverse index, and none is needed — this
        // runs once per retract, not once per edge.
        let mut plan: BTreeMap<(u8, u64), ClusterPlan> = BTreeMap::new();
        let members = self.member_widths().clone();
        let prefix = face_prefix(&ns, EDGE_MEMBER_SLOT);
        for (suffix, _) in scan_suffix_kv(self.store(), &prefix)? {
            if suffix.len() <= 9 {
                continue;
            }
            let Some(id) = be_u64(&suffix[1..9]) else {
                continue;
            };
            let Some((edge, taken)) = EdgeKey::parse(&suffix[9..], &members) else {
                continue;
            };
            if taken + 9 != suffix.len() {
                continue;
            }
            if doomed.contains(&edge) {
                plan.entry((suffix[0], id)).or_default().remove.insert(edge);
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

        // ---- 3. The edges themselves: exactly their rows, in this batch ----
        for edge in &doomed {
            if self.get_edge(edge)?.is_none() {
                continue;
            }
            // A delete takes the whole assertion, its open attributes included —
            // the segment is keyed by the same tuple, so it cannot outlive the
            // edge it describes.
            self.purge_into(&mut batch, edge)?;
            report.edges += 1;
        }

        // ---- 4. The cluster faces, through the same path a write uses ----
        report.memberships = fragment.apply_plan(self, &ns, &BTreeSet::new(), plan, &mut batch)?;
        self.store().commit_batch(batch)?;
        report.nodes = nodes.len();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_faces_are_the_fixed_band_the_adr_numbered() {
        assert_eq!(CLUSTER_SLOT, 0x0A);
        assert_eq!(EDGE_MEMBER_SLOT, 0x0B);
        assert_eq!(CHILD_INDEX_SLOT, 0x0C);
    }

    #[test]
    fn a_member_key_carries_the_whole_identity_after_the_cluster_prefix() {
        let ns = [0x00u8, 0x21];
        let cluster = ClusterRef::new(Kind::COMMUNITY, 7);
        // [ns 2][0x0A][kind 1][id 8] = 12 bytes. A member row is that prefix
        // plus the edge's whole identity `[src][pred][dst]` — no number and no
        // length, because a `u64`-keyed member table makes both endpoints ten
        // bytes wide (ADR-0067).
        let src = NodeRef::new(&[0x00, 0x41], &1u64.to_be_bytes());
        let dst = NodeRef::new(&[0x00, 0x41], &2u64.to_be_bytes());
        let edge = EdgeKey::new(src, 3, dst);
        assert_eq!(cluster_key(&ns, Kind::COMMUNITY, 7).len(), 12);
        assert_eq!(edge_member_key(&ns, cluster, &edge).len(), 12 + 10 + 2 + 10);
        assert!(edge_member_key(&ns, cluster, &edge).starts_with(&members_prefix(&ns, cluster)));
        assert!(
            !edge_member_key(&ns, cluster, &edge).starts_with(&cluster_key(
                &ns,
                Kind::COMMUNITY,
                7
            ))
        );
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
    fn a_child_row_is_the_prefix_then_the_child_id() {
        let ns = [0x10u8, 0x00];
        let key = child_key(&ns, Kind::EVENT, 3, 42);
        let prefix = children_prefix(&ns, Kind::EVENT, 3);
        assert!(key.starts_with(&prefix));
        // The face byte is the one the child index moved down to.
        assert_eq!(u16::from(key[2]), CHILD_INDEX_SLOT);
        assert_eq!(&key[3], &Kind::EVENT.byte());
        let suffix = &key[prefix.len()..];
        // [child 8]
        assert_eq!(suffix.len(), 8);
        assert_eq!(be_u64(suffix), Some(42));
    }
}
