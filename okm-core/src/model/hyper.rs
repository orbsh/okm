//! Clusters — a grouping of edges is a set of rows, not a node (ADR-0066).
//!
//! A community, an event's evidence, a subgraph, a permission boundary: each is
//! a **named set of assertions**, and the graph already carries the assertions.
//! Giving the set a node of its own would lose the set's identity, hide an edge
//! shared by two sets, and turn the fake node into a high-degree hub that
//! community detection reads as a core. So the set IS rows — one row per member,
//! in a face of its own — and the graph's topology is untouched.
//!
//!
//! Every face number and every width this module builds a key from comes from
//! [`crate::model::layout`] — one table for the whole engine, and the only place
//! a number is written down. This module says what the rows MEAN; that one says
//! where they are.
//!
//! A member is an edge, and an edge is its tuple `(src, predicate, dst)` — there
//! is no edge number to carry (ADR-0067). The tuple's widths come from the
//! graph's `#[ok_nodes]` declaration, so a member row parses by width exactly as
//! a traversal entry does.
//!
//! # What is authority and what is an index
//!
//! The cluster row and its edge members are truth: a cluster's
//! existence and its membership are assertions, and a recompute writes a NEW
//! cluster id rather than overwriting one. `0x0E` and `0x0F` are derived and
//! rebuildable — `0x0E` from `parent_id` in the cluster rows, `0x0F` from the
//! member rows themselves — so a miss on either is a freshness problem, never a
//! correctness one ([`Hyperedges::rebuild_derived`] is the repair verb).
//!
//! `0x0F` is the same fact as `0x0D` read the other way: `0x0D` is
//! `[kind][cluster][edge]` ("who is in this cluster"), `0x0F` is
//! `[edge][kind][cluster]` ("which clusters is this edge in"). It exists
//! because a **traversal is ordered by the tuple** and so is this face — the
//! edge leads, so both are in the same order and intersectable by key
//! (`intersect_seek` over `Graph::out` and `Hyperedges::members`), which is what
//! makes "the member edges of C that these nodes point at" one alignment instead
//! of a hash set. Writing it costs one more entry per member row, in the same
//! batch, on every add, remove and retract.
//!
//! # The participants, and the members that are entities
//!
//! A cluster's NODES come from two places, and both are legal (ADR-0068):
//!
//! - the endpoints of its member EDGES, deduplicated on the reading side — the
//!   group's edges are contiguous, so the dedupe is a sorted insert and no
//!   HashMap is needed. There is no derived face mirroring those endpoints: it
//!   would be a row per (cluster, endpoint) re-diffed by every membership write,
//!   and the participating kinds are heterogeneous, which the handle registry
//!   already absorbs;
//! - the node MEMBERS the cluster declares, one row on the node member face each,
//!   mirrored on the node → cluster face for the per-entity question. Declared
//!   rather than derived, because a projection cannot hold an entity with no edge
//!   and cannot survive an edge deletion without dropping a group the entity
//!   still belongs to.
//!
//! "Who is in this cluster" is the union of the two, and the faces say which half
//! a reader wants: `members` for edges, `nodes` for entities, and
//! [`ClusterStream`] over a node for both at once.
//!
//! # Completeness rests on one write-time rule
//!
//! A node exists as an endpoint of some edge, or as a member of a node cluster —
//! there is still no "create a bare node" verb ([`Error::BareNode`] guards the
//! edge path). A cluster needs no counterpart: it is a
//! SCOPE, and a scope is information by itself. "This subject is sensitive, do
//! not ask" and "something used to be here and is gone now" are both statements
//! only an empty scope can make, and a one-member cluster is a scope that
//! currently holds one thing. So a cluster row is CREATED when it is declared
//! and deleted only when it is explicitly deleted ([`Fragment::delete`], or an
//! [`Fragment::incorporate`] that merges the source away); a membership change
//! never touches it, and its `members` and `nodes` counts are honest counts that
//! may be zero.
//!
//! The bare-node rule is about CREATION: a delete may leave a node that is an
//! endpoint of nothing, because what a delete removes is the assertion, not the
//! endpoint.
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
//! // `Kind(1)` is whatever this application calls its first cluster kind — the byte is the
//! // caller's, and okm never learns the word for it.
//! fragment
//!     .cluster(Cluster::edges(Kind(1), attrs("acme")))
//!     .on_node_conflict::<Person>(|old, _declared| Person { seen: old.seen + 1 })
//!     .on_edge_conflict(|old, declared| Employment { weight: old.weight.max(declared.weight) })
//!     .on_node_set_conflict(|old, declared| {
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
//! A cluster outlives its members: removing the last one leaves the row and its
//! payload standing, with a count of zero — a scope is a statement, not an alias
//! for the edges inside it. The report counts every membership row that went.

use std::collections::{BTreeMap, BTreeSet};

use okm_value::{Dict, Variant, decode_named};

use crate::engine::storage::{
    Batch, BatchStore, ScanIter, VirtualStorage, prefix_end, scan_suffix_kv,
};
use crate::error::{Error, Result};
use crate::model::graph::{
    Direction, EdgeHead, EdgeKey, EdgeStep, EdgeStream, Graph, GraphEncode, Handle, HandleCache,
    HandleKey, Members, NodeRef, NodeKinds, in_prefix, out_prefix, parse_predicate_far,
};
use crate::model::handles::{HANDLE_WIDTH, Handles};
use crate::model::index::{Document, PRIMARY_SLOT, Slot, write_slot};
use crate::model::layout::U64_WIDTH;
use crate::model::layout::{CLUSTER_KIND_WIDTH, ClusterKindId};
use crate::model::obj_vocab::{CLUSTER_KIND, FIELD_VOCAB, Faces, Names, Vocabulary};

/// The cluster faces (0x0C–0x0F) and the kind vocabulary that leads them
/// (0x0A/0x0B) are numbered in [`crate::model::layout`] — the band table lives
/// there, once.
pub use crate::model::layout::{
    CHILD_INDEX_SLOT, CLUSTER_SLOT, EDGE_CLUSTER_SLOT, EDGE_MEMBER_SLOT, NODE_CLUSTER_SLOT,
    NODE_MEMBER_SLOT,
};

/// Payload name carrying the number of member EDGES, maintained by the write
/// path. Reserved: the writer stamps it last, after the set's `on_*_set_conflict` runs.
pub const MEMBER_COUNT: &str = "members";

/// Payload name carrying the number of member NODES, maintained by the same
/// path and on the same rule. A cluster can hold both kinds (ADR-0068), so the
/// two counts are two numbers rather than one sum: a reader that wants "how big
/// is this scope" adds them, and a reader that wants one half does not have to
/// guess which face the number came from.
pub const NODE_COUNT: &str = "nodes";


/// The cluster's kind: an interned id (0x0A/0x0B), two bytes in the key like a
/// predicate's.
///
/// **The vocabulary belongs to the caller.** okm assigns no meaning to a number:
/// "community" and "flow" are one application's nouns, and another application is
/// free to intern the same numbers for something else. What okm owns is the
/// dictionary — the pair of faces and the append-only cursor — never the words.
/// A database that shipped its callers' noun list as an enum would know its
/// application's business.
///
/// Zero is the unnamed value the type defaults to; a caller that never interns
/// anything can still pass `Kind(0)` and get a face nobody else uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Kind(pub ClusterKindId);

impl Kind {
    /// The key's two bytes, big-endian — the same encoding a predicate id gets.
    pub fn id(self) -> ClusterKindId {
        self.0
    }

    /// The segment as it lands in a key.
    fn write(self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.0.to_be_bytes());
    }
}

/// A group of ASSERTIONS (ADR-0066): its members are edges, its `0x0D`/`0x0B`
/// rows are written by the fragment that declares it. One of the two spellings a
/// cluster has; there is no shared ref, because the two memberships are two
/// relations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeSet {
    pub kind: Kind,
    pub id: u64,
}

/// A group of ENTITIES (ADR-0068): its members are declared handles, and no
/// edge of the same fragment joins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeSet {
    pub kind: Kind,
    pub id: u64,
}

impl EdgeSet {
    pub fn new(kind: Kind, id: u64) -> Self {
        Self { kind, id }
    }
}

impl NodeSet {
    pub fn new(kind: Kind, id: u64) -> Self {
        Self { kind, id }
    }
}

impl From<EdgeSet> for ClusterRef {
    fn from(set: EdgeSet) -> Self {
        ClusterRef::new(set.kind, set.id)
    }
}

impl From<NodeSet> for ClusterRef {
    fn from(set: NodeSet) -> Self {
        ClusterRef::new(set.kind, set.id)
    }
}

/// One cluster ROW, named by kind and runtime id — the pair every row of the group
/// carries, and the identity `incorporate`/`split`/`reassign` speak in. The two
/// spellings above convert into it; a caller picks a spelling, never this.
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
    pub fn edge_count(&self) -> u64 {
        count_of(&self.payload, MEMBER_COUNT)
    }

    /// Member nodes, from the payload (0 when the name is absent).
    pub fn node_count(&self) -> u64 {
        count_of(&self.payload, NODE_COUNT)
    }
}

/// Which assertions a declaration puts in the cluster.
///
/// The two sources are the two constructors, and the caller picks one by
/// spelling it: an edge cluster IS this fragment's edges (the batch idiom), and a
/// node cluster holds the entities it names and nothing else. There is no
/// third value and no default — a declaration that means "the batch's edges"
/// says so on the call, and one that means "these entities" cannot pick up an
/// edge by accident.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemberEdges {
    /// No edge joins: the cluster's members are only the entities it names.
    #[default]
    None,
    /// Every edge this fragment declares is a member.
    Fragment,
}

/// A cluster declaration for one write: its kind LABEL, its id (`None` =
/// create, and the identity comes back in [`WriteReport::edge_sets`] / [`WriteReport::node_sets`]), and
/// its payload.
///
/// `kind` is the caller's word, not a number: a cluster kind is a vocabulary
/// (0x0A/0x0B) exactly like a predicate is, so the label is interned on the
/// write path (0x0A/0x0B in the same batch) and the caller never has to mint an
/// id before it can open a cluster. That is what [`Cluster::edges`] buys over
/// remembering to call `kind_id` first — two steps that can drift apart are one
/// step that cannot.
///
/// `parent` nests the cluster through the row's own head, so no second face is
/// needed to express a hierarchy — `0` means "no parent".
#[derive(Clone, Debug, Default)]
pub struct Cluster {
    /// The caller's WORD (0x0A/0x0B), interned by the write path in the same batch
    /// as the row — exactly what an edge's predicate does. A declaration talks
    /// labels; ids never appear on the way in.
    pub kind: String,
    /// `None` allocates one; `Some(id)` incorporates into the cluster already there.
    pub id: Option<u64>,
    pub payload: Dict,
    /// The parent cluster's id, `0` for none.
    pub parent: u64,
    /// The ENTITIES this cluster holds (ADR-0068). Declared, not derived: a
    /// node no edge touches can be a member, and an edge's deletion does not
    /// remove one.
    pub nodes: BTreeSet<NodeRef>,
    /// Which assertions join (see [`MemberEdges`]).
    pub edges: MemberEdges,
}

impl Cluster {
    /// An cluster over this fragment's EDGE members: every `Decl::Edge` in the
    /// same commit joins it. This is the batch idiom — "these assertions, as one
    /// group" — and the group is exactly the batch, never more and never fewer.
    ///
    /// A fragment with no edges declares an empty cluster, which is a legitimate
    /// statement (ADR-0066 §3: a scope can hold nothing and say so).
    pub fn edges(kind: &str, payload: Dict) -> Self {
        Self {
            kind: kind.to_string(),
            id: None,
            payload,
            parent: 0,
            nodes: BTreeSet::new(),
            edges: MemberEdges::Fragment,
        }
    }

    /// A cluster over these ENTITIES, and over no edge at all: a skill pack, a
    /// permission boundary, a vertex partition. Node membership is DECLARED, so
    /// an entity no edge touches can be a member — and an edge this fragment
    /// happens to write cannot slip in.
    pub fn nodes<I: IntoIterator<Item = NodeRef>>(
        kind: &str,
        payload: Dict,
        nodes: I,
    ) -> Self {
        Self {
            kind: kind.to_string(),
            id: None,
            payload,
            parent: 0,
            nodes: nodes.into_iter().collect(),
            edges: MemberEdges::None,
        }
    }

    /// Incorporate into the cluster `id` instead of creating one: its members
    /// gain this declaration's half, its payload gains this declaration's names
    /// (a name both carry is overwritten).
    ///
    /// This is also how the two halves meet: declare [`Cluster::edges`] at `id`
    /// and [`Cluster::nodes`] at the same `id` in one fragment, and the batch
    /// writes ONE cluster with both member sets — the plan is keyed by
    /// `(kind, id)`, so two declarations at one id are one row.
    ///
    /// The id is also how a fragment addresses a cluster it is creating in the
    /// same commit — an id the fragment would allocate cannot be named before the
    /// call, so `at(id)` on an id no row holds creates that cluster.
    pub fn at(mut self, id: u64) -> Self {
        self.id = Some(id);
        self
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
    /// declaration order — the identity, `(kind, id)`, not a number: a bare id
    /// addresses nothing without the kind it was minted under.
    pub edge_sets: Vec<EdgeSet>,
    /// The ENTITY groups this write allocated, in declaration order.
    pub node_sets: Vec<NodeSet>,
}

/// What one `replace` did: the rows it removed and the rows it wrote, in one
/// batch.
///
/// Two counts, one commit — a replacement is a single state change, so a reader
/// never sees the gap between "the old shape is gone" and "the new one is here".
#[derive(Clone, Debug, Default)]
pub struct ReplaceReport {
    /// Nodes whose cascade removed something.
    pub nodes: usize,
    /// Edges removed: the declared heads and the ones a cascade pulled in.
    pub edges: usize,
    /// The write half: the edges asserted and the clusters allocated.
    pub written: WriteReport,
}

/// What one `retract` removed — the inverse of [`WriteReport`], counted in rows
/// that are no longer there. A refusal is never "the id was not there": an
/// absent node or edge is nothing to do, and it contributes zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetractReport {
    /// Node deletions honored: a node counts when it had edges or cluster
    /// memberships, so the cascade removed something. A node with neither is not
    /// in the graph (a node exists as an endpoint or as a declared member,
    /// ADR-0068), so retracting it reports zero.
    pub nodes: usize,
    /// Edges actually there and removed — including the ones a node's cascade
    /// pulled in.
    pub edges: usize,
    /// Membership rows removed: the doomed edges' own. A cluster is not deleted
    /// by losing members, so nothing leaves on that account.
    pub memberships: usize,
}

/// What a derived-face rebuild did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DerivedReport {
    /// Cluster rows walked — the child index's derived-from.
    pub clusters: usize,
    /// Member rows walked — the edge → cluster index's derived-from.
    pub members: usize,
    /// Node member rows walked — the node → cluster index's derived-from.
    pub node_members: usize,
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

/// The cluster row — `[ns][CLUSTER_SLOT][kind][hyper_id]`, one key shorter than its
/// members.
/// Intern a kind LABEL on the write path: the free-function twin of
/// [`Hyperedges::kind_id`], because a fragment plans against a store rather than
/// against a `&mut Hyperedges`. The vocabulary row lands in the batch that writes
/// the cluster, so a declaration and its kind cannot drift apart.
pub fn intern_kind<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    label: &str,
) -> Result<Kind> {
    let mut door = Faces::<_, ClusterKindId>::new(store, ns, CLUSTER_KIND);
    let mut vocab = Vocabulary::<ClusterKindId>::new();
    Ok(Kind(vocab.ensure(&mut door, label)?))
}

pub fn cluster_key(ns: &[u8; 2], kind: Kind, id: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CLUSTER_SLOT);
    kind.write(&mut key);
    push_u64(&mut key, id);
    key
}

/// The member row — cluster prefix then the edge tuple — by the
/// same row identity the traversal faces use (ADR-0067's boundary-only handle).
pub fn edge_member_key(ns: &[u8; 2], cluster: ClusterRef, row: &HandleKey) -> Vec<u8> {
    let mut key = members_prefix(ns, cluster);
    row.write(&mut key);
    key
}

/// The child index, one row per
/// (parent, child) pair.
pub fn child_key(ns: &[u8; 2], kind: Kind, parent: u64, child: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_INDEX_SLOT);
    kind.write(&mut key);
    push_u64(&mut key, parent);
    push_u64(&mut key, child);
    key
}

/// Every cluster of one kind.
pub fn clusters_prefix(ns: &[u8; 2], kind: Kind) -> Vec<u8> {
    let mut key = face_prefix(ns, CLUSTER_SLOT);
    kind.write(&mut key);
    key
}

/// Every member edge of one cluster.
pub fn members_prefix(ns: &[u8; 2], cluster: ClusterRef) -> Vec<u8> {
    let mut key = face_prefix(ns, EDGE_MEMBER_SLOT);
    cluster.kind.write(&mut key);
    push_u64(&mut key, cluster.id);
    key
}

/// Every child of one cluster.
pub fn children_prefix(ns: &[u8; 2], kind: Kind, parent: u64) -> Vec<u8> {
    let mut key = face_prefix(ns, CHILD_INDEX_SLOT);
    kind.write(&mut key);
    push_u64(&mut key, parent);
    key
}

/// One edge's membership in
/// one cluster, derived from the member row that says the same thing the other
/// way round.
///
/// The row first: the face is ordered by edge, so it aligns with a traversal
/// stream. `(kind, hyper_id)` after it, so the triple is one exact key.
pub fn edge_cluster_key(ns: &[u8; 2], row: &HandleKey, cluster: ClusterRef) -> Vec<u8> {
    let mut key = edge_clusters_prefix(ns, row);
    cluster.kind.write(&mut key);
    push_u64(&mut key, cluster.id);
    key
}

/// Every cluster one edge belongs to.
pub fn edge_clusters_prefix(ns: &[u8; 2], row: &HandleKey) -> Vec<u8> {
    let mut key = face_prefix(ns, EDGE_CLUSTER_SLOT);
    row.write(&mut key);
    key
}

/// The node member row — cluster prefix then the node's handle, eight bytes.
///
/// A node member is an ENTITY the cluster holds (ADR-0068), not an endpoint
/// projection: the row states a fact, and the handle is the graph's own identity
/// for that entity (ADR-0067), so the face is fixed-width in every graph.
pub fn node_member_key(ns: &[u8; 2], cluster: ClusterRef, handle: Handle) -> Vec<u8> {
    let mut key = node_members_prefix(ns, cluster);
    key.extend_from_slice(&handle.wire());
    key
}

/// Every member node of one cluster.
pub fn node_members_prefix(ns: &[u8; 2], cluster: ClusterRef) -> Vec<u8> {
    let mut key = face_prefix(ns, NODE_MEMBER_SLOT);
    cluster.kind.write(&mut key);
    push_u64(&mut key, cluster.id);
    key
}

/// One node's membership in one cluster, derived from the member row that says
/// the same thing the other way round.
///
/// The handle leads: the face is ordered by node, which is the direction the
/// per-entity question walks.
pub fn node_cluster_key(ns: &[u8; 2], handle: Handle, cluster: ClusterRef) -> Vec<u8> {
    let mut key = node_clusters_prefix(ns, handle);
    cluster.kind.write(&mut key);
    push_u64(&mut key, cluster.id);
    key
}

/// Every cluster one node belongs to.
pub fn node_clusters_prefix(ns: &[u8; 2], handle: Handle) -> Vec<u8> {
    let mut key = face_prefix(ns, NODE_CLUSTER_SLOT);
    key.extend_from_slice(&handle.wire());
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
    if value.len() < U64_WIDTH {
        return Err(Error::Corrupt {
            what: format!(
                "a cluster row is {} byte(s); its parent head is 8",
                value.len()
            ),
        });
    }
    let mut head = [0u8; U64_WIDTH];
    head.copy_from_slice(&value[..U64_WIDTH]);
    Ok((u64::from_be_bytes(head), &value[U64_WIDTH..]))
}

fn be_vocab(bytes: &[u8]) -> Option<ClusterKindId> {
    bytes.try_into().ok().map(ClusterKindId::from_be_bytes)
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    <[u8; U64_WIDTH]>::try_from(bytes).ok().map(u64::from_be_bytes)
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

/// The member rows of one cluster, in key order: one prefix scan of 0x0B.
///
/// A member row's whole suffix IS the edge's row identity — eighteen bytes, the
/// same bytes a traversal entry carries — so it parses by FIXED WIDTH, and a
/// membership row carries no edge number and no endpoint reference (ADR-0067).
fn member_rows<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<HandleKey>> {
    let mut out = Vec::new();
    for (suffix, _) in scan_suffix_kv(store, &members_prefix(ns, cluster))? {
        if let Some(row) = HandleKey::parse(&suffix) {
            out.push(row);
        }
    }
    Ok(out)
}

/// The node member rows of one cluster, in key order: one prefix scan of the
/// node member face. The suffix IS the handle, eight bytes.
pub(crate) fn node_member_handles<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
) -> Result<Vec<Handle>> {
    let mut out = Vec::new();
    for (suffix, _) in scan_suffix_kv(store, &node_members_prefix(ns, cluster))? {
        if let Some(handle) = Handle::parse(&suffix) {
            out.push(handle);
        }
    }
    Ok(out)
}

/// A handle's node when the registry holds it — the inverse direction, used
/// where a write has members it only knows as handles (a merge restating a
/// source's set).
fn resolve_handle<S: VirtualStorage>(store: &S, ns: &[u8; 2], handle: Handle) -> Option<NodeRef> {
    Handles::new(store, *ns).node(handle).ok().flatten()
}

/// The state one cluster write has to build on: the row and its member set.
struct ClusterState {
    exists: bool,
    parent: u64,
    payload: Dict,
    members: BTreeSet<HandleKey>,
    /// The stored node members, in handle space — the same space the rows are
    /// keyed by, so no resolution is needed to diff a write.
    nodes: BTreeSet<Handle>,
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
        nodes: BTreeSet::new(),
    };
    if let Some(raw) = store.get(&cluster_key(ns, cluster.kind, cluster.id))? {
        state.exists = true;
        let (parent, body) = parse_head(&raw)?;
        state.parent = parent;
        if !body.is_empty() {
            state.payload = decode_dict(store, ns, body)?;
        }
    }
    state.members = member_rows(store, ns, cluster)?.into_iter().collect();
    state.nodes = node_member_handles(store, ns, cluster)?.into_iter().collect();
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

    /// Intern the caller's label as a cluster kind (0x0A/0x0B): first seen claims
    /// the next id, append-only, never renumbered — the same door predicates come
    /// through (0x04/0x05), and the reason both are `u16` and both lead their
    /// faces. okm stores the word and never reads it: the vocabulary is the
    /// caller's, and "community" is nobody's business but the application's.
    pub fn kind_id(&mut self, label: &str) -> Result<Kind> {
        let mut door = Faces::<_, ClusterKindId>::new(&self.store, &self.ns, CLUSTER_KIND);
        let mut vocab = Vocabulary::<ClusterKindId>::new();
        Ok(Kind(vocab.ensure(&mut door, label)?))
    }

    /// The id a label was interned under — `None` for a word nobody minted, and
    /// NO allocation: a read names a kind by its word and must not mint one as a
    /// side effect (`kind_id` does, which is why a read never calls it).
    pub fn kind_of(&self, label: &str) -> Result<Option<Kind>> {
        let mut door = Faces::<_, ClusterKindId>::new(&self.store, &self.ns, CLUSTER_KIND);
        Ok(door.name_id(label)?.map(Kind))
    }

    /// The label a kind was interned under — `None` for a number nobody minted.
    pub fn kind_label(&mut self, kind: Kind) -> Result<Option<String>> {
        let mut door = Faces::<_, ClusterKindId>::new(&self.store, &self.ns, CLUSTER_KIND);
        let mut vocab = Vocabulary::<ClusterKindId>::new();
        vocab.name_of(&mut door, kind.0)
    }

    /// Every kind this namespace has interned, id-ordered: one prefix scan of
    /// 0x0A. A listing, never an inference walked back from the cluster rows.
    pub fn kinds(&mut self) -> Result<Vec<(ClusterKindId, String)>> {
        let mut door = Faces::<_, ClusterKindId>::new(&self.store, &self.ns, CLUSTER_KIND);
        let mut pairs: Vec<(ClusterKindId, String)> = Names::scan(&mut door)?
            .into_iter()
            .map(|(label, id)| (id, label))
            .collect();
        pairs.sort_by_key(|(id, _)| *id);
        Ok(pairs)
    }

    /// One cluster row: its parent and payload. `None` = no such cluster.
    pub fn lookup(&self, cluster: impl Into<ClusterRef>) -> Result<Option<ClusterRow>> {
        let cluster: ClusterRef = cluster.into();
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
    /// The ASSERTION groups of one kind, in id order.
    pub fn edge_sets(&self, kind: &str) -> Result<Vec<EdgeSet>> {
        let Some(kind) = self.kind_of(kind)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .cluster_ids(kind)?
            .into_iter()
            .map(|id| EdgeSet { kind, id })
            .collect())
    }

    /// The ENTITY groups of one kind, in id order.
    pub fn node_sets(&self, kind: &str) -> Result<Vec<NodeSet>> {
        let Some(kind) = self.kind_of(kind)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .cluster_ids(kind)?
            .into_iter()
            .map(|id| NodeSet { kind, id })
            .collect())
    }

    /// Every cluster id of one kind, in key order — the scan both spellings
    /// answer from (a row carries no member kind of its own).
    fn cluster_ids(&self, kind: Kind) -> Result<Vec<u64>> {
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
    pub fn edge_members(&self, set: EdgeSet) -> Result<Vec<EdgeKey>> {
        self.resolve(member_rows(&self.store, &self.ns, set.into())?)
    }

    /// The same member set as ROWS — no resolution. What a caller aligning this
    /// face with a traversal stream wants, since the stream speaks the same
    /// eighteen bytes.
    pub fn edge_member_rows(&self, set: EdgeSet) -> Result<Vec<HandleKey>> {
        member_rows(&self.store, &self.ns, set.into())
    }

    /// The member NODES of one cluster, in handle order — the authority for a
    /// cluster of entities (ADR-0068), resolved at the boundary (one registry
    /// batch for the whole set).
    pub fn node_members(&self, set: NodeSet) -> Result<Vec<NodeRef>> {
        let handles = node_member_handles(&self.store, &self.ns, set.into())?;
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        Handles::new(&self.store, self.ns).nodes(&handles)
    }

    /// Resolve row identities to tuples for the PUBLIC answer — one registry
    /// scan, the boundary the whole design defers resolution to.
    fn resolve(&self, rows: Vec<HandleKey>) -> Result<Vec<EdgeKey>> {
        let mut wanted = BTreeSet::new();
        for row in &rows {
            wanted.insert(row.src);
            wanted.insert(row.dst);
        }
        let sorted: Vec<Handle> = wanted.iter().copied().collect();
        let refs = Handles::new(&self.store, self.ns).nodes(&sorted)?;
        let nodes: BTreeMap<Handle, NodeRef> = sorted.into_iter().zip(refs).collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let (Some(src), Some(dst)) = (nodes.get(&row.src), nodes.get(&row.dst)) else {
                return Err(Error::Corrupt {
                    what: "a member row names a handle the registry does not hold".into(),
                });
            };
            out.push(EdgeKey {
                src: src.clone(),
                predicate: row.predicate,
                dst: dst.clone(),
            });
        }
        Ok(out)
    }

    /// The children of one cluster: one scan of `[ns][0x0C][kind][parent]`.
    pub fn children(&self, kind: &str, parent: u64) -> Result<Vec<u64>> {
        let Some(kind) = self.kind_of(kind)? else {
            return Ok(Vec::new());
        };
        Ok(
            scan_suffix_kv(&self.store, &children_prefix(&self.ns, kind, parent))?
                .into_iter()
                .filter_map(|(suffix, _)| be_u64(&suffix))
                .collect(),
        )
    }

    /// The parent of one cluster — one point read, because the head is in the
    /// row (the child index is the other direction only).
    pub fn parent(&self, cluster: impl Into<ClusterRef>) -> Result<u64> {
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

        // Phase 1 — sweep every derived face (0x0C, 0x0D). One sweep per face
        // because a key from one face is not a key of the other; the loop keeps
        // a half-built store bounded by `chunk` either way. The handle
        // registry's reverse half is NOT here: it is the registry's own sweep,
        // through [`Handles::rebuild_reverse`] in phase 4.
        for slot in [CHILD_INDEX_SLOT, EDGE_CLUSTER_SLOT, NODE_CLUSTER_SLOT] {
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
                batch.put(child_key(&self.ns, kind, parent, id), Vec::new());
            }
            report.clusters += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
                report.chunks += 1;
            }
        }

        // Phase 3 — the edge → cluster index, from the MEMBER rows: `0x0B` is
        // `[kind][cluster][edge row]`, so each row states one `(row, kind,
        // cluster)` triple and the derived key is that triple in the face's own
        // order. Nothing here reads the face being written, and nothing is
        // resolved: both faces speak the same eighteen bytes.
        let prefix = face_prefix(&self.ns, EDGE_MEMBER_SLOT);
        for item in self
            .store
            .scan_range(&prefix, prefix_end(&prefix).as_deref())
        {
            let (key, _) = item?;
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
            let cluster = ClusterRef::new(Kind(kind), id);
            let Some(row) = HandleKey::parse(&suffix[CLUSTER_KIND_WIDTH + U64_WIDTH..]) else {
                continue;
            };
            batch.put(edge_cluster_key(&self.ns, &row, cluster), Vec::new());
            report.members += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
                report.chunks += 1;
            }
        }
        // Phase 3b — the NODE → CLUSTER index, from the node MEMBER rows: the
        // node member face is `[kind][cluster][handle]`, so each row states one
        // `(handle, kind, cluster)` triple and the derived key is that triple in
        // the face's own order. The same sweep-and-derive rule as the edge
        // mirror: nothing reads the face being written, and nothing is resolved
        // — a handle is the node's row identity, so the two faces speak the same
        // eight bytes.
        let prefix = face_prefix(&self.ns, NODE_MEMBER_SLOT);
        for item in self
            .store
            .scan_range(&prefix, prefix_end(&prefix).as_deref())
        {
            let (key, _) = item?;
            let suffix = &key[prefix.len()..];
            if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH + HANDLE_WIDTH {
                continue;
            }
            let Some(kind) = be_vocab(&suffix[..CLUSTER_KIND_WIDTH]) else {
                continue;
            };
            let Some(id) = be_u64(&suffix[CLUSTER_KIND_WIDTH..CLUSTER_KIND_WIDTH + U64_WIDTH]) else {
                continue;
            };
            let Some(handle) = Handle::parse(&suffix[CLUSTER_KIND_WIDTH + U64_WIDTH..]) else {
                continue;
            };
            batch.put(
                node_cluster_key(&self.ns, handle, ClusterRef::new(Kind(kind), id)),
                Vec::new(),
            );
            report.node_members += 1;
            if batch.ops.len() >= chunk {
                self.store.commit_batch(std::mem::take(&mut batch))?;
                report.chunks += 1;
            }
        }
        // The tail of phase 3 — a partial batch is still a batch, and phase 4
        // commits through its own verb, so nothing may be left in this one.
        if !batch.ops.is_empty() {
            self.store.commit_batch(std::mem::take(&mut batch))?;
            report.chunks += 1;
        }

        // Phase 4 — the handle registry's reverse face, through the registry's OWN
        // rebuild. It is the same sweep-and-derive rule, and it is ONE
        // definition: this used to inline the sweep here, which meant two
        // places had to agree about a face whose key layout is
        // `model::handles`' business. The forward face is authority — it holds
        // `[node_ns][pkey]`, and an edge row names a handle rather than a
        // reference (ADR-0067) — and a handle no edge names any more is NOT
        // reclaimed: that is compaction's business, not a rebuild's.
        Handles::new(&self.store, self.ns).rebuild_reverse()?;
        report.chunks += 1;
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// The reverse index as a stream (0x0D)
// ---------------------------------------------------------------------------

/// What a cluster stream is asked about: one EDGE (`EdgeKey`) or one NODE
/// (`NodeRef`, or a reference to one).
///
/// Plumbing behind the two named reads, not a public verb: the cursor is generic
/// because the walk differs per subject — an edge's `0x0F` run is one prefix scan,
/// a node's `0x13` run is one prefix scan too, and neither is the other's cost.
/// A caller names the subject (`edge_sets_of` / `node_sets_of`) and never picks
/// this trait.
trait ClusterSubject {
    /// The cursor's open — the only thing the two subjects do differently.
    fn open<'a, S: VirtualStorage>(self, store: &'a S, ns: [u8; 2]) -> ClusterStream<'a, S>;
}

impl ClusterSubject for EdgeKey {
    fn open<'a, S: VirtualStorage>(self, store: &'a S, ns: [u8; 2]) -> ClusterStream<'a, S> {
        ClusterStream::of_edge(store, ns, row_of_key(store, ns, &self))
    }
}

impl ClusterSubject for &EdgeKey {
    fn open<'a, S: VirtualStorage>(self, store: &'a S, ns: [u8; 2]) -> ClusterStream<'a, S> {
        ClusterStream::of_edge(store, ns, row_of_key(store, ns, self))
    }
}

/// One edge's stored row identity, read-only: `None` when either endpoint never
/// appeared in an edge, because then no `0x0D` row can name it.
fn row_of_key<S: VirtualStorage>(
    store: &S,
    ns: [u8; 2],
    key: &EdgeKey,
) -> Result<Option<HandleKey>> {
    let registry = Handles::new(store, ns);
    let (Some(src), Some(dst)) = (registry.handle_of(&key.src)?, registry.handle_of(&key.dst)?)
    else {
        return Ok(None);
    };
    Ok(Some(HandleKey::new(src, key.predicate, dst)))
}

impl ClusterSubject for NodeRef {
    fn open<'a, S: VirtualStorage>(self, store: &'a S, ns: [u8; 2]) -> ClusterStream<'a, S> {
        ClusterStream::of_node(store, ns, &self)
    }
}

impl ClusterSubject for &NodeRef {
    fn open<'a, S: VirtualStorage>(self, store: &'a S, ns: [u8; 2]) -> ClusterStream<'a, S> {
        ClusterStream::of_node(store, ns, self)
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
///   [`EdgeStream`] a traversal walks) `flat_map`ed through the edge form —
///   "the clusters my assertions are in" — and then, as the LAST phase, the
///   node's own membership run on the node → cluster face: "the clusters that
///   declare me". The two senses share one cursor, adjacent duplicates collapse,
///   and the phases are ordered so the keys stay ascending.
///
/// # Cost, stated plainly
///
/// The edge form is one scan and reads exactly its own entries. The node form
/// opens one scan of the out face, then one of the in face, then ONE scan per
/// incident edge (`0x0F` is keyed by the tuple, so an edge's clusters are
/// contiguous and nothing else's are) — so the edge half costs
/// `out-degree + in-degree + 1` scans — and then exactly ONE more for the node's
/// own memberships, whatever its degree. The degree term is the price of keying
/// the edge face by the tuple (it is what makes that face alignable with a
/// traversal); the declared half is one scan because it is keyed by the handle.
/// A hub is therefore cheap for the question a node cluster answers and
/// expensive only for the question an edge cluster answers. A caller that needs
/// one cluster's members the other way round takes
/// [`members`](Hyperedges::members) or [`nodes`](Hyperedges::nodes) — those
/// faces are keyed by cluster, and each is one scan.
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
    /// The cursor over the CURRENT key space: one edge's `[kind][hyper_id]` run.
    scan: Option<ScanIter>,
    /// Bytes of key before the `[kind][hyper_id]` pair.
    suffix_at: usize,
    /// The entry on top: its raw key (the [`Seekable`] position) and the cluster
    /// it names.
    cur: Option<(Vec<u8>, ClusterRef)>,
    /// The edge this stream is about, in the edge form — `None` for the node
    /// form, and for an edge whose endpoints never appeared.
    edge: Option<HandleKey>,
    /// Whether this stream was opened on an EDGE (the form is a property of the
    /// QUESTION, not of whether the run turned out to be empty).
    edge_form: bool,
    /// The node this stream is about, in the node form.
    node: Option<NodeRef>,
    /// The node's out-edges, then (lazily) its in-edges — the two directions
    /// concatenated are still ascending by key, because `0x06` < `0x07`.
    out: Option<EdgeStream<'a, S>>,
    inn: Option<EdgeStream<'a, S>>,
    /// Whether the node form's LAST phase — the node → cluster run, `0x13` —
    /// has been opened. It comes after the incident-edge phases so the stream
    /// stays ascending by key: every `0x0F` entry sorts before every `0x13` one.
    mirror_open: bool,
    /// The last cluster answered, for adjacent-duplicate collapse.
    last: Option<ClusterRef>,
    /// The fault that ended the walk, if one did — [`Seekable`] has no channel
    /// for one, the same rule [`EdgeStream::error`] follows.
    err: Option<Error>,
}

impl<'a, S: VirtualStorage> ClusterStream<'a, S> {
    /// One edge's clusters. A row identity of `None` (an endpoint the graph never
    /// saw) is an EMPTY answer: no `0x0D` entry can name an edge that is not
    /// there.
    fn of_edge(store: &'a S, ns: [u8; 2], row: Result<Option<HandleKey>>) -> Self {
        let mut stream = Self {
            store,
            ns,
            scan: None,
            suffix_at: 0,
            cur: None,
            edge: None,
            edge_form: true,
            node: None,
            out: None,
            inn: None,
            mirror_open: false,
            last: None,
            err: None,
        };
        match row {
            Ok(Some(row)) => {
                stream.edge = Some(row);
                stream.open_edge(&row);
                stream.fill();
            }
            Ok(None) => {}
            Err(e) => stream.err = Some(e),
        }
        stream
    }

    /// One node's clusters: its incident edges, flattened.
    fn of_node(store: &'a S, ns: [u8; 2], node: &NodeRef) -> Self {
        let mut stream = Self {
            store,
            ns,
            scan: None,
            suffix_at: 0,
            cur: None,
            edge: None,
            edge_form: false,
            node: Some(node.clone()),
            out: Some(EdgeStream::new(store, ns, Direction::Out, node.clone())),
            inn: None,
            mirror_open: false,
            last: None,
            err: None,
        };
        stream.fill();
        stream
    }

    /// Point the cursor at one edge's run — the key space changes, so the
    /// position does too.
    fn open_edge(&mut self, row: &HandleKey) {
        let prefix = edge_clusters_prefix(&self.ns, row);
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
            self.inn = Some(EdgeStream::new(self.store, self.ns, Direction::In, node));
        }
        self.inn.as_mut()?.next()
    }

    /// One entry's cluster: `[kind][hyper_id]` after the scanned prefix, nothing
    /// else. A suffix of another width is not this face's and is skipped.
    fn decode(&self, key: &[u8]) -> Option<ClusterRef> {
        let suffix = key.get(self.suffix_at..)?;
        if suffix.len() != CLUSTER_KIND_WIDTH + U64_WIDTH {
            return None;
        }
        Some(ClusterRef::new(
            Kind(be_vocab(&suffix[..CLUSTER_KIND_WIDTH])?),
            be_u64(&suffix[CLUSTER_KIND_WIDTH..])?,
        ))
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
                Some(step) => self.open_edge(&step.row()),
                // The incident-edge phases are done. The node's OWN memberships
                // are the last phase; a second arrival here means nothing is left.
                None => {
                    if !self.open_mirror() {
                        return;
                    }
                }
            }
        }
    }

    /// Open the node form's last phase — the node → cluster run, `0x13` — once.
    /// The handle is that run's prefix, so the node is resolved exactly once, and
    /// only after the incident-edge phases have answered everything they had.
    /// `false` means there is no further key space (already open, or the node
    /// never reached the registry, in which case it cannot be a member either).
    fn open_mirror(&mut self) -> bool {
        if self.mirror_open {
            return false;
        }
        self.mirror_open = true;
        let Some(node) = self.node.clone() else {
            return false;
        };
        let handle = match Handles::new(self.store, self.ns).handle_of(&node) {
            Ok(Some(handle)) => handle,
            Ok(None) => return false,
            Err(e) => {
                self.err = Some(e);
                return false;
            }
        };
        let prefix = node_clusters_prefix(&self.ns, handle);
        self.suffix_at = prefix.len();
        self.cur = None;
        let end = prefix_end(&prefix);
        self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
        true
    }

    /// The fault that ended this walk, if an engine failed mid-scan.
    pub fn error(&self) -> Option<&Error> {
        self.err.as_ref()
    }

    /// The edge this stream was opened on — `None` for the node form, whose key
    /// space changes as it walks.
    pub fn edge(&self) -> Option<&HandleKey> {
        self.edge.as_ref()
    }

    /// Whether this stream walks one edge's run or a node's incident edges.
    pub fn is_edge_form(&self) -> bool {
        self.edge_form
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

/// Is `row` a member of `cluster`? One point read of the derived face — the
/// read that makes a membership test a `.filter` predicate inside a traversal
/// instead of a set built beforehand.
fn member_of_in<S: VirtualStorage + ?Sized>(
    store: &S,
    ns: &[u8; 2],
    cluster: ClusterRef,
    row: &HandleKey,
) -> Result<bool> {
    Ok(store.get(&edge_cluster_key(ns, row, cluster))?.is_some())
}

impl<S: VirtualStorage> Hyperedges<S> {
    /// The clusters one edge belongs to, in `(kind, hyper_id)` order — the
    /// derived face read by its leading segment.
    pub fn edge_sets_of(&self, edge: &EdgeKey) -> ClusterStream<'_, S> {
        ClusterStream::of_edge(&self.store, self.ns, row_of_key(&self.store, self.ns, edge))
    }

    /// Is this edge a member of this cluster? The derived face's exact key, one
    /// read — `Ok(false)` for an edge whose endpoints never appeared, since no
    /// row can name it.
    pub fn edge_member_of(&self, set: EdgeSet, edge: &EdgeKey) -> Result<bool> {
        match row_of_key(&self.store, self.ns, edge)? {
            Some(row) => member_of_in(&self.store, &self.ns, set.into(), &row),
            None => Ok(false),
        }
    }

    /// Is this ENTITY a member of this cluster? The node member face's exact key,
    /// one read — and the answer for an entity that has no edge at all, which is
    /// what a declared node member is for (ADR-0068).
    pub fn node_member_of(&self, set: NodeSet, node: &NodeRef) -> Result<bool> {
        let Some(handle) = Handles::new(&self.store, self.ns).handle_of(node)? else {
            // A node the registry never saw cannot be a member: no row names it.
            return Ok(false);
        };
        Ok(self
            .store
            .get(&node_member_key(&self.ns, set.into(), handle))?
            .is_some())
    }
}

impl<S: VirtualStorage, G: GraphEncode> Graph<S, G> {
    /// The clusters one edge belongs to — [`ClusterStream`] over `0x0D`, or over
    /// a node's incident edges when given a [`NodeRef`] (see the type's cost
    /// note: the node form costs one scan per incident edge).
    ///
    /// ```ignore
    /// for set in graph.edge_sets_of(&key) { /* derived, ordered by edge */ }
    /// for set in graph.node_sets_of(&node) { /* declared memberships, by handle */ }
    /// ```
    /// The ASSERTION groups this edge is a member of — one read of `0x0D`.
    pub fn edge_sets_of(&self, edge: &EdgeKey) -> ClusterStream<'_, S> {
        ClusterSubject::open(edge, self.store(), self.ns().to_be_bytes())
    }

    /// The ENTITY groups this entity is a member of — `0x13`, whose answer is
    /// independent of the entity's degree.
    pub fn node_sets_of(&self, node: &NodeRef) -> ClusterStream<'_, S> {
        ClusterSubject::open(node, self.store(), self.ns().to_be_bytes())
    }

    /// Is `edge` a member of cluster `(kind, cluster)`? One point read of the
    /// derived face — `Ok(false)` for a member row that is not there.
    ///
    /// Not a `bool`: a point read can fault, and a fault reported as \"not a
    /// member\" is how a broken store would read as an empty cluster. The
    /// traversal-filter spelling is
    /// `.filter(|step| graph.edge_member_of(kind, cluster, &graph.edge_key(step)?)...)`
    /// — the `unwrap_or` is the caller saying what a fault means to it.
    pub fn edge_member_of(&self, kind: &str, cluster: u64, edge: &EdgeKey) -> Result<bool> {
        let ns = self.ns().to_be_bytes();
        let mut door = Faces::<_, ClusterKindId>::new(self.store(), &ns, CLUSTER_KIND);
        let Some(kind) = door.name_id(kind)?.map(Kind) else {
            return Ok(false);
        };
        match row_of_key(self.store(), self.ns().to_be_bytes(), edge)? {
            Some(row) => member_of_in(
                self.store(),
                &self.ns().to_be_bytes(),
                ClusterRef::new(kind, cluster),
                &row,
            ),
            None => Ok(false),
        }
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
    /// Whether the payload came from the caller (so a set's `on_*_set_conflict` runs
    /// on it), as opposed to being carried over from the stored row.
    from_caller: bool,
    /// Which spelling declared this row (`on_edge_set_conflict` /
    /// `on_node_set_conflict` are two registration points, so the plan entry has
    /// to carry the kind it was declared with). Only read when `from_caller`.
    node_set: bool,
    /// Whether a declaration asked for this cluster (as opposed to an op that
    /// only moves members).
    declared: bool,
    /// The cluster row is removed outright: an explicit `delete`, or an
    /// `incorporate` that merges this cluster away as a source.
    delete_row: bool,
    add: BTreeSet<HandleKey>,
    remove: BTreeSet<HandleKey>,
    /// The entities the caller DECLARES as members (ADR-0068) — resolved to
    /// handles only at write time, since a node may not exist yet.
    add_nodes: BTreeSet<NodeRef>,
    remove_nodes: BTreeSet<NodeRef>,
}

/// One cluster write's outcome: the exact rows to put and delete.
struct ClusterOutcome {
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

enum ClusterOp {
    /// Drop this batch's edges from a cluster.
    RemoveEdges(ClusterRef),
    /// Drop these nodes from a cluster's DECLARED membership.
    RemoveNodes(ClusterRef, BTreeSet<NodeRef>),
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

/// One cluster's entry in a batch's plan, keyed by `(kind id, cluster id)`.
/// Both verbs build one, and `replace` hands the retract half's to the write half
/// so the counts are computed once.
type ClusterPlanMap = BTreeMap<(ClusterKindId, u64), ClusterPlan>;

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
    /// Assertions moved from one head to another: `(from, to)`. The payload
    /// travels inside okm, so a caller moving an edge's end never holds it.
    repoints: Vec<(EdgeHead, EdgeHead)>,
    node_deletes: Vec<NodeRef>,
    node_conflicts: Vec<Box<dyn NodeConflict>>,
    edge_conflict: Option<EdgeConflict<G>>,
    edge_set_conflict: Option<ClusterConflict>,
    node_set_conflict: Option<ClusterConflict>,
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
            repoints: Vec::new(),
            node_deletes: Vec::new(),
            node_conflicts: Vec::new(),
            edge_conflict: None,
            edge_set_conflict: None,
            node_set_conflict: None,
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
    pub fn on_edge_set_conflict(
        &mut self,
        f: impl Fn(&Dict, &Dict) -> Dict + 'static,
    ) -> &mut Self {
        self.edge_set_conflict = Some(Box::new(f));
        self
    }

    /// The ENTITY half of [`on_edge_set_conflict`](Self::on_edge_set_conflict):
    /// the same two-payload shape, chosen by which spelling declared the row.
    /// Two registration points, not one hook that has to guess the kind.
    pub fn on_node_set_conflict(
        &mut self,
        f: impl Fn(&Dict, &Dict) -> Dict + 'static,
    ) -> &mut Self {
        self.node_set_conflict = Some(Box::new(f));
        self
    }

    /// Drop this batch's edges from `cluster`. The cluster row stays: a scope is
    /// not an alias for the edges inside it, so losing the last member leaves an
    /// empty cluster with an honest count of zero (ADR-0066 §3).
    pub fn remove_edges(&mut self, set: EdgeSet) -> &mut Self {
        self.ops.push(ClusterOp::RemoveEdges(set.into()));
        self
    }

    /// Retract a DECLARED membership: these entities leave `cluster`, and the
    /// entity itself stays in the graph untouched — the claim being withdrawn is
    /// about the group, not about the node.
    ///
    /// It speaks for the declared half only. If a member EDGE of the same cluster
    /// still has one of these nodes as an endpoint, "who is in C" reports it
    /// anyway, because that half is a projection of the member edges (ADR-0068)
    /// and this verb does not touch an edge. A declaration in the same fragment
    /// wins over a removal, so a fragment that both adds and removes a node adds.
    pub fn remove_nodes<I: IntoIterator<Item = NodeRef>>(
        &mut self,
        set: NodeSet,
        nodes: I,
    ) -> &mut Self {
        self.ops
            .push(ClusterOp::RemoveNodes(set.into(), nodes.into_iter().collect()));
        self
    }

    /// Remove `cluster` outright: its row, its member rows and its derived rows,
    /// in this batch. Idempotent — a cluster that is not there has nothing to
    /// delete, the same rule the engine's own `del` follows.
    pub fn delete_edges(&mut self, set: EdgeSet) -> &mut Self {
        self.ops.push(ClusterOp::Delete(set.into()));
        self
    }

    /// Remove an ENTITY group outright: the row, its declared member rows and the
    /// entities' mirror rows, in this batch. The entities themselves stay.
    pub fn delete_nodes(&mut self, set: NodeSet) -> &mut Self {
        self.ops.push(ClusterOp::Delete(set.into()));
        self
    }

    /// Union `sources`' members into `into` and remove the sources.
    pub fn incorporate<I: IntoIterator<Item = EdgeSet>>(
        &mut self,
        sources: I,
        into: EdgeSet,
    ) -> &mut Self {
        self.incorporate_rows(sources.into_iter().map(Into::into).collect(), into.into())
    }

    /// Union several ENTITY groups into one, deleting the sources — the entity
    /// half of [`incorporate`](Self::incorporate). The op carries both faces
    /// (a merged set keeps its edges and its declared entities); the two entry
    /// points exist so a caller cannot mix the kinds in one call.
    pub fn incorporate_nodes<I: IntoIterator<Item = NodeSet>>(
        &mut self,
        sources: I,
        into: NodeSet,
    ) -> &mut Self {
        self.incorporate_rows(sources.into_iter().map(Into::into).collect(), into.into())
    }

    fn incorporate_rows(&mut self, sources: Vec<ClusterRef>, into: ClusterRef) -> &mut Self {
        self.ops.push(ClusterOp::Incorporate { sources, into });
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
    /// rows and its `0x0B` rows leave together, and every cluster that held the
    /// edge keeps its row with a lowered count — losing a member is not losing a
    /// scope. A miss would be a freshness problem rather than a correctness one
    /// (the child index rebuilds), which is why nothing here needs a trigger or a
    /// sweeper.
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

    /// Assert `to` with the payload the row at `from` holds, and retract `from` —
    /// one action, one batch.
    ///
    /// This is re-attachment: an edge's head IS its identity (ADR-0067), so
    /// moving one end is not an edit, it is a different row. Doing that as
    /// `delete_edge` + a declaration would lose the payload, because a
    /// declaration names a head and the new row's payload is the type's default.
    /// So the payload is read from the row being replaced and carried:
    ///
    /// - `to` has no row: the source payload lands as is,
    /// - `to` already has one: `on_edge_conflict(stored at 'to', source payload)`
    ///   decides — the merge of two assertions, with the moved one as the
    ///   declared side.
    ///
    /// The open attributes (the dynamic segment keyed by the edge) are NOT okm's:
    /// they are a segment of their own, so a caller that wants them moved moves
    /// them. Everything okm owns — the row, the three derived faces, the
    /// predicate faces, the cluster memberships — follows in the same batch.
    ///
    /// Only [`Graph::replace`] takes a fragment carrying these: `incorporate` and
    /// `retract` each have one meaning, and a repoint is both.
    pub fn repoint(&mut self, from: EdgeHead, to: EdgeHead) -> &mut Self {
        self.repoints.push((from, to));
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
            + self.repoints.len()
            + self.clusters.len()
            + self.node_conflicts.len()
            + usize::from(self.edge_conflict.is_some())
            + usize::from(self.edge_set_conflict.is_some())
            + usize::from(self.node_set_conflict.is_some())
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
        let deletions = self.edge_deletes.len() + self.node_deletes.len() + self.repoints.len();
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
        mut plan: ClusterPlanMap,
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
        //
        // The handle cache is per-FRAGMENT: an endpoint named by several edges
        // costs one registry probe, and every handle this batch issues rides the
        // batch itself (a refused fragment leaves none behind).
        let mut cache = HandleCache::default();
        let mut batch_rows: Vec<HandleKey> = Vec::new();

        // ---- 2a. The repoints: read each moved payload, then state both ends ----
        //
        // The source row is read BEFORE anything is written, and its payload is
        // resolved here rather than in the loop below: a moved assertion has a
        // declared side the loop cannot compute (a declaration carries no
        // payload). The pairing — state `to`, retract `from` — is what makes the
        // move one assertion rather than a gap.
        let mut carried: BTreeMap<crate::model::graph::EdgeIdentity, G> = BTreeMap::new();
        for (from, to) in std::mem::take(&mut self.repoints) {
            let Some(from_key) = graph.key_of(&from)? else {
                continue;
            };
            let Some(body) = graph.get_edge(&from_key)? else {
                continue;
            };
            let to_key = graph.write_key(&to)?;
            let moved = G::decode_payload(&body.payload);
            let stored = graph.get_edge(&to_key)?.map(|row| row.payload);
            let payload = match (stored.as_deref(), self.edge_conflict.as_ref()) {
                // Both ends hold a live row: the hook merges, the moved one is the
                // declared side.
                (Some(bytes), Some(resolve)) => resolve(&G::decode_payload(bytes), &moved),
                (Some(_), None) => moved,
                // The target is free: the payload lands as is, unless a hook wants
                // to see that too (default, moved).
                (None, Some(resolve)) => resolve(&G::default(), &moved),
                (None, None) => moved,
            };
            carried.insert(
                (
                    to_key.src.ns_id(),
                    to_key.src.pkey.clone(),
                    to_key.predicate,
                    to_key.dst.ns_id(),
                    to_key.dst.pkey.clone(),
                ),
                payload,
            );
            self.rows.push(Decl::Edge(to));
            self.edge_deletes.push(from);
        }

        for decl in &self.rows {
            let Decl::Edge(head) = decl else {
                continue;
            };
            // The vocabulary id is part of the row, so the predicate is interned
            // before the existence probe — the write would intern it anyway.
            let key = graph.write_key(head)?;
            // A moved assertion: the prepass resolved its payload, and the row it
            // replaces is already queued for retraction.
            if let Some(payload) = carried.remove(&(
                key.src.ns_id(),
                key.src.pkey.clone(),
                key.predicate,
                key.dst.ns_id(),
                key.dst.pkey.clone(),
            )) {
                // `row_of` answers whenever both handles are on file, and handles
                // outlive their assertions — the 0x06 row is what says an edge is
                // there, the same distinction the ordinary declare path makes.
                let live = graph.get_edge(&key)?.is_some();
                let row = match graph.row_of(&key)? {
                    Some(row) => {
                        if live {
                            graph.unlink_into(batch, &row)?;
                        }
                        graph.assert_into(batch, head, &payload, &mut cache)?;
                        row
                    }
                    None => graph.link_into(batch, head, &payload, &mut cache)?.1,
                };
                batch_rows.push(row);
                report.edges.push(key);
                continue;
            }
            let row = match graph.row_of(&key)? {
                Some(row) => {
                    // The handles are on file, but handle rows are permanent:
                    // they outlive the assertion they were issued for, so
                    // "the registry knows both endpoints" says nothing about
                    // whether this edge exists. The 0x06 row is the edge, so
                    // the stored payload — not the handle lookup — is what
                    // decides create-vs-update.
                    let stored = graph.get_edge(&key)?.map(|body| body.payload);
                    // The stored payload and the declared one go to the hook,
                    // whose answer is what lands: an update is a write, and it
                    // keeps the identity. An edge declaration names a head, not
                    // a payload, so the declared side is the type's default.
                    let declared = G::default();
                    let payload = match stored.as_deref() {
                        Some(bytes) => {
                            let stored_payload = G::decode_payload(bytes);
                            match self.edge_conflict.as_ref() {
                                Some(resolve) => resolve(&stored_payload, &declared),
                                None => stored_payload,
                            }
                        }
                        None => match self.edge_conflict.as_ref() {
                            Some(resolve) => resolve(&declared, &declared),
                            None => declared,
                        },
                    };
                    // The entries the OLD payload wrote into the `#[ok_index]`
                    // faces are only re-derivable from the old payload, so the
                    // previous assertion's rows leave first and the re-assertion
                    // lands in the same batch: the edge is never absent at
                    // commit. An absent row has no entries to re-derive — that
                    // is a creation at an identity the handles already fixed,
                    // and unlinking it would only fail on the missing row.
                    //
                    // `unlink_into` is not a delete: it takes the row, the three
                    // derived faces and the declared faces, and leaves the
                    // dynamic segment (0x01) alone — the open attributes are a
                    // segment of their own and no declaration's to clear.
                    if stored.is_some() {
                        graph.unlink_into(batch, &row)?;
                    }
                    graph.assert_into(batch, head, &payload, &mut cache)?;
                    row
                }
                None => {
                    // A creation: no row to compare against, so the hook sees
                    // (declared, declared) — its answer is the payload.
                    let declared = G::default();
                    let payload = match self.edge_conflict.as_ref() {
                        Some(resolve) => resolve(&declared, &declared),
                        None => declared,
                    };
                    graph.link_into(batch, head, &payload, &mut cache)?.1
                }
            };
            batch_rows.push(row);
            report.edges.push(key);
        }
        let batch_edges: Vec<HandleKey> = batch_rows;

        // ---- 3. Cluster ids: allocated at run time, per kind ----
        // A declaration speaks a LABEL and this is where it becomes an id, in the
        // same batch that writes the row — exactly what an edge's predicate does.
        // The vocabulary row lands with the cluster (0x0A/0x0B), so a caller never
        // mints a kind before naming one, and the two cannot drift.
        // A declaration speaks a LABEL; this is where it becomes an id, in the
        // batch that writes the row — the same door a predicate comes through.
        let mut declared_kinds: Vec<Kind> = Vec::with_capacity(self.clusters.len());
        for cluster in &self.clusters {
            declared_kinds.push(intern_kind(graph.store(), &ns, &cluster.kind)?);
        }
        let mut kinds: BTreeSet<ClusterKindId> = BTreeSet::new();
        for kind in &declared_kinds {
            kinds.insert(kind.0);
        }
        for op in &self.ops {
            match op {
                ClusterOp::RemoveEdges(cluster) | ClusterOp::RemoveNodes(cluster, _) => {
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
        let mut next_id: BTreeMap<ClusterKindId, u64> = BTreeMap::new();
        for kind in kinds {
            let base = next_free_cluster(graph.store(), &ns, Kind(kind))?;
            next_id.insert(kind, base);
        }
        // An explicit id reserves the range above it, so a `None` declared
        // BEFORE it cannot take a number it has already claimed — and `0` is
        // reserved for "no parent", so it is nobody's id.
        for (cluster, kind) in self.clusters.iter().zip(&declared_kinds) {
            match cluster.id {
                Some(0) => return Err(Error::ReservedClusterId),
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

        // ---- 4. The plan: one record per cluster this batch touches ----
        //
        // The plan ARRIVES seeded: `replace` hands the retract half's entries in,
        // so one `apply_plan` sees the removals and the additions together and the
        // maintained counts are computed once, from one base.
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
                entry.add.extend(batch_edges.iter().cloned());
            }
            entry.add_nodes.extend(cluster.nodes.iter().cloned());
        }
        for op in &self.ops {
            match op {
                ClusterOp::RemoveEdges(cluster) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove
                        .extend(batch_edges.iter().cloned());
                }
                ClusterOp::RemoveNodes(cluster, nodes) => {
                    plan.entry((cluster.kind.0, cluster.id))
                        .or_default()
                        .remove_nodes
                        .extend(nodes.iter().cloned());
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
                        let state = read_cluster(graph.store(), &ns, *source)?;
                        if !state.exists {
                            return Err(Error::ClusterNotFound {
                                kind: source.kind.0,
                                id: source.id,
                            });
                        }
                        let target = plan.entry((into.kind.0, into.id)).or_default();
                        target.add.extend(state.members.iter().cloned());
                        target
                            .add_nodes
                            .extend(state.nodes.iter().filter_map(|h| resolve_handle(graph.store(), &ns, *h)));
                        let gone = plan.entry((source.kind.0, source.id)).or_default();
                        gone.delete_row = true;
                        gone.remove.extend(state.members.iter().cloned());
                        gone.remove_nodes
                            .extend(state.nodes.iter().filter_map(|h| resolve_handle(graph.store(), &ns, *h)));
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
        plan: BTreeMap<(ClusterKindId, u64), ClusterPlan>,
        batch: &mut Batch,
    ) -> Result<usize> {
        // Every declared entity needs its handle before a row can be keyed, and a
        // cluster membership is the ONE place a node may appear before it is an
        // endpoint (ADR-0068 §4) — so the handle is issued here on first sight.
        // A removal never issues one: an entity the registry never saw was never
        // a member either.
        let handles = Handles::new(graph.store(), *ns);
        let mut node_handle: BTreeMap<(u16, Vec<u8>), Handle> = BTreeMap::new();
        for entry in plan.values() {
            for node in entry.add_nodes.iter() {
                let slot = (node.ns_id(), node.pkey.clone());
                if node_handle.contains_key(&slot) {
                    continue;
                }
                let handle = handles.issue_into(batch, node)?;
                node_handle.insert(slot, handle);
            }
            for node in entry.remove_nodes.iter() {
                let slot = (node.ns_id(), node.pkey.clone());
                if node_handle.contains_key(&slot) {
                    continue;
                }
                if let Some(handle) = handles.handle_of(node)? {
                    node_handle.insert(slot, handle);
                }
            }
        }
        let mut deleted_members = 0usize;
        let mut outcomes: Vec<ClusterOutcome> = Vec::with_capacity(plan.len());
        for ((kind, id), mut entry) in plan {
            let cluster = ClusterRef::new(Kind(kind), id);
            let state = read_cluster(graph.store(), ns, cluster)?;
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
            // A cluster is a scope: it is created when declared and deleted only
            // when explicitly deleted. No membership count — not even zero —
            // takes the row away, so an empty or one-member cluster is a legal
            // answer rather than a dropped one.
            let delete_row = entry.delete_row;

            let mut payload = state.payload.clone();
            if !delete_row {
                // The two sides the hook compares: the stored map, and that map
                // with this declaration's entries merged in. Without a hook the
                // merged map is what lands, which is the plain write.
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

            // The same rule for the entity set: a declared member wins over a
            // removal, and the delta is what lands. Handles are already the row
            // space, so nothing here resolves anything.
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

            if !delete_row {
                payload.insert(MEMBER_COUNT.to_string(), Variant::U64(members.len() as u64));
                payload.insert(NODE_COUNT.to_string(), Variant::U64(node_members.len() as u64));
            }
            entry.parent = Some(entry.parent.unwrap_or(state.parent));
            deleted_members += del_members.len() + del_nodes.len();
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
            for handle in &outcome.del_nodes {
                adapter.del(&node_member_key(ns, cluster, *handle))?;
                adapter.del(&node_cluster_key(ns, *handle, cluster))?;
            }
            if outcome.delete_row {
                adapter.del(&cluster_key(ns, cluster.kind, cluster.id))?;
                continue;
            }
            for edge in &outcome.put_members {
                adapter.put(edge_member_key(ns, cluster, edge), Vec::new())?;
                adapter.put(edge_cluster_key(ns, edge, cluster), Vec::new())?;
            }
            for handle in &outcome.put_nodes {
                adapter.put(node_member_key(ns, cluster, *handle), Vec::new())?;
                adapter.put(node_cluster_key(ns, *handle, cluster), Vec::new())?;
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
        let report = fragment.write_into(self, &mut rows, BTreeMap::new())?;
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
    ///   membership rows — plus the node's OWN memberships in node clusters
    ///   (ADR-0068), found through the node → cluster face in one scan. A node
    ///   cannot be left with a dangling edge or a membership it no longer owns.
    /// - deleting an EDGE leaves both endpoints alone, even when they are then
    ///   connected to nothing, and leaves every node cluster alone: an entity
    ///   does not leave a group because an assertion about it went away.
    ///
    /// Refusals are the caller's to ignore, and nothing here refuses for
    /// absence: an id no row holds, or a node no edge names, is nothing to do
    /// and reports zero. Everything runs in ONE batch, so a reader sees the
    /// whole removal or none of it.
    fn retract_plan(
        &mut self,
        fragment: &Fragment<G>,
    ) -> Result<(BTreeSet<HandleKey>, BTreeSet<NodeRef>, ClusterPlanMap)> {
        let ns: [u8; 2] = self.ns().to_be_bytes();
        // ---- 1. The doomed edges: a node's cascade first, then the declared
        //         heads, deduplicated — one edge is one removal whatever named it.
        //
        // A declaration names a head and the head IS the identity (ADR-0067), so
        // a doomed edge is stated, never looked up by number.
        let mut doomed: BTreeSet<HandleKey> = BTreeSet::new();
        let mut nodes: BTreeSet<NodeRef> = BTreeSet::new();
        // The plan is opened here because a node's cascade now states TWO things:
        // the edges to drop, and the node's own memberships (ADR-0068).
        let mut plan: BTreeMap<(ClusterKindId, u64), ClusterPlan> = BTreeMap::new();
        for node in &fragment.node_deletes {
            // The cascade is read in HANDLE space: the node's out and in faces
            // are keyed by its handle, so the doomed rows come off the entries
            // with no endpoint resolution at all.
            let Some(near) = self.handles().handle_of(node)? else {
                continue;
            };
            let mut incident: Vec<HandleKey> = Vec::new();
            for (suffix, _) in scan_suffix_kv(self.store(), &out_prefix(&ns, near))? {
                if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                    incident.push(HandleKey::new(near, predicate, far));
                }
            }
            for (suffix, _) in scan_suffix_kv(self.store(), &in_prefix(&ns, near))? {
                if let Some((predicate, far)) = parse_predicate_far(&suffix) {
                    incident.push(HandleKey::new(far, predicate, near));
                }
            }
            // The entity's own memberships: one prefix scan of the node →
            // cluster face — the precise question, where the edge path has to
            // sweep a whole member face to answer its version.
            let mut held: Vec<ClusterRef> = Vec::new();
            for (suffix, _) in scan_suffix_kv(self.store(), &node_clusters_prefix(&ns, near))? {
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
                // Neither an endpoint nor a declared member: the registry has no
                // handle for it at all, so nothing here is in the graph.
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
        // A repoint RETRACTS the head it replaces, so its source has to reach the
        // plan too. Without this the moved assertion is duplicated instead of
        // moved whenever no node deletion happens to cascade over it — the
        // cascade hid the bug, because a merge deletes a node and a split does
        // not.
        for head in fragment
            .edge_deletes
            .iter()
            .chain(fragment.repoints.iter().map(|(from, _)| from))
        {
            // A predicate nobody interned names no row: absence is nothing to do.
            if let Some(key) = self.key_of(head)?
                && let Some(row) = self.row_of(&key)?
            {
                doomed.insert(row);
            }
        }

        // ---- 2. Membership rows follow the edges: ONE pass over 0x0B ----
        //
        // A member row is `[kind][cluster][edge row]`, so an edge's clusters are
        // found by SUFFIX. There is no reverse index, and none is needed — this
        // runs once per retract, not once per edge. Both sides are row
        // identities, so the containment test is a byte comparison.
        let prefix = face_prefix(&ns, EDGE_MEMBER_SLOT);
        for (suffix, _) in scan_suffix_kv(self.store(), &prefix)? {
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
            if doomed.contains(&row) {
                plan.entry((kind, id)).or_default().remove.insert(row);
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

    pub fn retract(&mut self, mut fragment: Fragment<G>) -> Result<RetractReport> {
        fragment.check_retractable()?;
        let ns: [u8; 2] = self.ns().to_be_bytes();
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
            self.purge_into(&mut batch, row)?;
            report.edges += 1;
        }

        // ---- 4. The cluster faces, through the same path a write uses ----
        report.memberships = fragment.apply_plan(self, &ns, &BTreeSet::new(), plan, &mut batch)?;
        self.store().commit_batch(batch)?;
        report.nodes = nodes.len();
        Ok(report)
    }

    /// Every half-edge a retired entity would cut has to be STATED in the
    /// fragment — repointed or explicitly deleted.
    ///
    /// The cascade in [`Graph::retract`] removes a node's edges without being
    /// told to, and that is right for a deletion: the assertions are gone anyway.
    /// A REPLACEMENT is a different claim — "this shape becomes that one" — and a
    /// connection nobody decided about is exactly the data loss a replacement is
    /// supposed to make impossible. So the cascade is not allowed to be the
    /// answer here: the caller names each cut, and a fragment that forgets one is
    /// refused rather than completed by guessing.
    ///
    /// The cost is one scan per deleted node's two traversal faces, bounded by
    /// its degree.
    fn check_cut_is_stated(&mut self, fragment: &Fragment<G>) -> Result<()> {
        if fragment.node_deletes.is_empty() {
            return Ok(());
        }
        let mut stated: BTreeSet<crate::model::graph::EdgeIdentity> = BTreeSet::new();
        for head in fragment
            .edge_deletes
            .iter()
            .chain(fragment.repoints.iter().map(|(from, _)| from))
        {
            let Some(key) = self.key_of(head)? else {
                continue;
            };
            stated.insert((
                key.src.ns_id(),
                key.src.pkey.clone(),
                key.predicate,
                key.dst.ns_id(),
                key.dst.pkey.clone(),
            ));
        }
        let mut unaccounted = 0usize;
        for node in &fragment.node_deletes {
            let mut incident = self.out_edges(node)?;
            incident.extend(self.in_edges(node)?);
            for key in incident {
                let id = (
                    key.src.ns_id(),
                    key.src.pkey.clone(),
                    key.predicate,
                    key.dst.ns_id(),
                    key.dst.pkey.clone(),
                );
                if !stated.contains(&id) {
                    unaccounted += 1;
                }
            }
        }
        if unaccounted > 0 {
            return Err(Error::ReplaceIncomplete { unaccounted });
        }
        Ok(())
    }

    /// Persist a selection as a cluster: the members become rows of a node set,
    /// and the answer is the set's identity.
    ///
    /// `subgraph` and `cut` produce selections that exist only in memory — right
    /// for a read, wrong for one that will be asked about again. This is the one
    /// step that turns a computed selection into a stored partition, so an
    /// application never hand-builds the cluster declaration to keep one.
    ///
    /// It is a WRITE: one batch, members stated as they are. A member the graph
    /// never saw has no handle, so it contributes no row — the set records what
    /// exists, not what was asked for.
    pub fn materialize(
        &mut self,
        selection: super::graph::Selection,
        kind: &str,
        payload: Dict,
    ) -> Result<super::hyper::NodeSet> {
        let members = self.resolve_selection(selection)?;
        let mut fragment: Fragment<G> = Fragment::new([]);
        fragment.cluster(Cluster::nodes(kind, payload, members));
        let report = self.incorporate(fragment)?;
        report
            .node_sets
            .first()
            .cloned()
            .ok_or(crate::error::Error::Corrupt {
                what: "a materialized selection allocated no node set".into(),
            })
    }

    /// Replace a subgraph: ONE fragment may retract and declare, and the batch
    /// lands as a single state change.
    ///
    /// The two halves are planned together, not run one after the other:
    /// [`Graph::retract`]'s removals become the seed of the write half's cluster
    /// plan, so every touched cluster is read once, counted once and written
    /// once. Run as two passes over one batch the second pass would read the
    /// pre-batch state and re-stamp a count that ignores the first half's rows —
    /// the shape of a silent corruption this verb exists to avoid.
    ///
    /// The rules a caller lives with:
    ///
    /// - **A cluster rides the fragment.** An edge the fragment declares joins
    ///   the cluster this fragment declares it in (`Cluster::edges`); with no such
    ///   declaration the edge belongs to no cluster, and the old cluster it left
    ///   is not re-created. A `delete` clears the row first, and a declaration at
    ///   the same id then merges into what is left — the plan is keyed by
    ///   `(kind, id)`, one entry per cluster either way.
    /// - **A head both removed and declared ends up declared.** The batch is an
    ///   ordered list of ops and the later one wins, the same rule the cluster
    ///   plan already states for `add` over `remove`.
    /// - **Nothing here is derived.** No reduce runs for a replacement, and no
    ///   counter is invented: what the fragment declares is what lands.
    ///
    /// Refusals are the caller's to ignore, exactly as in [`Graph::retract`]: a
    /// head no row holds is nothing to do.
    pub fn replace(&mut self, fragment: Fragment<G>) -> Result<ReplaceReport> {
        self.check_cut_is_stated(&fragment)?;
        let mut batch = Batch::default();
        let (doomed, nodes, plan) = self.retract_plan(&fragment)?;
        let mut edges = 0;
        for row in &doomed {
            if !self.has_row(row)? {
                continue;
            }
            self.purge_into(&mut batch, row)?;
            edges += 1;
        }
        let written = fragment.write_into(self, &mut batch, plan)?;
        self.store().commit_batch(batch)?;
        Ok(ReplaceReport {
            nodes: nodes.len(),
            edges,
            written,
        })
    }
}

#[cfg(test)]
mod tests {
use crate::model::handles::HANDLE_WIDTH;
use crate::model::index::{CLUSTER_KIND_ID_SLOT, CLUSTER_KIND_NAME_SLOT};

// The CALLER's vocabulary, defined here because a test is a caller: `Kind` is one
// interned id and okm assigns it no meaning. An application picks its own numbers
// beside its schema (k10r: community and flow); the store never learns the words.
const COMMUNITY: Kind = Kind(1);
const EVENT: Kind = Kind(2);
    use super::*;

    #[test]
    fn the_faces_are_the_fixed_band_the_adr_numbered() {
        // The vocabulary leads, then the clusters: 0x0A/0x0B are the dictionary
        // for the 0x0C–0x0F faces, the way 0x04/0x05 lead 0x06–0x09.
        assert_eq!(CLUSTER_KIND_ID_SLOT, 0x0A);
        assert_eq!(CLUSTER_KIND_NAME_SLOT, 0x0B);
        assert_eq!(CLUSTER_SLOT, 0x0C);
        assert_eq!(EDGE_MEMBER_SLOT, 0x0D);
        assert_eq!(CHILD_INDEX_SLOT, 0x0E);
        assert_eq!(EDGE_CLUSTER_SLOT, 0x0F);
        // The entity faces come after the registry that keys them (ADR-0068).
        assert_eq!(NODE_MEMBER_SLOT, 0x12);
        assert_eq!(NODE_CLUSTER_SLOT, 0x13);
    }

    #[test]
    fn a_member_key_carries_the_whole_identity_after_the_cluster_prefix() {
        let ns = [0x00u8, 0x21];
        let cluster = ClusterRef::new(COMMUNITY, 7);
        // [ns 2][0x0C][kind 2][id 8] = 13 bytes. A member row is that prefix
        // plus the edge's stored identity `[src_h 8][pred 2][dst_h 8]` — the
        // FIXED-WIDTH handle row (ADR-0067's boundary-only handle), not the
        // ref-and-pkey tuple: a member table's key width never reaches this
        // face, so its width is the same eighteen bytes in every graph.
        let row = HandleKey::new(Handle::new(1), 3, Handle::new(2));
        let head = 3 + CLUSTER_KIND_WIDTH + U64_WIDTH; // ns, slot, kind, hyper_id
        assert_eq!(cluster_key(&ns, COMMUNITY, 7).len(), head);
        assert_eq!(HandleKey::WIDTH, 2 * HANDLE_WIDTH + CLUSTER_KIND_WIDTH);
        assert_eq!(
            edge_member_key(&ns, cluster, &row).len(),
            head + HandleKey::WIDTH,
            "a member row is the cluster prefix plus the eighteen-byte handle row"
        );
        assert_eq!(
            &edge_member_key(&ns, cluster, &row)[head..],
            &[
                0, 0, 0, 0, 0, 0, 0, 1, // src handle 1
                0, 3, // predicate
                0, 0, 0, 0, 0, 0, 0, 2, // dst handle 2
            ],
            "the row is [src_h 8][pred 2][dst_h 8], big-endian, no lengths"
        );
        assert!(edge_member_key(&ns, cluster, &row).starts_with(&members_prefix(&ns, cluster)));
        assert!(
            !edge_member_key(&ns, cluster, &row).starts_with(&cluster_key(&ns, COMMUNITY, 7))
        );
        // Every scan prefix is the whole key of its own row kind.
        assert_eq!(clusters_prefix(&ns, COMMUNITY).len(), 3 + CLUSTER_KIND_WIDTH);
        assert_eq!(members_prefix(&ns, cluster).len(), head);
        assert_eq!(children_prefix(&ns, COMMUNITY, 1).len(), head);
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
        let key = child_key(&ns, EVENT, 3, 42);
        let prefix = children_prefix(&ns, EVENT, 3);
        assert!(key.starts_with(&prefix));
        // The face byte is the one the child index moved down to.
        assert_eq!(u16::from(key[2]), CHILD_INDEX_SLOT);
        assert_eq!(&key[3..3 + CLUSTER_KIND_WIDTH], &EVENT.id().to_be_bytes());
        let suffix = &key[prefix.len()..];
        // [child 8]
        assert_eq!(suffix.len(), 8);
        assert_eq!(be_u64(suffix), Some(42));
    }
}
