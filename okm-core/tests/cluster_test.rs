//! Clusters (ADR-0066) on the real engine matrix: the three fixed
//! faces, their byte layout, the `Fragment` write verb, and the rules the ADR
//! fixes.
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
    Cluster, ClusterRef, Decl, Document, DocumentEncode, EdgeHead, EdgeKey, Error, Fragment, Graph,
    GraphEncode, Hyperedges, KeyEncode, Kind, NodeRef, Seekable, VirtualStorage, edge_cluster_key,
};
use okm_query::intersect_seek;
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

/// A node table with NO conflict hook registered: its nodes exist (as endpoints)
/// but nothing materializes a row for them.
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

fn head(src: NodeRef, dst: NodeRef) -> EdgeHead {
    EdgeHead {
        src,
        dst,
        predicate: "works_at".to_string(),
    }
}

fn edge(src: NodeRef, dst: NodeRef) -> Decl<Employment> {
    Decl::edge(head(src, dst))
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

fn members(store: &TestStore, engine: &str, cluster: ClusterRef) -> Vec<EdgeKey> {
    hyper(store)
        .members(cluster)
        .unwrap_or_else(|e| panic!("{engine}: members: {e}"))
}

/// A head that names an edge which already exists. The tuple IS the identity
/// (ADR-0067), so this is the only thing a re-assertion or a delete needs — the
/// predicate's name is one vocabulary read away.
fn head_of(g: &mut Graph<TestStore, Employment>, key: &EdgeKey) -> EdgeHead {
    let predicate = g
        .predicate_vocab_name(key.predicate)
        .expect("vocab")
        .expect("a key's predicate was interned when it was written");
    EdgeHead {
        src: key.src.clone(),
        dst: key.dst.clone(),
        predicate,
    }
}

// ---------------------------------------------------------------------------
// 1. Every row kind, at the layout the ADR states
// ---------------------------------------------------------------------------

#[test]
fn every_row_kind_round_trips_at_the_layout_the_adr_states() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(9)), edge(person(2), org(9))]);
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, attrs("acme")))
            .on_edge_conflict(|_stored, _declared| Employment { weight: 7 });
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        assert_eq!(
            report.edges.len(),
            2,
            "{engine}: one identity per edge decl"
        );
        assert_eq!(report.hyper_ids.len(), 1, "{engine}: one id per cluster");
        let id = report.hyper_ids[0];
        let cluster = ClusterRef::new(Kind::COMMUNITY, id);

        // The edge rows themselves are the ordinary graph rows: the hook ran, so
        // the payload is the fragment's, not a default.
        for edge in &report.edges {
            let body = g.edge(edge).expect("edge").expect("present");
            assert_eq!(body.weight, 7, "{engine}: the edge conflict hook ran");
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

        // 0x0B — one empty-valued row per member edge, the edge's whole identity
        // after the cluster prefix (ADR-0067).
        assert_eq!(members(&store, engine, cluster), report.edges);
        for edge in &report.edges {
            assert_eq!(
                store.get(&key(0x0B, id, &edge.wire())).expect("read"),
                Some(Vec::new()),
                "{engine}: a member row carries no value — the edge does"
            );
        }

        // 0x0C is the child index — no parent, so no child row, and nothing of
        // the removed node-member face may reappear here.
        assert!(
            face(&store, 0x0C).is_empty(),
            "{engine}: no parent, no child"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The derived face rebuilds to exactly what the write path wrote
// ---------------------------------------------------------------------------

#[test]
fn derived_face_rebuilds_to_exactly_what_the_write_path_wrote() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // A parent cluster, then a child under it, so 0x0C has a row.
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(2))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("parent")));
        let parent = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: parent: {e}"))
            .hyper_ids[0];
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(3), org(4))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("child")).under(parent));
        let child = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: child: {e}"))
            .hyper_ids[0];

        let cluster = ClusterRef::new(Kind::COMMUNITY, child);
        let members_before = members(&store, engine, cluster);
        let children_before = face(&store, 0x0C);
        assert_eq!(children_before.len(), 1, "{engine}: one child row");

        let report = hyper(&store)
            .rebuild_derived(usize::MAX)
            .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
        assert_eq!(report.clusters, 2, "{engine}: two cluster rows were walked");

        assert_eq!(
            face(&store, 0x0C),
            children_before,
            "{engine}: 0x0C rebuilt to the same bytes the write path produced"
        );

        // The derived rows are an INDEX, not a second truth: the authority is
        // untouched, so the answers are identical.
        assert_eq!(
            members(&store, engine, cluster),
            members_before,
            "{engine}: membership survived the rebuild"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Idempotent membership
// ---------------------------------------------------------------------------

#[test]
fn membership_is_idempotent() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(2))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        let before = face(&store, 0x0B);
        let keys = report.edges.clone();

        // The same (kind, cluster, edge) triple twice: naming the two edges
        // again and putting them into the cluster they are already in is one
        // row, byte-identical — an upsert, not an append.
        for _ in 0..2 {
            let mut fragment = Fragment::<Employment>::new([
                Decl::edge(head(person(1), org(1))),
                Decl::edge(head(person(2), org(2))),
            ]);
            fragment.cluster(Cluster::into(Kind::COMMUNITY, cluster.id, Dict::new()));
            let again = g
                .incorporate(fragment)
                .unwrap_or_else(|e| panic!("{engine}: re-assert: {e}"));
            assert_eq!(again.edges, keys, "{engine}: the same tuples are answered");
            assert!(
                again.hyper_ids.is_empty(),
                "{engine}: nothing was created, so nothing was allocated"
            );
        }
        assert_eq!(
            face(&store, 0x0B),
            before,
            "{engine}: member rows unchanged"
        );

        // Naming the same head twice in a fragment is still one edge: identity is
        // the tuple, so the second declaration re-asserts the first rather than
        // creating a parallel edge.
        let works_pred = g
            .predicate_id("works_at")
            .expect("vocab")
            .expect("the predicate was interned");
        let twice_over = EdgeKey::new(person(3), works_pred, org(3));
        let fragment = Fragment::<Employment>::new([
            Decl::edge(head(person(3), org(3))),
            Decl::edge(head(person(3), org(3))),
        ]);
        let twice = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: duplicate naming: {e}"));
        assert_eq!(
            twice.edges,
            vec![twice_over.clone(), twice_over.clone()],
            "{engine}: both declarations answered the same tuple"
        );
        assert_eq!(
            g.edges()
                .expect("edges")
                .into_iter()
                .filter(|key| *key == twice_over)
                .count(),
            1,
            "{engine}: one row, not two"
        );
        assert_eq!(
            members(&store, engine, cluster),
            keys,
            "{engine}: naming an edge writes no membership row"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Same-batch atomicity: a refused batch leaves nothing
// ---------------------------------------------------------------------------

#[test]
fn a_refused_batch_leaves_nothing() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        // Two edges, one foreign endpoint: the first edge is already encoded
        // into the batch when the second one is refused.
        let foreign = NodeRef::new(&0x99u16.to_be_bytes(), &7u64.to_be_bytes());
        let fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), foreign)]);
        let err = g
            .incorporate(fragment)
            .expect_err("a foreign endpoint is not a member of this graph");
        assert!(
            matches!(err, Error::ForeignEndpoint { .. }),
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
        let mut fragment = Fragment::<Employment>::new([edge(person(1), org(1))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let err = g
            .incorporate(fragment)
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
    }
}

// ---------------------------------------------------------------------------
// 5. A node exists only as an endpoint
// ---------------------------------------------------------------------------

#[test]
fn a_node_exists_only_as_an_endpoint() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        // A declared node no edge names: refused, nothing written.
        let fragment =
            Fragment::<Employment>::new([Decl::node(EmploymentNode::Person, &PersonId { id: 3 })]);
        let err = g
            .incorporate(fragment)
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
        let mut fragment = Fragment::<Employment>::new([
            Decl::node(EmploymentNode::Person, &PersonId { id: 3 }),
            edge(person(3), org(3)),
            edge(person(4), org(4)),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .expect("an endpoint may be declared");
        assert_eq!(report.edges.len(), 2);
    }
}

// ---------------------------------------------------------------------------
// 6. Deleting an edge takes exactly its rows
// ---------------------------------------------------------------------------

#[test]
fn an_edge_delete_takes_exactly_its_rows() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // A = {e1, e2, e3}, B = {e2, e3, e4}: e2 and e3 are shared.
        let mut fragment = Fragment::<Employment>::new([
            edge(person(1), org(1)),
            edge(person(2), org(2)),
            edge(person(3), org(3)),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("a")));
        let first = g.incorporate(fragment).expect("a");
        let a = ClusterRef::new(Kind::COMMUNITY, first.hyper_ids[0]);
        let shared: Vec<EdgeKey> = first.edges[1..].to_vec();
        drop(first);

        // B reuses those two edges: a member row is (kind, cluster, edge), so
        // naming an existing edge in a second cluster is a legitimate write. The
        // cluster's id is explicit because the members move into it in the SAME
        // batch that creates it — an id allocated by that batch could not be
        // named before the call.
        let b = ClusterRef::new(Kind::COMMUNITY, 50);
        let fragment = Fragment::<Employment>::new([edge(person(4), org(4))]);
        let fourth = g.incorporate(fragment).expect("fourth").edges[0].clone();
        // `0` is the head's "no parent" value, so it is nobody's id.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::into(Kind::COMMUNITY, 0, Dict::new()));
        let err = g.incorporate(fragment).expect_err("hyper_id 0 is reserved");
        assert!(
            matches!(err, Error::ReservedClusterId),
            "{engine}: unexpected error: {err:?}"
        );
        // Moving members between clusters is a COMPOSITION, not a verb: name
        // the edges (an id-carrying declaration), drop them from the cluster
        // they leave and declare them into the one they join — one fragment,
        // one commit. This is what `reassign` used to do.
        let mut moved: Vec<Decl<Employment>> = Vec::new();
        for key in [&shared[0], &shared[1], &fourth] {
            moved.push(Decl::edge(head_of(&mut g, key)));
        }
        let mut fragment = Fragment::<Employment>::new(moved);
        fragment
            .cluster(Cluster::into(Kind::COMMUNITY, b.id, attrs("b")))
            .remove(a);
        g.incorporate(fragment).expect("move");
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
        let doomed = shared[0].clone();
        let others_before: Vec<EdgeKey> = members(&store, engine, b)
            .into_iter()
            .filter(|key| *key != doomed)
            .collect();
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.delete_edge(head_of(&mut g, &doomed));
        let retracted = g.retract(fragment).expect("retract");
        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (0, 1, 1),
            "{engine}: one edge and one membership row left, no node touched"
        );

        assert!(
            g.edge(&doomed).expect("read").is_none(),
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
                g.edge(&survivor).expect("read").is_some(),
                "{engine}: an unrelated edge is untouched"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 6b. The declarations alone are a whole write
// ---------------------------------------------------------------------------

#[test]
fn the_declarations_alone_are_a_whole_write() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // No cluster, no reducer: the fragment is the declarations and nothing else.
        let report = g
            .incorporate(Fragment::new([
                edge(person(1), org(1)),
                edge(person(2), org(2)),
            ]))
            .expect("declarations alone");

        assert_eq!(
            report.edges.len(),
            2,
            "{engine}: one identity per edge decl"
        );
        assert!(
            report.hyper_ids.is_empty(),
            "{engine}: no cluster was declared, so no id was allocated"
        );
        assert!(
            face(&store, 0x0A).is_empty() && face(&store, 0x0B).is_empty(),
            "{engine}: no cluster row and no member row without a declaration"
        );
        assert!(
            g.edge(&report.edges[0]).expect("read").is_some(),
            "{engine}: the edge is there"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. The three conflict hooks, in the write fragment
// ---------------------------------------------------------------------------

#[test]
fn the_three_conflict_hooks_run_in_the_write_fragment() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(1))]);
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, Dict::new()))
            .on_node_conflict::<Person>(|old, _declared| Person { seen: old.seen + 1 })
            .on_edge_conflict(|_stored, _declared| Employment { weight: 1 })
            .on_cluster_conflict(|_stored, declared| {
                let mut row = declared.clone();
                row.insert("weight".to_string(), Variant::U64(2));
                row
            });
        let report = g.incorporate(fragment).expect("commit");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);

        let people = Person::collection(store.clone());
        for id in [1u64, 2] {
            let row = people
                .get_row(&PersonId { id })
                .expect("read")
                .unwrap_or_else(|| panic!("{engine}: person {id} was created on demand"));
            assert_eq!(
                row.seen, 1,
                "{engine}: the node conflict hook ran for person {id}"
            );
        }
        // A collection with no registered hook gets no row: an endpoint
        // exists because an edge names it, not because a table was touched.
        assert!(
            Org::collection(store.clone())
                .get_row(&OrgId { id: 1 })
                .expect("read")
                .is_none(),
            "{engine}: no hook, no row"
        );

        let row = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!(
            row.payload.get("weight"),
            Some(&Variant::U64(2)),
            "{engine}: the cluster conflict hook ran"
        );
        assert_eq!(row.member_count(), 2, "{engine}: counts stamped last");

        // A second fragment with the same collection accumulates instead of
        // resetting: the stored row and the declared one reach the hook together.
        let mut fragment = Fragment::<Employment>::new([edge(person(1), org(2))]);
        fragment.on_node_conflict::<Person>(|old, _declared| Person { seen: old.seen + 1 });
        g.incorporate(fragment).expect("second fragment");
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

/// The hook that sees BOTH sides: a cluster's stored stamp wins over a
/// declaration whose own stamp is older, and the rest of the write (members,
/// counts) still lands. A mutator that only saw the stored row could not state
/// this — it had nothing to compare the incoming `ts` against — and a hook that
/// only saw the declared one would have lost the stamp on every write.
#[test]
fn a_cluster_hook_keeps_the_stored_payload_when_the_declared_one_is_older() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        // Newest stamp wins; equal stamps take the declaration.
        fn newest(old: &Dict, declared: &Dict) -> Dict {
            fn stamp(row: &Dict) -> u64 {
                match row.get("ts") {
                    Some(Variant::U64(ts)) => *ts,
                    _ => 0,
                }
            }
            if stamp(declared) >= stamp(old) {
                declared.clone()
            } else {
                old.clone()
            }
        }

        let mut stamped = Dict::new();
        stamped.insert("ts".to_string(), Variant::U64(10));
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(1))]);
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, stamped))
            .on_cluster_conflict(newest);
        let report = g.incorporate(fragment).expect("first write");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        assert_eq!(
            hyper(&store)
                .lookup(cluster)
                .expect("lookup")
                .expect("row")
                .payload
                .get("ts"),
            Some(&Variant::U64(10)),
            "{engine}: a creation has no stored map, so the declared stamp lands"
        );

        // The same cluster, named at its id, with an OLDER stamp and one more
        // member: membership moves, the stamp does not.
        let mut older = Dict::new();
        older.insert("ts".to_string(), Variant::U64(4));
        let mut fragment = Fragment::<Employment>::new([edge(person(3), org(1))]);
        fragment
            .cluster(Cluster::into(Kind::COMMUNITY, cluster.id, older))
            .on_cluster_conflict(newest);
        g.incorporate(fragment).expect("second write");

        let row = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!(
            row.payload.get("ts"),
            Some(&Variant::U64(10)),
            "{engine}: the stored stamp survived a declaration with an older one"
        );
        assert_eq!(
            row.member_count(),
            3,
            "{engine}: the rest of the declaration still landed"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. Merge is a verb; move and split are compositions of the same rows
// ---------------------------------------------------------------------------

#[test]
fn merge_is_a_verb_and_a_split_is_the_same_fragment_composed() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(2))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("left")));
        let left = g.incorporate(fragment).expect("left");
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(4), org(4))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("right")));
        let right = g.incorporate(fragment).expect("right");
        let a = ClusterRef::new(Kind::COMMUNITY, left.hyper_ids[0]);
        let b = ClusterRef::new(Kind::COMMUNITY, right.hyper_ids[0]);

        // Two clusters over disjoint edges. "What differs between them" is two
        // `members()` scans and a set operation on the caller's side — the
        // engine states the members, the caller states the question.
        let a_members = members(&store, engine, a);
        let b_members = members(&store, engine, b);
        assert_eq!(a_members.len(), 2, "{engine}: a's members");
        assert_eq!(b_members.len(), 2, "{engine}: b's members");
        assert!(
            a_members.iter().all(|id| !b_members.contains(id)),
            "{engine}: nothing is shared"
        );

        // Merge b into a: the union, and the source row is gone.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.incorporate([b], a);
        g.incorporate(fragment).expect("merge");
        assert_eq!(members(&store, engine, a).len(), 4, "{engine}: the union");
        assert!(
            hyper(&store).lookup(b).expect("lookup").is_none(),
            "{engine}: the source cluster is removed"
        );

        // Split two of them back out: the same fragment composed — name the
        // edges, declare a NEW cluster over them, and drop them from the old
        // one with `.remove`. One commit, and no `split` verb to spell it.
        let taken = members(&store, engine, a)[..2].to_vec();
        let mut decls: Vec<Decl<Employment>> = Vec::new();
        for key in &taken {
            decls.push(Decl::edge(head_of(&mut g, key)));
        }
        let mut fragment = Fragment::<Employment>::new(decls);
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, attrs("split")))
            .remove(a);
        let report = g.incorporate(fragment).expect("split");
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
            Some(&Variant::Str("split".to_string())),
            "{engine}: a split is a new cluster, so it carries its own declaration"
        );
        // The edges themselves never moved: a split rewrites membership rows,
        // not assertions.
        for key in &taken {
            assert!(
                g.edge(key).expect("read").is_some(),
                "{engine}: the edge keeps its identity and row"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 9. Nesting, and that a kind's cluster scan returns only clusters
// ---------------------------------------------------------------------------

#[test]
fn a_kinds_cluster_scan_returns_clusters_and_nothing_else() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(2))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("parent")));
        let parent = g.incorporate(fragment).expect("parent").hyper_ids[0];
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(4), org(4))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("child")).under(parent));
        let child = g.incorporate(fragment).expect("child").hyper_ids[0];

        let listed = hyper(&store).clusters(Kind::COMMUNITY).expect("clusters");
        assert_eq!(
            listed,
            vec![parent, child],
            "{engine}: one scan of 0x0A, ids only — no member row is read as a cluster"
        );
        // The member and child rows live in other faces, so the same scan is
        // blind to them by construction.
        assert_eq!(face(&store, 0x0B).len(), 4, "{engine}: member rows exist");
        assert_eq!(face(&store, 0x0C).len(), 1, "{engine}: child rows exist");
        assert_eq!(
            hyper(&store)
                .children(Kind::COMMUNITY, parent)
                .expect("children"),
            vec![child],
            "{engine}: one scan of 0x0C"
        );
        assert_eq!(
            hyper(&store)
                .parent(ClusterRef::new(Kind::COMMUNITY, child))
                .expect("parent"),
            parent,
            "{engine}: the head is the parent, one point read"
        );

        // Moving the child under the other root rewrites the derived index.
        let mut fragment =
            Fragment::<Employment>::new([edge(person(5), org(5)), edge(person(6), org(6))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("other")));
        let other = g.incorporate(fragment).expect("other").hyper_ids[0];
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::into(Kind::COMMUNITY, child, Dict::new()).under(other));
        g.incorporate(fragment).expect("reparent");
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

// ---------------------------------------------------------------------------
// 10. A node's delete takes every edge that names it
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_node_takes_every_edge_incident_to_it() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // p1 has two edges out, one of them shared with p2 (o1); p3 is an
        // unrelated assertion, so "everything else survived" has a witness.
        let mut fragment = Fragment::<Employment>::new([
            edge(person(1), org(1)),
            edge(person(1), org(2)),
            edge(person(2), org(1)),
            edge(person(3), org(3)),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g.incorporate(fragment).expect("write");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        let first = report.edges[0].clone();
        let second = report.edges[1].clone();
        let shared = report.edges[2].clone();
        let unrelated = report.edges[3].clone();

        // One entry per edge on each of the four traversal faces — and nowhere
        // else: `0x06` IS the payload row (ADR-0067), so there is no `0x00` row
        // for an edge at all.
        let faces = [0x06u8, 0x07, 0x08, 0x09];
        let before: Vec<usize> = faces.iter().map(|slot| face(&store, *slot).len()).collect();
        assert!(
            face(&store, 0x00).is_empty(),
            "{engine}: an edge has no primary row of its own"
        );
        assert_eq!(
            before,
            vec![4, 4, 4, 4],
            "{engine}: one row per edge per face"
        );

        let mut fragment = Fragment::<Employment>::new([]);
        fragment.delete_node(person(1));
        let retracted = g.retract(fragment).expect("retract");

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (1, 2, 2),
            "{engine}: the node, both its edges, and their two membership rows"
        );
        // Every face lost exactly the two rows those edges wrote — no sweep, no
        // collateral.
        let after: Vec<usize> = faces.iter().map(|slot| face(&store, *slot).len()).collect();
        assert_eq!(
            after,
            vec![2, 2, 2, 2],
            "{engine}: exactly two rows per face"
        );

        assert!(
            g.get_edge(&first).expect("read").is_none()
                && g.get_edge(&second).expect("read").is_none(),
            "{engine}: no dangling edge, because the edges are gone"
        );
        assert!(
            g.edge(&unrelated).expect("read").is_some(),
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
        assert!(
            g.typed_out("works_at", &person(1))
                .expect("typed")
                .is_empty(),
            "{engine}: the predicate face lost them too"
        );
        assert_eq!(
            members(&store, engine, cluster),
            vec![shared, unrelated],
            "{engine}: the cluster kept the members that still exist"
        );
        assert_eq!(
            hyper(&store)
                .lookup(cluster)
                .expect("lookup")
                .expect("row")
                .member_count(),
            2,
            "{engine}: the count follows the cascade"
        );
    }
}

// ---------------------------------------------------------------------------
// 11. An edge's delete leaves both endpoints alone
// ---------------------------------------------------------------------------

#[test]
fn deleting_an_edge_leaves_both_endpoints_alone() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = Fragment::<Employment>::new([
            edge(person(1), org(1)),
            edge(person(1), org(2)),
            edge(person(2), org(3)),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
        let report = g.incorporate(fragment).expect("write");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);
        let doomed = report.edges[0].clone();
        let kept = report.edges[1].clone();

        let mut fragment = Fragment::<Employment>::new([]);
        fragment.delete_edge(head_of(&mut g, &doomed));
        let retracted = g.retract(fragment).expect("retract");

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
            "{engine}: its incoming face is gone"
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
            .incorporate(Fragment::new([edge(person(1), org(1))]))
            .expect("the endpoints are still endpoints");
        assert_eq!(
            g.in_edges(&org(1)).expect("in"),
            vec![again.edges[0].clone()],
            "{engine}: the endpoint took a new edge"
        );
        assert_eq!(
            members(&store, engine, cluster),
            vec![kept, report.edges[2].clone()],
            "{engine}: the cluster kept its other members"
        );
    }
}

// ---------------------------------------------------------------------------
// 12. Absence is nothing to do
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_node_that_no_edge_names_is_a_no_op() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        g.incorporate(Fragment::new([
            edge(person(1), org(1)),
            edge(person(2), org(2)),
        ]))
        .expect("write");
        let primary_before = face(&store, 0x00);
        let members_before = face(&store, 0x0B);

        // A node exists only as an endpoint, so a node no edge names is not in
        // the graph: there is nothing to cascade from.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.delete_node(person(7));
        let retracted = g.retract(fragment).expect("retract");

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (0, 0, 0),
            "{engine}: a node no edge names is absent, so it reports zero"
        );
        assert_eq!(
            face(&store, 0x00),
            primary_before,
            "{engine}: not one edge row moved"
        );
        assert_eq!(
            face(&store, 0x0B),
            members_before,
            "{engine}: no membership row moved either"
        );
    }
}

#[test]
fn retracting_what_is_absent_reports_zero() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(2), org(2))]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("kept")));
        let report = g.incorporate(fragment).expect("write");
        let cluster = ClusterRef::new(Kind::COMMUNITY, report.hyper_ids[0]);

        let primary_before = face(&store, 0x00);
        let clusters_before = face(&store, 0x0A);
        let members_before = face(&store, 0x0B);

        // An edge no row holds, a node no edge names, a cluster that was never
        // created: three absences, three zeroes — and not a row written or
        // deleted. Declines are the caller's to ignore, and tolerance is the
        // reading that needs no bookkeeping on their side.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment
            .delete_edge(head(person(4), org(4)))
            .delete_node(org(99))
            .delete(ClusterRef::new(Kind::COMMUNITY, 999));
        let retracted = g.retract(fragment).expect("retract");

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (0, 0, 0),
            "{engine}: absent is nothing to do"
        );
        assert_eq!(face(&store, 0x00), primary_before, "{engine}: edges intact");
        assert_eq!(
            face(&store, 0x0A),
            clusters_before,
            "{engine}: clusters intact"
        );
        assert_eq!(
            face(&store, 0x0B),
            members_before,
            "{engine}: memberships intact"
        );
        assert_eq!(
            hyper(&store).members(cluster).expect("members").len(),
            2,
            "{engine}: the cluster that WAS there kept its members"
        );
    }
}

#[test]
fn the_two_verbs_refuse_each_others_declarations() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let report = g
            .incorporate(Fragment::new([edge(person(1), org(1))]))
            .expect("write");

        // A write fragment that also deletes: one batch, two meanings.
        let mut fragment = Fragment::<Employment>::new([edge(person(2), org(2))]);
        fragment.delete_edge(head_of(&mut g, &report.edges[0]));
        let err = g.incorporate(fragment).expect_err("incorporate writes");
        assert!(
            matches!(err, Error::IncorporateWithDeletes { deletions: 1 }),
            "{engine}: unexpected error: {err:?}"
        );

        // A retract fragment that also declares rows.
        let mut fragment = Fragment::<Employment>::new([edge(person(3), org(3))]);
        fragment.delete_edge(head_of(&mut g, &report.edges[0]));
        let err = g.retract(fragment).expect_err("retract removes");
        assert!(
            matches!(err, Error::RetractWithWrites { declarations: 1 }),
            "{engine}: unexpected error: {err:?}"
        );

        // Neither refusal wrote or removed anything.
        assert!(
            g.get_edge(&report.edges[0]).expect("read").is_some(),
            "{engine}: the refused batch left the edge alone"
        );
        assert_eq!(
            g.edges().expect("edges"),
            report.edges,
            "{engine}: and no edge was born either"
        );
    }
}

// ---------------------------------------------------------------------------
// 13. There is no update verb: an update is a conflict hook over a named id
// ---------------------------------------------------------------------------

#[test]
fn an_update_is_a_reduce_folded_over_a_named_edge() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment =
            Fragment::<Employment>::new([edge(person(1), org(1)), edge(person(1), org(2))]);
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, Dict::new()))
            .on_edge_conflict(|_stored, _declared| Employment { weight: 5 });
        let report = g.incorporate(fragment).expect("write");
        let target = report.edges[0].clone();
        let other = report.edges[1].clone();
        let members_before = face(&store, 0x0B);
        let keys_before = g.edges().expect("edges");

        // The update: declare the SAME tuple again, with a conflict hook that
        // reads what is stored. No verb of its own — the tuple, the faces and
        // the membership rows stand (ADR-0067).
        let mut fragment = Fragment::<Employment>::new([Decl::edge(head(person(1), org(1)))]);
        fragment.on_edge_conflict(|stored, _declared| Employment {
            weight: stored.weight + 100,
        });
        let updated = g.incorporate(fragment).expect("update");

        assert_eq!(
            updated.edges,
            vec![target.clone()],
            "{engine}: the same tuple is the one written"
        );
        assert!(
            updated.hyper_ids.is_empty(),
            "{engine}: an update allocates nothing"
        );
        assert_eq!(
            g.edge(&target).expect("read").expect("edge").weight,
            105,
            "{engine}: the hook read the STORED payload, not a default"
        );
        assert_eq!(
            g.edge(&other).expect("read").expect("edge").weight,
            5,
            "{engine}: the edge the fragment did not name is untouched"
        );
        assert_eq!(
            face(&store, 0x0B),
            members_before,
            "{engine}: membership rows did not move"
        );
        assert_eq!(
            g.edges().expect("edges"),
            keys_before,
            "{engine}: no edge was born or retired"
        );

        // A different head is a DIFFERENT edge: the tuple is the identity, so
        // nothing rewrites in place (ADR-0067). The old assertion keeps its
        // rows and the new one joins it.
        let fragment = Fragment::<Employment>::new([edge(person(2), org(2))]);
        let added = g.incorporate(fragment).expect("a second edge");
        assert_eq!(added.edges.len(), 1, "{engine}: one new tuple");
        assert_eq!(
            g.in_edges(&org(1)).expect("in"),
            vec![target],
            "{engine}: the first assertion still answers at its own head"
        );
        assert_eq!(
            g.in_edges(&org(2)).expect("in"),
            vec![other, added.edges[0].clone()],
            "{engine}: ... and the new one answers where it points"
        );
        assert_eq!(
            g.edges().expect("edges").len(),
            keys_before.len() + 1,
            "{engine}: a second tuple is a second edge"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. The derived edge → cluster index (0x0D)
// ---------------------------------------------------------------------------

/// A cluster's members rendered in the derived face's own key space, ascending
/// by identity — the cursor that makes [`intersect_seek`] legal against
/// `Graph::clusters_of`. Membership rows (`0x0B`) are keyed by cluster and
/// therefore ascending by identity INSIDE one cluster, which is exactly the
/// order this face stores; the test spells the keys with the public helper
/// rather than reaching into the module.
struct MemberClusters {
    keys: Vec<Vec<u8>>,
    at: usize,
}

impl MemberClusters {
    fn of(cluster: ClusterRef, mut edges: Vec<EdgeKey>) -> Self {
        edges.sort();
        Self {
            keys: edges
                .iter()
                .map(|edge| edge_cluster_key(&NS.to_be_bytes(), edge, cluster))
                .collect(),
            at: 0,
        }
    }
}

impl Seekable for MemberClusters {
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

/// 0x0D is written with the member rows, answers membership as ONE point read,
/// and leaves again when the membership does — retract included.
#[test]
fn the_edge_index_answers_membership_and_leaves_with_the_member_rows() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = Fragment::<Employment>::new([
            edge(person(1), org(1)),
            edge(person(2), org(2)),
            edge(person(3), org(3)),
        ]);
        fragment.cluster(Cluster::new(Kind::COMMUNITY, attrs("acme")));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let id = report.hyper_ids[0];
        let cluster = ClusterRef::new(Kind::COMMUNITY, id);
        let keys = report.edges.clone();

        // The layout the ADR states, byte for byte:
        // `[ns][0x0D][src][pred][dst][kind][hyper]`.
        let mut spelled = Vec::new();
        spelled.extend_from_slice(&NS.to_be_bytes());
        spelled.push(0x0D);
        keys[0].write(&mut spelled);
        spelled.push(kind_byte());
        spelled.extend_from_slice(&id.to_be_bytes());
        assert_eq!(
            store.get(&spelled).expect("read"),
            Some(Vec::new()),
            "{engine}: the edge → cluster row is one empty-valued entry"
        );
        assert_eq!(
            face(&store, 0x0D).len(),
            3,
            "{engine}: one derived row per member row"
        );

        // Membership is one point read, and the KIND is part of the key.
        assert!(
            g.member_of(Kind::COMMUNITY, id, &keys[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: a member answers true"
        );
        assert!(
            !g.member_of(Kind::COMMUNITY, id, &EdgeKey::new(person(4), 999, org(4)))
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: an edge that is not a member answers false"
        );
        assert!(
            !g.member_of(Kind::EVENT, id, &keys[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the same id under another kind is a different cluster"
        );

        // The stream answers the same fact, ordered by (kind, hyper_id).
        assert_eq!(
            g.clusters_of(&keys[1]).collect::<Vec<_>>(),
            vec![cluster],
            "{engine}: one edge's clusters, from the derived face"
        );

        // Retract one edge: its member row AND its mirror leave in one batch,
        // and the cluster (now two members) survives.
        let mut retract = Fragment::<Employment>::new(Vec::<Decl<Employment>>::new());
        retract.delete_edge(head_of(&mut g, &keys[0]));
        g.retract(retract)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));

        assert!(
            !g.member_of(Kind::COMMUNITY, id, &keys[0])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the mirror left with the member row"
        );
        assert!(
            g.clusters_of(&keys[0]).next().is_none(),
            "{engine}: a retracted edge belongs to no cluster"
        );
        assert_eq!(
            members(&store, engine, cluster),
            vec![keys[1].clone(), keys[2].clone()],
            "{engine}: the authority still holds the surviving members"
        );
        assert_eq!(
            face(&store, 0x0D).len(),
            2,
            "{engine}: and the derived face followed it exactly"
        );
    }
}

/// The derived face rebuilds from `0x0B` ALONE: wipe it, rebuild, and the bytes
/// are the ones the write path produced.
#[test]
fn the_edge_index_rebuilds_from_the_member_rows_alone() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut fragment = Fragment::<Employment>::new([
            edge(person(1), org(1)),
            edge(person(2), org(2)),
            edge(person(3), org(3)),
        ]);
        // Two clusters of different kinds over the same edges: six member rows,
        // so the face cannot be rebuilt correctly by accident.
        fragment
            .cluster(Cluster::new(Kind::COMMUNITY, attrs("acme")))
            .cluster(Cluster::new(Kind::EVENT, attrs("event")));
        g.incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        let written = face(&store, 0x0D);
        assert_eq!(written.len(), 6, "{engine}: three edges, two clusters");

        // Wipe the derived face by hand — the state an interrupted rebuild, a
        // restore without it, or a build that never wrote it would leave.
        for (key, _) in &written {
            store.del(key).expect("delete");
        }
        assert!(
            face(&store, 0x0D).is_empty(),
            "{engine}: the face really is gone"
        );

        let report = hyper(&store)
            .rebuild_derived(usize::MAX)
            .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
        assert_eq!(
            report.members, 6,
            "{engine}: the rebuild walked the member rows"
        );
        assert_eq!(
            face(&store, 0x0D),
            written,
            "{engine}: 0x0D rebuilt to the same bytes the write path produced"
        );
    }
}

/// `intersect(clusters_of(node), members(C))` — the two derived answers align
/// because they share one key space, and the answer is the eager one.
#[test]
fn traversal_and_membership_intersect_on_the_derived_face() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // Four edges; the cluster holds the two that point at org 2, and each
        // lives in a different fragment so the others stay ungrouped.
        let plain = g
            .incorporate(Fragment::new([
                edge(person(1), org(1)),
                edge(person(2), org(1)),
            ]))
            .unwrap_or_else(|e| panic!("{engine}: plain: {e}"));
        let mut grouped =
            Fragment::<Employment>::new([edge(person(1), org(2)), edge(person(2), org(2))]);
        grouped.cluster(Cluster::new(Kind::COMMUNITY, attrs("org2")));
        let report = g
            .incorporate(grouped)
            .unwrap_or_else(|e| panic!("{engine}: grouped: {e}"));
        let id = report.hyper_ids[0];
        let cluster = ClusterRef::new(Kind::COMMUNITY, id);

        let out_of_p1 = g
            .out_edges(&person(1))
            .unwrap_or_else(|e| panic!("{engine}: out: {e}"));
        assert_eq!(
            out_of_p1,
            vec![plain.edges[0].clone(), report.edges[0].clone()]
        );

        // The intersection of the two derived streams: the node's incident edges
        // in the face's key space, against the cluster's members in the same one.
        let members_of_c = MemberClusters::of(cluster, report.edges.clone());
        let shared: Vec<Vec<u8>> = intersect_seek(g.clusters_of(person(1)), members_of_c).collect();
        assert_eq!(shared.len(), 1, "{engine}: one of p1's edges is in C");
        assert_eq!(
            shared,
            vec![edge_cluster_key(
                &NS.to_be_bytes(),
                &report.edges[0],
                cluster
            )],
            "{engine}: and it is the edge that points at org 2"
        );
        // Same answer as the eager set intersection of the identity spaces.
        let in_c = members(&store, engine, cluster);
        let eager: Vec<EdgeKey> = out_of_p1
            .iter()
            .filter(|edge| in_c.contains(edge))
            .cloned()
            .collect();
        assert_eq!(
            eager,
            vec![report.edges[0].clone()],
            "{engine}: the derived alignment answers what the sets say"
        );

        // The point query as a filter predicate inside the traversal: same edge.
        let filtered: Vec<EdgeKey> = g
            .out(&person(1))
            .filter(|step| {
                g.member_of(Kind::COMMUNITY, id, &step.key())
                    .unwrap_or(false)
            })
            .map(|step| step.key())
            .collect();
        assert_eq!(
            filtered,
            vec![report.edges[0].clone()],
            "{engine}: `member_of` filters a hop without a set built first"
        );

        // And the node form of the cluster stream agrees with both.
        let via_node: Vec<u64> = g.clusters_of(person(1)).map(|cluster| cluster.id).collect();
        assert_eq!(via_node, vec![id], "{engine}: p1 is in exactly C");
        assert!(
            g.clusters_of(&plain.edges[1]).next().is_none(),
            "{engine}: an ungrouped edge belongs to no cluster"
        );
    }
}
