//! Dynamic graph edges (ADR-0017 §4, second form): empty declarations,
//! attributes as nTLV frames in the dynamic segment (fixed face 0x01). The
//! byte-equality test against the fixed-ontology form is the drift lock — the
//! two forms MUST land identical bytes for the same edge.

use okm_core::obj_dynamic::DynamicValue;
use okm_core::schema::CollectionSchema;
use okm_core::storage::VirtualStorage;
use okm_core::{EdgeHead, EdgeKey, Graph as StaticGraph, GraphEncode, NodeRef, NodeSet};
use okm_dynamic::{DynEdge, Graph};
use std::collections::BTreeMap;

const NS: u16 = 0x0190; // 400

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

fn dyn_edge(src: NodeRef, dst: NodeRef, predicate: &str, since: u16) -> DynEdge {
    DynEdge { src, dst, predicate: predicate.into(), attrs: attrs(since) }
}

/// The fixed-ontology twin: same ns, no declared payload fields — the
/// byte-equality counterpart of the empty dynamic declaration.
#[derive(Clone, Default, GraphEncode)]
#[ok_ns(400)]
#[ok_nodes(WorksAtNodes)]
pub struct WorksAt {}

pub enum WorksAtNodes {
    People,
    Orgs,
}

impl NodeSet for WorksAtNodes {
    const NS_VALUES: &'static [u16] = &[0x10, 0x11];
}

fn works_at_schema() -> CollectionSchema {
    CollectionSchema::of::<EdgeKey, WorksAt>()
}

#[test]
fn dynamic_link_traverse_and_fetch_back() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());

    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();

    assert_eq!(g.out_edges(&person(1)), vec![1]);
    assert_eq!(g.in_edges(&org(9)), vec![1]);
    assert_eq!(g.typed_out("works_at", &person(1)), vec![1]);
    assert_eq!(g.typed_in("works_at", &org(9)), vec![1]);
    assert_eq!(g.edges_of_predicate("works_at"), vec![1]);
    // Unknown predicates scan empty (no face, no panic) — on a handle that
    // was never primed: the read itself loads the vocabulary.
    assert!(g.typed_out("owns", &person(1)).is_empty());

    let back = g.get_edge(1).unwrap();
    assert_eq!(back.src, person(1));
    assert_eq!(back.dst, org(9));
    assert_eq!(back.predicate, "works_at");
    assert_eq!(back.attrs.get("since"), Some(&DynamicValue::UInt(2020)));

    // The predicate is listed from the KIND vocabulary (0x04), never from
    // the index faces, and attribute names never share its id space.
    assert_eq!(g.predicates(), vec![(0u16, "works_at".to_string())]);
    assert_eq!(g.predicate_name(0).unwrap(), "works_at");
}

#[test]
fn dynamic_unlink_removes_all_faces() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());

    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();
    g.link(&dyn_edge(person(2), org(9), "works_at", 2021), 2).unwrap();
    g.unlink(1).unwrap();

    assert_eq!(g.out_edges(&person(1)), Vec::<u64>::new());
    assert_eq!(g.edges_of_predicate("works_at"), vec![2]);
    assert!(g.get_edge(1).is_none());
    assert!(g.unlink(1).is_err(), "double unlink errors");
    // The dynamic segment entry goes with it: no attribute frame survives.
    let mut dyn_key = NS.to_be_bytes().to_vec();
    okm_core::write_slot(&mut dyn_key, okm_core::index::DYNAMIC_SLOT);
    dyn_key.extend_from_slice(&1u64.to_be_bytes());
    assert!(store.get(&dyn_key).is_none());
}

#[test]
fn dynamic_rejects_live_id_and_handles_mixed_pkey_widths() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());
    g.link(&dyn_edge(person(1), org(9), "works_at", 2020), 1).unwrap();
    assert!(g.link(&dyn_edge(person(2), org(8), "works_at", 2020), 1).is_err());

    // Mixed pkey widths need NO registration — refs carry their own
    // len. A 4-byte-key node round-trips beside an 8-byte one.
    let team = NodeRef::new(&0x12u16.to_be_bytes(), &7u32.to_be_bytes());
    g.link(&dyn_edge(person(1), team.clone(), "joins", 2021), 2).unwrap();
    let back = g.get_edge(2).unwrap();
    assert_eq!(back.dst.pkey, 7u32.to_be_bytes().to_vec());
    assert_eq!(g.in_edges(&team), vec![2]);
}

#[test]
#[should_panic(expected = "disagrees with the schema's declared ns")]
fn assembly_rejects_a_disagreeing_ns_pair() {
    // The pair comes from ONE declaration: an ns argument that disagrees
    // with the schema's own ns is rejected, never overridden.
    let store = okm_core::TestStore::slatedb_mem();
    let _g: Graph<_> = Graph::new(store, NS + 1, works_at_schema());
}

#[test]
fn byte_equality_with_fixed_ontology_form() {
    // THE drift lock: the same edge written through the dynamic form and the
    // fixed-ontology form must land an identical entry set. Attribute frames
    // differ by declaration regime (nTLV in the dynamic segment vs declared
    // payload fields), so the lock covers an edge with NO attributes — the
    // shared wire is everything else, including the empty obj payload.
    let store_dyn = okm_core::TestStore::slatedb_mem();
    let mut gd: Graph<_> = Graph::new(store_dyn.clone(), NS, works_at_schema());
    gd.link(
        &DynEdge {
            src: person(1),
            dst: org(9),
            predicate: "works_at".into(),
            attrs: BTreeMap::new(),
        },
        7,
    )
    .unwrap();

    let store_fix = okm_core::TestStore::slatedb_mem();
    let mut gf: StaticGraph<_, WorksAt> = StaticGraph::new(store_fix.clone());
    gf.link(
        &EdgeHead { src: person(1), dst: org(9), predicate: "works_at".into() },
        &WorksAt::default(),
        7,
    )
    .unwrap();

    let collect = |store: &okm_core::TestStore, prefix: &[u8]| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for sfx in store.scan_suffix(prefix) {
            let mut full = prefix.to_vec();
            full.extend_from_slice(&sfx);
            out.push(full);
        }
        out
    };

    // Fixed faces (ADR-0030): one byte each, no counter.
    let face = |s: u8| [NS.to_be_bytes().as_slice(), &[s]].concat();
    for f in [0x00u8, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A] {
        let mut a = collect(&store_dyn, &face(f));
        let mut b = collect(&store_fix, &face(f));
        a.sort();
        b.sort();
        assert_eq!(a, b, "face {f:02X} must be byte-identical across forms");
    }
}