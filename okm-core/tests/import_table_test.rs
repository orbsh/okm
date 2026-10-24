//! The restore mirrors: an edge table and a reduce table go back through the
//! SAME write contracts the live path uses (`link_into` / `replace_into`,
//! the accumulator entry), so a restored store is indistinguishable from one
//! that was never exported — ids included.

use std::collections::BTreeMap;

use okm_core::bridge::tooling::parquet_io;
use okm_core::obj_dynamic::DynamicValue;
use okm_core::{
    Document, DocumentEncode, EdgeHead, Graph, GraphEncode, KeyEncode, Batch, NodeRef,
    ReduceCodec, TestStore, VirtualStorage,
};

// ---- two node collections and one graph ----

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
pub struct Org {
    #[ok_default(0)]
    active: u8,
}

#[derive(Clone, Default, GraphEncode)]
#[ok_ns(100)]
#[ok_nodes(User, Org)]
pub struct Employment {
    #[ok_default(0)]
    since_year: u16,
    #[ok_default(0)]
    weight: u32,
}

// ---- a table with a watermark ----

#[derive(Clone, Default, KeyEncode)]
pub struct LogKey {
    pub seq: u64,
}

#[derive(Clone, Default, DocumentEncode)]
#[ok_ns(41)]
#[ok_ref(LogKey)]
#[ok_reduce(HighWater(seq))]
pub struct LogRow {
    #[ok_default(0)]
    seq: u64,
}

fn user(id: u64) -> NodeRef {
    NodeRef::new(&10u16.to_be_bytes(), &id.to_be_bytes())
}

fn org(id: u64) -> NodeRef {
    NodeRef::new(&11u16.to_be_bytes(), &id.to_be_bytes())
}

fn head(src: NodeRef, dst: NodeRef, predicate: &str) -> EdgeHead {
    EdgeHead {
        src,
        dst,
        predicate: predicate.into(),
    }
}

fn name_of(ns: u16) -> Option<String> {
    match ns {
        10 => Some("users".into()),
        11 => Some("orgs".into()),
        _ => None,
    }
}

fn ns_of(name: &str) -> Option<u16> {
    match name {
        "users" => Some(10),
        "orgs" => Some(11),
        _ => None,
    }
}

#[test]
fn an_edge_table_restores_into_a_fresh_graph() {
    let dir = tempfile::tempdir().unwrap();
    let src = TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(src.clone());
    g.link(
        &head(user(1), org(9), "employs"),
        &Employment {
            since_year: 2020,
            weight: 7,
        },
        1,
    )
    .unwrap();
    g.link(
        &head(user(2), org(9), "leads"),
        &Employment {
            since_year: 2021,
            weight: 3,
        },
        2,
    )
    .unwrap();
    // One edge carries open attributes (the 0x01 segment).
    let mut attrs = BTreeMap::new();
    attrs.insert("note".to_string(), DynamicValue::Str("hi".into()));
    let mut batch = Batch::default();
    g.replace_into(&mut batch, 2, &attrs).unwrap();
    g.store_mut().commit_batch(batch).unwrap();

    // Export, write, and READ THE FILE BACK — the restore's real input.
    let table = g.to_record_batch_table(name_of);
    assert_eq!(table.num_rows(), 2);
    let file = dir.path().join("facts.parquet");
    parquet_io::write_batch(&table, &file).unwrap();
    let restored = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(restored.clone());
    let n = r.import_table(&table, ns_of).unwrap();
    assert_eq!(n, 2);

    // Ids, endpoints, predicate, payload, attributes: all back as they were.
    let one = r.get_edge(1).unwrap();
    assert_eq!((one.src.clone(), one.dst.clone()), (user(1), org(9)));
    assert_eq!(
        r.predicate_vocab_name(one.predicate_vocab_id).unwrap(),
        "employs"
    );
    let payload = r.edge(1).unwrap();
    assert_eq!((payload.since_year, payload.weight), (2020, 7));
    assert_eq!(r.edge(2).unwrap().since_year, 2021);
    assert_eq!(
        r.attrs(2).unwrap().get("note"),
        Some(&DynamicValue::Str("hi".into()))
    );
    assert!(
        r.attrs(1).is_none(),
        "an edge with no attributes has none"
    );

    // Derived faces follow the write: traversal answers on the restored graph.
    assert_eq!(r.out_edges(&user(1)), vec![1]);
    assert_eq!(r.typed_in("employs", &org(9)), vec![1]);
    assert_eq!(r.predicates().len(), 2);
}

#[test]
fn an_unknown_endpoint_collection_is_refused_by_name() {
    let store = TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    g.link(&head(user(1), org(9), "employs"), &Employment::default(), 1)
        .unwrap();
    let table = g.to_record_batch_table(name_of);

    let fresh = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(fresh);
    let err = r
        .import_table(&table, |_| None)
        .expect_err("an unknown collection is a refusal");
    assert!(err.contains("no collection named `users`"), "{err}");
    assert!(err.contains("row 0"), "{err}");
}

#[test]
fn a_column_set_that_is_not_an_edge_table_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = TestStore::slatedb_mem();
    let log = <LogRow as Document>::collection(store.clone());
    log.put(&LogKey { seq: 1 }, &LogRow { seq: 1 }).unwrap();

    // A DOCUMENT table handed to the edge importer: the column count is the
    // declaration's, and it does not match an edge table's shape.
    let log = <LogRow as Document>::collection(store.clone());
    let wrong = log.to_record_batch();
    let file = dir.path().join("log.parquet");
    parquet_io::write_batch(&wrong, &file).unwrap();

    let fresh = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(fresh);
    let err = r
        .import_table(&wrong, ns_of)
        .expect_err("a document table is not an edge table");
    assert!(err.contains("columns"), "{err}");
}

#[test]
fn a_watermark_restores_and_is_never_lowered() {
    let dir = tempfile::tempdir().unwrap();
    let src = TestStore::slatedb_mem();
    let log = <LogRow as Document>::collection(src.clone());
    log.put(&LogKey { seq: 5 }, &LogRow { seq: 5 }).unwrap();

    let file = dir.path().join("log__reduces.parquet");
    parquet_io::export_reduces::<_, __OkmReduce_LogRow_0>(
        &src,
        <LogRow as Document>::NS_PREFIX,
        "log",
        &file,
    )
    .unwrap();

    // A fresh store has no watermark; the file pushes one.
    let dst = TestStore::slatedb_mem();
    let n = parquet_io::import_reduces::<_, __OkmReduce_LogRow_0>(
        &dst,
        <LogRow as Document>::NS_PREFIX,
        "log",
        &file,
    )
    .unwrap();
    assert_eq!(n, 1);
    let restored = okm_core::reduce::scan_reduces::<_, __OkmReduce_LogRow_0>(
        &dst,
        <LogRow as Document>::NS_PREFIX,
    );
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].1, 5);

    // A second run changes nothing: an existing watermark is never lowered,
    // and an equal one is not rewritten (0 rows pushed).
    let again = parquet_io::import_reduces::<_, __OkmReduce_LogRow_0>(
        &dst,
        <LogRow as Document>::NS_PREFIX,
        "log",
        &file,
    )
    .unwrap();
    assert_eq!(again, 0);
}

#[test]
fn a_reduce_file_for_another_table_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src = TestStore::slatedb_mem();
    let log = <LogRow as Document>::collection(src.clone());
    log.put(&LogKey { seq: 1 }, &LogRow { seq: 1 }).unwrap();
    let file = dir.path().join("log__reduces.parquet");
    parquet_io::export_reduces::<_, __OkmReduce_LogRow_0>(
        &src,
        <LogRow as Document>::NS_PREFIX,
        "log",
        &file,
    )
    .unwrap();

    let dst = TestStore::slatedb_mem();
    let err = parquet_io::import_reduces::<_, __OkmReduce_LogRow_0>(
        &dst,
        <LogRow as Document>::NS_PREFIX,
        "not-log",
        &file,
    )
    .expect_err("a file that names another table is refused");
    assert!(err.to_string().contains("not `not-log`"), "{err}");
}
