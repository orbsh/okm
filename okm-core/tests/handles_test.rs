//! The handle registry (ADR-0067, ADR-0030) on the real engine matrix: the two
//! fixed faces of the registry, issue-on-first-sight, boundary-only
//! resolution, the bulk resolver, what a delete leaves behind, the reverse
//! face's rebuild, and the fact that a hop reads no row.
//!
//! Every assertion is about BYTES or about an answer a read verb gives — no mock
//! store, no parallel implementation of the layout. The face prefixes are built
//! by hand from the exported slot constants on purpose: the test states the
//! layout the ADR states, so a drift in `model::handles` fails here instead of
//! agreeing with itself.
//!
//! Six claims, each asserted on the engines a build actually carries:
//!
//! 1. **Issue once.** A node named by several edges gets ONE handle and ONE row
//!    per direction; the second mention reuses it.
//! 2. **Boundary only.** A traversal yields handles, and a caller resolves the
//!    ones it asks for — nothing is resolved inside the walk.
//! 3. **Bulk = N singles.** One range scan answers exactly what N point reads
//!    answer, in input order, and a handle no row holds is a fault.
//! 4. **A delete reclaims nothing.** The handle outlives its last edge and a
//!    later resolve still works; the numbering continues, it never reissues.
//! 5. **The reverse face is derived.** Sweeping and corrupting it, then
//!    rebuilding, produces the identical table the write path had.
//! 6. **A hop reads no row.** The counting store proves it: zero fetches.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use okm_async::TestStore;
use okm_core::engine::storage::{ExternalScan, ScanIter, prefix_end};
use okm_core::error::Result;
use okm_core::{
    Decl, Document, DocumentEncode, EdgeHead, EdgeStep, Error, FIRST_HANDLE, Fragment, Graph,
    GraphEncode, HANDLE_NODE_SLOT, Handle, Handles, Hyperedges, KeyEncode, NODE_HANDLE_SLOT,
    NodeRef, VirtualStorage, slot_len,
};

// ---- the node collections and the graph that names them ----

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PersonId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(PersonId)]
#[ok_ns(0x42)]
pub struct Person {
    pub seen: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OrgId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(OrgId)]
#[ok_ns(0x43)]
pub struct Org {}

#[derive(Clone, Default, GraphEncode)]
#[ok_ns(0x0194)] // 404, its own namespace
#[ok_nodes(Person, Org)]
pub struct Employment {
    pub weight: u64,
}

const NS: u16 = 0x0194;

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

fn graph(store: &TestStore) -> Graph<TestStore, Employment> {
    Graph::new(store.clone())
}

/// A head naming an edge that already exists — the identity is the tuple, so a
/// delete needs nothing else (ADR-0067).
fn head_of(g: &mut Graph<TestStore, Employment>, key: &okm_core::EdgeKey) -> EdgeHead {
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

// ---- the registry's key space, spelled out ----

/// A face prefix: `[ns 2B][slot]` — one byte for a fixed face, two for a
/// counting family. Both registry faces are FIXED faces (`0x0E`, `0x0F`), and
/// the prefix is built with the crate's own writer so the test cannot disagree
/// with the layout it is pinning.
fn face_prefix(slot: u16) -> Vec<u8> {
    let mut key = NS.to_be_bytes().to_vec();
    okm_core::write_slot(&mut key, slot);
    key
}

/// Every `(key, value)` row of one face, in key order.
///
/// The forward face is the one face that also carries a FACE-LEVEL SINGLETON —
/// the allocation cursor at the bare prefix `[ns][0x0E]` — so it is not a
/// row and is skipped here; every real row sits exactly eight bytes past the
/// prefix (ADR-0030's "a face-level singleton is not a band").
fn face(store: &TestStore, slot: u16) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prefix = face_prefix(slot);
    let end = prefix_end(&prefix);
    store
        .scan_range(&prefix, end.as_deref())
        .map(|item| item.expect("engine read"))
        .filter(|(key, _)| key.len() > prefix.len())
        .collect()
}

/// The forward face's bare-prefix singleton: the next handle to issue.
fn cursor(store: &TestStore) -> Option<u64> {
    let value = store.get(&face_prefix(HANDLE_NODE_SLOT)).expect("read")?;
    Some(u64::from_be_bytes(
        <[u8; 8]>::try_from(value.as_slice()).expect("eight bytes"),
    ))
}

/// `[ns][0x0E][handle u64]` — the forward row.
fn forward_key(handle: u64) -> Vec<u8> {
    let mut key = face_prefix(HANDLE_NODE_SLOT);
    key.extend_from_slice(&handle.to_be_bytes());
    key
}

/// `[ns][0x0F][node_ns 2B][pkey]` — the reverse row.
fn reverse_key(node: &NodeRef) -> Vec<u8> {
    let mut key = face_prefix(NODE_HANDLE_SLOT);
    key.extend_from_slice(&node.wire());
    key
}

// ---- the counting store (claim 6) ----

#[derive(Clone, Default)]
struct Calls {
    /// `get` calls — a point read, i.e. a ROW fetched.
    gets: Rc<Cell<usize>>,
    /// Entries the cursor handed the consumer.
    pulled: Arc<AtomicUsize>,
}

impl Calls {
    fn reset(&self) {
        self.gets.set(0);
        self.pulled.store(0, Ordering::Relaxed);
    }

    fn gets(&self) -> usize {
        self.gets.get()
    }

    fn pulled(&self) -> usize {
        self.pulled.load(Ordering::Relaxed)
    }
}

struct Counted {
    items: std::vec::IntoIter<Result<(Vec<u8>, Vec<u8>)>>,
    pulled: Arc<AtomicUsize>,
}

impl Iterator for Counted {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.items.next()?;
        self.pulled.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }
}

impl DoubleEndedIterator for Counted {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.items.next_back()
    }
}

/// A store that forwards everything and records the reads that mean "fetch a
/// row" and "hand the consumer an entry". The stream is type-erased because
/// [`ScanIter`] is opaque, so a wrapper can only count pulls through
/// `ScanIter::External` — which is exactly the consumer-side number the claim is
/// about.
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    calls: Calls,
}

impl<S: VirtualStorage> VirtualStorage for Counting<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.calls.gets.set(self.calls.gets.get() + 1);
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> Result<()> {
        self.inner.del(key)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        let items: Vec<Result<(Vec<u8>, Vec<u8>)>> = self.inner.scan_range(begin, end).collect();
        ScanIter::External(ExternalScan::new(Box::new(Counted {
            items: items.into_iter(),
            pulled: self.calls.pulled.clone(),
        })))
    }
}

// ================= 1. issue once, reuse on the second mention =================

fn verify_issue_once(store: &TestStore, engine: &str) {
    // Two FIXED faces, one byte each, adjacent in the core band — the direction
    // IS the face number, so no key carries a discriminator byte. They sit
    // right after the core (0x00–0x0D); the band's reserve (0x10–0x1D) is above
    // them, Extension and the meta at the tail (ADR-0030).
    assert_eq!(
        (HANDLE_NODE_SLOT, NODE_HANDLE_SLOT),
        (0x0E, 0x0F),
        "{engine}: the registry took the two faces right after the core"
    );
    assert_eq!(
        slot_len(HANDLE_NODE_SLOT),
        1,
        "{engine}: a fixed face's slot is one byte"
    );
    assert_eq!(
        slot_len(NODE_HANDLE_SLOT),
        1,
        "{engine}: the reverse face is a fixed face too"
    );
    assert_eq!(
        NODE_HANDLE_SLOT,
        HANDLE_NODE_SLOT + 1,
        "{engine}: and the two faces are adjacent in it"
    );

    let mut g = graph(store);
    let report = g
        .incorporate(Fragment::new([
            Decl::edge(head(person(1), org(1))),
            Decl::edge(head(person(1), org(2))),
            Decl::edge(head(person(2), org(1))),
        ]))
        .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
    assert_eq!(report.edges.len(), 3);

    // Four nodes over seven endpoint mentions: one row per node per direction.
    assert_eq!(
        face(store, HANDLE_NODE_SLOT).len(),
        4,
        "{engine}: one forward row per node, not per mention"
    );
    assert_eq!(
        face(store, NODE_HANDLE_SLOT).len(),
        4,
        "{engine}: and one reverse row per node"
    );

    let registry = Handles::new(store, NS.to_be_bytes());
    let first = registry
        .handle_of(&person(1))
        .unwrap_or_else(|e| panic!("{engine}: probe: {e}"))
        .expect("p1 was an endpoint");
    assert_eq!(first.id(), FIRST_HANDLE, "{engine}: p1 was first sight");
    assert_eq!(
        registry.handle_of(&person(1)).unwrap(),
        Some(first),
        "{engine}: the second mention reuses the handle"
    );
    assert_eq!(
        registry.node(first).unwrap().as_ref(),
        Some(&person(1)),
        "{engine}: and the forward row still names p1"
    );
    assert_eq!(
        store.get(&forward_key(first.id())).unwrap(),
        Some(person(1).wire()),
        "{engine}: the forward row's value is the node's self-describing ref"
    );
    assert_eq!(
        store.get(&reverse_key(&person(1))).unwrap(),
        Some(first.wire()),
        "{engine}: the reverse row is the same handle, big-endian"
    );
    // Issue order is first sight: p1, o1, o2, p2. The watermark is the face's
    // bare-prefix SINGLETON — eight bytes of next handle at `[ns][0x0E]`,
    // told apart from a row by LENGTH, not by a reserved value.
    assert_eq!(
        cursor(store),
        Some(FIRST_HANDLE + 4),
        "{engine}: four handles issued, the cursor names the fifth"
    );
    assert_eq!(
        registry.next_handle().unwrap().id(),
        FIRST_HANDLE + 4,
        "{engine}: and the reader agrees with the singleton"
    );
}

#[test]
fn a_handle_is_issued_on_first_sight_and_reused_after() {
    for (engine, store) in TestStore::matrix() {
        verify_issue_once(&store, engine);
    }
}

// ============ 2. a traversal yields handles, resolves on demand ============

fn verify_traversal_yields_handles(store: &TestStore, engine: &str) {
    let mut g = graph(store);
    g.incorporate(Fragment::new([
        Decl::edge(head(person(1), org(1))),
        Decl::edge(head(person(1), org(2))),
        Decl::edge(head(person(1), org(3))),
    ]))
    .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

    let registry = Handles::new(store, NS.to_be_bytes());
    let here = registry.handle_of(&person(1)).unwrap().expect("p1");
    let steps: Vec<EdgeStep> = g.out(&person(1)).collect();
    assert_eq!(steps.len(), 3, "{engine}: three out-edges");

    // Every step states BOTH endpoints as handles — there is no ref in a step to
    // resolve for free, which is what makes resolution the caller's choice.
    for step in &steps {
        assert!(
            step.far_handle.id() >= FIRST_HANDLE,
            "{engine}: a real handle"
        );
        assert_eq!(step.head.src_handle, here, "{engine}: the near side is p1");
        assert_ne!(
            step.far_handle, here,
            "{engine}: nothing points at itself here"
        );
    }

    // Out entries are ordered by `[src_h][pred][dst_h]`, and the handles were
    // issued in first-sight order, so the far handles ascend.
    let far: Vec<u64> = steps.iter().map(|step| step.far_handle.id()).collect();
    let mut ascending = far.clone();
    ascending.sort();
    assert_eq!(far, ascending, "{engine}: the face orders by handle");

    // The caller resolves the ONE it asked for; the answer is that node alone.
    assert_eq!(
        g.node(steps[0].far_handle).unwrap(),
        org(1),
        "{engine}: the first far handle is org 1"
    );
    assert_eq!(
        g.node(steps[2].far_handle).unwrap(),
        org(3),
        "{engine}: and the last one is org 3 — entry order, not a guess"
    );
    // Resolving a whole step is the same boundary, both sides named.
    let tuple = g.edge_key(&steps[1]).unwrap();
    assert_eq!(
        (tuple.src, tuple.dst),
        (person(1), org(2)),
        "{engine}: a step resolves to the tuple it was written as"
    );
}

#[test]
fn a_traversal_yields_handles_and_resolves_only_what_is_asked() {
    for (engine, store) in TestStore::matrix() {
        verify_traversal_yields_handles(&store, engine);
    }
}

// ============== 3. the bulk resolver answers like N lookups ==============

fn verify_bulk_matches_singles(store: &TestStore, engine: &str) {
    let mut g = graph(store);
    let mut decls = Vec::new();
    for id in 1..=3 {
        for dst in 1..=2 {
            decls.push(Decl::edge(head(person(id), org(dst))));
        }
    }
    g.incorporate(Fragment::new(decls))
        .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

    let registry = Handles::new(store, NS.to_be_bytes());
    let all = registry.all().unwrap();
    assert_eq!(all.len(), 5, "{engine}: three people and two orgs");

    // A SUBSET — sorted, because the bulk resolver's contract is a sorted set
    // (handles are issued monotonically, so a result set is sorted by
    // construction) — answering one ref per input entry, in input order.
    let mut subset: Vec<Handle> = vec![all[3].0, all[0].0, all[1].0];
    subset.sort();
    let bulk = g
        .nodes(&subset)
        .unwrap_or_else(|e| panic!("{engine}: bulk: {e}"));
    let singles: Vec<NodeRef> = subset.iter().map(|h| g.node(*h).unwrap()).collect();
    assert_eq!(bulk, singles, "{engine}: one scan answers N point reads");
    assert_eq!(
        bulk,
        vec![all[0].1.clone(), all[1].1.clone(), all[3].1.clone()],
        "{engine}: one ref per input, in the order it was asked"
    );

    // The sparse case: the two ends of the registry, one span between them.
    let mut ends = vec![all[0].0, all[all.len() - 1].0];
    ends.sort();
    assert_eq!(
        g.nodes(&ends).unwrap(),
        vec![all[0].1.clone(), all[all.len() - 1].1.clone()],
        "{engine}: a sparse set resolves"
    );

    // Empty in, empty out — no scan opened.
    assert!(
        g.nodes(&[]).unwrap().is_empty(),
        "{engine}: no handles, no work"
    );

    // A handle no row holds is a FAULT, not a short answer: a caller that asked
    // for it read it off a face that promises it exists.
    let ghost = Handle::new(all[all.len() - 1].0.id() + 1);
    let with_ghost = vec![all[0].0, ghost];
    assert!(
        matches!(g.nodes(&with_ghost), Err(Error::Corrupt { .. })),
        "{engine}: a missing registry row is corruption, not absence"
    );
}

#[test]
fn the_bulk_resolver_answers_identically_to_n_single_lookups() {
    for (engine, store) in TestStore::matrix() {
        verify_bulk_matches_singles(&store, engine);
    }
}

// ================ 4. a delete leaves the handle behind ================

fn verify_delete_leaves_the_handle(store: &TestStore, engine: &str) {
    let mut g = graph(store);
    let report = g
        .incorporate(Fragment::new([
            Decl::edge(head(person(1), org(1))),
            Decl::edge(head(person(2), org(2))),
        ]))
        .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

    let registry = Handles::new(store, NS.to_be_bytes());
    let one = registry.handle_of(&person(1)).unwrap().expect("p1");
    let rows = face(store, HANDLE_NODE_SLOT).len();

    // Delete p1's ONLY edge: p1 is now an endpoint of nothing, and its handle
    // stays — a delete removes the assertion, not the endpoint (ADR-0066).
    let mut fragment = Fragment::<Employment>::new([]);
    fragment.delete_edge(head_of(&mut g, &report.edges[0]));
    let gone = g
        .retract(fragment)
        .unwrap_or_else(|e| panic!("{engine}: retract: {e}"));
    assert_eq!(
        (gone.nodes, gone.edges),
        (0, 1),
        "{engine}: one edge, no node"
    );

    assert!(
        g.edge(&report.edges[0]).unwrap().is_none(),
        "{engine}: the edge is gone"
    );
    assert_eq!(
        registry.handle_of(&person(1)).unwrap(),
        Some(one),
        "{engine}: the handle outlives its last edge"
    );
    assert_eq!(
        face(store, HANDLE_NODE_SLOT).len(),
        rows,
        "{engine}: a delete reclaims no handle — no reference counting exists"
    );
    // …and a later resolve still answers: the registry is not the edge set.
    assert_eq!(
        g.node(one).unwrap(),
        person(1),
        "{engine}: the leftover handle still resolves"
    );
    assert_eq!(
        g.nodes(&[one]).unwrap(),
        vec![person(1)],
        "{engine}: bulk resolve too"
    );

    // A later write continues the numbering: never a reissue.
    let cursor = registry.next_handle().unwrap();
    g.incorporate(Fragment::new([Decl::edge(head(person(3), org(3)))]))
        .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));
    let three = registry.handle_of(&person(3)).unwrap().expect("p3");
    assert!(
        three >= cursor,
        "{engine}: the new node is issued past the watermark, not into the hole"
    );
    assert_eq!(
        registry.handle_of(&person(1)).unwrap(),
        Some(one),
        "{engine}: and the leftover is still the same handle"
    );
}

#[test]
fn a_delete_leaves_a_handle_behind_without_breaking_a_later_resolve() {
    for (engine, store) in TestStore::matrix() {
        verify_delete_leaves_the_handle(&store, engine);
    }
}

// ============ 5. rebuild_derived derives the same registry ============

fn verify_rebuild(store: &TestStore, engine: &str) {
    let mut g = graph(store);
    g.incorporate(Fragment::new([
        Decl::edge(head(person(1), org(1))),
        Decl::edge(head(person(2), org(1))),
        Decl::edge(head(person(1), org(2))),
    ]))
    .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

    let registry = Handles::new(store, NS.to_be_bytes());
    let before_forward = face(store, HANDLE_NODE_SLOT);
    let before_reverse = face(store, NODE_HANDLE_SLOT);
    let before_table = registry.all().unwrap();
    let before_cursor = registry.next_handle().unwrap();
    assert_eq!(before_reverse.len(), before_forward.len());

    // Corrupt the DERIVED half the way a crash or a half-run rebuild would: one
    // row missing, and one row naming a node the graph never saw.
    store.del(&reverse_key(&person(2))).unwrap();
    store
        .put(reverse_key(&person(9)), 99u64.to_be_bytes().to_vec())
        .unwrap();
    assert_ne!(
        face(store, NODE_HANDLE_SLOT),
        before_reverse,
        "{engine}: the reverse face is damaged"
    );

    let report = Hyperedges::of::<Employment>(store.clone())
        .rebuild_derived(2)
        .unwrap_or_else(|e| panic!("{engine}: rebuild: {e}"));
    assert!(report.chunks >= 1, "{engine}: the rebuild committed");

    // The reverse face comes back byte for byte, and the stale row is swept:
    // it is derived from the forward face, which is authority.
    assert_eq!(
        face(store, NODE_HANDLE_SLOT),
        before_reverse,
        "{engine}: the derived half is identical after a rebuild"
    );
    assert!(
        store.get(&reverse_key(&person(9))).unwrap().is_none(),
        "{engine}: and a row no forward entry derives was swept"
    );
    // The forward face — and therefore the table the boundary sees — is
    // untouched: a rebuild derives, it never reissues.
    assert_eq!(
        face(store, HANDLE_NODE_SLOT),
        before_forward,
        "{engine}: the authority was not rewritten"
    );
    assert_eq!(
        Handles::new(store, NS.to_be_bytes()).all().unwrap(),
        before_table,
        "{engine}: the same handle table, from the same rows"
    );
    assert_eq!(
        registry.next_handle().unwrap(),
        before_cursor,
        "{engine}: and the same watermark"
    );
    // The repaired face answers what it answered before the damage.
    assert_eq!(
        registry.handle_of(&person(2)).unwrap(),
        before_table
            .iter()
            .find(|(_, node)| node == &person(2))
            .map(|(handle, _)| *handle),
        "{engine}: the swept row is back"
    );
}

#[test]
fn rebuild_derived_produces_an_identical_handle_table() {
    for (engine, store) in TestStore::matrix() {
        verify_rebuild(&store, engine);
    }
}

// ==================== 6. a hop reads no row ====================

fn verify_hop_reads_no_row(store: &TestStore, engine: &str) {
    let calls = Calls::default();
    let counted = Counting {
        inner: store.clone(),
        calls: calls.clone(),
    };
    let mut g: Graph<_, Employment> = Graph::new(counted);
    g.incorporate(Fragment::new([
        Decl::edge(head(person(1), org(1))),
        Decl::edge(head(person(1), org(2))),
    ]))
    .unwrap_or_else(|e| panic!("{engine}: commit: {e}"));

    // Opening the walk costs ONE registry probe — the source node's handle —
    // and no row: the traversal faces are keyed by handles, not by refs.
    calls.reset();
    let mut walk = g.out(&person(1));
    assert_eq!(
        calls.gets(),
        1,
        "{engine}: the open resolves the starting NODE, not an edge"
    );

    calls.reset();
    let step = walk.next().expect("an out-edge");
    assert_eq!(
        calls.gets(),
        0,
        "{engine}: a hop reads NO row — both endpoints are in the entry"
    );
    assert_eq!(
        calls.pulled(),
        1,
        "{engine}: and it pulls one entry, the one it hands back"
    );
    assert!(step.far_handle.id() >= FIRST_HANDLE);

    // The row read is the CALLER's, and it pays exactly one when it asks.
    calls.reset();
    let far = g.node(step.far_handle).unwrap();
    assert_eq!(far, org(1), "{engine}: the far handle resolves to org 1");
    assert_eq!(calls.gets(), 1, "{engine}: one asked-for handle, one row");
}

#[test]
fn a_counting_store_proves_a_hop_reads_no_row() {
    for (engine, store) in TestStore::matrix() {
        verify_hop_reads_no_row(&store, engine);
    }
}
