//! Clusters on the DYNAMIC form (ADR-0066 + ADR-0017 §4) over the real engine
//! matrix: the four faces at the layout the ADR states, one `incorporate` verb,
//! one `retract` verb, and the two rules a delete follows.
//!
//! Every assertion is about BYTES or about an answer a read verb gives — no mock
//! store, no parallel implementation of the layout. Keys are built by hand in the
//! helper below on purpose: the test states the layout the ADR states, so a drift
//! in `okm-dynamic/src/cluster.rs` fails here instead of agreeing with itself.
//!
//! The seven claims:
//!
//! 1. Every row kind round-trips at the ADR's layout, nested clusters included.
//! 2. Membership is idempotent: a double add is one row, byte for byte.
//! 3. A node's delete takes every edge that names it.
//! 4. An edge's delete leaves both endpoints (and their other edges) alone.
//! 5. `member_of` answers before and after a retract — the derived face follows.
//! 6. The derived faces rebuild from `0x0A`/`0x0B` alone to the bytes the write
//!    path produced.
//! 7. A traversal is lazy: `find` reads a few entries of a 50-entry face, the
//!    predicate pushdown reads the predicate's own entries, and the stream is
//!    `Seekable`, so `okm-query`'s `intersect_seek` applies unchanged.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use okm_async::TestStore;
use okm_core::engine::storage::{ExternalScan, ScanIter};
use okm_core::error::Result as CoreResult;
use okm_core::schema::CollectionSchema;
use okm_core::{
    DocumentEncode, EdgeKey, GraphEncode, KeyEncode, NodeRef, Seekable, VirtualStorage, write_slot,
};
use okm_dynamic::{Cluster, ClusterRef, Decl, DynEdge, DynFragment, Graph, Kind};
use okm_query::intersect_seek;
use okm_value::{Dict, Variant};

const NS: u16 = 0x0190; // 400 — the same ns the byte-equality suite uses

fn node(ns: u16, key: u64) -> NodeRef {
    NodeRef::new(&ns.to_be_bytes(), &key.to_be_bytes())
}

fn person(id: u64) -> NodeRef {
    node(0x10, id)
}

fn org(id: u64) -> NodeRef {
    node(0x11, id)
}

fn attrs(pairs: &[(&str, Variant)]) -> Dict {
    let mut map = Dict::new();
    for (name, value) in pairs {
        map.insert((*name).to_string(), value.clone());
    }
    map
}

fn edge(src: NodeRef, dst: NodeRef, predicate: &str) -> DynEdge {
    DynEdge {
        src,
        dst,
        predicate: predicate.into(),
        attrs: Dict::new(),
    }
}

fn edge_with(src: NodeRef, dst: NodeRef, predicate: &str, attrs: Dict) -> DynEdge {
    DynEdge {
        src,
        dst,
        predicate: predicate.into(),
        attrs,
    }
}

// ---- the node collections the member set names (tables, by type) ----

#[derive(Clone, Default, KeyEncode)]
pub struct PersonId {
    pub id: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(0x10)]
#[ok_ref(PersonId)]
pub struct Person {}

#[derive(Clone, Default, KeyEncode)]
pub struct OrgId {
    pub id: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(0x11)]
#[ok_ref(OrgId)]
pub struct Org {}

/// The graph declaration the dynamic handle is assembled from (ns + member
/// set). The dynamic form declares no payload type — that is the point — so the
/// declaration's only content is its ns and its members.
#[derive(Clone, Default, GraphEncode)]
#[ok_ns(400)]
#[ok_nodes(Person, Org)]
pub struct WorksAt {}

fn schema() -> CollectionSchema {
    CollectionSchema::of_graph::<WorksAt>()
}

fn graph(store: &TestStore) -> Graph<TestStore> {
    Graph::new(store.clone(), NS, schema())
}

// ---- the layout, spelled out by hand ----

/// `[ns 2B][slot][kind][hyper_id][tail…]` — the face address the ADR states.
fn row_key(slot: u8, hyper_id: u64, tail: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(3 + 1 + 8 + tail.len());
    k.extend_from_slice(&NS.to_be_bytes());
    k.push(slot);
    k.push(Kind::COMMUNITY.byte());
    k.extend_from_slice(&hyper_id.to_be_bytes());
    k.extend_from_slice(tail);
    k
}

/// The cluster row's key: `[ns][0x0A][kind][hyper_id]`, one segment shorter
/// than its members'.
fn cluster_face(hyper_id: u64, tail: &[u8]) -> Vec<u8> {
    row_key(0x0A, hyper_id, tail)
}

/// `[ns 2B][0x0D][src][pred][dst][kind][hyper_id]` — the derived reverse index.
fn edge_cluster_face(edge: &EdgeKey, kind: Kind, hyper_id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(3 + edge.wire().len() + 1 + 8);
    k.extend_from_slice(&NS.to_be_bytes());
    k.push(0x0D);
    edge.write(&mut k);
    k.push(kind.byte());
    k.extend_from_slice(&hyper_id.to_be_bytes());
    k
}

/// Every `(key, value)` under one fixed face of the graph's ns.
fn face(store: &TestStore, slot: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut prefix = NS.to_be_bytes().to_vec();
    prefix.push(slot);
    let end = okm_core::engine::storage::prefix_end(&prefix);
    store
        .scan_range(&prefix, end.as_deref())
        .map(|item| item.expect("engine read"))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Every row kind, at the layout the ADR states
// ---------------------------------------------------------------------------

#[test]
fn every_cluster_face_round_trips_at_the_adr_layout() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = DynFragment::new([
            Decl::edge(edge_with(
                person(1),
                org(9),
                "works_at",
                attrs(&[("since", Variant::U64(2020))]),
            )),
            Decl::edge(edge(person(2), org(9), "works_at")),
        ]);
        fragment.cluster(Cluster::new(
            Kind::COMMUNITY,
            attrs(&[("label", Variant::Str("acme".into()))]),
        ));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        assert_eq!(report.edges.len(), 2, "{engine}: one edge per edge decl");
        assert_eq!(report.hyper_ids.len(), 1, "{engine}: one id per cluster");
        let id = report.hyper_ids[0];
        assert_eq!(id, 1, "{engine}: ids start at 1 — 0 is \"no parent\"");

        // 0x0A — `[parent u64 BE][dynamic payload]`, and NOTHING longer lives
        // under it: the members sit on a face of their own.
        let row = store
            .get(&cluster_face(id, &[]))
            .expect("read")
            .unwrap_or_else(|| panic!("{engine}: cluster row"));
        assert_eq!(&row[..8], &0u64.to_be_bytes(), "{engine}: root head");
        assert_eq!(
            store.get(&cluster_face(id, &[0x00])).expect("read"),
            None,
            "{engine}: the cluster row is the only row of its face"
        );
        let stored = g.lookup(Kind::COMMUNITY, id).expect("lookup").expect("row");
        assert_eq!(stored.parent, 0, "{engine}: no parent");
        assert_eq!(
            stored.payload.get("label"),
            Some(&Variant::Str("acme".to_string())),
            "{engine}: the Dict payload round-trips through the field vocabulary"
        );
        assert_eq!(
            g.member_count(Kind::COMMUNITY, id).expect("count"),
            2,
            "{engine}: member count stamped in the row"
        );

        // 0x0B — one empty-valued row per member edge, in edge-key order.
        assert_eq!(
            g.members(Kind::COMMUNITY, id).expect("members"),
            report.edges,
            "{engine}: one scan of 0x0B"
        );
        for edge in &report.edges {
            assert_eq!(
                store.get(&row_key(0x0B, id, &edge.wire())).expect("read"),
                Some(Vec::new()),
                "{engine}: a member row carries no value — the edge does"
            );
        }

        // 0x0D — the same membership the other way round, one row per member.
        for edge in &report.edges {
            assert_eq!(
                store
                    .get(&edge_cluster_face(edge, Kind::COMMUNITY, id))
                    .expect("read"),
                Some(Vec::new()),
                "{engine}: the derived edge → cluster row"
            );
        }
        assert_eq!(
            face(&store, 0x0D).len(),
            2,
            "{engine}: one derived row per member row"
        );
        // 0x0C is the child index — no parent, so no child row.
        assert!(
            face(&store, 0x0C).is_empty(),
            "{engine}: no parent, no child"
        );

        // Nesting rides the row's own head: a second cluster `.under(id)`.
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(3), org(3), "works_at")),
            Decl::edge(edge(person(4), org(4), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()).under(id));
        let child = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: child: {e}"))
            .hyper_ids[0];

        assert_eq!(
            g.children(Kind::COMMUNITY, id).expect("children"),
            vec![child],
            "{engine}: one scan of 0x0C"
        );
        assert_eq!(
            g.parent(Kind::COMMUNITY, child).expect("parent"),
            id,
            "{engine}: the head is the parent, one point read"
        );
        assert_eq!(
            g.clusters(Kind::COMMUNITY).expect("clusters"),
            vec![id, child],
            "{engine}: one scan of 0x0A, ids only"
        );
        assert_eq!(face(&store, 0x0C).len(), 1, "{engine}: one child row");
    }
}

// ---------------------------------------------------------------------------
// 2. Idempotent membership
// ---------------------------------------------------------------------------

#[test]
fn a_double_add_is_one_row_and_leaves_the_attributes_standing() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = DynFragment::new([
            Decl::edge(edge_with(
                person(1),
                org(1),
                "works_at",
                attrs(&[("since", Variant::U64(2020))]),
            )),
            Decl::edge(edge(person(2), org(2), "works_at")),
        ]);
        fragment.cluster(Cluster::new(
            Kind::COMMUNITY,
            attrs(&[("label", Variant::Str("a".to_string()))]),
        ));
        let first = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let id = first.hyper_ids[0];
        let ids = first.edges.clone();

        let members_before = face(&store, 0x0B);
        let reverse_before = face(&store, 0x0D);
        let attrs_before = face(&store, 0x01);

        // The same (src, predicate, dst) triple twice: naming an edge by its own
        // head and putting it into the cluster it is already in is one row,
        // byte-identical — an upsert, not an append.
        for round in 0..2 {
            let mut fragment = DynFragment::new([
                Decl::edge(edge(person(1), org(1), "works_at")),
                Decl::edge(edge(person(2), org(2), "works_at")),
            ]);
            fragment.cluster(Cluster::into(Kind::COMMUNITY, id, Dict::new()));
            let again = g
                .incorporate(fragment)
                .unwrap_or_else(|e| panic!("{engine}: re-assert {round}: {e}"));
            assert_eq!(
                again.edges, ids,
                "{engine}: the same heads are answered back"
            );
            assert!(
                again.hyper_ids.is_empty(),
                "{engine}: nothing was created, so nothing was allocated"
            );
        }

        assert_eq!(
            face(&store, 0x0B),
            members_before,
            "{engine}: member rows unchanged"
        );
        assert_eq!(
            face(&store, 0x0D),
            reverse_before,
            "{engine}: the derived mirror unchanged"
        );
        assert_eq!(
            face(&store, 0x01),
            attrs_before,
            "{engine}: a re-assertion carries the stored attributes, not a rewrite"
        );
        assert_eq!(
            g.get_edge(&ids[0])
                .expect("read")
                .expect("edge")
                .attrs
                .get("since"),
            Some(&Variant::U64(2020)),
            "{engine}: the Dict payload stands"
        );
        assert_eq!(
            g.member_count(Kind::COMMUNITY, id).expect("count"),
            2,
            "{engine}: the count did not double"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A node's delete takes every edge that names it
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_node_takes_every_edge_incident_to_it() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // p1 has two edges out, one of them shared with p2 (o1); p3 is an
        // unrelated assertion, so "everything else survived" has a witness.
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(1), org(2), "works_at")),
            Decl::edge(edge(person(2), org(1), "works_at")),
            Decl::edge(edge(person(3), org(3), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: write: {e}"));
        let id = report.hyper_ids[0];
        let (first, second) = (report.edges[0].clone(), report.edges[1].clone());
        let shared = report.edges[2].clone();
        let unrelated = report.edges[3].clone();

        // One entry per edge on each of the four traversal faces: the byte-level
        // statement of "incident". There is no `0x00` row — an edge's payload
        // lives on `0x06`, so the tuple face holds the row itself.
        let faces = [0x06u8, 0x07, 0x08, 0x09];
        let before: Vec<usize> = faces.iter().map(|slot| face(&store, *slot).len()).collect();
        assert_eq!(
            before,
            vec![4, 4, 4, 4],
            "{engine}: one row per edge on each of the four tuple faces (0x06–0x09)"
        );
        assert!(
            face(&store, 0x00).is_empty(),
            "{engine}: an edge writes no primary row — 0x06 holds the payload"
        );

        let mut fragment = DynFragment::new([]);
        fragment.delete_node(person(1));
        let retracted = g
            .retract(fragment)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (1, 2, 2),
            "{engine}: the node, both its edges, and their two membership rows"
        );
        let after: Vec<usize> = faces.iter().map(|slot| face(&store, *slot).len()).collect();
        assert_eq!(
            after,
            vec![2, 2, 2, 2],
            "{engine}: every face lost exactly the two rows those edges wrote"
        );
        // The dynamic segment goes with the edge: four edges wrote four
        // attribute entries, and the two doomed ones left with their edges.
        assert_eq!(
            face(&store, 0x01).len(),
            2,
            "{engine}: only the surviving edges' segments are left"
        );
        assert!(
            g.get_edge(&first).expect("read").is_none()
                && g.get_edge(&second).expect("read").is_none(),
            "{engine}: no dangling edge, because the edges are gone"
        );
        assert!(
            g.get_edge(&unrelated).expect("read").is_some(),
            "{engine}: an edge that does not name the node is untouched"
        );
        assert!(
            g.out_edges(&person(1)).expect("out").is_empty(),
            "{engine}: the node has no outgoing edge left"
        );
        assert_eq!(
            g.in_edges(&org(1)).expect("in"),
            vec![shared.clone()],
            "{engine}: the other edge into o1 stays"
        );
        assert_eq!(
            g.members(Kind::COMMUNITY, id).expect("members"),
            vec![shared, unrelated],
            "{engine}: the cluster kept the members that still exist"
        );
        assert_eq!(
            g.member_count(Kind::COMMUNITY, id).expect("count"),
            2,
            "{engine}: the count follows the cascade"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. An edge's delete leaves both endpoints alone
// ---------------------------------------------------------------------------

#[test]
fn deleting_an_edge_leaves_both_endpoints_and_their_other_edges() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(1), org(2), "works_at")),
            Decl::edge(edge(person(2), org(3), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: write: {e}"));
        let id = report.hyper_ids[0];
        let (doomed, kept) = (report.edges[0].clone(), report.edges[1].clone());

        let mut fragment = DynFragment::new([]);
        fragment.delete_edge(doomed.clone());
        let retracted = g
            .retract(fragment)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (0, 1, 1),
            "{engine}: one edge and one membership row — no node is a delete's business"
        );
        assert!(
            g.get_edge(&doomed).expect("read").is_none(),
            "{engine}: the edge row is gone"
        );
        assert!(
            g.in_edges(&org(1)).expect("in").is_empty(),
            "{engine}: its incoming face is gone too"
        );
        assert_eq!(
            g.out_edges(&person(1)).expect("out"),
            vec![kept.clone()],
            "{engine}: the endpoint keeps its OTHER edges"
        );

        // Both endpoints are still nodes of this graph: a new assertion between
        // them lands without anything being re-created. That is the rule — "a
        // node exists only as an endpoint" is about creation, and a delete may
        // leave an endpoint connected to nothing.
        let again = g
            .incorporate(DynFragment::new([Decl::edge(edge(
                person(1),
                org(1),
                "works_at",
            ))]))
            .unwrap_or_else(|e| panic!("{engine}: the endpoints are still endpoints: {e}"));
        assert_eq!(
            g.in_edges(&org(1)).expect("in"),
            vec![again.edges[0].clone()],
            "{engine}: the endpoint took a new edge"
        );
        assert_eq!(
            g.members(Kind::COMMUNITY, id).expect("members"),
            vec![kept, report.edges[2].clone()],
            "{engine}: the cluster kept its other members"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. `member_of` before and after a retract
// ---------------------------------------------------------------------------

#[test]
fn member_of_answers_before_and_after_a_retract() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(2), org(2), "works_at")),
            Decl::edge(edge(person(3), org(3), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: write: {e}"));
        let id = report.hyper_ids[0];
        let ids = report.edges.clone();

        assert!(
            g.member_of(Kind::COMMUNITY, id, &ids[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: a member answers true — one point read of 0x0D"
        );
        assert!(
            !g.member_of(
                Kind::COMMUNITY,
                id,
                &EdgeKey::new(person(9), ids[1].predicate, org(9))
            )
            .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: an edge that is not a member answers false"
        );
        assert!(
            !g.member_of(Kind::EVENT, id, &ids[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the same head under another kind is a different cluster"
        );
        assert_eq!(face(&store, 0x0D).len(), 3, "{engine}: three derived rows");

        // Retract one edge: its member row AND its derived mirror leave in one
        // batch, and the cluster — now two members — survives.
        let mut fragment = DynFragment::new([]);
        fragment.delete_edge(ids[0].clone());
        g.retract(fragment)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));

        assert!(
            !g.member_of(Kind::COMMUNITY, id, &ids[0])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the mirror left with the member row"
        );
        assert_eq!(
            g.members(Kind::COMMUNITY, id).expect("members"),
            vec![ids[1].clone(), ids[2].clone()],
            "{engine}: the authority still holds the surviving members"
        );
        assert_eq!(
            face(&store, 0x0D).len(),
            2,
            "{engine}: the derived face followed it exactly"
        );
        assert_eq!(
            g.member_count(Kind::COMMUNITY, id).expect("count"),
            2,
            "{engine}: the count is the member rows' own number"
        );

        // A cluster's `delete` is a removal, so it rides `retract` too.
        let mut fragment = DynFragment::new([]);
        fragment.delete(ClusterRef::new(Kind::COMMUNITY, id));
        let removed = g
            .retract(fragment)
            .unwrap_or_else(|e| panic!("{engine}: delete cluster: {e}"));
        assert_eq!(
            removed.memberships, 2,
            "{engine}: the cluster's remaining member rows went with it"
        );
        assert!(
            g.lookup(Kind::COMMUNITY, id).expect("lookup").is_none(),
            "{engine}: the cluster row is gone"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. The derived faces rebuild to exactly what the write path wrote
// ---------------------------------------------------------------------------

#[test]
fn the_derived_faces_rebuild_to_the_write_paths_bytes() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // Three edges in one community cluster, the same three adopted by an
        // event cluster, and a child cluster nested under the first — so `0x0C`
        // and `0x0D` both have rows and neither can be rebuilt by accident.
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(2), org(2), "works_at")),
            Decl::edge(edge(person(3), org(3), "works_at")),
        ]);
        fragment.cluster(Cluster::new(
            Kind::COMMUNITY,
            attrs(&[("label", Variant::Str("a".to_string()))]),
        ));
        let first = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: first: {e}"));
        let parent = first.hyper_ids[0];
        let ids = first.edges.clone();

        // The same three heads re-asserted under a second cluster: the edges are
        // the ones already there, so only the membership is new.
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(2), org(2), "works_at")),
            Decl::edge(edge(person(3), org(3), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::EVENT, Dict::new()));
        g.incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: second: {e}"));

        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(4), org(4), "works_at")),
            Decl::edge(edge(person(5), org(5), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()).under(parent));
        g.incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: child: {e}"));

        let written_child = face(&store, 0x0C);
        let written_reverse = face(&store, 0x0D);
        assert_eq!(written_child.len(), 1, "{engine}: one nested cluster");
        assert_eq!(
            written_reverse.len(),
            8,
            "{engine}: three member rows twice, two more for the child"
        );

        // Wipe the derived faces by hand — the state an interrupted rebuild, a
        // restore without them, or a build that never wrote them would leave.
        for slot in [0x0Cu8, 0x0D] {
            for (key, _) in face(&store, slot) {
                store.del(&key).expect("delete");
            }
            assert!(
                face(&store, slot).is_empty(),
                "{engine}: face {slot:02X} really is gone"
            );
        }

        let report = g
            .rebuild_derived(usize::MAX)
            .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
        assert_eq!(
            report.clusters, 3,
            "{engine}: three cluster rows were walked"
        );
        assert_eq!(
            report.members, 8,
            "{engine}: the rebuild walked the member rows"
        );
        assert_eq!(
            face(&store, 0x0C),
            written_child,
            "{engine}: 0x0C rebuilt to the same bytes the write path produced"
        );
        assert_eq!(
            face(&store, 0x0D),
            written_reverse,
            "{engine}: 0x0D rebuilt to the same bytes the write path produced"
        );
        // Derived rows are an INDEX, not a second truth.
        assert_eq!(
            g.members(Kind::COMMUNITY, parent).expect("members"),
            ids,
            "{engine}: the authority was never touched"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Traversal: laziness, pushdown, and the repositionable stream
// ---------------------------------------------------------------------------

/// What the store was asked to do, shared with the test after the graph has
/// taken the handle by value.
#[derive(Clone, Default)]
struct Calls {
    /// `scan_range` calls: one per scan opened.
    scans: Rc<Cell<usize>>,
    /// `get` calls — a point read, i.e. a ROW fetched.
    gets: Rc<Cell<usize>>,
    /// Entries the CURSOR handed the traversal. This is the number the laziness
    /// claims are about, and the only one a consumer can influence.
    pulled: Arc<AtomicUsize>,
}

impl Calls {
    fn reset(&self) {
        self.scans.set(0);
        self.gets.set(0);
        self.pulled.store(0, Ordering::Relaxed);
    }

    fn pulled(&self) -> usize {
        self.pulled.load(Ordering::Relaxed)
    }
}

/// A store that forwards everything and records the calls that mean "read
/// something": an ordered scan, a point fetch, and every entry the scan's cursor
/// actually handed out.
///
/// The range is materialized here on purpose: [`ScanIter`] is opaque, so a
/// wrapper cannot count its pulls from the outside — but the counter this test
/// needs is the CONSUMER's, i.e. "how many entries did the traversal ask for",
/// and that is measurable through `ScanIter::External`, whose item stream this
/// store owns.
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    calls: Calls,
}

struct Counted {
    items: std::vec::IntoIter<CoreResult<(Vec<u8>, Vec<u8>)>>,
    pulled: Arc<AtomicUsize>,
}

impl Iterator for Counted {
    type Item = CoreResult<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.items.next()?;
        self.pulled.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }
}

impl DoubleEndedIterator for Counted {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.items.next_back()
    }
}

impl<S: VirtualStorage> VirtualStorage for Counting<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> CoreResult<()> {
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> CoreResult<Option<Vec<u8>>> {
        self.calls.gets.set(self.calls.gets.get() + 1);
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> CoreResult<()> {
        self.inner.del(key)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        self.calls.scans.set(self.calls.scans.get() + 1);
        let items: Vec<CoreResult<(Vec<u8>, Vec<u8>)>> =
            self.inner.scan_range(begin, end).collect();
        ScanIter::External(ExternalScan::new(Box::new(Counted {
            items: items.into_iter(),
            pulled: self.calls.pulled.clone(),
        })))
    }
}

/// Entry keys already in hand, ordered, seekable by binary search — the second
/// side of a set operator when the keys did not come from a scan.
struct KeySet {
    keys: Vec<Vec<u8>>,
    at: usize,
}

impl KeySet {
    fn new(mut keys: Vec<Vec<u8>>) -> Self {
        keys.sort();
        Self { keys, at: 0 }
    }
}

impl Seekable for KeySet {
    type Key = Vec<u8>;

    fn key(&self) -> Option<&Vec<u8>> {
        self.keys.get(self.at)
    }

    fn step(&mut self) -> bool {
        if self.at < self.keys.len() {
            self.at += 1;
        }
        self.key().is_some()
    }

    fn seek(&mut self, lower: &Vec<u8>) -> bool {
        let base = self.at;
        self.at += self.keys[base..].partition_point(|key| key < lower);
        self.key().is_some()
    }
}

/// The `0x06` entry key of one edge, spelled by hand: the face a walk of its
/// source visits. `[ns][0x06][src][pred][dst]` — the identity IS the suffix.
fn out_face_key(edge: &EdgeKey) -> Vec<u8> {
    let mut key = NS.to_be_bytes().to_vec();
    write_slot(&mut key, 0x06);
    edge.write(&mut key);
    key
}

const WORKS: u64 = 20;
const ADVISES: u64 = 30;

#[test]
fn a_traversal_find_reads_fewer_entries_than_the_face_holds() {
    for (engine, store) in TestStore::matrix() {
        let calls = Calls::default();
        let counted = Counting {
            inner: store.clone(),
            calls: calls.clone(),
        };
        let mut g: Graph<_> = Graph::new(counted, NS, schema());

        // One hub (person 1) with WORKS + ADVISES out-edges, and a second person
        // pointing at the first org so the in-face has a mirror.
        let mut decls: Vec<Decl> = Vec::new();
        for id in 1..=WORKS {
            decls.push(Decl::edge(edge(person(1), org(id), "works_at")));
        }
        for id in 100..100 + ADVISES {
            decls.push(Decl::edge(edge(person(1), org(id), "advises")));
        }
        decls.push(Decl::edge(edge(person(2), org(1), "works_at")));
        let report = g
            .incorporate(DynFragment::new(decls))
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let edges = report.edges;
        assert_eq!(edges.len() as u64, WORKS + ADVISES + 1);
        let works: Vec<EdgeKey> = edges[..WORKS as usize].to_vec();

        // ---- one edge, both directions: the same head, mirrored far ends ----
        let out_step = g
            .out(&person(1))
            .find(|step| step.key() == works[0])
            .expect("the hub's first out-edge");
        let in_step = g
            .inn(&org(1))
            .find(|step| step.key() == works[0])
            .expect("the same edge from the far side");
        assert_eq!(
            (
                out_step.far.clone(),
                out_step.head.src.clone(),
                out_step.head.dst.clone()
            ),
            (org(1), person(1), org(1)),
            "{engine}: an out hop's far end is the destination"
        );
        assert_eq!(
            (in_step.far.clone(), in_step.head.dst.clone()),
            (person(1), org(1)),
            "{engine}: the in hop's far end is the source"
        );
        assert_eq!(
            out_step.head, in_step.head,
            "{engine}: one edge states one head, whichever face read it"
        );
        let works_id = g
            .predicate_id("works_at")
            .expect("probe")
            .expect("interned");
        assert_eq!(
            out_step.head.predicate, works_id,
            "{engine}: the untyped face still carries the predicate — it is in the key"
        );

        // ---- the far endpoint comes from the KEY: no row is read ----
        calls.reset();
        let walked = g.out(&person(1)).count();
        assert_eq!(walked as u64, WORKS + ADVISES, "{engine}: the whole face");
        assert_eq!(
            calls.gets.get(),
            0,
            "{engine}: a hop reads no primary row — the entry names both endpoints"
        );

        // ---- laziness: the answer's position bounds the reads ----
        calls.reset();
        let found = g
            .out(&person(1))
            .find(|step| step.far == org(1))
            .expect("org 1 is the first destination");
        assert_eq!(found.key(), works[0]);
        let find_pulled = calls.pulled();
        assert!(
            find_pulled <= 3,
            "{engine}: `find` must stop at the answer, pulled {find_pulled} of {}",
            WORKS + ADVISES
        );

        calls.reset();
        let head_of_face: Vec<EdgeKey> = g.out(&person(1)).take(3).map(|step| step.key()).collect();
        assert_eq!(head_of_face.len(), 3);
        let take_pulled = calls.pulled();
        assert!(
            take_pulled <= 4,
            "{engine}: `take(3)` reads three entries plus the one position it keeps \
             on top, pulled {take_pulled}"
        );

        // ---- predicate pushdown: same set, fewer entries ----
        assert!(
            g.predicate_id("works_at").expect("probe").is_some(),
            "{engine}: the predicate was interned"
        );
        calls.reset();
        let untyped = g.out(&person(1)).map(|step| step.key()).collect::<Vec<_>>();
        let untyped_pulled = calls.pulled();
        assert_eq!(untyped.len() as u64, WORKS + ADVISES);

        calls.reset();
        let narrowed = g.out(&person(1)).by("works_at").expect("predicate id");
        let mut pushed = narrowed.map(|step| step.key()).collect::<Vec<_>>();
        let pushed_pulled = calls.pulled();
        pushed.sort_unstable();
        let mut want = works.clone();
        want.sort_unstable();
        assert_eq!(
            pushed, want,
            "{engine}: the pushed-down stream answers exactly the predicate's edges"
        );
        assert!(
            pushed_pulled < untyped_pulled,
            "{engine}: pushdown reads fewer entries ({pushed_pulled} vs {untyped_pulled} \
             over a {}-entry face)",
            WORKS + ADVISES
        );
        assert!(
            calls.gets.get() <= 1,
            "{engine}: the pushdown's only point read is the predicate's own vocabulary \
             entry ({}), never an edge row",
            calls.gets.get()
        );

        // A predicate nobody interned answers empty without opening a scan.
        calls.reset();
        let probe = g.out(&person(1));
        let opened = calls.scans.get();
        let unknown = probe.by("never_written").expect("a probe, not a fault");
        assert_eq!(
            unknown.count(),
            0,
            "{engine}: an unknown predicate answers empty"
        );
        assert_eq!(
            calls.scans.get(),
            opened,
            "{engine}: and opens no further scan to read and throw away"
        );

        // ---- Seekable: `intersect_seek` over a walk and a key set ----
        //
        // Two of the hub's edges go into one cluster; "which of this node's
        // edges are members of C" is then one alignment of two ordered streams
        // that share a key space (the 0x06 face), not a set built beforehand.
        let mut fragment = DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(1), org(2), "works_at")),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let grouped = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: cluster: {e}"));
        let cluster = grouped.hyper_ids[0];

        let member_keys: Vec<Vec<u8>> = g
            .members(Kind::COMMUNITY, cluster)
            .expect("members")
            .iter()
            .map(out_face_key)
            .collect();
        let shared: Vec<Vec<u8>> =
            intersect_seek(g.out(&person(1)), KeySet::new(member_keys)).collect();
        assert_eq!(shared.len(), 2, "{engine}: two of the hub's edges are in C");
        let mut want = vec![out_face_key(&works[0]), out_face_key(&works[1])];
        want.sort_unstable();
        assert_eq!(
            shared, want,
            "{engine}: the alignment answers what the member scan says"
        );

        // The point query as a filter predicate inside the traversal: same edges,
        // no set built first.
        let filtered: Vec<EdgeKey> = g
            .out(&person(1))
            .filter(|step| {
                g.member_of(Kind::COMMUNITY, cluster, &step.key())
                    .unwrap_or(false)
            })
            .map(|step| step.key())
            .collect();
        let mut filtered_sorted = filtered;
        filtered_sorted.sort_unstable();
        let mut want_edges = vec![works[0].clone(), works[1].clone()];
        want_edges.sort_unstable();
        assert_eq!(
            filtered_sorted, want_edges,
            "{engine}: `member_of` filters a hop without a set built first"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. The two verbs refuse each other's declarations
// ---------------------------------------------------------------------------

#[test]
fn the_two_verbs_refuse_each_others_declarations() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = g
            .incorporate(DynFragment::new([Decl::edge(edge(
                person(1),
                org(1),
                "works_at",
            ))]))
            .expect("write");

        // A write fragment that also deletes: one batch, two meanings.
        let mut fragment = DynFragment::new([Decl::edge(edge(person(2), org(2), "works_at"))]);
        fragment.delete_edge(report.edges[0].clone());
        let err = g.incorporate(fragment).expect_err("incorporate writes");
        assert!(
            err.contains("retract's verb"),
            "{engine}: unexpected error: {err}"
        );

        // A retract fragment that also declares rows.
        let mut fragment = DynFragment::new([Decl::edge(edge(person(3), org(3), "works_at"))]);
        fragment.delete_edge(report.edges[0].clone());
        let err = g.retract(fragment).expect_err("retract removes");
        assert!(
            err.contains("incorporate's verb"),
            "{engine}: unexpected error: {err}"
        );

        // Neither refusal wrote or removed anything.
        assert!(
            g.get_edge(&report.edges[0]).expect("read").is_some(),
            "{engine}: the refused batch left the edge alone"
        );
        assert!(
            g.out_edges(&person(2)).expect("out").is_empty()
                && g.out_edges(&person(3)).expect("out").is_empty(),
            "{engine}: and no edge was born either"
        );

        // A cluster over ONE member is refused, and the edge that would have
        // been its member is not written either — every check runs before the
        // commit.
        let mut fragment = DynFragment::new([Decl::edge(edge(person(4), org(4), "works_at"))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let err = g
            .incorporate(fragment)
            .expect_err("a cluster needs two members");
        assert!(
            err.contains("two or more members"),
            "{engine}: unexpected error: {err}"
        );
        assert!(
            g.out_edges(&person(4)).expect("out").is_empty(),
            "{engine}: the refused edge did not land"
        );
        assert!(
            g.clusters(Kind::COMMUNITY).expect("clusters").is_empty(),
            "{engine}: and no cluster row was written"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. The edge conflict hook decides between the stored and the declared frame
// ---------------------------------------------------------------------------

/// The two-argument hook on this form: a re-assertion hands it the row's STORED
/// attributes beside the declaration's own, so "the newer frame wins" is one
/// expression. A mutator that only saw the stored map could not state it — it had
/// nothing to compare the incoming `ts` against — and a hook that only saw the
/// declaration would drop the stored frame on every re-assertion.
#[test]
fn an_edge_hook_keeps_the_newer_frame_across_a_reassertion() {
    fn newest(stored: &Dict, declared: &Dict) -> Dict {
        fn ts(row: &Dict) -> u64 {
            match row.get("ts") {
                Some(Variant::U64(ts)) => *ts,
                _ => 0,
            }
        }
        if ts(declared) >= ts(stored) {
            declared.clone()
        } else {
            stored.clone()
        }
    }

    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = DynFragment::new([Decl::edge(edge_with(
            person(1),
            org(1),
            "works_at",
            attrs(&[
                ("ts", Variant::U64(10)),
                ("label", Variant::Str("old".into())),
            ]),
        ))]);
        fragment.on_edge_conflict(newest);
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let id = report.edges[0].clone();

        // An OLDER declaration for the same head: the stored frame stands, and
        // the head and its faces are untouched.
        let mut fragment = DynFragment::new([Decl::edge(edge_with(
            person(1),
            org(1),
            "works_at",
            attrs(&[
                ("ts", Variant::U64(4)),
                ("label", Variant::Str("stale".into())),
            ]),
        ))]);
        fragment.on_edge_conflict(newest);
        g.incorporate(fragment).expect("stale re-assert");
        let stored = g.get_edge(&id).expect("read").expect("edge");
        assert_eq!(
            stored.attrs.get("ts"),
            Some(&Variant::U64(10)),
            "{engine}: the stored frame survived a declaration with an older ts"
        );
        assert_eq!(
            stored.attrs.get("label"),
            Some(&Variant::Str("old".to_string())),
            "{engine}: the whole frame is the stored one, not a name-by-name mix"
        );

        // A NEWER declaration: the declared frame lands, at the same head.
        let mut fragment = DynFragment::new([Decl::edge(edge_with(
            person(1),
            org(1),
            "works_at",
            attrs(&[
                ("ts", Variant::U64(11)),
                ("label", Variant::Str("new".into())),
            ]),
        ))]);
        fragment.on_edge_conflict(newest);
        g.incorporate(fragment).expect("fresh re-assert");
        let stored = g.get_edge(&id).expect("read").expect("edge");
        assert_eq!(
            stored.attrs.get("ts"),
            Some(&Variant::U64(11)),
            "{engine}: the newer frame won"
        );
        assert_eq!(
            stored.attrs.get("label"),
            Some(&Variant::Str("new".to_string())),
            "{engine}: and it is the declared one, whole"
        );
        assert_eq!(
            g.out_edges(&person(1)).expect("out").len(),
            1,
            "{engine}: a re-assertion is the same edge — no second one appeared"
        );
    }
}
