//! Clusters / hyperedges (ADR-0066) on the real engine matrix: the four fixed
//! faces, their byte layout, the write entry point, and the rules the ADR fixes.
//!
//! Every assertion is about BYTES or about an answer a read verb gives — no mock
//! store, no parallel implementation of the layout. Keys are built by hand in
//! the helper below on purpose: the test states the layout the ADR states, so a
//! drift in `model::hyper` fails here instead of agreeing with itself.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::engine::storage::prefix_end;
use okm_core::{
    ClusterRef, Decl, DeclSet, Document, DocumentEncode, EdgeHead, Error, Graph, GraphEncode,
    HyperEdge, Hyperedges, KeyEncode, Kind, NodeRef, VirtualStorage,
};
use okm_value::{Dict, Variant};

// ---- the node collections and the graph that names them ----

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PersonId {
    pub id: u64,
}

/// A node table that carries a counter — the ADR-0028 shape the node reduce is
/// for.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(PersonId)]
#[ok_ns(0x30)]
pub struct Person {
    pub seen: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OrgId {
    pub id: u64,
}

/// A node table with NO reducer registered: its nodes exist (as endpoints) but
/// nothing materializes a row for them.
#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(OrgId)]
#[ok_ns(0x31)]
pub struct Org {}

#[derive(Clone, Default, GraphEncode)]
#[ok_ns(0x0192)] // 402, its own namespace
#[ok_nodes(Person, Org)]
pub struct Employment {
    pub weight: u64,
}

const NS: u16 = 0x0192;

fn person(id: u64) -> NodeRef {
    NodeRef::new(Person::NS_PREFIX, &PersonId { id }.encode())
}

fn org(id: u64) -> NodeRef {
    NodeRef::new(Org::NS_PREFIX, &OrgId { id }.encode())
}

fn edge(src: NodeRef, dst: NodeRef) -> Decl<Employment> {
    Decl::edge(EdgeHead {
        src,
        dst,
        predicate: "works_at".to_string(),
    })
}

fn attrs(label: &str) -> Dict {
    let mut map = Dict::new();
    map.insert("label".to_string(), Variant::Str(label.to_string()));
    map
}

fn kind_byte() -> u8 {
    Kind::COMMUNITY.byte()
}

/// The face address, spelled out: `[ns 2B][slot][kind][hyper_id][tail…]`.
fn key(slot: u8, hyper_id: u64, tail: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(3 + 1 + 8 + tail.len());
    k.extend_from_slice(&NS.to_be_bytes());
    k.push(slot);
    k.push(kind_byte());
    k.extend_from_slice(&hyper_id.to_be_bytes());
    k.extend_from_slice(tail);
    k
}

/// Every `(key, value)` under one fixed face of the graph's ns.
fn face(store: &TestStore, slot: u8) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut prefix = NS.to_be_bytes().to_vec();
    prefix.push(slot);
    let end = prefix_end(&prefix);
    store
        .scan_range(&prefix, end.as_deref())
        .map(|item| item.expect("engine read"))
        .collect()
}

fn graph(store: &TestStore) -> Graph<TestStore, Employment> {
    Graph::new(store.clone())
}

fn hyper(store: &TestStore) -> Hyperedges<TestStore> {
    Hyperedges::of::<Employment>(store.clone())
}

fn members(store: &TestStore, engine: &str, cluster: ClusterRef) -> Vec<u64> {
    hyper(store)
        .members(cluster)
        .unwrap_or_else(|e| panic!("{engine}: members: {e}"))
}

fn nodes_of(store: &TestStore, engine: &str, cluster: ClusterRef) -> Vec<NodeRef> {
    hyper(store)
        .nodes(cluster)
        .unwrap_or_else(|e| panic!("{engine}: nodes: {e}"))
}

// ---------------------------------------------------------------------------
// 1. Every row kind, at the layout the ADR states
// ---------------------------------------------------------------------------

#[test]
fn every_row_kind_round_trips_at_the_layout_the_adr_states() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(9)), edge(person(2), org(9))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("acme")))
            .reduce_edge(|e: &mut Employment| e.weight += 7)
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        assert_eq!(report.edge_ids.len(), 2, "{engine}: one id per edge decl");
        assert_eq!(report.hyper_ids.len(), 1, "{engine}: one id per cluster");
        let id = report.hyper_ids[0];
        let cluster = ClusterRef::new(Kind::COMMUNITY, id);

        // The edge rows themselves are the ordinary graph rows: the reduce ran,
        // so the payload is the batch's, not a default.
        for edge_id in &report.edge_ids {
            let body = g.edge(*edge_id).expect("edge").expect("present");
            assert_eq!(body.weight, 7, "{engine}: the edge reducer ran");
        }

        // 0x0A — the cluster row: `[parent u64 BE][dynamic payload]`, and NOTHING
        // longer lives under it (no member row shares this face).
        let row = store
            .get(&key(0x0A, id, &[]))
            .expect("read")
            .unwrap_or_else(|| panic!("{engine}: cluster row"));
        assert_eq!(&row[..8], &0u64.to_be_bytes(), "{engine}: root head");
        assert_eq!(
            store.get(&key(0x0A, id, &[0x00])).expect("read"),
            None,
            "{engine}: the cluster row is the only row of its face"
        );
        let stored = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!(stored.parent, 0);
        assert_eq!(
            stored.payload.get("label"),
            Some(&Variant::Str("acme".to_string())),
            "{engine}: the payload round-trips through the field vocabulary"
        );
        assert_eq!(
            stored.member_count(),
            2,
            "{engine}: member count in the row"
        );
        assert_eq!(stored.node_count(), 3, "{engine}: node count in the row");

        // 0x0B — one empty-valued row per member edge, in edge-id order.
        assert_eq!(members(&store, engine, cluster), report.edge_ids);
        for edge_id in &report.edge_ids {
            assert_eq!(
                store
                    .get(&key(0x0B, id, &edge_id.to_be_bytes()))
                    .expect("read"),
                Some(Vec::new()),
                "{engine}: a member row carries no value — the edge does"
            );
        }

        // 0x0C — one row per (member edge, endpoint): the node ref wire, then
        // the edge id. Four rows for two edges; three distinct nodes.
        let mut expected: Vec<(u64, NodeRef)> = vec![
            (report.edge_ids[0], person(1)),
            (report.edge_ids[0], org(9)),
            (report.edge_ids[1], person(2)),
            (report.edge_ids[1], org(9)),
        ];
        for (edge_id, node) in &expected {
            let mut tail = node.encode();
            tail.extend_from_slice(&edge_id.to_be_bytes());
            assert_eq!(
                store.get(&key(0x0C, id, &tail)).expect("read"),
                Some(Vec::new()),
                "{engine}: node member row"
            );
        }
        expected.sort_by_key(|(_, node)| node.encode());
        assert_eq!(nodes_of(&store, engine, cluster).len(), 3);

        // 0x0D — no parent, so no child row.
        assert!(
            face(&store, 0x0D).is_empty(),
            "{engine}: no parent, no child"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Ordering and the streaming dedupe
// ---------------------------------------------------------------------------

#[test]
fn node_members_are_ordered_so_duplicates_are_adjacent() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = Vec::<Decl<Employment>>::new()
            .decls([
                edge(person(1), org(1)),
                edge(person(1), org(2)),
                edge(person(2), org(1)),
            ])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);

        // Six rows: two endpoints per member edge.
        let rows = hyper(&store).node_rows(cluster).expect("rows");
        assert_eq!(rows.len(), 6, "{engine}: one row per endpoint");

        // Byte order over the node segment: same ns, then same-width pkeys in
        // numeric order. That is what makes equal nodes adjacent.
        let mut sorted = rows.clone();
        sorted.sort_by_key(|(node, _)| node.encode());
        assert_eq!(
            rows, sorted,
            "{engine}: the face orders by node, not by edge"
        );

        // A node appears ONCE in a run: walking the rows, a node never comes
        // back after another one was seen — one comparison with the previous
        // row is a complete dedupe.
        let mut walked: Vec<Vec<u8>> = Vec::new();
        let mut previous: Option<Vec<u8>> = None;
        for (node, _) in &rows {
            let wire = node.encode();
            if previous.as_deref() != Some(wire.as_slice()) {
                assert!(
                    !walked.contains(&wire),
                    "{engine}: node {node:?} came back after another one"
                );
                walked.push(wire.clone());
                previous = Some(wire);
            }
        }
        // …and that dedupe equals the set of endpoints: person(1) twice,
        // person(2), org(1) twice, org(2) once.
        assert_eq!(
            nodes_of(&store, engine, cluster),
            vec![person(1), person(2), org(1), org(2)],
            "{engine}: nodes are the deduplicated endpoints"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The derived faces rebuild to exactly what the write path wrote
// ---------------------------------------------------------------------------

#[test]
fn derived_faces_rebuild_to_exactly_what_the_write_path_wrote() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // A parent cluster, then a child under it, so 0x0D has rows too.
        let parent = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), org(2))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("parent")))
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: parent: {e}"))
            .hyper_ids[0];
        let child = Vec::<Decl<Employment>>::new()
            .decls([edge(person(3), org(3)), edge(person(3), org(4))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("child")).under(parent))
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: child: {e}"))
            .hyper_ids[0];

        let cluster = ClusterRef::new(Kind::COMMUNITY, child);
        let members_before = members(&store, engine, cluster);
        let nodes_before = face(&store, 0x0C);
        let children_before = face(&store, 0x0D);
        assert!(!nodes_before.is_empty(), "{engine}: node rows exist");
        assert_eq!(children_before.len(), 1, "{engine}: one child row");

        let report = hyper(&store)
            .rebuild_derived(usize::MAX)
            .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
        assert_eq!(report.clusters, 2, "{engine}: two cluster rows were walked");
        assert_eq!(report.members, 4, "{engine}: four member rows");
        assert_eq!(report.node_rows, 8, "{engine}: eight derived node rows");

        assert_eq!(
            face(&store, 0x0C),
            nodes_before,
            "{engine}: 0x0C rebuilt to the same bytes the write path produced"
        );
        assert_eq!(
            face(&store, 0x0D),
            children_before,
            "{engine}: 0x0D rebuilt to the same bytes"
        );

        // The derived rows are an INDEX, not a second truth: the authority is
        // untouched, so the answers are identical.
        assert_eq!(
            members(&store, engine, cluster),
            members_before,
            "{engine}: membership survived the rebuild"
        );
        assert_eq!(
            nodes_of(&store, engine, cluster).len(),
            3,
            "{engine}: person(3) is one node even though two edges name it"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Idempotent membership
// ---------------------------------------------------------------------------

#[test]
fn membership_is_idempotent() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), org(2))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        let before = face(&store, 0x0B);
        let node_rows_before = face(&store, 0x0C);
        let edge_id = report.edge_ids[0];

        // The same (kind, cluster, member) triple twice: one row, and the row
        // sets are byte-identical — an upsert, not an append.
        for _ in 0..2 {
            Vec::<Decl<Employment>>::new()
                .decls([])
                .reassign(edge_id, cluster, cluster)
                .commit(&mut g)
                .unwrap_or_else(|e| panic!("{engine}: reassign: {e}"));
        }
        assert_eq!(
            face(&store, 0x0B),
            before,
            "{engine}: member rows unchanged"
        );
        assert_eq!(
            face(&store, 0x0C),
            node_rows_before,
            "{engine}: derived rows unchanged"
        );

        // A member listed twice in one operation is still one row: `split`
        // dedupes through the member key, not through a pre-pass.
        let split = Vec::<Decl<Employment>>::new()
            .decls([])
            .split(cluster, vec![edge_id, edge_id, report.edge_ids[1]])
            .commit(&mut g)
            .unwrap_or_else(|e| panic!("{engine}: split: {e}"));
        let moved = ClusterRef::new(Kind::COMMUNITY, split.hyper_ids[0]);
        assert_eq!(
            members(&store, engine, moved).len(),
            2,
            "{engine}: a duplicate member is one row"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Same-batch atomicity: a refused write set leaves nothing
// ---------------------------------------------------------------------------

#[test]
fn a_refused_write_set_leaves_nothing() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        // Two edges, one foreign endpoint: the first edge is already encoded
        // into the batch when the second one is refused.
        let foreign = NodeRef::new(&0x99u16.to_be_bytes(), &7u64.to_be_bytes());
        let err = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), foreign)])
            .commit(&mut g)
            .expect_err("a foreign endpoint is not a member of this graph");
        assert!(
            matches!(err, Error::EdgeNotMember { .. }),
            "{engine}: unexpected error: {err:?}"
        );
        assert!(
            !g.has_edges().expect("has_edges"),
            "{engine}: no edge landed"
        );
        assert!(
            face(&store, 0x01).is_empty() && face(&store, 0x0B).is_empty(),
            "{engine}: no face landed either"
        );

        // A cluster over ONE member is refused, and the edge that would have
        // been its member is not written either.
        let err = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .commit(&mut g)
            .expect_err("a cluster needs two members");
        assert!(
            matches!(err, Error::ClusterTooSmall { members: 1, .. }),
            "{engine}: unexpected error: {err:?}"
        );
        assert!(
            !g.has_edges().expect("has_edges"),
            "{engine}: still nothing"
        );
        assert!(
            hyper(&store)
                .clusters(Kind::COMMUNITY)
                .expect("clusters")
                .is_empty(),
            "{engine}: no cluster row"
        );

        // A batch handed to a caller and never committed writes nothing.
        let mut batch = okm_core::Batch::default();
        Vec::<Decl<Employment>>::new()
            .decls([edge(person(3), org(3)), edge(person(4), org(4))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .write_into(&mut g, &mut batch)
            .expect("encode");
        drop(batch);
        assert!(
            !g.has_edges().expect("has_edges"),
            "{engine}: an uncommitted batch writes nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. A node exists only as an endpoint
// ---------------------------------------------------------------------------

#[test]
fn a_node_exists_only_as_an_endpoint() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        // A declared node no edge names: refused, nothing written.
        let err = Vec::<Decl<Employment>>::new()
            .decls([Decl::node(EmploymentNode::Person, &PersonId { id: 3 })])
            .commit(&mut g)
            .expect_err("a bare node has no creation path");
        assert!(
            matches!(err, Error::BareNode { ns: 0x30, .. }),
            "{engine}: unexpected error: {err:?}"
        );
        assert!(
            Person::collection(store.clone())
                .get_row(&PersonId { id: 3 })
                .expect("read")
                .is_none(),
            "{engine}: no row materialized"
        );

        // The same declaration is accepted once an edge names that endpoint —
        // it is the endpoint, not the declaration, that makes the node exist.
        let report = Vec::<Decl<Employment>>::new()
            .decls([
                Decl::node(EmploymentNode::Person, &PersonId { id: 3 }),
                edge(person(3), org(3)),
                edge(person(4), org(4)),
            ])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .commit(&mut g)
            .expect("an endpoint may be declared");
        assert_eq!(report.edge_ids.len(), 2);
    }
}

// ---------------------------------------------------------------------------
// 7. Deleting an edge takes exactly its rows
// ---------------------------------------------------------------------------

#[test]
fn an_edge_delete_takes_exactly_its_rows() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // A = {e1, e2, e3}, B = {e2, e3, e4}: e2 and e3 are shared.
        let first = Vec::<Decl<Employment>>::new()
            .decls([
                edge(person(1), org(1)),
                edge(person(2), org(2)),
                edge(person(3), org(3)),
            ])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("a")))
            .commit(&mut g)
            .expect("a");
        let a = ClusterRef::new(Kind::COMMUNITY, first.hyper_ids[0]);
        let shared: Vec<u64> = first.edge_ids[1..].to_vec();
        drop(first);

        // B reuses those two edges: a member row is (kind, cluster, edge), so
        // naming an existing edge in a second cluster is a legitimate write. The
        // id is explicit because the members move into it in the SAME batch that
        // creates it — an id allocated by that batch could not be named before
        // the call.
        let b = ClusterRef::new(Kind::COMMUNITY, 50);
        let fourth = Vec::<Decl<Employment>>::new()
            .decls([edge(person(4), org(4))])
            .commit(&mut g)
            .expect("fourth")
            .edge_ids[0];
        // `0` is the head's "no parent" value, so it is nobody's id.
        let err = Vec::<Decl<Employment>>::new()
            .decls([])
            .hyper_edge(HyperEdge::merge_into(Kind::COMMUNITY, 0, Dict::new()))
            .commit(&mut g)
            .expect_err("hyper_id 0 is reserved");
        assert!(
            matches!(err, Error::ReservedClusterId),
            "{engine}: unexpected error: {err:?}"
        );
        Vec::<Decl<Employment>>::new()
            .decls([])
            .hyper_edge(HyperEdge::merge_into(Kind::COMMUNITY, b.id, attrs("b")))
            .reassign(shared[0], a, b)
            .reassign(shared[1], a, b)
            .reassign(fourth, a, b)
            .commit(&mut g)
            .expect("move");
        // A lost two members but kept one, so it is below the minimum and is
        // gone; B has three.
        assert!(
            hyper(&store).lookup(a).expect("lookup").is_none(),
            "{engine}: a cluster below two members stops existing"
        );
        assert_eq!(
            hyper(&store).members(b).expect("members").len(),
            3,
            "{engine}: b holds the moved edges"
        );

        // Delete the shared edge: its rows leave, every other row stays.
        let doomed = shared[0];
        let others_before: Vec<u64> = members(&store, engine, b)
            .into_iter()
            .filter(|id| *id != doomed)
            .collect();
        Vec::<Decl<Employment>>::new()
            .decls([])
            .unlink(doomed)
            .commit(&mut g)
            .expect("unlink");

        assert!(
            g.edge(doomed).expect("read").is_none(),
            "{engine}: the edge row is gone"
        );
        assert_eq!(
            members(&store, engine, b),
            others_before,
            "{engine}: exactly one member row left"
        );
        assert_eq!(
            hyper(&store)
                .lookup(b)
                .expect("lookup")
                .expect("row")
                .member_count(),
            2,
            "{engine}: the count follows the deletion"
        );
        for survivor in others_before {
            assert!(
                g.edge(survivor).expect("read").is_some(),
                "{engine}: an unrelated edge is untouched"
            );
        }
        // …and the deleted edge's node rows are gone from every face.
        let mut stale = person(2).encode();
        stale.extend_from_slice(&doomed.to_be_bytes());
        assert!(
            store.get(&key(0x0C, b.id, &stale)).expect("read").is_none(),
            "{engine}: derived node rows go with the edge"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. The three reduces, in the write batch
// ---------------------------------------------------------------------------

#[test]
fn the_three_reduces_run_in_the_write_batch() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), org(1))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, Dict::new()))
            .reduce_node::<Person>(|p| p.seen += 1)
            .reduce_edge(|e: &mut Employment| e.weight += 1)
            .reduce_hyper_edge(|h| {
                h.insert("weight".to_string(), Variant::U64(2));
            })
            .commit(&mut g)
            .expect("commit");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);

        let people = Person::collection(store.clone());
        for id in [1u64, 2] {
            let row = people
                .get_row(&PersonId { id })
                .expect("read")
                .unwrap_or_else(|| panic!("{engine}: person {id} was created on demand"));
            assert_eq!(
                row.seen, 1,
                "{engine}: the node reducer ran for person {id}"
            );
        }
        // A collection with no registered reducer gets no row: an endpoint
        // exists because an edge names it, not because a table was touched.
        assert!(
            Org::collection(store.clone())
                .get_row(&OrgId { id: 1 })
                .expect("read")
                .is_none(),
            "{engine}: no reducer, no row"
        );

        let row = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!(
            row.payload.get("weight"),
            Some(&Variant::U64(2)),
            "{engine}: the cluster reducer ran"
        );
        assert_eq!(row.member_count(), 2, "{engine}: counts stamped last");
        assert_eq!(row.node_count(), 3, "{engine}: person1, person2, org1");

        // A second batch with the same collection accumulates instead of
        // resetting: the row is read, mutated and rewritten in one commit.
        Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(2))])
            .reduce_node::<Person>(|p| p.seen += 1)
            .commit(&mut g)
            .expect("second batch");
        assert_eq!(
            people
                .get_row(&PersonId { id: 1 })
                .expect("read")
                .expect("row")
                .seen,
            2,
            "{engine}: the counter accumulated"
        );
        assert_eq!(
            people
                .get_row(&PersonId { id: 2 })
                .expect("read")
                .expect("row")
                .seen,
            1,
            "{engine}: an untouched node keeps its count"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. merge / split / diff
// ---------------------------------------------------------------------------

#[test]
fn merge_split_and_diff_are_set_operations() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let left = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), org(2))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("left")))
            .commit(&mut g)
            .expect("left");
        let right = Vec::<Decl<Employment>>::new()
            .decls([edge(person(3), org(3)), edge(person(4), org(4))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("right")))
            .commit(&mut g)
            .expect("right");
        let a = ClusterRef::new(Kind::COMMUNITY, left.hyper_ids[0]);
        let b = ClusterRef::new(Kind::COMMUNITY, right.hyper_ids[0]);

        // Two versions over disjoint edges: the diff is all one-sided.
        let diff = hyper(&store).diff(a, b).expect("diff");
        assert_eq!(diff.only_a.len(), 2, "{engine}: a's members");
        assert_eq!(diff.only_b.len(), 2, "{engine}: b's members");
        assert!(diff.shared.is_empty());

        // Merge b into a: the union, and the source row is gone.
        Vec::<Decl<Employment>>::new()
            .decls([])
            .merge([b], a)
            .commit(&mut g)
            .expect("merge");
        assert_eq!(members(&store, engine, a).len(), 4, "{engine}: the union");
        assert!(
            hyper(&store).lookup(b).expect("lookup").is_none(),
            "{engine}: the source cluster is removed"
        );

        // Split two of them back out into a fresh cluster.
        let taken = members(&store, engine, a)[..2].to_vec();
        let report = Vec::<Decl<Employment>>::new()
            .decls([])
            .split(a, taken.clone())
            .commit(&mut g)
            .expect("split");
        let c = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        assert_eq!(members(&store, engine, c), taken, "{engine}: moved");
        assert_eq!(members(&store, engine, a).len(), 2, "{engine}: left behind");
        assert_eq!(
            hyper(&store)
                .lookup(c)
                .expect("lookup")
                .expect("row")
                .payload
                .get("label"),
            Some(&Variant::Str("left".to_string())),
            "{engine}: a split carries the payload it came from"
        );
    }
}

// ---------------------------------------------------------------------------
// 10. Nesting, and that a kind's cluster scan returns only clusters
// ---------------------------------------------------------------------------

#[test]
fn a_kinds_cluster_scan_returns_clusters_and_nothing_else() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let parent = Vec::<Decl<Employment>>::new()
            .decls([edge(person(1), org(1)), edge(person(2), org(2))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("parent")))
            .commit(&mut g)
            .expect("parent")
            .hyper_ids[0];
        let child = Vec::<Decl<Employment>>::new()
            .decls([edge(person(3), org(3)), edge(person(4), org(4))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("child")).under(parent))
            .commit(&mut g)
            .expect("child")
            .hyper_ids[0];

        let listed = hyper(&store).clusters(Kind::COMMUNITY).expect("clusters");
        assert_eq!(
            listed,
            vec![parent, child],
            "{engine}: one scan of 0x0A, ids only — no member row is read as a cluster"
        );
        // The member and child rows live in other faces, so the same scan is
        // blind to them by construction.
        assert_eq!(face(&store, 0x0B).len(), 4, "{engine}: member rows exist");
        assert_eq!(face(&store, 0x0D).len(), 1, "{engine}: child rows exist");
        assert_eq!(
            hyper(&store)
                .children(Kind::COMMUNITY, parent)
                .expect("children"),
            vec![child],
            "{engine}: one scan of 0x0D"
        );
        assert_eq!(
            hyper(&store)
                .parent(ClusterRef::new(Kind::COMMUNITY, child))
                .expect("parent"),
            parent,
            "{engine}: the head is the parent, one point read"
        );

        // Moving the child under the other root rewrites the derived index.
        let other = Vec::<Decl<Employment>>::new()
            .decls([edge(person(5), org(5)), edge(person(6), org(6))])
            .hyper_edge(HyperEdge::create(Kind::COMMUNITY, attrs("other")))
            .commit(&mut g)
            .expect("other")
            .hyper_ids[0];
        Vec::<Decl<Employment>>::new()
            .decls([])
            .hyper_edge(HyperEdge::merge_into(Kind::COMMUNITY, child, Dict::new()).under(other))
            .commit(&mut g)
            .expect("reparent");
        assert!(
            hyper(&store)
                .children(Kind::COMMUNITY, parent)
                .expect("children")
                .is_empty(),
            "{engine}: the old child row is gone"
        );
        assert_eq!(
            hyper(&store)
                .children(Kind::COMMUNITY, other)
                .expect("children"),
            vec![child],
            "{engine}: the new child row is there"
        );
        // The merged declaration kept the payload: an empty Dict merges in
        // nothing (a name both carry would be overwritten).
        assert_eq!(
            hyper(&store)
                .lookup(ClusterRef::new(Kind::COMMUNITY, child))
                .expect("lookup")
                .expect("row")
                .payload
                .get("label"),
            Some(&Variant::Str("child".to_string())),
            "{engine}: merging into a cluster keeps what it carried"
        );
    }
}
