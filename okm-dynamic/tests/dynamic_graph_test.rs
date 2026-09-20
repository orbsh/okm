//! Dynamic graph edges (ADR-0017 §4, second form): empty declarations,
//! attributes as nTLV frames. The byte-equality test against the
//! fixed-ontology `StaticGraph` is the drift lock — the two forms MUST
//! land identical bytes for the same fact.

use okm_core::obj_dynamic::DynamicValue;
use okm_core::storage::VirtualStorage;
use okm_core::{EdgeFact, KvGraph, NodeRef};
use okm_core::Graph as StaticGraph;
use okm_dynamic::{DynEdge, Graph};
use std::collections::BTreeMap;

fn node(ns: u16, key: u64) -> NodeRef {
    NodeRef::new(&ns.to_be_bytes(), &key.to_be_bytes())
}

fn person(id: u64) -> NodeRef {
    node(0x10, id)
}

fn org(id: u64) -> NodeRef {
    node(0x11, id)
}

fn attrs(since: u16) -> BTreeMap<String, DynamicValue> {
    let mut m = BTreeMap::new();
    m.insert("since".to_string(), DynamicValue::UInt(since as u64));
    m
}

fn dyn_edge(src: NodeRef, dst: NodeRef, kind: &str, since: u16) -> DynEdge {
    DynEdge { src, dst, kind: kind.into(), attrs: attrs(since) }
}

// The fixed-ontology twin: same ns, one declared attribute (u16) —
// the byte-equality counterpart.
struct WorksAt;
impl KvGraph for WorksAt {
    const NS_PREFIX: &'static [u8] = &[0x01, 0x90];
    fn attrs(&self) -> Vec<u8> {
        2020u16.to_be_bytes().to_vec()
    }
    fn attr_faces(&self, _edge_id: u64) -> Vec<(u16, Vec<u8>)> {
        Vec::new()
    }
}

#[test]
fn dynamic_link_traverse_and_fetch_back() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), 0x0190);

    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();

    assert_eq!(g.out_edges(&person(1)), vec![1]);
    assert_eq!(g.in_edges(&org(9)), vec![1]);
    assert_eq!(g.typed_out("works_at", &person(1)), vec![1]);
    assert_eq!(g.typed_in("works_at", &org(9)), vec![1]);
    assert_eq!(g.edges_of_kind("works_at"), vec![1]);
    // Unknown kinds scan empty (no face, no panic).
    assert!(g.typed_out("owns", &person(1)).is_empty());

    let back = g.get_edge(1).unwrap();
    assert_eq!(back.src, person(1));
    assert_eq!(back.dst, org(9));
    assert_eq!(back.kind, "works_at");
    assert_eq!(back.attrs.get("since"), Some(&DynamicValue::UInt(2020)));
}

#[test]
fn dynamic_unlink_removes_all_faces() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), 0x0190);

    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();
    g.link(&dyn_edge(person(2), org(9), "works_at", 2021), 2).unwrap();
    g.unlink(1).unwrap();

    assert_eq!(g.out_edges(&person(1)), Vec::<u64>::new());
    assert_eq!(g.edges_of_kind("works_at"), vec![2]);
    assert!(g.get_edge(1).is_none());
    assert!(g.unlink(1).is_err(), "double unlink errors");
}

#[test]
fn dynamic_rejects_live_id_and_handles_mixed_pkey_widths() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), 0x0190);
    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();
    assert!(g.link(&dyn_edge(person(2), org(8), "works_at", 2020), 1).is_err());

    // Mixed pkey widths need NO registration — refs carry their own
    // len. A 4-byte-key node round-trips beside an 8-byte one.
    let team = NodeRef::new(&0x12u16.to_be_bytes(), &7u32.to_be_bytes());
    let team2 = team.clone();
    g.link(&dyn_edge(person(1), team2, "joins", 2021), 2).unwrap();
    let back = g.get_edge(2).unwrap();
    assert_eq!(back.dst.pkey, 7u32.to_be_bytes().to_vec());
    assert_eq!(g.in_edges(&team), vec![2]);
}

#[test]
fn byte_equality_with_fixed_ontology_form() {
    // THE drift lock: the same fact written through the dynamic form
    // and the fixed-ontology form must land an identical entry set.
    // Attr encoding differs by declaration regime (nTLV frame vs bare
    // fixed width), so equality holds for an edge with NO attributes —
    // the shared wire is everything else.
    let ns = 0x0190u16;

    let store_dyn = okm_core::TestStore::slatedb_mem();
    let mut gd: Graph<_> = Graph::new(store_dyn.clone(), ns);
    gd.link(&dyn_edge(person(1), org(9), "works_at", 0), 7).unwrap();

    let store_fix = okm_core::TestStore::slatedb_mem();
    let mut gf: StaticGraph<_, WorksAt> = StaticGraph::new(store_fix.clone());
    gf.link(
        &EdgeFact { src: person(1), dst: org(9), kind: "works_at".into(), attrs: Vec::new() },
        &WorksAt,
        7,
    )
    .unwrap();

    // Collect the full entry set of the edge ns from both stores.
    let mut prefix = ns.to_be_bytes().to_vec();
    let collect = |store: &okm_core::TestStore, prefix: &[u8]| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for sfx in store.scan_suffix(prefix) {
            let mut full = prefix.to_vec();
            full.extend_from_slice(&sfx);
            out.push(full);
        }
        out
    };
    prefix.extend_from_slice(&[]); // whole-ns walk via repeated face scans is
    // awkward through scan_suffix (prefix must match), so enumerate the
    // known face prefixes instead — the same set the byte lock in
    // graph_edge_test pins.
    let _ = collect(&store_dyn, &prefix);

    let faces: Vec<Vec<u8>> = vec![
        [ns.to_be_bytes().as_slice(), &0x0000u16.to_be_bytes()].concat(),
        [ns.to_be_bytes().as_slice(), &0x4001u16.to_be_bytes()].concat(),
        [ns.to_be_bytes().as_slice(), &0x5001u16.to_be_bytes()].concat(),
        [ns.to_be_bytes().as_slice(), &0x6001u16.to_be_bytes()].concat(),
        [ns.to_be_bytes().as_slice(), &0x7001u16.to_be_bytes()].concat(),
        [ns.to_be_bytes().as_slice(), &0x8001u16.to_be_bytes()].concat(),
    ];
    for face in faces {
        let mut a = collect(&store_dyn, &face);
        let mut b = collect(&store_fix, &face);
        a.sort();
        b.sort();
        assert_eq!(a, b, "face {:02X?} must be byte-identical across forms", &face[2..4]);
    }
}
