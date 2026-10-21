//! Graph Edge integration tests (ADR-0017 §4): one declaration IS the graph —
//! ns + payload + declared attribute faces + member set. Covers the predicate
//! vocabulary (0x04/0x05), the five graph faces (0x06–0x0A), declared-index
//! faces, exact unlink, membership checks, and the cross-collection batch.

use okm_core::{
    DocumentEncode, EdgeHead, Graph, GraphEncode, KeyEncode, MemBatch, NodeRef, NodeSet,
    VirtualStorage, PRIMARY_SLOT,
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
    /// declared index (counting family 0x1) is the node-kind face — zero
    /// graph-specific layout (ADR-0017 §3 node-side note).
    #[ok_default(0)]
    kind: u8,
    #[ok_default(0)]
    active: u8,
}

// ---- the graph's edge collection: one declaration ----

#[derive(Clone, Default, GraphEncode)]
#[ok_ns(100)]
#[ok_nodes(EmploymentNodes)]
#[ok_index(by_since { fields(since_year) })]
pub struct Employment {
    #[ok_default(0)]
    since_year: u16,
    #[ok_default(0)]
    weight: u32,
}

/// The member set: variants are COLLECTIONS (one ns each), never document
/// types. Membership is a compile-time fact — no slot, no registry.
pub enum EmploymentNodes {
    Users,
    Orgs,
}

impl NodeSet for EmploymentNodes {
    const NS_VALUES: &'static [u16] = &[10, 11];
}

fn user(id: u64) -> NodeRef {
    NodeRef::new(&10u16.to_be_bytes(), &id.to_be_bytes())
}

fn org(id: u64) -> NodeRef {
    NodeRef::new(&11u16.to_be_bytes(), &id.to_be_bytes())
}

fn head(src: NodeRef, dst: NodeRef, predicate: &str) -> EdgeHead {
    EdgeHead { src, dst, predicate: predicate.into() }
}

#[test]
fn link_traverse_and_fetch_back_with_attributes() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    let attrs = Employment { since_year: 2020, weight: 7 };
    g.link(&head(user(1), org(9), "employs"), &attrs, 1).unwrap();

    // Traversal faces answer.
    assert_eq!(g.out_edges(&user(1)), vec![1]);
    assert_eq!(g.in_edges(&org(9)), vec![1]);
    assert_eq!(g.typed_out("employs", &user(1)), vec![1]);
    assert_eq!(g.typed_in("employs", &org(9)), vec![1]);
    assert_eq!(g.edges_of_predicate("employs"), vec![1]);
    // An unknown predicate scans nothing (no face, no panic) — and a fresh
    // handle answers without anyone priming the cache first.
    assert!(g.typed_out("owns", &user(1)).is_empty());
    assert!(g.edges_of_predicate("owns").is_empty());

    // Fetch back: both refs cut, predicate name via the graph's own
    // vocabulary, payload decoded into the declaration's type.
    let body = g.get_edge(1).unwrap();
    assert_eq!(body.src, user(1));
    assert_eq!(body.dst, org(9));
    assert_eq!(g.predicate_vocab_name(body.predicate_vocab_id).unwrap(), "employs");
    let back = g.edge(1).unwrap();
    assert_eq!((back.since_year, back.weight), (2020, 7));

    // Declared-attribute face: scan [since_year = 2020] → edge 1.
    let slot = <__OkmIndex_Employment_by_since as okm_core::KvIndex>::SLOT;
    let ids = g.by_attr_face(slot, &2020u16.to_be_bytes());
    assert_eq!(ids, vec![1]);
    // A value no edge carries scans empty.
    assert!(g.by_attr_face(slot, &1999u16.to_be_bytes()).is_empty());
}

#[test]
fn predicates_are_listed_from_the_predicate_vocabulary() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1).unwrap();
    g.link(&head(user(1), org(9), "contracts"), &Employment::default(), 2).unwrap();
    g.link(&head(user(2), org(9), "employs"), &Employment::default(), 3).unwrap();

    // One 0x04 prefix scan — an exact list, id-ordered, never inferred
    // from the index faces.
    assert_eq!(
        g.predicates(),
        vec![(0u16, "employs".to_string()), (1u16, "contracts".to_string())]
    );
    // Repeating an existing name allocates nothing new.
    assert_eq!(g.predicates().len(), 2);
}

#[test]
fn the_predicate_cursor_is_the_id_faces_bare_prefix() {
    // ADR-0030/ADR-0032: the allocation cursor is the id face's face-level
    // singleton — `[ns][0x04]` with NO predicate id after it. Pinning it pins
    // the FACE, which is the part that can rot silently: a cursor written onto
    // the field-name face (0x02) breaks nothing here and now, it resurfaces as
    // ids restarting after a process boundary and colliding with names already
    // allocated.
    let store = okm_core::TestStore::slatedb_mem();
    let ns: &[u8] = &100u16.to_be_bytes();
    let bare = |s: u16| {
        let mut k = ns.to_vec();
        okm_core::write_slot(&mut k, s);
        k
    };
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1)
        .unwrap();

    assert_eq!(
        store.get(&bare(okm_core::index::PREDICATE_VOCAB_ID_SLOT)),
        Some(1u16.to_be_bytes().to_vec()),
        "next predicate id: one entry, no suffix"
    );
    assert!(
        store
            .get(&bare(okm_core::index::FIELD_VOCAB_ID_SLOT))
            .is_none(),
        "a predicate allocation writes no field-name cursor"
    );

    // A second handle is a fresh cache — the process-boundary case the cursor
    // exists for. Numbering resumes past the allocated predicate instead of
    // restarting at zero.
    let mut g2: Graph<_, Employment> = Graph::new(store.clone());
    assert_eq!(g2.predicate_vocab_name(0).as_deref(), Some("employs"));
    g2.link(&head(user(2), org(9), "contracts"), &Employment::default(), 2)
        .unwrap();
    assert_eq!(
        g2.predicates(),
        vec![(0u16, "employs".to_string()), (1u16, "contracts".to_string())]
    );
    assert_eq!(
        store.get(&bare(okm_core::index::PREDICATE_VOCAB_ID_SLOT)),
        Some(2u16.to_be_bytes().to_vec()),
        "the cursor advanced with the second allocation"
    );
}

#[test]
fn unlink_and_relabeled_parallel_edges() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1).unwrap();
    g.link(&head(user(1), org(9), "contracts"), &Employment::default(), 2).unwrap();

    // Parallel edges (same endpoints, different predicates) are
    // independent edges on every face.
    assert_eq!(g.typed_out("employs", &user(1)), vec![1]);
    assert_eq!(g.typed_out("contracts", &user(1)), vec![2]);
    assert_eq!(g.edges_of_predicate("contracts"), vec![2]);

    g.unlink(1).unwrap();
    assert_eq!(g.typed_out("employs", &user(1)), Vec::<u64>::new());
    assert_eq!(g.edges_of_predicate("employs"), Vec::<u64>::new());
    assert_eq!(g.out_edges(&user(1)), vec![2], "the other edge survives");
}

#[test]
fn unlink_deletes_the_declared_face_entry_exactly() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let a = Employment { since_year: 2020, weight: 1 };
    let b = Employment { since_year: 2020, weight: 2 }; // same declared value
    g.link(&head(user(1), org(9), "employs"), &a, 1).unwrap();
    g.link(&head(user(2), org(9), "employs"), &b, 2).unwrap();

    let slot = <__OkmIndex_Employment_by_since as okm_core::KvIndex>::SLOT;
    assert_eq!(g.by_attr_face(slot, &2020u16.to_be_bytes()), vec![1, 2]);

    g.unlink(1).unwrap();
    // Exact deletion: the payload was decoded, the same declared value was
    // re-derived, and only edge 1's entry went away.
    assert_eq!(g.by_attr_face(slot, &2020u16.to_be_bytes()), vec![2]);
    assert!(g.unlink(1).is_err(), "double unlink errors, not silently re-deletes");
}

#[test]
fn rewrite_is_one_batch() {
    // A functional rewrite is unlink + link in ONE batch (ADR-0010's owed
    // item): same edge_id, new endpoints/predicate/payload, and every stale
    // face goes with the old row — no window between two commits.
    let mut store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let a = Employment { since_year: 2020, weight: 1 };
    let b = Employment { since_year: 2021, weight: 2 };
    g.link(&head(user(1), org(9), "employs"), &a, 1).unwrap();

    let mut batch = MemBatch::default();
    g.unlink_into(&mut batch, 1).unwrap();
    g.link_into(&mut batch, &head(user(1), org(10), "contracts"), &b, 1)
        .unwrap();
    store.commit_batch(batch).unwrap();

    // The row is the new fact, under the same identity.
    let body = g.get_edge(1).unwrap();
    assert_eq!(body.dst, org(10));
    assert_eq!(g.predicate_vocab_name(body.predicate_vocab_id).unwrap(), "contracts");
    // Faces: the old destination and the old predicate are gone, the new ones
    // answer, and the declared face moved with the payload.
    assert!(g.in_edges(&org(9)).is_empty());
    assert_eq!(g.in_edges(&org(10)), vec![1]);
    assert!(g.edges_of_predicate("employs").is_empty());
    assert_eq!(g.edges_of_predicate("contracts"), vec![1]);
    let slot = <__OkmIndex_Employment_by_since as okm_core::KvIndex>::SLOT;
    assert!(g.by_attr_face(slot, &2020u16.to_be_bytes()).is_empty());
    assert_eq!(g.by_attr_face(slot, &2021u16.to_be_bytes()), vec![1]);
}

#[test]
fn member_set_is_checked_at_write() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    // ns 12 is nobody's collection in this graph.
    let stranger = NodeRef::new(&12u16.to_be_bytes(), &1u64.to_be_bytes());
    let err = g
        .link(&head(user(1), stranger, "employs"), &Employment::default(), 1)
        .unwrap_err();
    assert!(err.contains("not a member"), "unexpected error: {err}");
    assert!(g.get_edge(1).is_none(), "nothing was written");
    // A member pair still lands.
    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1).unwrap();
}

#[test]
fn link_into_shares_one_batch_with_documents() {
    let mut store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let nodes = okm_core::Collection::new(store.clone());

    let mut batch = MemBatch::default();
    // A document write and an edge write into ONE batch.
    nodes.save_into(&mut batch, &UserId { id: 1 }, &User { active: 1 });
    g.link_into(&mut batch, &head(user(1), org(9), "employs"), &Employment::default(), 1)
        .unwrap();
    store.commit_batch(batch).unwrap();

    assert!(nodes.get(&UserId { id: 1 }).is_some(), "document landed");
    assert_eq!(g.out_edges(&user(1)), vec![1], "edge landed in the same commit");
}

#[test]
fn node_kind_filter_intersects_typed_traversal() {
    // ADR-0017 §3 node-side note: composite filtering
    // `(:User)-[employs]->(:Org kind=2)` = predicate face ∪ node kind
    // face, intersected in memory — two cheap faces, one convergence.
    let store = okm_core::TestStore::slatedb_mem();
    let mut orgs = okm_core::Collection::new(store.clone());
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    // Org 9 kind=2, Org 10 kind=3.
    orgs.put(&OrgId { id: 9 }, &Org { kind: 2, active: 1 });
    orgs.put(&OrgId { id: 10 }, &Org { kind: 3, active: 1 });
    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1).unwrap();
    g.link(&head(user(1), org(10), "employs"), &Employment::default(), 2).unwrap();

    // Node kind face = Org's own declared index (standard document layout).
    let kind2: Vec<u64> = orgs
        .scan::<__OkmIndex_Org_by_kind>(&2u8.to_be_bytes())
        .into_iter()
        .filter_map(|(pk, _)| if pk.taken == 8 { Some(pk.decoded.id) } else { None })
        .collect();
    assert_eq!(kind2, vec![9]);

    // Predicate-filtered traversal from user(1), keep only edges whose dst
    // is in the kind-2 node set.
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
fn noderef_varint_len_self_describing() {
    // The wire carries the pkey width: same node -> same bytes (prefix
    // scans match), mixed widths need no registration.
    let a = NodeRef::new(&10u16.to_be_bytes(), &3u64.to_be_bytes());
    let b = NodeRef::new(&11u16.to_be_bytes(), &[9, 9, 9, 9]);
    assert_eq!(a.encode().len(), 11);
    assert_eq!(b.encode().len(), 2 + 1 + 4); // ns 2B + len 1B + 4-byte pkey
    let (back, n) = NodeRef::decode(&b.encode()).unwrap();
    assert_eq!(n, b.encode().len());
    assert_eq!(back, b);
}

#[test]
fn hex_lock_one_link() {
    // Byte-level lock of one link (ns 100, edge_id 1, predicate "employs"
    // → id 0, user(1)→org(9), payload {since_year: 2020, weight: 0}).
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    let attrs = Employment { since_year: 2020, weight: 0 };
    g.link(&head(user(1), org(9), "employs"), &attrs, 1).unwrap();

    let ns = 100u16.to_be_bytes();
    let slot = |s: u16| {
        let mut v = Vec::new();
        okm_core::write_slot(&mut v, s);
        v
    };
    let id = 1u64.to_be_bytes().to_vec();
    let pid = 0u16.to_be_bytes().to_vec();
    let src = user(1).encode();
    let dst = org(9).encode();
    assert_eq!(src.len(), 11, "[ns 2B][len 1B][pkey 8B]");
    assert_eq!(dst.len(), 11);

    let mut primary = ns.to_vec();
    okm_core::write_slot(&mut primary, PRIMARY_SLOT);
    assert_eq!(primary.len(), 3, "primary head = ns 2B + fixed face 0x00, one byte");
    primary.extend_from_slice(&id);
    let body = store.get(&primary).unwrap();
    // Body = [src][dst][predicate_vocab_id][obj payload]; the payload is the same
    // two-segment obj a document row carries: [version u8][hot_len u16 BE]
    // [hot segment] — since_year u16 then weight u32, both fixed-width.
    let payload = {
        let mut v = Vec::new();
        v.push(1u8);
        v.extend_from_slice(&6u16.to_be_bytes());
        v.extend_from_slice(&2020u16.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v
    };
    assert_eq!(body, [src.clone(), dst.clone(), pid.clone(), payload].concat());

    // The graph's faces are fixed faces — one byte each (ADR-0030), and the
    // vocabulary precedes the faces that read its ids. The vocabulary class
    // itself is counting family 4 since ADR-0032 (two-byte slots).
    let key = |parts: &[Vec<u8>]| parts.concat();
    let vocab = |s: u16| slot(s);
    assert!(
        store
            .get(&key(&[
                ns.to_vec(),
                vocab(okm_core::index::PREDICATE_VOCAB_ID_SLOT),
                pid.clone()
            ]))
            .is_some(),
        "predicate id → name"
    );
    assert!(
        store
            .get(&key(&[
                ns.to_vec(),
                vocab(okm_core::index::PREDICATE_VOCAB_NAME_SLOT),
                b"employs".to_vec()
            ]))
            .is_some(),
        "predicate name → id"
    );
    assert!(
        store.get(&key(&[ns.to_vec(), slot(0x06), pid.clone(), id.clone()])).is_some(),
        "predicate index"
    );
    assert!(store.get(&key(&[ns.to_vec(), slot(0x07), src.clone(), id.clone()])).is_some(), "out");
    assert!(store.get(&key(&[ns.to_vec(), slot(0x08), dst.clone(), id.clone()])).is_some(), "in");
    assert!(
        store.get(&key(&[ns.to_vec(), slot(0x09), pid.clone(), src.clone(), id.clone()])).is_some(),
        "predicate+out"
    );
    assert!(
        store.get(&key(&[ns.to_vec(), slot(0x0A), pid, dst, id.clone()])).is_some(),
        "predicate+in"
    );
    // Nothing in the retired 0x02/0x03 band names a predicate: that band is
    // now free (the field vocabulary moved to family 4, ADR-0032).
    assert!(
        store
            .get(&key(&[ns.to_vec(), slot(0x02), 0u16.to_be_bytes().to_vec()]))
            .is_none()
    );

    // Declared face (counting family 0x1, declaration order):
    // since_year=2020 — two bytes, unlike the fixed faces above.
    let mut face = ns.to_vec();
    okm_core::write_slot(&mut face, <__OkmIndex_Employment_by_since as okm_core::KvIndex>::SLOT);
    face.extend_from_slice(&2020u16.to_be_bytes());
    face.extend_from_slice(&id);
    assert_eq!(face.len(), 2 + 2 + 2 + 8);
    assert!(store.get(&face).is_some(), "declared-attribute face");
}

#[test]
fn neighbor_nodes_resolve_to_node_refs() {
    let store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());

    let a1 = Employment { since_year: 2020, weight: 1 };
    let a2 = Employment { since_year: 2021, weight: 2 };
    g.link(&head(user(1), org(9), "employs"), &a1, 1).unwrap();
    g.link(&head(user(1), org(10), "employs"), &a2, 2).unwrap();

    // One-hop neighbors as NodeRef — the traversal currency.
    let mut outs = g.out_nodes(&user(1));
    outs.sort();
    assert_eq!(outs, vec![org(9), org(10)]);
    assert_eq!(g.in_nodes(&org(9)), vec![user(1)]);

    // Predicate-qualified neighbors share the resolve step.
    assert_eq!(g.typed_out_nodes("employs", &user(1)), vec![org(9), org(10)]);
    assert_eq!(g.typed_out_nodes("contracts", &user(1)), Vec::<NodeRef>::new());
    assert_eq!(g.typed_in_nodes("employs", &org(10)), vec![user(1)]);

    // Parallel-edge collapse: two heads between the same pair produce
    // the same neighbor twice — multiplicity preserved, dedupe is the
    // caller's set semantics (NodeRef is Hash + Ord).
    let a3 = Employment { since_year: 2022, weight: 3 };
    g.link(&head(user(2), org(9), "employs"), &a3, 3).unwrap();
    g.link(&head(user(2), org(9), "employs"), &a3, 4).unwrap();
    assert_eq!(g.out_nodes(&user(2)), vec![org(9), org(9)]);
    let mut deduped = g.out_nodes(&user(2));
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped, vec![org(9)]);
}
/// ADR-0034: the edge set is ONE table. The endpoints are (table, primary key)
/// pairs — with the pkey as raw bytes, because one edge table spans node
/// collections and cannot decode another table's key — the declaration's fields
/// are typed columns, and the edge's open attributes ride a `variant` column.
#[cfg(feature = "parquet")]
#[test]
fn the_edge_set_exports_as_a_table_with_named_endpoints() {
    use arrow::array::{Array, BinaryArray};
    use std::collections::BTreeMap;

    let mut store = okm_core::TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    g.link(
        &head(user(1), org(9), "employs"),
        &Employment {
            since_year: 2020,
            weight: 7,
        },
        1,
    )
    .unwrap();

    let mut attrs: BTreeMap<String, okm_core::obj_dynamic::DynamicValue> = BTreeMap::new();
    attrs.insert(
        "role".to_string(),
        okm_core::obj_dynamic::DynamicValue::Str("lead".to_string()),
    );
    let mut batch = okm_core::MemBatch::default();
    g.put_fields_into(&mut batch, 1, &attrs);
    store.commit_batch(batch).unwrap();
    assert_eq!(
        g.get_fields(1).map(|f| f.len()),
        Some(1),
        "the attribute segment reads back"
    );

    let names = |ns: u16| match ns {
        10 => Some("users".to_string()),
        11 => Some("orgs".to_string()),
        _ => None,
    };
    let batch = g.to_record_batch_table(names);
    let schema = batch.schema();
    let column = |name: &str| schema.index_of(name).expect("column present");

    assert_eq!(batch.num_rows(), 1);
    assert_eq!(column("src_table"), 0);
    assert_eq!(column("dst_table"), 2);
    assert_eq!(column("predicate"), 4);
    assert!(
        schema.index_of("variant").unwrap() > column("weight"),
        "the variant column sits after the declaration's fields"
    );
    assert_eq!(
        schema.index_of("src_pkey").unwrap(),
        1,
        "the two refs are adjacent (name, pkey)"
    );

    let src_table = batch
        .column(column("src_table"))
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    assert_eq!(src_table.value(0), "users");
    let src_pkey = batch
        .column(column("src_pkey"))
        .as_any()
        .downcast_ref::<BinaryArray>()
        .unwrap();
    assert_eq!(src_pkey.value(0), &1u64.to_be_bytes(), "the raw key bytes");
    let predicate = batch
        .column(column("predicate"))
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    assert_eq!(predicate.value(0), "employs");
    assert_eq!(
        batch.column(column("dst_table")).null_count(),
        0,
        "a known ns is never null"
    );

    // A node ns the caller's map does not know stays null rather than guessed.
    let unnamed = g.to_record_batch_table(|_| None);
    assert_eq!(unnamed.column(0).null_count(), 1);
}
