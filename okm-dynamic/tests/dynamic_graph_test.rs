//! Dynamic graph edges (ADR-0017 §4, second form): empty declarations,
//! attributes as nTLV frames in the dynamic segment (fixed face 0x01). The
//! byte-equality test against the fixed-ontology form is the drift lock — the
//! two forms MUST land identical bytes for the same edge.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::storage::VirtualStorage;
use okm_core::{Decl, Fragment};
use okm_core::{
    DocumentEncode, EdgeHead, EdgeKey, Graph as StaticGraph, GraphEncode, KeyEncode, NodeRef,
};
use okm_dynamic::{DynEdge, Graph};
use okm_value::{Dict, Variant};

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

fn attrs(since: u16) -> Dict {
    let mut m = Dict::new();
    m.insert("since".to_string(), Variant::U64(since as u64));
    m
}

fn dyn_edge(src: NodeRef, dst: NodeRef, predicate: &str, since: u16) -> DynEdge {
    DynEdge {
        src,
        dst,
        predicate: predicate.into(),
        attrs: attrs(since),
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

#[derive(Clone, Default, KeyEncode)]
pub struct TeamId {
    pub id: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(0x12)]
#[ok_ref(TeamId)]
pub struct Team {}

/// The fixed-ontology twin: same ns, no declared payload fields — the
/// byte-equality counterpart of the empty dynamic declaration. Its member set
/// is GENERATED from the three node tables (ns 0x10/0x11/0x12).
#[derive(Clone, Default, GraphEncode)]
#[ok_ns(400)]
#[ok_nodes(Person, Org, Team)]
pub struct WorksAt {}

fn works_at_schema() -> CollectionSchema {
    CollectionSchema::of_graph::<WorksAt>()
}

#[test]
fn dynamic_link_traverse_and_fetch_back() {
    let store = okm_async::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());

    let key = g
        .link(&dyn_edge(person(1), org(9), "works_at", 2020))
        .unwrap();
    // The writer's answer IS the identity: the tuple, with the predicate as the
    // vocabulary id it was interned at (ADR-0067).
    assert_eq!(key.src, person(1));
    assert_eq!(key.dst, org(9));

    assert_eq!(g.out_edges(&person(1)).unwrap(), vec![key.clone()]);
    assert_eq!(g.in_edges(&org(9)).unwrap(), vec![key.clone()]);
    assert_eq!(
        g.typed_out("works_at", &person(1)).unwrap(),
        vec![key.clone()]
    );
    assert_eq!(g.typed_in("works_at", &org(9)).unwrap(), vec![key.clone()]);
    assert_eq!(g.edges_of_predicate("works_at").unwrap(), vec![key.clone()]);
    assert_eq!(
        g.edge_at("works_at", &person(1), &org(9)).unwrap(),
        Some(key.clone()),
        "the identity lookup is one exact key"
    );
    // Unknown predicates scan empty (no face, no panic) — on a handle that
    // was never primed: the read itself loads the vocabulary.
    assert!(g.typed_out("owns", &person(1)).unwrap().is_empty());

    let back = g.get_edge(&key).unwrap().unwrap();
    assert_eq!(back.src, person(1));
    assert_eq!(back.dst, org(9));
    assert_eq!(back.predicate, "works_at");
    assert_eq!(back.attrs.get("since"), Some(&Variant::U64(2020)));

    // The predicate is listed from the predicate vocabulary (0x04), never from
    // the index faces, and attribute names never share its id space.
    assert_eq!(
        g.predicates().unwrap(),
        vec![(0u16, "works_at".to_string())]
    );
    assert_eq!(g.predicate_vocab_name(0).unwrap().unwrap(), "works_at");
}

#[test]
fn dynamic_unlink_removes_all_faces() {
    let store = okm_async::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());

    let first = g
        .link(&dyn_edge(person(1), org(9), "works_at", 2020))
        .unwrap();
    let second = g
        .link(&dyn_edge(person(2), org(9), "works_at", 2021))
        .unwrap();
    g.unlink(&first).unwrap();

    assert_eq!(g.out_edges(&person(1)).unwrap(), Vec::<EdgeKey>::new());
    assert_eq!(
        g.edges_of_predicate("works_at").unwrap(),
        vec![second.clone()]
    );
    assert!(g.get_edge(&first).unwrap().is_none());
    assert!(g.unlink(&first).is_err(), "double unlink errors");
    // The dynamic segment entry goes with it: no attribute frame survives.
    let mut dyn_key = NS.to_be_bytes().to_vec();
    okm_core::write_slot(&mut dyn_key, okm_core::index::DYNAMIC_SLOT);
    first.write(&mut dyn_key);
    assert!(store.get(&dyn_key).unwrap().is_none());
}

#[test]
fn a_reassertion_is_the_same_row_and_widths_come_from_the_declaration() {
    let store = okm_async::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, works_at_schema());
    let first = g
        .link(&dyn_edge(person(1), org(9), "works_at", 2020))
        .unwrap();
    // Writing the same tuple again is writing the same row (ADR-0067) — there is
    // no number to collide over. The attribute segment is REPLACED, not merged.
    let again = g
        .link(&dyn_edge(person(1), org(9), "works_at", 2021))
        .unwrap();
    assert_eq!(again, first, "the tuple is the identity");
    assert_eq!(
        g.out_edges(&person(1)).unwrap(),
        vec![first.clone()],
        "one row, not two"
    );
    assert_eq!(
        g.get_edge(&first).unwrap().unwrap().attrs.get("since"),
        Some(&Variant::U64(2021)),
        "the second assertion's frame replaced the first's"
    );

    // Widths come from each member table's OWN declaration: a pkey that does
    // not match its table is refused, not smuggled in by a length prefix. The
    // `Team` table is keyed by a `u64`, so a 4-byte pkey in its ns is wrong.
    let wrong_width = NodeRef::new(&0x12u16.to_be_bytes(), &7u32.to_be_bytes());
    let err = g
        .link(&dyn_edge(person(1), wrong_width, "joins", 2021))
        .unwrap_err();
    assert!(err.contains("byte(s) wide"), "unexpected error: {err}");

    // The declared width round-trips.
    let team = NodeRef::new(&0x12u16.to_be_bytes(), &7u64.to_be_bytes());
    let joins = g
        .link(&dyn_edge(person(1), team.clone(), "joins", 2021))
        .unwrap();
    let back = g.get_edge(&joins).unwrap().unwrap();
    assert_eq!(back.dst.pkey, 7u64.to_be_bytes().to_vec());
    assert_eq!(g.in_edges(&team).unwrap(), vec![joins]);
}

#[test]
fn member_set_travels_in_the_schema_and_is_enforced() {
    // The declaration's member set reaches a runtime writer as DATA — the
    // half of a graph declaration the payload layout cannot carry.
    let schema = works_at_schema();
    assert_eq!(
        schema.members,
        vec![0x10, 0x11, 0x12],
        "WorksAtNode::NS_VALUES exported"
    );

    let store = okm_async::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store.clone(), NS, schema);
    // ns 0x13 is nobody's collection in this graph: rejected at write, the same
    // way the static form's `E::Nodes` rejects it.
    let stranger = NodeRef::new(&0x13u16.to_be_bytes(), &1u64.to_be_bytes());
    let err = g
        .link(&dyn_edge(person(1), stranger, "works_at", 2020))
        .unwrap_err();
    assert!(err.contains("not a member"), "unexpected error: {err}");
    assert!(
        g.out_edges(&person(1)).unwrap().is_empty(),
        "nothing was written"
    );

    // Without a member set there is no width for any endpoint, so
    // `[src][pred][dst]` has no boundary to parse by (ADR-0067). The writer
    // refuses rather than guessing a length — an edge needs a declaration to
    // exist at all.
    let mut plain: Graph<_> = Graph::new(
        store.clone(),
        NS,
        CollectionSchema::of::<EdgeKey, WorksAt>(),
    );
    assert!(plain.schema().members.is_empty());
    assert!(plain.schema().member_widths.is_empty());
    let loose = NodeRef::new(&0x13u16.to_be_bytes(), &1u64.to_be_bytes());
    let err = plain
        .link(&dyn_edge(person(1), loose, "works_at", 2020))
        .unwrap_err();
    assert!(
        err.contains("declare it in the schema's member set"),
        "unexpected error: {err}"
    );
}

#[test]
#[should_panic(expected = "disagrees with the schema's declared ns")]
fn assembly_rejects_a_disagreeing_ns_pair() {
    // The pair comes from ONE declaration: an ns argument that disagrees
    // with the schema's own ns is rejected, never overridden.
    let store = okm_async::TestStore::slatedb_mem();
    let _g: Graph<_> = Graph::new(store, NS + 1, works_at_schema());
}

#[test]
fn byte_equality_with_fixed_ontology_form() {
    // THE drift lock: the same edge written through the dynamic form and the
    // fixed-ontology form must land an identical entry set. Attribute frames
    // differ by declaration regime (nTLV in the dynamic segment vs declared
    // payload fields), so the lock covers an edge with NO attributes — the
    // shared wire is everything else, including the empty obj payload.
    let store_dyn = okm_async::TestStore::slatedb_mem();
    let mut gd: Graph<_> = Graph::new(store_dyn.clone(), NS, works_at_schema());
    gd.link(&DynEdge {
        src: person(1),
        dst: org(9),
        predicate: "works_at".into(),
        attrs: Dict::new(),
    })
    .unwrap();

    let store_fix = okm_async::TestStore::slatedb_mem();
    let mut gf: StaticGraph<_, WorksAt> = StaticGraph::new(store_fix.clone());
    let fragment = Fragment::<WorksAt>::new([Decl::edge(EdgeHead {
        src: person(1),
        dst: org(9),
        predicate: "works_at".into(),
    })]);
    gf.incorporate(fragment).unwrap();

    let collect = |store: &okm_async::TestStore, prefix: &[u8]| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for sfx in store
            .scan_suffix(prefix)
            .expect("a scan on a working store")
        {
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

/// The graph holds its two vocabularies per handle. Its allocations commit
/// immediately (through the real store, not an edge batch), so a successful
/// `link` promotes them — otherwise every later write would probe the engine
/// once per name, for the life of the handle.
#[test]
fn link_promotes_its_vocabulary_allocations() {
    let store = okm_async::TestStore::slatedb_mem();
    let mut g: Graph<_> = Graph::new(store, NS, works_at_schema());

    let first = g
        .link(&dyn_edge(person(1), org(9), "works_at", 2020))
        .unwrap();
    assert_eq!(g.vocab_pending(), 0, "an immediate commit is evidence");

    // The same names on a new edge allocate nothing, and leave nothing pending.
    // A re-assertion of the same tuple reuses them too.
    g.link(&dyn_edge(person(1), org(8), "works_at", 2021))
        .unwrap();
    assert_eq!(g.vocab_pending(), 0);
    g.link(&dyn_edge(person(1), org(9), "works_at", 2021))
        .unwrap();
    assert_eq!(g.vocab_pending(), 0, "a re-assertion interns nothing");
    let back = g.get_edge(&first).unwrap().unwrap();
    assert_eq!(back.predicate, "works_at");
    assert_eq!(back.attrs.get("since"), Some(&Variant::U64(2021)));
}
