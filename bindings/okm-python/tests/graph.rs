//! The dynamic graph's binding face (ADR-0017 §4 / ADR-0066) end to end: the
//! two write verbs, the traversal cursor, the cluster reads and the key-set
//! algebra, over the real engine (no mock store, no parallel layout).
//!
//! The layout itself is locked in okm-dynamic's own suite; what is locked HERE
//! is the BINDING wiring — that the ids the verbs report are the ids the reads
//! answer, that a delete cascades (nodes) or does not (edges), that a walk is
//! lazy enough to abandon, and that a `KeySet` keeps "set algebra first, one
//! fetch after" instead of a loop over ids.
//!
//! Gate: `cargo test --no-default-features --features host-tests` — the same
//! posture as `injection.rs`, for the same reason: the test names pyclass
//! types, so the rlib drags the interpreter's symbols in and the
//! wheel-posture run (extension-module, libpython forbidden) cannot link it.
//! The nix host also needs the interpreter on the linker's path:
//! `RUSTFLAGS=-L native=<python3-env>/lib`.
//!
//! Two layers, and both are claims about the BINDING:
//!
//! - the tests below drive the plain-Rust helpers (`incorporate_dyn`,
//!   `next_step`, `member_of_dyn`, …) — the same code the shells call;
//! - `python_face` drives it through Python objects: dict fragments,
//!   `__next__`, `pulled`, and the `ValueError` a mixed fragment raises.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use okm::pyo3_impl::{Collection, Engine, EngineBox, Graph, KeySet};
use okm_core::schema::CollectionSchema;
use okm_core::{Direction, NodeRef};
use okm_dynamic::{Cluster, ClusterRef, Decl, Dict, DynEdge, DynFragment, Kind, Value as Variant};

/// The graph table's ns — a second table in the store, beside the collection's.
const NS: u16 = 0x0190;
/// The document collection's ns: same store, different table.
const DOC_NS: u16 = 7;

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

/// A graph declaration as an export carries it, with an EMPTY member set — so
/// the writer checks nothing rather than guessing, which is what a graph whose
/// endpoints are bare refs wants.
fn graph_schema() -> CollectionSchema {
    serde_json::from_value(serde_json::json!({
        "name": "edges",
        "key_len": 8,
        "key_fields": [{"name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
        "layout_version": 1,
        "hot_width": 8,
        "hot_fields": [{"name": "at", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
        "cold_fields": [],
        "members": [],
        "indexes": [],
        "slots": {"primary": 0, "dynamic": 1, "field_vocab_id": 2, "field_vocab_name": 3,
                  "declared_index_base": 4096, "declared_reduce_base": 8192,
                  "junction_base": 12288}
    }))
    .expect("the graph schema parses")
}

/// A document table's entry — the sibling collection in test 0's store.
fn doc_entry() -> serde_json::Value {
    serde_json::json!({
        "schema": {
            "key_len": 8,
            "key_fields": [{"name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
            "layout_version": 1,
            "hot_width": 8,
            "hot_fields": [{"name": "count", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
            "cold_fields": [],
            "slots": {"primary": 0, "dynamic": 1, "field_vocab_id": 2, "field_vocab_name": 3,
                      "declared_index_base": 4096, "declared_reduce_base": 8192,
                      "junction_base": 12288}
        },
        "indexes": [],
        "reduces": []
    })
}

fn graph() -> Graph {
    Graph::assemble(
        EngineBox::Test(okm_async::TestStore::slatedb_mem()),
        graph_schema(),
        NS,
    )
    .unwrap_or_else(|_| panic!("graph assembly over a fresh store"))
}

/// Every step of one walk, with the face key — the shape a set operator takes.
fn walk(g: &Graph, node: &NodeRef) -> Vec<(Vec<u8>, okm_dynamic::EdgeStep)> {
    drain(g.out_dyn(node))
}

/// The same, over the mirror face: the edges that END at `node`.
fn walk_in(g: &Graph, node: &NodeRef) -> Vec<(Vec<u8>, okm_dynamic::EdgeStep)> {
    drain(g.inn_dyn(node))
}

fn drain(mut stream: okm::pyo3_impl::EdgeStream) -> Vec<(Vec<u8>, okm_dynamic::EdgeStep)> {
    let mut out = Vec::new();
    while let Some(step) = stream.next_step() {
        out.push(step);
    }
    out
}

/// A default fragment with the builder calls already applied — `DynFragment`'s
/// builders take `&mut self`, so the tests read better chained.
trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&mut Self)) -> Self;
}

impl Tap for DynFragment {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}

// ---------------------------------------------------------------------------
// 0. One store, two faces: a graph beside a document collection
// ---------------------------------------------------------------------------

/// A host-owned in-memory engine (the realm store's shape at the byte seam).
struct MemEngine {
    rows: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemEngine {
    fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
        }
    }
    fn len(&self) -> usize {
        self.rows.lock().unwrap().len()
    }
}

impl Engine for MemEngine {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), String> {
        self.rows.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.rows.lock().unwrap().get(key).cloned())
    }
    fn del(&self, key: &[u8]) -> Result<(), String> {
        self.rows.lock().unwrap().remove(key);
        Ok(())
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>, String> {
        let rows = self.rows.lock().unwrap();
        let start = begin.to_vec();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match end {
            Some(e) => Box::new(rows.range(start..e.to_vec())),
            None => Box::new(rows.range(start..)),
        };
        Ok(iter.map(|(k, _)| k.clone()).collect())
    }
}

/// The binding's own claim about sharing: a graph assembled over a COLLECTION's
/// store handle writes into the engine the host handed the collection — not an
/// orphan store that would quietly answer "empty" for the rows next door.
///
/// This is why `EngineBox` is `Clone`: a handle is a keyspace, not a copy.
#[test]
fn the_graph_writes_into_the_store_the_collection_was_given() {
    let engine = Arc::new(MemEngine::new());
    let collection =
        Collection::with_store(engine.clone(), &doc_entry(), DOC_NS).expect("collection");
    let g = Graph::assemble(collection.store_handle(), graph_schema(), NS)
        .unwrap_or_else(|_| panic!("graph over the collection's store"));

    assert_eq!(engine.len(), 0, "a fresh engine holds nothing yet");
    let report = g
        .incorporate_dyn(DynFragment::new([Decl::edge(edge(
            person(1),
            org(1),
            "works_at",
        ))]))
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
    assert_eq!(report.edge_ids.len(), 1);
    assert!(
        engine.len() > 0,
        "the graph wrote into the HOST engine, not an orphan store"
    );

    // The document table still works beside it: both faces, one keyspace.
    let mut doc = Dict::new();
    doc.insert("id".into(), Variant::U64(1));
    doc.insert("count".into(), Variant::U64(42));
    collection
        .put_doc(&1u64.to_be_bytes(), &doc, false)
        .unwrap_or_else(|e| panic!("collection put: {e}"));
    assert_eq!(
        collection
            .fetch_doc(&1u64.to_be_bytes())
            .expect("collection read")
            .and_then(|m| m.get("count").cloned()),
        Some(Variant::U64(42))
    );
    // ... and the graph's row survived the collection's write.
    assert!(
        g.edge_dyn(report.edge_ids[0]).expect("edge read").is_some(),
        "the document write did not disturb the graph's rows"
    );
}

// ---------------------------------------------------------------------------
// 1. A write is visible after `incorporate`
// ---------------------------------------------------------------------------

#[test]
fn a_write_is_visible_after_incorporate() {
    let g = graph();
    let mut fragment = DynFragment::new([
        Decl::edge(DynEdge {
            attrs: attrs(&[("since", Variant::U64(2020))]),
            ..edge(person(1), org(9), "works_at")
        }),
        Decl::edge(edge(person(2), org(9), "works_at")),
    ]);
    fragment.cluster(Cluster::new(
        Kind::COMMUNITY,
        attrs(&[("label", Variant::Str("acme".into()))]),
    ));
    let report = g
        .incorporate_dyn(fragment)
        .unwrap_or_else(|e| panic!("incorporate: {e}"));

    assert_eq!(report.edge_ids.len(), 2, "one id per edge declaration");
    assert_eq!(report.hyper_ids.len(), 1, "one id per cluster declaration");
    let cluster = report.hyper_ids[0];
    assert_eq!(cluster, 1, "ids start at 1 — 0 is \"no parent\"");

    // The edge row, decoded: endpoints, predicate by NAME, attributes by name.
    let first = g
        .edge_dyn(report.edge_ids[0])
        .expect("edge read")
        .expect("the edge is there");
    assert_eq!(first.src, person(1));
    assert_eq!(first.dst, org(9));
    assert_eq!(
        first.predicate, "works_at",
        "the predicate resolves to its name, never a bare id"
    );
    assert_eq!(first.attrs.get("since"), Some(&Variant::U64(2020)));

    // The cluster: its row, its members (in edge-id order) and its count.
    let row = g
        .lookup_dyn(Kind::COMMUNITY, cluster)
        .expect("lookup")
        .expect("the cluster row");
    assert_eq!(row.parent, 0);
    assert_eq!(
        row.payload.get("label"),
        Some(&Variant::Str("acme".into())),
        "the Dict payload round-trips through the field vocabulary"
    );
    assert_eq!(
        g.members_dyn(Kind::COMMUNITY, cluster).expect("members"),
        report.edge_ids,
        "one scan of the authority face"
    );
    assert_eq!(
        g.member_count_dyn(Kind::COMMUNITY, cluster).expect("count"),
        2
    );
    assert_eq!(
        g.clusters_dyn(Kind::COMMUNITY).expect("clusters"),
        vec![cluster]
    );
    assert_eq!(
        g.all_clusters_dyn().expect("all"),
        vec![ClusterRef::new(Kind::COMMUNITY, cluster)]
    );
    // The reverse direction: which clusters does an edge belong to?
    assert_eq!(
        g.clusters_of_dyn(report.edge_ids[1]).expect("clusters_of"),
        vec![ClusterRef::new(Kind::COMMUNITY, cluster)]
    );
    assert!(
        g.clusters_of_dyn(4_242).expect("clusters_of").is_empty(),
        "an edge nobody clustered answers nothing"
    );

    // A SECOND handle over the same store sees it — visible through the store,
    // not through a warm cache in the handle that wrote it.
    let again = Graph::assemble(g.store_handle(), graph_schema(), NS)
        .unwrap_or_else(|_| panic!("second handle"));
    assert_eq!(
        again
            .edge_dyn(report.edge_ids[0])
            .expect("read")
            .map(|e| e.predicate),
        Some("works_at".to_string())
    );

    // The traversal answers the same fact from the entry alone.
    let steps = walk(&g, &person(1));
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].1.edge_id, report.edge_ids[0]);
    assert_eq!(
        steps[0].1.far,
        org(9),
        "an out hop's far end is the destination"
    );
    assert_eq!(steps[0].1.head.src, person(1));
    assert_eq!(
        steps[0].1.head.predicate, None,
        "the untyped face carries no predicate — it is not in the key"
    );

    // The mirror face: same head, mirrored far end.
    let mut stream = g.inn_dyn(&org(9));
    let mut incoming = Vec::new();
    while let Some(step) = stream.next_step() {
        incoming.push(step);
    }
    assert_eq!(incoming.len(), 2, "two edges end at org 9");
    let mirrored = incoming
        .iter()
        .find(|(_, s)| s.edge_id == report.edge_ids[0])
        .expect("the same edge from the far side")
        .1
        .clone();
    assert_eq!(mirrored.far, person(1), "the in hop's far end is the source");
    assert_eq!(mirrored.head, steps[0].1.head, "one edge states one head");
}

// ---------------------------------------------------------------------------
// 2. A node delete takes its edges; an edge delete leaves the endpoints
// ---------------------------------------------------------------------------

#[test]
fn deleting_a_node_takes_its_edges_and_deleting_an_edge_leaves_the_endpoints() {
    let g = graph();
    let report = g
        .incorporate_dyn(DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(2), org(1), "works_at")),
            Decl::edge(edge(person(2), org(3), "works_at")),
        ]))
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
    let (p1_to_o1, p2_to_o1, p2_to_o3) = (
        report.edge_ids[0],
        report.edge_ids[1],
        report.edge_ids[2],
    );
    assert_eq!(walk(&g, &person(2)).len(), 2);

    // ---- a node's delete cascades: both faces, the row, the segment ----
    let removed = g
        .retract_dyn(DynFragment::default().tap(|f| { f.delete_node(person(1)); }))
        .unwrap_or_else(|e| panic!("retract: {e}"));
    assert_eq!(
        (removed.nodes, removed.edges, removed.memberships),
        (1, 1, 0),
        "one node, its one edge, no membership rows to sweep"
    );
    assert!(
        g.edge_dyn(p1_to_o1).expect("read").is_none(),
        "the node took its edge's primary row with it"
    );
    assert!(
        walk(&g, &person(1)).is_empty(),
        "and its traversal entry: a node cannot be left with a dangling edge"
    );
    assert_eq!(
        walk(&g, &person(2)).len(),
        2,
        "the other node's edges are untouched"
    );
    assert_eq!(
        walk_in(&g, &org(1)).len(),
        1,
        "org 1's in-face lost exactly one"
    );

    // ---- an edge's delete leaves both endpoints standing ----
    let removed = g
        .retract_dyn(DynFragment::default().tap(|f| { f.delete_edge(p2_to_o1); }))
        .unwrap_or_else(|e| panic!("retract: {e}"));
    assert_eq!((removed.nodes, removed.edges), (0, 1));
    assert!(g.edge_dyn(p2_to_o1).expect("read").is_none());
    assert!(
        g.edge_dyn(p2_to_o3).expect("read").is_some(),
        "the surviving edge is not collateral"
    );
    assert_eq!(
        walk(&g, &person(2))
            .iter()
            .map(|(_, s)| s.edge_id)
            .collect::<Vec<_>>(),
        vec![p2_to_o3],
        "person 2 is still an endpoint — of one edge now"
    );
    assert!(
        walk_in(&g, &org(1)).is_empty(),
        "org 1 is still an endpoint too — of nothing"
    );
    // An id nobody holds is nothing to do, not a refusal.
    let nothing = g
        .retract_dyn(DynFragment::default().tap(|f| { f.delete_edge(9_999); }))
        .unwrap_or_else(|e| panic!("retract: {e}"));
    assert_eq!((nothing.nodes, nothing.edges, nothing.memberships), (0, 0, 0));
}

// ---------------------------------------------------------------------------
// 3. The traversal stops when the caller breaks
// ---------------------------------------------------------------------------

#[test]
fn a_traversal_reads_only_the_entries_the_caller_asks_for() {
    let g = graph();
    const WORKS: u64 = 40;
    const ADVISES: u64 = 10;
    let mut decls: Vec<Decl> = Vec::new();
    for id in 1..=WORKS {
        decls.push(Decl::edge(edge(person(1), org(id), "works_at")));
    }
    for id in 100..100 + ADVISES {
        decls.push(Decl::edge(edge(person(1), org(id), "advises")));
    }
    let report = g
        .incorporate_dyn(DynFragment::new(decls))
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
    let works: Vec<u64> = report.edge_ids[..WORKS as usize].to_vec();

    // The whole face, one entry per step.
    let mut stream = g.out_dyn(&person(1));
    let mut all = Vec::new();
    while let Some((_, step)) = stream.next_step() {
        all.push(step.edge_id);
    }
    assert_eq!(all.len() as u64, WORKS + ADVISES);
    assert_eq!(
        stream.pulled_count(),
        (WORKS + ADVISES) as usize,
        "a full walk reads the face exactly once"
    );

    // ---- the caller breaks after three: the scan stops where it stands ----
    let mut stream = g.out_dyn(&person(1));
    for _ in 0..3 {
        assert!(stream.next_step().is_some());
    }
    let pulled = stream.pulled_count();
    assert_eq!(stream.step_count(), 3);
    assert!(
        pulled <= 4,
        "breaking after three must not read the {} entry face (read {pulled})",
        WORKS + ADVISES
    );
    drop(stream);

    // ---- predicate pushdown: same set, fewer entries ----
    let mut untyped = g.out_dyn(&person(1));
    while untyped.next_step().is_some() {}
    let untyped_pulled = untyped.pulled_count();

    let mut pushed = okm::pyo3_impl::EdgeStream::filtered(
        g.store_handle(),
        NS,
        Direction::Out,
        person(1),
        "works_at",
    )
    .expect("probe");
    assert!(pushed.filter_id().is_some(), "the pushdown names a face");
    let mut ids = Vec::new();
    while let Some((_, step)) = pushed.next_step() {
        ids.push(step.edge_id);
    }
    ids.sort_unstable();
    let mut want = works;
    want.sort_unstable();
    assert_eq!(
        ids, want,
        "the pushed-down walk answers exactly its predicate"
    );
    assert!(
        pushed.pulled_count() < untyped_pulled,
        "pushdown reads fewer entries ({} vs {untyped_pulled})",
        pushed.pulled_count()
    );

    // A predicate nobody interned answers empty, without opening a scan — not
    // "scan the whole face and discard".
    let mut unknown = okm::pyo3_impl::EdgeStream::filtered(
        g.store_handle(),
        NS,
        Direction::Out,
        person(1),
        "never_written",
    )
    .expect("a probe, not a fault");
    assert!(unknown.next_step().is_none());
    assert_eq!(
        unknown.pulled_count(),
        0,
        "and reads no entry only to throw it away"
    );
    assert_eq!(unknown.direction_of(), Direction::Out);
    assert!(unknown.error_text().is_none(), "an empty face is not a fault");
}

// ---------------------------------------------------------------------------
// 4. `member_of` before and after a retract
// ---------------------------------------------------------------------------

#[test]
fn member_of_answers_before_and_after_a_retract() {
    let g = graph();
    let mut fragment = DynFragment::new([
        Decl::edge(edge(person(1), org(1), "works_at")),
        Decl::edge(edge(person(2), org(2), "works_at")),
        Decl::edge(edge(person(3), org(3), "works_at")),
    ]);
    fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
    let report = g
        .incorporate_dyn(fragment)
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
    let cluster = report.hyper_ids[0];
    let ids = report.edge_ids.clone();

    for id in &ids {
        assert!(
            g.member_of_dyn(Kind::COMMUNITY, cluster, *id)
                .expect("member_of"),
            "every edge of the batch is a member"
        );
    }
    assert!(
        !g.member_of_dyn(Kind::COMMUNITY, cluster, 4_242)
            .expect("member_of"),
        "an id no cluster holds"
    );
    assert!(
        !g.member_of_dyn(Kind::EVENT, cluster, ids[0]).expect("member_of"),
        "membership is per KIND — the same id on another kind is another cluster"
    );

    // One member leaves: the cluster survives on the other two, and the derived
    // faces follow the retract rather than the row they used to hold.
    let removed = g
        .retract_dyn(DynFragment::default().tap(|f| { f.delete_edge(ids[1]); }))
        .unwrap_or_else(|e| panic!("retract: {e}"));
    assert_eq!(removed.edges, 1);
    assert_eq!(removed.memberships, 1, "one membership row swept");
    assert!(
        !g.member_of_dyn(Kind::COMMUNITY, cluster, ids[1]).expect("member_of"),
        "the retracted edge is no longer a member"
    );
    assert!(
        g.member_of_dyn(Kind::COMMUNITY, cluster, ids[0]).expect("member_of"),
        "the others still are"
    );
    assert_eq!(
        g.members_dyn(Kind::COMMUNITY, cluster).expect("members"),
        vec![ids[0], ids[2]]
    );
    assert_eq!(
        g.member_count_dyn(Kind::COMMUNITY, cluster).expect("count"),
        2,
        "the row's count was re-stamped"
    );
    assert!(
        g.clusters_of_dyn(ids[1]).expect("clusters_of").is_empty(),
        "the edge-first mirror lost the row too"
    );

    // A second retract drops it below two members, so the cluster stops
    // existing — its row and the last membership rows go with it.
    let removed = g
        .retract_dyn(DynFragment::default().tap(|f| { f.delete_edge(ids[2]); }))
        .unwrap_or_else(|e| panic!("retract: {e}"));
    assert_eq!(
        removed.memberships, 2,
        "the cluster row and the remaining member row"
    );
    assert!(
        !g.member_of_dyn(Kind::COMMUNITY, cluster, ids[0]).expect("member_of"),
        "a one-member cluster is not a cluster (ADR-0066 §3)"
    );
    assert!(g
        .lookup_dyn(Kind::COMMUNITY, cluster)
        .expect("lookup")
        .is_none());
    assert!(g
        .members_dyn(Kind::COMMUNITY, cluster)
        .expect("members")
        .is_empty());
    assert_eq!(
        g.member_count_dyn(Kind::COMMUNITY, cluster).expect("count"),
        0
    );
}

// ---------------------------------------------------------------------------
// 5. `KeySet`: set algebra first, one fetch after
// ---------------------------------------------------------------------------

#[test]
fn a_key_set_intersects_then_fetches_in_one_call() {
    let g = graph();
    let report = g
        .incorporate_dyn(DynFragment::new([
            Decl::edge(edge(person(1), org(1), "works_at")),
            Decl::edge(edge(person(1), org(2), "works_at")),
            Decl::edge(edge(person(1), org(3), "works_at")),
            Decl::edge(edge(person(1), org(4), "works_at")),
            Decl::edge(edge(person(1), org(5), "works_at")),
        ]))
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
    // A cluster over TWO of them — "which of this node's edges are in C" — by
    // naming those two ids: a declaration adopts exactly the batch's edges.
    let mut fragment = DynFragment::new([
        Decl::edge_at(edge(person(1), org(1), "works_at"), report.edge_ids[0]),
        Decl::edge_at(edge(person(1), org(2), "works_at"), report.edge_ids[1]),
    ]);
    fragment.cluster(Cluster::new(Kind::COMMUNITY, Dict::new()));
    let grouped_ids = g
        .incorporate_dyn(fragment)
        .unwrap_or_else(|e| panic!("cluster: {e}"));
    let cluster = grouped_ids.hyper_ids[0];
    let members = g.members_dyn(Kind::COMMUNITY, cluster).expect("members");

    // The walk's own face keys — the position okm-query's operators speak.
    let steps = walk(&g, &person(1));
    assert_eq!(steps.len(), 5);
    let all_keys: Vec<Vec<u8>> = steps.iter().map(|(key, _)| key.clone()).collect();
    let member_keys: Vec<Vec<u8>> = steps
        .iter()
        .filter(|(_, step)| members.contains(&step.edge_id))
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(member_keys.len(), 2, "the cluster holds two of the five");

    let all = KeySet::of(all_keys.clone());
    let grouped = KeySet::of(member_keys.clone());
    assert_eq!(all.keys_ref().len(), 5);
    assert_eq!(grouped.keys_ref().len(), 2);

    // Set algebra: one call each, no loop over ids in the caller.
    assert_eq!(
        all.intersect_dyn(&grouped).keys_ref(),
        member_keys.as_slice()
    );
    assert_eq!(all.union_dyn(&grouped).keys_ref(), all_keys.as_slice());
    assert_eq!(all.difference_dyn(&grouped).keys_ref().len(), 3);
    assert_eq!(grouped.intersect_dyn(&all).keys_ref().len(), 2);
    // A set is a set: input order and duplicates do not survive.
    assert_eq!(
        KeySet::of(vec![b"z".to_vec(), b"a".to_vec(), b"a".to_vec()]).keys_ref(),
        [b"a".to_vec(), b"z".to_vec()]
    );

    // ... then ONE fetch for the surviving rows.
    let store = g.store_handle();
    let rows = all
        .intersect_dyn(&grouped)
        .fetch_dyn(&store)
        .unwrap_or_else(|e| panic!("fetch: {e}"));
    assert_eq!(rows.len(), 2);
    for (key, value) in &rows {
        assert!(
            member_keys.contains(key),
            "the fetch answers the intersection"
        );
        assert_eq!(
            value.as_deref(),
            Some(&[] as &[u8]),
            "a traversal entry carries no value — the key is the fact"
        );
    }
    // The trailing eight bytes of an entry key are the edge id, so the two-step
    // shape answers ids without a second scan.
    let mut fetched_ids: Vec<u64> = rows
        .iter()
        .map(|(key, _)| {
            u64::from_be_bytes(key[key.len() - 8..].try_into().expect("an id suffix"))
        })
        .collect();
    fetched_ids.sort_unstable();
    let mut want = members.clone();
    want.sort_unstable();
    assert_eq!(fetched_ids, want);

    // A key that is no longer in the store answers `None`, not a refusal.
    let stale = KeySet::of(vec![b"nope".to_vec()])
        .fetch_dyn(&store)
        .expect("fetch");
    assert_eq!(stale, vec![(b"nope".to_vec(), None)]);
}

// ---------------------------------------------------------------------------
// 6. The Python face (host-tests: the interpreter is linked)
// ---------------------------------------------------------------------------

#[cfg(feature = "host-tests")]
mod python_face {
    use super::*;
    use pyo3::prelude::*;
    use pyo3::types::{PyDict, PyList};

    fn node_dict<'py>(py: Python<'py>, ns: u16, key: u64) -> Bound<'py, PyDict> {
        let dict = PyDict::new(py);
        dict.set_item("ns", ns).unwrap();
        dict.set_item("key", key.to_be_bytes().to_vec()).unwrap();
        dict
    }

    fn edge_dict<'py>(
        py: Python<'py>,
        src: (u16, u64),
        dst: (u16, u64),
        predicate: &str,
    ) -> Bound<'py, PyDict> {
        let dict = PyDict::new(py);
        dict.set_item("src", node_dict(py, src.0, src.1)).unwrap();
        dict.set_item("dst", node_dict(py, dst.0, dst.1)).unwrap();
        dict.set_item("predicate", predicate).unwrap();
        dict
    }

    fn as_dict<'py>(obj: Bound<'py, PyAny>) -> Bound<'py, PyDict> {
        obj.cast_into::<PyDict>().unwrap()
    }

    /// The dict fragments, the iterator protocol and the `ValueError` mapping —
    /// the layer the plain tests above cannot reach, because it needs a GIL.
    #[test]
    fn the_verbs_and_the_iterator_answer_through_python() {
        Python::initialize();
        Python::attach(|py| {
            // One Bound handle: every call below is the SAME Python path a
            // script takes, not a Rust method with a nicer name.
            let g = Py::new(py, graph()).unwrap().into_bound(py);

            // ---- incorporate: dict in, id report out ----
            let fragment = PyDict::new(py);
            let edges = PyList::empty(py);
            edges
                .append(edge_dict(py, (0x10, 1), (0x11, 1), "works_at"))
                .unwrap();
            edges
                .append(edge_dict(py, (0x10, 1), (0x11, 2), "works_at"))
                .unwrap();
            fragment.set_item("edges", edges).unwrap();
            let cluster = PyDict::new(py);
            cluster.set_item("kind", "community").unwrap();
            let payload = PyDict::new(py);
            payload.set_item("label", "acme").unwrap();
            cluster.set_item("payload", payload).unwrap();
            fragment
                .set_item("clusters", PyList::new(py, [cluster]).unwrap())
                .unwrap();
            // The reducer is a dict of fields, not a callable (see the docstring).
            let fold = PyDict::new(py);
            fold.set_item("seen", 1).unwrap();
            fragment.set_item("reduce_edge", fold).unwrap();

            let report = as_dict(g.call_method1("incorporate", (fragment,)).unwrap());
            let ids: Vec<u64> = report
                .get_item("edge_ids")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            let hypers: Vec<u64> = report
                .get_item("hyper_ids")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(ids.len(), 2);
            assert_eq!(hypers, vec![1u64]);

            // ---- a mixed fragment is REFUSED, in okm-dynamic's own words ----
            let mixed = PyDict::new(py);
            let deletes = PyList::empty(py);
            deletes.append(ids[0]).unwrap();
            mixed.set_item("edges", deletes).unwrap();
            let err = g
                .call_method1("incorporate", (mixed,))
                .expect_err("incorporate writes; a deletion is retract's verb");
            assert!(
                err.to_string().contains("retract's verb"),
                "the binding reports the Rust face's refusal: {err}"
            );
            let mixed = PyDict::new(py);
            let declares = PyList::empty(py);
            declares
                .append(edge_dict(py, (0x10, 3), (0x11, 3), "works_at"))
                .unwrap();
            mixed.set_item("edges", declares).unwrap();
            let err = g
                .call_method1("retract", (mixed,))
                .expect_err("retract removes; a declaration is incorporate's verb");
            assert!(
                err.to_string().contains("incorporate's verb"),
                "and the symmetric one: {err}"
            );
            // A callable reducer is refused with the reason, not silently taken.
            let callable = PyDict::new(py);
            callable
                .set_item("reduce_edge", py.get_type::<PyList>())
                .unwrap();
            let err = g
                .call_method1("incorporate", (callable,))
                .expect_err("a callable would re-encode the payload per entry");
            assert!(err.to_string().contains("dict of fields"), "{err}");

            // ---- the traversal: a real iterator, and `pulled` proves it ----
            let walk = g
                .call_method1("out", (node_dict(py, 0x10, 1),))
                .unwrap();
            let first = as_dict(walk.call_method0("__next__").unwrap());
            assert_eq!(
                first
                    .get_item("edge_id")
                    .unwrap()
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                ids[0]
            );
            let far = as_dict(first.get_item("far").unwrap().unwrap());
            assert_eq!(
                far.get_item("ns")
                    .unwrap()
                    .unwrap()
                    .extract::<u16>()
                    .unwrap(),
                0x11
            );
            assert!(
                first
                    .get_item("key")
                    .unwrap()
                    .unwrap()
                    .extract::<Vec<u8>>()
                    .unwrap()
                    .len()
                    > 8,
                "the step carries the face key — the position a KeySet speaks"
            );
            // The walk answers BOTH edges of the node's face, in entry order,
            // and only then trips the iterator protocol's StopIteration — the
            // thing that makes `for step in g.out(n)` terminate.
            let second = as_dict(walk.call_method0("__next__").unwrap());
            assert_eq!(
                second
                    .get_item("edge_id")
                    .unwrap()
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                ids[1]
            );
            let end = walk.call_method0("__next__");
            assert!(
                end.expect_err("a spent iterator answers StopIteration")
                    .is_instance_of::<pyo3::exceptions::PyStopIteration>(py),
                "exhaustion is StopIteration, not an error of ours"
            );
            assert_eq!(
                walk.getattr("pulled").unwrap().extract::<usize>().unwrap(),
                2
            );
            assert_eq!(
                walk.getattr("direction").unwrap().extract::<String>().unwrap(),
                "out"
            );

            // `.by()` reopens on the predicate's own face — a NEW iterator.
            let narrowed = walk.getattr("by").unwrap().call1(("works_at",)).unwrap();
            let mut narrowed_count = 0;
            loop {
                match narrowed.call_method0("__next__") {
                    Ok(_) => narrowed_count += 1,
                    Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => break,
                    Err(e) => panic!("unexpected error from a walk: {e}"),
                }
            }
            assert_eq!(narrowed_count, 2, "both edges carry the predicate");
            assert_eq!(
                narrowed
                    .getattr("pulled")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                2
            );

            // ---- cluster reads are plain values ----
            assert!(
                g.call_method1("member_of", ("community", 1u64, ids[1]))
                    .unwrap()
                    .extract::<bool>()
                    .unwrap()
            );
            let row = as_dict(g.call_method1("lookup", ("community", 1u64)).unwrap());
            assert_eq!(
                row.get_item("member_count")
                    .unwrap()
                    .unwrap()
                    .extract::<u64>()
                    .unwrap(),
                2
            );
            let refs = g.call_method1("clusters_of", (ids[0],)).unwrap();
            assert_eq!(refs.len().unwrap(), 1);

            // ---- KeySet: algebra inside one call, rows in the next ----
            let keys = PyList::empty(py);
            keys.append(first.get_item("key").unwrap().unwrap())
                .unwrap();
            let big = py.get_type::<KeySet>().call1((keys.clone(),)).unwrap();
            let small = py.get_type::<KeySet>().call1((keys,)).unwrap();
            let both = big.call_method1("__and__", (small,)).unwrap();
            let rows = both.call_method1("fetch", (g.clone(),)).unwrap();
            assert_eq!(rows.len().unwrap(), 1, "one key in, one row out");
        });
    }
}
