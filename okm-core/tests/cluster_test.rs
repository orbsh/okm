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
use okm_core::{Cluster, ClusterRef, Decl, Document, DocumentEncode, EdgeHead, EdgeKey, EdgeSet, NodeSet, Error, Fragment, Graph, GraphEncode, HandleKey, Hyperedges, KeyEncode, Kind, NodeRef, Seekable, VirtualStorage, edge_cluster_key};
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

// The CALLER's vocabulary, defined here because a test is a caller: `Kind` is one
// byte and okm assigns it no meaning. An application picks its own bytes beside
// its schema (k10r: 1 = community, 5 = flow); the store never learns the words.
const COMMUNITY: &str = "community";
const EVENT: &str = "event";
const SUBGRAPH: &str = "subgraph";
const SCOPE: &str = "scope";
const FLOW: &str = "flow";

/// A label's interned id, resolved the way the store resolves it. The fixtures
/// name kinds by WORD; a number is only ever what the vocabulary answers, never
/// an input — so nothing here spells a kind as `Kind(n)`.
fn kid(store: &TestStore, label: &str) -> Kind {
    let hy = hyper(store);
    if let Some(kind) = hy.kind_of(label).expect("kind_of") {
        return kind;
    }
    // A fixture that spells a key BEFORE the write has no vocabulary row to read;
    // minting here gives the same id the write will land on (first-seen, append-only).
    let mut mint = hy;
    mint.kind_id(label).expect("kind_id")
}


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

/// The face address, spelled out: `[ns 2B][slot][kind 2B][hyper_id][tail…]` —
/// the kind is an interned id, so it is two bytes, exactly like a predicate.
fn key(kind: Kind, slot: u8, hyper_id: u64, tail: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(3 + 2 + 8 + tail.len());
    k.extend_from_slice(&NS.to_be_bytes());
    k.push(slot);
    k.extend_from_slice(&kind.0.to_be_bytes());
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


fn members(store: &TestStore, engine: &str, set: EdgeSet) -> Vec<EdgeKey> {
    hyper(store)
        .edge_members(set)
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

/// The stored row identity of a caller's tuple — the handle registry read
/// twice, which is what every face below is actually keyed by (ADR-0067). The
/// test spells it out instead of trusting `hyper` to agree with itself.
fn row_of(g: &Graph<TestStore, Employment>, key: &EdgeKey) -> HandleKey {
    let registry = g.handles();
    let src = registry
        .handle_of(&key.src)
        .expect("registry")
        .expect("a written edge's source was issued a handle");
    let dst = registry
        .handle_of(&key.dst)
        .expect("registry")
        .expect("a written edge's destination was issued a handle");
    HandleKey::new(src, key.predicate, dst)
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
            .cluster(Cluster::edges(COMMUNITY, attrs("acme")))
            .on_edge_conflict(|_stored, _declared| Employment { weight: 7 });
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        assert_eq!(
            report.edges.len(),
            2,
            "{engine}: one identity per edge decl"
        );
        assert_eq!(report.edge_sets.len() + report.node_sets.len(), 1, "{engine}: one id per cluster");
        let id = report.edge_sets[0].id;
        let cluster = EdgeSet::new(kid(&store, COMMUNITY), id);

        // The edge rows themselves are the ordinary graph rows: the hook ran, so
        // the payload is the fragment's, not a default.
        for edge in &report.edges {
            let body = g.edge(edge).expect("edge").expect("present");
            assert_eq!(body.weight, 7, "{engine}: the edge conflict hook ran");
        }

        // 0x0A — the cluster row: `[parent u64 BE][dynamic payload]`, and NOTHING
        // longer lives under it (no member row shares this face).
        let row = store
            .get(&key(kid(&store, COMMUNITY), 0x0C, id, &[]))
            .expect("read")
            .unwrap_or_else(|| panic!("{engine}: cluster row"));
        assert_eq!(&row[..8], &0u64.to_be_bytes(), "{engine}: root head");
        assert_eq!(
            store.get(&key(kid(&store, COMMUNITY), 0x0C, id, &[0x00])).expect("read"),
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
            stored.edge_count(),
            2,
            "{engine}: member count in the row"
        );

        // 0x0B — one empty-valued row per member edge, the edge's stored handle
        // row after the cluster prefix (ADR-0067): `[src_h 8][pred 2][dst_h 8]`,
        // eighteen bytes whatever width the member tables' keys are.
        assert_eq!(members(&store, engine, cluster), report.edges);
        for edge in &report.edges {
            let spelled = row_of(&g, edge);
            assert_eq!(
                spelled.wire().len(),
                HandleKey::WIDTH,
                "{engine}: a member row's tail is the fixed-width handle row"
            );
            assert_eq!(
                store.get(&key(kid(&store, COMMUNITY), 0x0D, id, &spelled.wire())).expect("read"),
                Some(Vec::new()),
                "{engine}: a member row carries no value — the edge does"
            );
        }

        // 0x0C is the child index — no parent, so no child row, and nothing of
        // the removed node-member face may reappear here.
        assert!(
            face(&store, 0x0E).is_empty(),
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("parent")));
        let parent = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: parent: {e}"))
            .edge_sets[0].id;
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(3), org(4))]);
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("child")).under(parent));
        let child = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: child: {e}"))
            .edge_sets[0].id;

        let cluster = EdgeSet::new(kid(&store, COMMUNITY), child);
        let members_before = members(&store, engine, cluster);
        let children_before = face(&store, 0x0E);
        assert_eq!(children_before.len(), 1, "{engine}: one child row");

        let report = hyper(&store)
            .rebuild_derived(usize::MAX)
            .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
        assert_eq!(report.clusters, 2, "{engine}: two cluster rows were walked");

        assert_eq!(
            face(&store, 0x0E),
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
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let cluster = report.edge_sets[0];
        let before = face(&store, 0x0D);
        let keys = report.edges.clone();

        // The same (kind, cluster, edge) triple twice: naming the two edges
        // again and putting them into the cluster they are already in is one
        // row, byte-identical — an upsert, not an append.
        for _ in 0..2 {
            let mut fragment = Fragment::<Employment>::new([
                Decl::edge(head(person(1), org(1))),
                Decl::edge(head(person(2), org(2))),
            ]);
            fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()).at(cluster.id));
            let again = g
                .incorporate(fragment)
                .unwrap_or_else(|e| panic!("{engine}: re-assert: {e}"));
            assert_eq!(again.edges, keys, "{engine}: the same tuples are answered");
            assert!(
                again.edge_sets.is_empty() && again.node_sets.is_empty(),
                "{engine}: nothing was created, so nothing was allocated"
            );
        }
        assert_eq!(
            face(&store, 0x0D),
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
            face(&store, 0x01).is_empty() && face(&store, 0x0D).is_empty(),
            "{engine}: no face landed either"
        );

        // A cluster over ONE member is a legal write: a cluster is a scope, not
        // a grouping that needs two edges before it can exist (ADR-0066 §3). The
        // row lands with an honest count of one, and its single member row with
        // it — this fragment is accepted, not refused.
        let mut fragment = Fragment::<Employment>::new([edge(person(1), org(1))]);
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()));
        let report = g
            .incorporate(fragment)
            .expect("a one-member cluster is a scope, not a refusal");
        assert_eq!(
            report.edges.len(),
            1,
            "{engine}: the edge that would have been its member landed"
        );
        let one = report.edge_sets[0];
        assert_eq!(
            hyper(&store)
                .lookup(one)
                .expect("lookup")
                .expect("row")
                .edge_count(),
            1,
            "{engine}: the row states the count it has"
        );
        assert_eq!(
            hyper(&store).edge_members(one).expect("members").len(),
            1,
            "{engine}: one member row, written in the same batch"
        );
    }
}

// ---------------------------------------------------------------------------
// 4b. An empty cluster is a scope, and its row is a row like any other
// ---------------------------------------------------------------------------

#[test]
fn a_cluster_created_with_no_members_at_all_is_a_scope() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // The declaration and nothing else: no edge, no member. An empty scope is
        // a statement — "this subject is sensitive, do not ask" — so it is a
        // legal write, and the row it makes says zero.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::edges(SCOPE, attrs("do-not-ask")));
        let report = g
            .incorporate(fragment)
            .expect("an empty scope is a declared scope");
        assert_eq!(report.edge_sets.len() + report.node_sets.len(), 1, "{engine}: one id allocated");
        assert!(report.edges.is_empty(), "{engine}: no edge declared");
        let scope = report.edge_sets[0];

        let row = hyper(&store).lookup(scope).expect("lookup").expect("row");
        assert_eq!(row.edge_count(), 0, "{engine}: zero is the honest count");
        assert_eq!(
            row.payload.get("label"),
            Some(&Variant::Str("do-not-ask".to_string())),
            "{engine}: the payload is the statement the scope makes"
        );
        assert!(
            hyper(&store).edge_members(scope).expect("members").is_empty(),
            "{engine}: an empty member list, not an error"
        );
        assert_eq!(
            hyper(&store).edge_sets(SCOPE).expect("sets").iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![scope.id],
            "{engine}: and the kind scan still lists it"
        );
        assert!(
            face(&store, 0x0D).is_empty(),
            "{engine}: nothing was invented to join it to the graph"
        );
    }
}

#[test]
fn an_explicitly_declared_empty_cluster_survives_a_rebuild() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // A parent scope with no edges and a child scope under it, also empty:
        // the child index is derived from the cluster rows, so the rebuild has to
        // work off rows whose member face is empty.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::edges(SCOPE, attrs("parent")));
        let parent = g.incorporate(fragment).expect("parent").edge_sets[0].id;
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::edges(SCOPE, attrs("child")).under(parent));
        let child = g.incorporate(fragment).expect("child").edge_sets[0].id;
        assert_eq!(
            hyper(&store)
                .children(SCOPE, parent)
                .expect("children"),
            vec![child],
            "{engine}: the child index was written with the rows"
        );

        let report = hyper(&store).rebuild_derived(usize::MAX).expect("rebuild");
        assert_eq!(report.clusters, 2, "{engine}: both rows walked");
        assert_eq!(report.members, 0, "{engine}: and no member row exists");

        for (id, label) in [(parent, "parent"), (child, "child")] {
            let row = hyper(&store)
                .lookup(EdgeSet::new(kid(&store, SCOPE), id))
                .expect("lookup")
                .unwrap_or_else(|| panic!("{engine}: {label} survived the rebuild"));
            assert_eq!(row.edge_count(), 0, "{engine}: {label} still says zero");
        }
        assert_eq!(
            hyper(&store)
                .children(SCOPE, parent)
                .expect("children"),
            vec![child],
            "{engine}: the derived child index rebuilt from the rows alone"
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
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()));
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("a")));
        let first = g.incorporate(fragment).expect("a");
        let a = first.edge_sets[0];
        let shared: Vec<EdgeKey> = first.edges[1..].to_vec();
        drop(first);

        // B reuses those two edges: a member row is (kind, cluster, edge), so
        // naming an existing edge in a second cluster is a legitimate write. The
        // cluster's id is explicit because the members move into it in the SAME
        // batch that creates it — an id allocated by that batch could not be
        // named before the call.
        let b = EdgeSet::new(kid(&store, COMMUNITY), 50);
        let fragment = Fragment::<Employment>::new([edge(person(4), org(4))]);
        let fourth = g.incorporate(fragment).expect("fourth").edges[0].clone();
        // `0` is the head's "no parent" value, so it is nobody's id.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()).at(0));
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
            .cluster(Cluster::edges(COMMUNITY, attrs("b")).at(b.id))
            .remove_edges(a);
        g.incorporate(fragment).expect("move");
        // A lost two members and kept one — and it KEEPS ITS ROW: a scope is not
        // an alias for the edges inside it. The count is the honest one, and the
        // payload (the scope's own statement) is still there.
        let kept = hyper(&store)
            .lookup(a)
            .expect("lookup")
            .expect("a cluster outlives its members");
        assert_eq!(
            kept.edge_count(),
            1,
            "{engine}: the count is honest, not a minimum"
        );
        assert_eq!(
            kept.payload.get("label"),
            Some(&Variant::Str("a".to_string())),
            "{engine}: the scope's payload stayed with it"
        );
        assert_eq!(
            hyper(&store).edge_members(a).expect("members").len(),
            1,
            "{engine}: exactly the member it kept"
        );
        assert_eq!(
            hyper(&store).edge_members(b).expect("members").len(),
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
                .edge_count(),
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
            report.edge_sets.is_empty() && report.node_sets.is_empty(),
            "{engine}: no cluster was declared, so no id was allocated"
        );
        assert!(
            face(&store, 0x0C).is_empty() && face(&store, 0x0D).is_empty(),
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
            .cluster(Cluster::edges(COMMUNITY, Dict::new()))
            .on_node_conflict::<Person>(|old, _declared| Person { seen: old.seen + 1 })
            .on_edge_conflict(|_stored, _declared| Employment { weight: 1 })
            .on_edge_set_conflict(|_stored, declared| {
                let mut row = declared.clone();
                row.insert("weight".to_string(), Variant::U64(2));
                row
            });
        let report = g.incorporate(fragment).expect("commit");
        let cluster = report.edge_sets[0];

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
        assert_eq!(row.edge_count(), 2, "{engine}: counts stamped last");

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
            .cluster(Cluster::edges(COMMUNITY, stamped))
            .on_edge_set_conflict(newest);
        let report = g.incorporate(fragment).expect("first write");
        let cluster = report.edge_sets[0];
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
            .cluster(Cluster::edges(COMMUNITY, older).at(cluster.id))
            .on_edge_set_conflict(newest);
        g.incorporate(fragment).expect("second write");

        let row = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!(
            row.payload.get("ts"),
            Some(&Variant::U64(10)),
            "{engine}: the stored stamp survived a declaration with an older one"
        );
        assert_eq!(
            row.edge_count(),
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("left")));
        let left = g.incorporate(fragment).expect("left");
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(4), org(4))]);
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("right")));
        let right = g.incorporate(fragment).expect("right");
        let a = left.edge_sets[0];
        let b = right.edge_sets[0];

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
            .cluster(Cluster::edges(COMMUNITY, attrs("split")))
            .remove_edges(a);
        let report = g.incorporate(fragment).expect("split");
        let c = report.edge_sets[0];
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("parent")));
        let parent = g.incorporate(fragment).expect("parent").edge_sets[0].id;
        let mut fragment =
            Fragment::<Employment>::new([edge(person(3), org(3)), edge(person(4), org(4))]);
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("child")).under(parent));
        let child = g.incorporate(fragment).expect("child").edge_sets[0].id;

        let listed = hyper(&store).edge_sets(COMMUNITY).expect("sets").iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(
            listed,
            vec![parent, child],
            "{engine}: one scan of 0x0A, ids only — no member row is read as a cluster"
        );
        // The member and child rows live in other faces, so the same scan is
        // blind to them by construction.
        assert_eq!(face(&store, 0x0D).len(), 4, "{engine}: member rows exist");
        assert_eq!(face(&store, 0x0E).len(), 1, "{engine}: child rows exist");
        assert_eq!(
            hyper(&store)
                .children(COMMUNITY, parent)
                .expect("children"),
            vec![child],
            "{engine}: one scan of 0x0C"
        );
        assert_eq!(
            hyper(&store)
                .parent(EdgeSet::new(kid(&store, COMMUNITY), child))
                .expect("parent"),
            parent,
            "{engine}: the head is the parent, one point read"
        );

        // Moving the child under the other root rewrites the derived index.
        let mut fragment =
            Fragment::<Employment>::new([edge(person(5), org(5)), edge(person(6), org(6))]);
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("other")));
        let other = g.incorporate(fragment).expect("other").edge_sets[0].id;
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()).at(child).under(other));
        g.incorporate(fragment).expect("reparent");
        assert!(
            hyper(&store)
                .children(COMMUNITY, parent)
                .expect("children")
                .is_empty(),
            "{engine}: the old child row is gone"
        );
        assert_eq!(
            hyper(&store)
                .children(COMMUNITY, other)
                .expect("children"),
            vec![child],
            "{engine}: the new child row is there"
        );
        // The merged declaration kept the payload: an empty Dict merges in
        // nothing (a name both carry would be overwritten).
        assert_eq!(
            hyper(&store)
                .lookup(EdgeSet::new(kid(&store, COMMUNITY), child))
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
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()));
        let report = g.incorporate(fragment).expect("write");
        let cluster = report.edge_sets[0];
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
                .edge_count(),
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
        fragment.cluster(Cluster::edges(COMMUNITY, Dict::new()));
        let report = g.incorporate(fragment).expect("write");
        let cluster = report.edge_sets[0];
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

        // Both endpoints are still endpoints of this graph, and their handles
        // are what the delete left alone: re-asserting the same tuple lands on
        // the identity it already had, with no node row and no handle re-issued.
        // That is the rule — "a node exists only as an endpoint" is about
        // creation, and a delete may leave an endpoint connected to nothing.
        let again = g
            .incorporate(Fragment::new([edge(person(1), org(1))]))
            .expect(
                "deleting an edge leaves both endpoints' node rows and their other edges intact",
            );
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
        let members_before = face(&store, 0x0D);

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
            face(&store, 0x0D),
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("kept")));
        let report = g.incorporate(fragment).expect("write");
        let cluster = report.edge_sets[0];

        let primary_before = face(&store, 0x00);
        let clusters_before = face(&store, 0x0C);
        let members_before = face(&store, 0x0D);

        // An edge no row holds, a node no edge names, a cluster that was never
        // created: three absences, three zeroes — and not a row written or
        // deleted. Declines are the caller's to ignore, and tolerance is the
        // reading that needs no bookkeeping on their side.
        let mut fragment = Fragment::<Employment>::new([]);
        fragment
            .delete_edge(head(person(4), org(4)))
            .delete_node(org(99))
            .delete_edges(EdgeSet::new(kid(&store, COMMUNITY), 999));
        let retracted = g.retract(fragment).expect("retract");

        assert_eq!(
            (retracted.nodes, retracted.edges, retracted.memberships),
            (0, 0, 0),
            "{engine}: absent is nothing to do"
        );
        assert_eq!(face(&store, 0x00), primary_before, "{engine}: edges intact");
        assert_eq!(
            face(&store, 0x0C),
            clusters_before,
            "{engine}: clusters intact"
        );
        assert_eq!(
            face(&store, 0x0D),
            members_before,
            "{engine}: memberships intact"
        );
        assert_eq!(
            hyper(&store).edge_members(cluster).expect("members").len(),
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
            .cluster(Cluster::edges(COMMUNITY, Dict::new()))
            .on_edge_conflict(|_stored, _declared| Employment { weight: 5 });
        let report = g.incorporate(fragment).expect("write");
        let target = report.edges[0].clone();
        let other = report.edges[1].clone();
        let members_before = face(&store, 0x0D);
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
            updated.edge_sets.is_empty() && updated.node_sets.is_empty(),
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
            face(&store, 0x0D),
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
/// by the member ROW — the cursor that makes [`intersect_seek`] legal against
/// `Graph::clusters_of`. Both faces are keyed by the edge's handle row, and
/// 0x0B carries that same row after the cluster prefix, which is exactly the
/// order this face stores; the test spells the keys with the public helper
/// rather than reaching into the module.
///
/// The order is the ROW's, not the tuple's: handles are issued on first sight
/// (ADR-0067), so a `Vec<EdgeKey>` sorted by reference is not the face's order
/// and must be converted before it is sorted.
struct MemberClusters {
    keys: Vec<Vec<u8>>,
    at: usize,
}

impl MemberClusters {
    fn of(g: &Graph<TestStore, Employment>, cluster: ClusterRef, edges: Vec<EdgeKey>) -> Self {
        let mut rows: Vec<HandleKey> = edges.iter().map(|edge| row_of(g, edge)).collect();
        rows.sort();
        Self {
            keys: rows
                .iter()
                .map(|row| edge_cluster_key(&NS.to_be_bytes(), row, cluster))
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

/// 0x0F is written with the member rows, answers membership as ONE point read,
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
        fragment.cluster(Cluster::edges(COMMUNITY, attrs("acme")));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let id = report.edge_sets[0].id;
        let cluster = EdgeSet::new(kid(&store, COMMUNITY), id);
        let keys = report.edges.clone();

        // The layout the ADR states, byte for byte:
        // `[ns][0x0F][src_h][pred][dst_h][kind][hyper]`.
        let mut spelled = Vec::new();
        spelled.extend_from_slice(&NS.to_be_bytes());
        spelled.push(0x0F);
        row_of(&g, &keys[0]).write(&mut spelled);
        spelled.extend_from_slice(&kid(&store, COMMUNITY).0.to_be_bytes());
        spelled.extend_from_slice(&id.to_be_bytes());
        assert_eq!(
            store.get(&spelled).expect("read"),
            Some(Vec::new()),
            "{engine}: the edge → cluster row is one empty-valued entry"
        );
        assert_eq!(
            face(&store, 0x0F).len(),
            3,
            "{engine}: one derived row per member row"
        );

        // Membership is one point read, and the KIND is part of the key.
        assert!(
            g.edge_member_of(COMMUNITY, id, &keys[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: a member answers true"
        );
        assert!(
            !g.edge_member_of(COMMUNITY, id, &EdgeKey::new(person(4), 999, org(4)))
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: an edge that is not a member answers false"
        );
        assert!(
            !g.edge_member_of(EVENT, id, &keys[1])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the same id under another kind is a different cluster"
        );

        // The stream answers the same fact, ordered by (kind, hyper_id).
        assert_eq!(
            g.edge_sets_of(&keys[1]).collect::<Vec<_>>(),
            vec![okm_core::ClusterRef::from(cluster)],
            "{engine}: one edge's clusters, from the derived face"
        );

        // Retract one edge: its member row AND its mirror leave in one batch,
        // and the cluster (now two members) survives.
        let mut retract = Fragment::<Employment>::new(Vec::<Decl<Employment>>::new());
        retract.delete_edge(head_of(&mut g, &keys[0]));
        g.retract(retract)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));

        assert!(
            !g.edge_member_of(COMMUNITY, id, &keys[0])
                .unwrap_or_else(|e| panic!("{engine}: member_of: {e}")),
            "{engine}: the mirror left with the member row"
        );
        assert!(
            g.edge_sets_of(&keys[0]).next().is_none(),
            "{engine}: a retracted edge belongs to no cluster"
        );
        assert_eq!(
            members(&store, engine, cluster),
            vec![keys[1].clone(), keys[2].clone()],
            "{engine}: the authority still holds the surviving members"
        );
        assert_eq!(
            face(&store, 0x0F).len(),
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
            .cluster(Cluster::edges(COMMUNITY, attrs("acme")))
            .cluster(Cluster::edges(EVENT, attrs("event")));
        g.incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        let written = face(&store, 0x0F);
        assert_eq!(written.len(), 6, "{engine}: three edges, two clusters");

        // Wipe the derived face by hand — the state an interrupted rebuild, a
        // restore without it, or a build that never wrote it would leave.
        for (key, _) in &written {
            store.del(key).expect("delete");
        }
        assert!(
            face(&store, 0x0F).is_empty(),
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
            face(&store, 0x0F),
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
        grouped.cluster(Cluster::edges(COMMUNITY, attrs("org2")));
        let report = g
            .incorporate(grouped)
            .unwrap_or_else(|e| panic!("{engine}: grouped: {e}"));
        let id = report.edge_sets[0].id;
        let cluster = EdgeSet::new(kid(&store, COMMUNITY), id);

        let out_of_p1 = g
            .out_edges(&person(1))
            .unwrap_or_else(|e| panic!("{engine}: out: {e}"));
        assert_eq!(
            out_of_p1,
            vec![plain.edges[0].clone(), report.edges[0].clone()]
        );

        // The intersection of the two derived streams: the node's incident edges
        // in the face's key space, against the cluster's members in the same one.
        let members_of_c = MemberClusters::of(&g, cluster.into(), report.edges.clone());
        let shared: Vec<Vec<u8>> = intersect_seek(g.node_sets_of(&person(1)), members_of_c).collect();
        assert_eq!(shared.len(), 1, "{engine}: one of p1's edges is in C");
        assert_eq!(
            shared,
            vec![edge_cluster_key(
                &NS.to_be_bytes(),
                &row_of(&g, &report.edges[0]),
                cluster.into()
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
                g.edge_member_of(
                    COMMUNITY,
                    id,
                    &g.edge_key(step).expect("a step resolves"),
                )
                .unwrap_or(false)
            })
            .map(|step| g.edge_key(&step).expect("a step resolves"))
            .collect();
        assert_eq!(
            filtered,
            vec![report.edges[0].clone()],
            "{engine}: `member_of` filters a hop without a set built first"
        );

        // And the node form of the cluster stream agrees with both.
        let via_node: Vec<u64> = g.node_sets_of(&person(1)).map(|cluster| cluster.id).collect();
        assert_eq!(via_node, vec![id], "{engine}: p1 is in exactly C");
        assert!(
            g.edge_sets_of(&plain.edges[1]).next().is_none(),
            "{engine}: an ungrouped edge belongs to no cluster"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. A flow is a kind of its own, and one edge may sit in several clusters
// ---------------------------------------------------------------------------

/// The shape a scheduler needs, and the reason `FLOW` is not `SUBGRAPH`: the SAME
/// edge can be a member of a discovered community and of a declared flow at once,
/// and which is which is answered by the kind byte in the key — not by a second
/// table, and not by a node recording "my cluster".
#[test]
fn one_edge_belongs_to_a_community_and_a_flow_at_once() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);

        let mut first = Fragment::<Employment>::new([edge(person(1), org(9))]);
        first
            .cluster(Cluster::edges(COMMUNITY, attrs("bin")))
            .on_edge_conflict(|_stored, _declared| Employment { weight: 1 });
        let bin = g
            .incorporate(first)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let community = bin.edge_sets[0];
        let shared = bin.edges[0].clone();

        let mut second = Fragment::<Employment>::new([edge(person(1), org(9))]);
        second.cluster(Cluster::edges(FLOW, attrs("login")));
        let flow_report = g
            .incorporate(second)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let flow = flow_report.edge_sets[0];

        assert_ne!(
            kid(&store, FLOW).0,
            kid(&store, SUBGRAPH).0,
            "{engine}: a flow is not a subgraph"
        );
        assert_eq!(
            flow_report.edge_sets[0].id,
            bin.edge_sets[0].id,
            "{engine}: the id spaces are independent, so equal ids are legal"
        );

        // Both memberships stand, on the same edge, with the same row identity.
        assert_eq!(
            members(&store, engine, community),
            vec![shared.clone()],
            "{engine}: the community still holds it"
        );
        assert_eq!(
            members(&store, engine, flow),
            vec![shared.clone()],
            "{engine}: the flow holds it too"
        );

        // The edge side answers both, ordered by (kind, id) — COMMUNITY (1)
        // before FLOW (5) — which is how a reader filters candidates by kind
        // instead of keeping an index of its own.
        let kinds: Vec<(u16, u64)> = g
            .edge_sets_of(&shared)
            .map(|cluster| (cluster.kind.id(), cluster.id))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (kid(&store, COMMUNITY).0, community.id),
                (kid(&store, FLOW).0, flow.id)
            ],
            "{engine}: one scan of 0x0F answers \"which clusters is this edge in\""
        );

        assert!(
            g.edge_member_of(FLOW, flow.id, &shared).expect("read"),
            "{engine}: the point form agrees"
        );
        assert_eq!(
            hyper(&store)
                .lookup(flow)
                .expect("read")
                .map(|row| row.edge_count()),
            Some(1),
            "{engine}: the count lives on the cluster row, so no member tally is stored"
        );
    }
}

// ---------------------------------------------------------------------------
// 10. The kind vocabulary: the caller names its groupings, the store counts them
// ---------------------------------------------------------------------------

/// The dictionary okm OWNS and the words it does not: a label is interned once
/// per namespace, first-seen-claims-next, and never renumbered. Two applications
/// may mean different things by the same id, and the store cannot tell — which is
/// the point.
#[test]
fn a_kind_is_interned_once_and_read_back_by_number() {
    for (engine, store) in TestStore::matrix() {
        let mut hyper = hyper(&store);

        let community = hyper.kind_id("community").expect("intern");
        let flow = hyper.kind_id("flow").expect("intern");
        assert_ne!(community, flow, "{engine}: two labels, two ids");
        assert_eq!(
            hyper.kind_id("community").expect("intern again"),
            community,
            "{engine}: a label seen twice keeps its number"
        );
        assert_eq!(
            hyper.kind_label(community).expect("read"),
            Some("community".to_string())
        );
        assert_eq!(
            hyper.kind_label(Kind(999)).expect("read"),
            None,
            "{engine}: a number nobody minted has no label"
        );
        assert_eq!(
            hyper.kinds().expect("list"),
            vec![(community.id(), "community".to_string()), (flow.id(), "flow".to_string())],
            "{engine}: listing is one scan, id-ordered"
        );

        // The interned id is what the rows carry, and the vocabulary is per-ns:
        // another namespace starts its own numbering at zero.
        let mut fragment = Fragment::<Employment>::new([edge(person(1), org(1))]);
        fragment.cluster(Cluster::edges("flow", attrs("login")));
        let mut g = graph(&store);
        let report = g.incorporate(fragment).expect("commit");
        let cluster = report.edge_sets[0];
        assert_eq!(
            members(&store, engine, cluster).len(),
            1,
            "{engine}: the interned kind addresses a real cluster"
        );
        assert_eq!(
            face(&store, 0x0B).len(),
            2,
            "{engine}: the label face (0x0B) holds exactly the two words; the cursor sits on 0x0A"
        );
    }
}

// ---------------------------------------------------------------------------
// A cluster of entities (ADR-0068)
// ---------------------------------------------------------------------------

#[test]
fn a_node_cluster_holds_an_entity_that_has_no_edge() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        // An entity no edge names: the ONLY thing that puts it in the graph is
        // the cluster that declares it (ADR-0068 §4).
        let lonely = person(99);
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::nodes(SCOPE, Dict::new(), [lonely.clone()]));
        let report = g
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let cluster = report.node_sets[0];

        let hy = hyper(&store);
        assert_eq!(
            hy.node_members(cluster).expect("nodes"),
            vec![lonely.clone()],
            "{engine}: the declared entity is a member, resolved at the boundary"
        );
        assert!(
            hy.node_member_of(cluster, &lonely).expect("point read"),
            "{engine}: the member face's exact key answers"
        );
        let row = hy.lookup(cluster).expect("lookup").expect("row");
        assert_eq!(row.node_count(), 1, "{engine}: the entity count");
        assert_eq!(row.edge_count(), 0, "{engine}: no edge members at all");

        // One row per face: the authority carries the eight-byte handle, the
        // mirror is derived from it, and nothing was written as an edge.
        assert_eq!(face(&store, 0x12).len(), 1, "{engine}: one member row");
        assert_eq!(face(&store, 0x13).len(), 1, "{engine}: one mirror row");
        assert_eq!(face(&store, 0x0D).len(), 0, "{engine}: no edge members");
    }
}

#[test]
fn retracting_a_node_takes_its_own_memberships() {
    for (engine, store) in TestStore::matrix() {
        let member = person(7);
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::nodes(SCOPE, Dict::new(), [member.clone()]));
        let report = graph(&store).incorporate(fragment).expect("commit");
        let cluster = report.node_sets[0];

        let mut retract = Fragment::<Employment>::new([]);
        retract.delete_node(member.clone());
        let done = graph(&store)
            .retract(retract)
            .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));
        assert_eq!(
            done.nodes, 1,
            "{engine}: a node whose only trace is a membership still counts"
        );

        let hy = hyper(&store);
        assert!(
            !hy.node_member_of(cluster, &member).expect("point read"),
            "{engine}: the membership went with the entity"
        );
        assert_eq!(
            hy.lookup(cluster).expect("lookup").expect("row").node_count(),
            0,
            "{engine}: the count is the honest zero"
        );
        assert_eq!(face(&store, 0x12).len(), 0, "{engine}: member rows gone");
        assert_eq!(face(&store, 0x13).len(), 0, "{engine}: mirror rows gone");
    }
}

#[test]
fn retracting_an_edge_leaves_every_node_cluster_alone() {
    for (engine, store) in TestStore::matrix() {
        let src = person(1);
        let dst = org(1);
        // The fragment writes the edge and declares an ENTITY group over its source.
        // The two are different groups: this one holds the entity and no assertion,
        // which is the whole reason the two spellings are separate.
        let mut fragment = Fragment::<Employment>::new([edge(src.clone(), dst.clone())]);
        fragment.cluster(Cluster::nodes(COMMUNITY, Dict::new(), [src.clone()]));
        let mut g = graph(&store);
        let report = g.incorporate(fragment).expect("commit");
        let cluster = report.node_sets[0];
        let row = hyper(&store).lookup(cluster).expect("lookup").expect("row");
        assert_eq!((row.edge_count(), row.node_count()), (0, 1), "{engine}");

        let mut retract = Fragment::<Employment>::new([]);
        retract.delete_edge(head_of(&mut g, &report.edges[0]));
        g.retract(retract).expect("retract");

        let hy = hyper(&store);
        assert!(
            hy.node_member_of(cluster, &src).expect("point read"),
            "{engine}: deleting an assertion does not remove an entity from a group"
        );
        let row = hy.lookup(cluster).expect("lookup").expect("row");
        assert_eq!(row.edge_count(), 0, "{engine}: the edge member row left");
        assert_eq!(row.node_count(), 1, "{engine}: the entity stayed");
    }
}

#[test]
fn the_node_index_rebuilds_from_the_node_member_rows_alone() {
    for (engine, store) in TestStore::matrix() {
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(
            Cluster::nodes(SCOPE, Dict::new(), [person(1), person(2), org(1)]),
        );
        graph(&store).incorporate(fragment).expect("commit");
        let written = face(&store, 0x13);
        assert_eq!(written.len(), 3, "{engine}: one mirror row per entity");

        // Wipe the mirror and rebuild: the answer is the bytes the write path
        // wrote, derived from the member face alone.
        for (key, _) in &written {
            store.del(key).expect("wipe");
        }
        assert_eq!(face(&store, 0x13).len(), 0, "{engine}: the mirror is gone");
        let report = hyper(&store).rebuild_derived(64).expect("rebuild");
        assert_eq!(report.node_members, 3, "{engine}: three member rows walked");
        assert_eq!(
            face(&store, 0x13),
            written,
            "{engine}: rebuild equals the write path, byte for byte"
        );
    }
}

#[test]
fn removing_a_declared_member_leaves_the_entity_alone() {
    for (engine, store) in TestStore::matrix() {
        let a = person(1);
        let b = person(2);
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::nodes(SCOPE, Dict::new(), [a.clone(), b.clone()]));
        let report = graph(&store).incorporate(fragment).expect("commit");
        let cluster = report.node_sets[0];

        // A retracted DECLARATION, not a deleted entity: the node keeps every row
        // it ever had, and only the claim about the group goes (ADR-0068 §4).
        let mut removal = Fragment::<Employment>::new([]);
        removal.remove_nodes(cluster, [a.clone()]);
        graph(&store).incorporate(removal).expect("removal");

        let hy = hyper(&store);
        assert_eq!(hy.node_members(cluster).expect("nodes"), vec![b.clone()], "{engine}");
        assert!(
            !hy.node_member_of(cluster, &a).expect("point read"),
            "{engine}: the declared row went"
        );
        assert_eq!(
            hy.lookup(cluster).expect("lookup").expect("row").node_count(),
            1,
            "{engine}: the count follows the face"
        );
        assert_eq!(face(&store, 0x12).len(), 1, "{engine}: one member row left");
        assert_eq!(face(&store, 0x13).len(), 1, "{engine}: one mirror row left");
    }
}

#[test]
fn a_removed_declaration_leaves_the_assertion_alone() {
    for (engine, store) in TestStore::matrix() {
        let src = person(1);
        let dst = org(1);
        let mut fragment = Fragment::<Employment>::new([edge(src.clone(), dst.clone())]);
        fragment.cluster(Cluster::nodes(SCOPE, Dict::new(), [src.clone()]));
        let mut g = graph(&store);
        let report = g.incorporate(fragment).expect("commit");
        let cluster = report.node_sets[0];

        let mut removal = Fragment::<Employment>::new([]);
        removal.remove_nodes(cluster, [src.clone()]);
        g.incorporate(removal).expect("removal");

        let hy = hyper(&store);
        assert!(
            !hy.node_member_of(cluster, &src).expect("point read"),
            "{engine}: the declaration is what the verb speaks for"
        );
        assert!(hy.node_members(cluster).expect("nodes").is_empty(), "{engine}");
        // No EDGE group ever held it either: the assertion belongs to the graph,
        // and group membership is something a caller declares per set.
        let via: Vec<u64> = g.node_sets_of(&src).map(|c| c.id).collect();
        assert!(via.is_empty(), "{engine}: no edge set names it");
        assert_eq!(
            hy.lookup(cluster).expect("lookup").expect("row").edge_count(),
            0,
            "{engine}: an entity group holds no assertions"
        );
    }
}

#[test]
fn a_declaration_wins_over_a_removal_in_one_fragment() {
    for (engine, store) in TestStore::matrix() {
        let a = person(1);
        let mut fragment = Fragment::<Employment>::new([]);
        fragment.cluster(Cluster::nodes(SCOPE, Dict::new(), [a.clone()]));
        fragment.remove_nodes(NodeSet::new(kid(&store, SCOPE), 1), [a.clone()]);
        // The id is not known until the write allocates it, so this fragment names
        // the first cluster of its kind: the same rule as `.at(..)` on a
        // freshly allocated row.
        graph(&store)
            .incorporate(fragment)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

        let hy = hyper(&store);
        assert!(
            hy.node_member_of(NodeSet::new(kid(&store, SCOPE), 1), &a).expect("point read"),
            "{engine}: the declaration is the later claim and it wins"
        );
    }
}

// ---------------------------------------------------------------------------
// The two spellings at the operation layer (ADR-0068 §7)
// ---------------------------------------------------------------------------

#[test]
fn incorporating_entity_sets_carries_the_declared_members() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let (a, b, c) = (person(1), person(2), person(3));
        let mut first = Fragment::<Employment>::new([]);
        first.cluster(Cluster::nodes(SCOPE, Dict::new(), [a.clone(), b.clone()]));
        let report = g
            .incorporate(first)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let source = report.node_sets[0];

        let mut second = Fragment::<Employment>::new([]);
        second.cluster(Cluster::nodes(SCOPE, Dict::new(), [c.clone()]));
        let report = g
            .incorporate(second)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let into = report.node_sets[0];

        // The union is declared members -> declared members: the op reads the
        // source's `0x12` run, so the entities carry over without an edge.
        let mut merge = Fragment::<Employment>::new([]);
        merge.incorporate_nodes([source], into);
        g.incorporate(merge)
            .unwrap_or_else(|e| panic!("{engine}: merge: {e}"));

        let hy = hyper(&store);
        assert!(
            hy.lookup(source).expect("lookup").is_none(),
            "{engine}: the source row goes"
        );
        assert_eq!(
            hy.node_members(into).expect("nodes"),
            vec![a.clone(), b.clone(), c.clone()],
            "{engine}: every declared entity lands in the target"
        );
        assert!(
            hy.node_member_of(into, &a).expect("point read"),
            "{engine}: and the mirror answers for them"
        );
        assert_eq!(
            hy.lookup(into)
                .expect("lookup")
                .expect("row")
                .node_count(),
            3,
            "{engine}: the count is re-stamped from the face"
        );
    }
}

#[test]
fn each_spelling_answers_its_own_conflict_hook() {
    for (engine, store) in TestStore::matrix() {
        let mut g = graph(&store);
        let mut first = Fragment::<Employment>::new([]);
        let mut payload = Dict::new();
        payload.insert("weight".to_string(), Variant::U64(1));
        first.cluster(Cluster::edges(COMMUNITY, payload));
        let report = g
            .incorporate(first)
            .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
        let set = report.edge_sets[0];

        // A second write over the same row: the ENTITY hook would overwrite the
        // payload with a marker, so a payload that keeps its weight proves the
        // edge-set hook — and only that one — ran.
        let mut again = Fragment::<Employment>::new([]);
        let mut marker = Dict::new();
        marker.insert("wrong_hook".to_string(), Variant::U64(1));
        again
            .cluster(Cluster::edges(COMMUNITY, Dict::new()).at(set.id))
            .on_edge_set_conflict(|_stored, declared| {
                let mut row = declared.clone();
                row.insert("edge_hook".to_string(), Variant::U64(1));
                row
            })
            .on_node_set_conflict(move |_stored, _declared| marker.clone());
        g.incorporate(again)
            .unwrap_or_else(|e| panic!("{engine}: second commit: {e}"));

        let row = hyper(&store)
            .lookup(set)
            .expect("lookup")
            .expect("the row is still there");
        assert_eq!(
            row.payload.get("weight"),
            Some(&Variant::U64(1)),
            "{engine}: the edge-set hook kept the stored payload"
        );
        assert_eq!(
            row.payload.get("edge_hook"),
            Some(&Variant::U64(1)),
            "{engine}: the edge-set hook ran"
        );
        assert!(
            row.payload.get("wrong_hook").is_none(),
            "{engine}: the entity-set hook did not run for an edge set"
        );
    }
}
