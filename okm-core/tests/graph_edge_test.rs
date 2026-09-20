//! Graph Edge integration tests (ADR-0017): the fixed-ontology form
//! through the `GraphEdgeEncode` derive — declared attributes, 0x1
//! attribute faces, registry cut/decode, cross-collection batch via
//! `link_into`, and node-kind filtering per the ADR's node-side section.

use okm_core::{
    DocumentEncode, GraphEdgeEncode, KeyEncode, VirtualStorage,
};

// ---- node collections (standard documents, standard layout) ----

#[derive(Clone, Default, KeyEncode)]
pub struct UserId {
    pub id: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(10)]
#[ok_ref(UserId)]
pub struct User {
    #[ok_default(0)]
    active: u8,
}

#[derive(Clone, Default, KeyEncode)]
pub struct OrgId {
    pub id: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(11)]
#[ok_ref(OrgId)]
#[ok_index(by_kind { fields(kind) })]
pub struct Org {
    /// Declared node-kind field: kind = 2 org, 3 team, … An ordinary
    /// declared index (0x1 segment) is the node-kind face (ADR-0017
    /// §3 node-side note).
    #[ok_default(0)]
    kind: u8,
    #[ok_default(0)]
    active: u8,
}

// ---- the graph's edge collection ----

#[derive(Clone, Default, GraphEdgeEncode)]
#[ok_edge(ns = 100, nodes(10 = 8, 11 = 8))]
struct Employment {
    #[ok_default(0)]
    since_year: u16,
    #[ok_default(0)]
    weight: u32,
}

use okm_core::{
    EdgeFact, EndpointRegistry, Graph, NodeRef, PRIMARY_SLOT,
};

fn user(id: u64) -> NodeRef {
    NodeRef::new(&10u16.to_be_bytes(), &id.to_be_bytes())
}

fn org(id: u64) -> NodeRef {
    NodeRef::new(&11u16.to_be_bytes(), &id.to_be_bytes())
}

#[test]
fn derived_graph_impl_carries_ns_and_registry() {
    assert_eq!(<Employment as okm_core::KvGraph>::NS_PREFIX, &100u16.to_be_bytes());
    let r = <Employment as okm_core::KvGraph>::registry();
    assert_eq!(r.key_len_of(10), Some(8));
    assert_eq!(r.key_len_of(11), Some(8));
    assert_eq!(r.key_len_of(12), None);
}

#[test]
fn link_traverse_and_fetch_back_with_attributes() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    let attrs = Employment { since_year: 2020, weight: 7 };
    let fact = EdgeFact {
        src: user(1),
        dst: org(9),
        kind: "employs".into(),
        attrs: okm_core::KvGraph::attrs(&attrs),
    };
    g.link(&fact, &attrs, 1).unwrap();

    // Traversal faces answer through the derived constants.
    assert_eq!(g.out_edges(&user(1)), vec![1]);
    assert_eq!(g.in_edges(&org(9)), vec![1]);
    assert_eq!(g.typed_out("employs", &user(1)), vec![1]);
    assert_eq!(g.typed_in("employs", &org(9)), vec![1]);
    assert_eq!(g.edges_of_kind("employs"), vec![1]);

    // Fetch back: registry cuts both refs, attrs decode round-trip.
    let body = g.get_edge(1).unwrap();
    assert_eq!(body.src, user(1));
    assert_eq!(body.dst, org(9));
    assert_eq!(body.attrs, okm_core::KvGraph::attrs(&attrs));
    let kind_name = g.kind_name(body.kind_id).unwrap();
    assert_eq!(kind_name, "employs");

    // Declared-attribute face: scan [since_year = 2020] → edge 1.
    let slot = <Employment as GraphEdgeAccess>::__okm_face_slot("since_year");
    let ids = g.by_attr_face(slot, &2020u16.to_be_bytes());
    assert_eq!(ids, vec![1]);
    // A value no edge carries scans empty.
    assert!(g.by_attr_face(slot, &1999u16.to_be_bytes()).is_empty());
}

/// Small compile-time bridge: the generated marker structs are
/// per-field; the test resolves a slot by walking the known pair.
trait GraphEdgeAccess {
    fn __okm_face_slot(field: &str) -> u16;
}

impl GraphEdgeAccess for Employment {
    fn __okm_face_slot(field: &str) -> u16 {
        match field {
            "since_year" => __OkmGraphIndex_Employment_since_year::SLOT,
            "weight" => __OkmGraphIndex_Employment_weight::SLOT,
            other => panic!("no face for {other}"),
        }
    }
}

#[test]
fn unlink_and_relabeled_parallel_edges() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    let mk = |src: NodeRef, dst: NodeRef, kind: &str| EdgeFact {
        src,
        dst,
        kind: kind.into(),
        attrs: Vec::new(),
    };
    g.link(&mk(user(1), org(9), "employs"), &Employment::default(), 1).unwrap();
    g.link(&mk(user(1), org(9), "contracts"), &Employment::default(), 2).unwrap();

    // Parallel edges (same endpoints, different kinds) are independent
    // facts on every face.
    assert_eq!(g.typed_out("employs", &user(1)), vec![1]);
    assert_eq!(g.typed_out("contracts", &user(1)), vec![2]);
    assert_eq!(g.edges_of_kind("contracts"), vec![2]);

    g.unlink(1).unwrap();
    assert_eq!(g.typed_out("employs", &user(1)), Vec::<u64>::new());
    assert_eq!(g.edges_of_kind("employs"), Vec::<u64>::new());
    assert_eq!(g.out_edges(&user(1)), vec![2], "the other fact survives");
}

#[test]
fn link_into_shares_one_batch_with_documents() {
    let mut store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let nodes = okm_core::Collection::new(store.clone());

    let mut batch = store.batch();
    // A document write and an edge write into ONE batch.
    nodes.save_into(&mut batch, &UserId { id: 1 }, &User { active: 1 });
    let fact = EdgeFact { src: user(1), dst: org(9), kind: "employs".into(), attrs: Vec::new() };
    g.link_into(&mut batch, &fact, &Employment::default(), 1).unwrap();
    store.commit_batch(batch).unwrap();

    assert!(nodes.get(&UserId { id: 1 }).is_some(), "document landed");
    assert_eq!(g.out_edges(&user(1)), vec![1], "edge landed in the same commit");
}

#[test]
fn node_kind_filter_intersects_typed_traversal() {
    // ADR-0017 §3 node-side note: composite filtering
    // `(:User)-[employs]->(:Org kind=2)` = typed edge face ∪ node kind
    // face, intersected in memory — two cheap faces, one convergence.
    let store = okm_core::TestStore::slatedb_mem();
    let mut orgs = okm_core::Collection::new(store.clone());
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    // Org 9 kind=2, Org 10 kind=3.
    orgs.put(&OrgId { id: 9 }, &Org { kind: 2, active: 1 });
    orgs.put(&OrgId { id: 10 }, &Org { kind: 3, active: 1 });
    let mk = |dst: NodeRef| EdgeFact {
        src: user(1),
        dst,
        kind: "employs".into(),
        attrs: Vec::new(),
    };
    g.link(&mk(org(9)), &Employment::default(), 1).unwrap();
    g.link(&mk(org(10)), &Employment::default(), 2).unwrap();

    // Node kind face = Org's own declared index (standard document
    // layout — nothing graph-specific here).
    let kind2: Vec<u64> = orgs
        .scan::<__OkmIndex_Org_by_kind>(&2u8.to_be_bytes())
        .into_iter()
        .filter_map(|(pk, _)| if pk.taken == 8 { Some(pk.decoded.id) } else { None })
        .collect();
    assert_eq!(kind2, vec![9]);

    // Typed edges from user(1), keep only those whose dst is in the
    // kind-2 node set.
    let kept: Vec<u64> = g
        .typed_out("employs", &user(1))
        .into_iter()
        .filter(|id| {
            let b = g.get_edge(*id).unwrap();
            let dst_id = u64::from_be_bytes(b.dst.pkey.as_slice().try_into().unwrap());
            kind2.contains(&dst_id)
        })
        .collect();
    assert_eq!(kept, vec![1], "kind-3 edge converged out in memory");
}

#[test]
fn runtime_registry_form_matches_compile_time() {
    // The dynamic-form registry: caller-maintained, same cut semantics.
    let mut r = EndpointRegistry::new();
    r.register(10, 8);
    r.register(11, 8);
    r.register(10, 8); // same width re-register = no-op
    assert_eq!(r.entries().len(), 2);
    let wire = user(3).encode();
    assert_eq!(r.cut(&wire).unwrap().pkey, 3u64.to_be_bytes().to_vec());
}

#[test]
fn hex_lock_six_face_layout() {
    // Byte-level lock of one link's six faces (ns 100, edge_id 1,
    // kind "employs" → id 0, user(1)→org(9), attrs empty).
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let attrs = Employment { since_year: 2020, weight: 0 };
    let fact = EdgeFact {
        src: user(1),
        dst: org(9),
        kind: "employs".into(),
        attrs: okm_core::KvGraph::attrs(&attrs),
    };
    g.link(&fact, &attrs, 1).unwrap();

    let ns = 100u16.to_be_bytes();
    let slot = |s: u16| s.to_be_bytes().to_vec();
    let id = 1u64.to_be_bytes().to_vec();
    let kid = 0u16.to_be_bytes().to_vec();
    let src = user(1).encode();
    let dst = org(9).encode();
    let key = |parts: &[Vec<u8>]| parts.concat();

    let mut primary = ns.to_vec();
    primary.extend_from_slice(&PRIMARY_SLOT.to_be_bytes());
    primary.extend_from_slice(&id);
    let body = store.get(&primary).unwrap();
    // Edge body = [src][dst][kind_id][attrs] — attrs from the typed
    // Employment { since_year: 2020, weight: 0 } this link wrote.
    assert_eq!(
        body,
        [
            src.clone(),
            dst.clone(),
            kid.clone(),
            2020u16.to_be_bytes().to_vec(),
            0u32.to_be_bytes().to_vec(),
        ]
        .concat()
    );

    assert!(store.get(&key(&[ns.to_vec(), slot(0x4001), kid.clone(), id.clone()])).is_some(), "kind index");
    assert!(store.get(&key(&[ns.to_vec(), slot(0x5001), src.clone(), id.clone()])).is_some(), "out");
    assert!(store.get(&key(&[ns.to_vec(), slot(0x6001), dst.clone(), id.clone()])).is_some(), "in");
    assert!(
        store.get(&key(&[ns.to_vec(), slot(0x7001), kid.clone(), src.clone(), id.clone()])).is_some(),
        "kind+out"
    );
    assert!(store.get(&key(&[ns.to_vec(), slot(0x8001), kid, dst, id.clone()])).is_some(), "kind+in");

    // Attribute face (0x1 segment, declaration order): since_year=2020.
    let mut face = ns.to_vec();
    face.extend_from_slice(&0x1001u16.to_be_bytes());
    face.extend_from_slice(&2020u16.to_be_bytes());
    face.extend_from_slice(&id);
    assert!(store.get(&face).is_some(), "declared-attribute face");
}
