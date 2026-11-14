//! The restore mirrors: an edge table and a reduce table go back through the
//! SAME write contracts the live path uses (`link_into` / `replace_into`,
//! the accumulator entry), so a restored store is indistinguishable from one
//! that was never exported — ids included.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]
use okm_lake::bridge::EdgeTable;
use okm_lake::bridge::ToRecordBatch;
use okm_lake::snapshot::parquet_io;
use okm_lake::snapshot::parquet_io::EdgeRestore;

use okm_core::{
    Batch, Decl, Document, DocumentEncode, EdgeHead, EdgeKey, Fragment, Graph, GraphEncode,
    KeyEncode, NodeRef, ReduceCodec, VirtualStorage,
};

/// The one-edge write a test needs, expressed the way the API expresses it: a
/// head declaration plus the payload in an edge reducer. The tuple is the
/// identity (ADR-0067), so nothing is numbered.
fn link<S: VirtualStorage, E: GraphEncode + Clone + Default + 'static>(
    g: &mut Graph<S, E>,
    head: EdgeHead,
    payload: &E,
) -> okm_core::Result<()> {
    let mut fragment = Fragment::<E>::new([Decl::edge(head)]);
    let given = payload.clone();
    fragment.on_edge_conflict(move |_stored, _declared| given.clone());
    g.incorporate(fragment).map(|_| ())
}
use okm_value::{Dict, Variant};

use okm_async::TestStore;
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
    link(
        &mut g,
        head(user(1), org(9), "employs"),
        &Employment {
            since_year: 2020,
            weight: 7,
        },
    )
    .unwrap();
    link(
        &mut g,
        head(user(2), org(9), "leads"),
        &Employment {
            since_year: 2021,
            weight: 3,
        },
    )
    .unwrap();
    // One edge carries open attributes (the 0x01 segment). "employs" was
    // interned first (key order: user 1 before user 2), so it is id 0.
    let employs = EdgeKey::new(user(1), 0, org(9));
    let leads = EdgeKey::new(user(2), 1, org(9));
    let mut attrs = Dict::new();
    attrs.insert("note".to_string(), Variant::Str("hi".into()));
    let mut batch = Batch::default();
    g.replace_into(&mut batch, &leads, &attrs).unwrap();
    g.store_mut().commit_batch(batch).unwrap();

    // Export, write, and READ THE FILE BACK — the restore's real input.
    let table = g.to_record_batch_table(name_of).unwrap();
    assert_eq!(table.num_rows(), 2);
    let file = dir.path().join("facts.parquet");
    parquet_io::write_batch(&table, &file).unwrap();
    let restored = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(restored.clone());
    let n = r.import_table(&table, ns_of).unwrap();
    assert_eq!(n, 2);

    // Identities, endpoints, predicate, payload, attributes: all back as they
    // were. The identity is the tuple, so nothing was numbered on the way in or
    // out.
    let one = r.get_edge(&employs).unwrap().unwrap();
    assert_eq!((one.src.clone(), one.dst.clone()), (user(1), org(9)));
    assert_eq!(
        r.predicate_vocab_name(one.predicate_vocab_id)
            .unwrap()
            .unwrap(),
        "employs"
    );
    let payload = r.edge(&employs).unwrap().unwrap();
    assert_eq!((payload.since_year, payload.weight), (2020, 7));
    assert_eq!(r.edge(&leads).unwrap().unwrap().since_year, 2021);
    assert_eq!(
        r.attrs(&leads).unwrap().unwrap().get("note"),
        Some(&Variant::Str("hi".into()))
    );
    assert!(
        r.attrs(&employs).unwrap().is_none(),
        "an edge with no attributes has none"
    );

    // Derived faces follow the write: traversal answers on the restored graph.
    let out = r.out_edges(&user(1)).unwrap();
    assert_eq!(out, vec![employs.clone()]);
    assert_eq!(
        r.typed_in("employs", &org(9)).unwrap(),
        vec![employs],
        "the predicate face names the same identity"
    );
    assert_eq!(r.predicates().unwrap().len(), 2);
}

/// A restore writes edges BY HEAD, so it never carries a handle in the file: the
/// registry is issued by the graph's own write path, in the order the endpoints
/// are first seen. The forward face (`0x10`, handle → node) is authority; the
/// reverse one (`0x11`, node → handle) is derived, and the registry's rebuild is
/// what restates it from the rows the restore just wrote.
#[test]
fn a_restore_writes_the_handle_registry_it_can_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let src = TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(src.clone());
    link(
        &mut g,
        head(user(1), org(9), "employs"),
        &Employment {
            since_year: 2020,
            weight: 7,
        },
    )
    .unwrap();
    link(
        &mut g,
        head(user(2), org(9), "leads"),
        &Employment {
            since_year: 2021,
            weight: 3,
        },
    )
    .unwrap();
    let table = g.to_record_batch_table(name_of).unwrap();
    let file = dir.path().join("facts.parquet");
    parquet_io::write_batch(&table, &file).unwrap();

    // A restore over an EMPTY store: no registry rows existed, so every handle
    // the edges name was issued here.
    let restored = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(restored.clone());
    r.import_file(&file, ns_of).unwrap();

    let registry = r.handles();
    let issued: Vec<(okm_core::Handle, NodeRef)> = registry.all().unwrap();
    assert_eq!(
        issued.len(),
        3,
        "two users and one org — one handle each, nothing resolved twice"
    );
    let u1 = registry
        .handle_of(&user(1))
        .unwrap()
        .expect("user 1 issued");
    assert_eq!(
        registry.node(u1).unwrap().as_ref(),
        Some(&user(1)),
        "handle → node is the authority a read resolves through"
    );

    // The reverse half is derived state: wipe it and the registry still names
    // every node's handle through the forward face alone.
    let mut reverse = <Employment as Document>::NS_PREFIX.to_vec();
    okm_core::write_slot(&mut reverse, okm_core::NODE_HANDLE_SLOT);
    let end = okm_core::engine::storage::prefix_end(&reverse);
    let stale: Vec<Vec<u8>> = restored
        .scan_range(&reverse, end.as_deref())
        .map(|item| item.map(|(k, _)| k))
        .collect::<okm_core::Result<_>>()
        .unwrap();
    assert_eq!(stale.len(), 3, "one reverse row per issued handle");
    for key in &stale {
        restored.del(key).unwrap();
    }
    assert!(
        registry.handle_of(&user(1)).unwrap().is_none(),
        "the derived half is gone; the forward face is untouched"
    );

    // The rebuild is the registry's own phase (`rebuild_derived` calls it):
    // sweep the derived face, restate it from authority.
    assert_eq!(
        registry.rebuild_reverse().unwrap(),
        3,
        "every live handle is restated"
    );
    assert_eq!(
        registry.handle_of(&user(1)).unwrap(),
        Some(u1),
        "the same number for the same node — a rebuild reissues nothing"
    );
    // ... and the restored edges still answer through it.
    let employs = EdgeKey::new(user(1), 0, org(9));
    let leads = EdgeKey::new(user(2), 1, org(9));
    assert_eq!(r.out_edges(&user(1)).unwrap(), vec![employs.clone()]);
    assert_eq!(r.typed_in("leads", &org(9)).unwrap(), vec![leads]);
    // The forward face is what made that possible: a read that resolved a
    // handle never touched the ref the export carried.
    assert_eq!(r.edge(&employs).unwrap().unwrap().since_year, 2020);
}

#[test]
fn an_unknown_endpoint_collection_is_refused_by_name() {
    let store = TestStore::slatedb_mem();
    let mut g: Graph<_, Employment> = Graph::new(store.clone());
    link(
        &mut g,
        head(user(1), org(9), "employs"),
        &Employment::default(),
    )
    .unwrap();
    let table = g.to_record_batch_table(name_of).unwrap();

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

    // A DOCUMENT table handed to the edge importer: the importer looks its columns
    // up by name, and a document table has no `src_table` — the refusal says which
    // column it looked for, which is more useful than a count ever was.
    let log = <LogRow as Document>::collection(store.clone());
    let wrong = log.to_record_batch().unwrap();
    let file = dir.path().join("log.parquet");
    parquet_io::write_batch(&wrong, &file).unwrap();

    let fresh = TestStore::slatedb_mem();
    let mut r: Graph<_, Employment> = Graph::new(fresh);
    let err = r
        .import_table(&wrong, ns_of)
        .expect_err("a document table is not an edge table");
    assert!(err.contains("no `src_table` column"), "{err}");
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
    )
    .unwrap();
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
