//! Lazy graph traversal (`Graph::out` / `Graph::inn` / `Graph::walk`) on the real
//! engine matrix: one hop per `next()`, the far endpoint read off the traversal
//! entry, predicate filtering pushed down to the 0x08/0x09 prefix, early
//! termination for free, and the two set operators over a repositionable stream.
//!
//! Five claims, each asserted on the engines a build actually carries rather
//! than on a test double:
//!
//! 1. **The mirror.** One edge read from both directions answers the same head
//!    and two far endpoints that swap — the double write, seen from either face.
//! 2. **The far endpoint is the key.** A hop reads no primary row: the entry
//!    already names both endpoints.
//! 3. **Pushdown.** `by("p")` reads the predicate's own entries, not the node's
//!    whole face with discards — the counting store proves the difference.
//! 4. **Laziness.** `find` / `take` / `take_while` stop where the answer is;
//!    the entries behind it are never pulled.
//! 5. **Order.** Both streams are `Seekable` over their entry key, so
//!    `intersect_seek`, `difference` and `union` work on them unchanged.

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
use okm_core::engine::storage::{ExternalScan, ScanIter};
use okm_core::error::Result;
use okm_core::{
    Decl, Document, DocumentEncode, EdgeHead, EdgeKey, Fragment, Graph, GraphEncode, KeyEncode,
    NodeRef, Seekable, VirtualStorage,
};
use okm_query::{difference, intersect_seek, union};

// ---- the node collections and the graph that names them ----

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PersonId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(PersonId)]
#[ok_ns(0x40)]
pub struct Person {
    pub seen: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OrgId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug, Default)]
#[ok_ref(OrgId)]
#[ok_ns(0x41)]
pub struct Org {}

#[derive(Clone, Default, GraphEncode)]
#[ok_ns(0x0193)]
#[ok_nodes(Person, Org)]
pub struct Employment {
    pub weight: u64,
}

fn person(id: u64) -> NodeRef {
    NodeRef::new(Person::NS_PREFIX, &PersonId { id }.encode())
}

fn org(id: u64) -> NodeRef {
    NodeRef::new(Org::NS_PREFIX, &OrgId { id }.encode())
}

fn head(src: NodeRef, dst: NodeRef, predicate: &str) -> EdgeHead {
    EdgeHead {
        src,
        dst,
        predicate: predicate.to_string(),
    }
}

/// How many edges the hub of [`verify`] has out, split by predicate: the
/// untyped face holds `WORKS + ADVISES` entries, the pushed-down one `WORKS`.
const WORKS: u64 = 20;
const ADVISES: u64 = 30;

// ================= the counting store =================

/// What the store was asked to do, shared with the test after the graph has
/// taken the handle by value.
#[derive(Clone, Default)]
struct Calls {
    /// `scan_range` calls: one per scan opened (a seek inside the engine's own
    /// cursor does not show up here).
    scans: Rc<Cell<usize>>,
    /// `get` calls — a point read, i.e. a ROW fetched.
    gets: Rc<Cell<usize>>,
    /// Entries the CURSOR handed the traversal. This is the number the laziness
    /// claims are about, and the only one a consumer can influence.
    pulled: Arc<AtomicUsize>,
}

impl Calls {
    fn reset(&self) {
        self.scans.set(0);
        self.gets.set(0);
        self.pulled.store(0, Ordering::Relaxed);
    }

    fn pulled(&self) -> usize {
        self.pulled.load(Ordering::Relaxed)
    }
}

/// A store that forwards everything and records the calls that mean "read
/// something": an ordered scan, a point fetch, and every entry the scan's cursor
/// actually handed out.
///
/// The range is materialized here on purpose: [`ScanIter`] is opaque, so a
/// wrapper cannot count its pulls from the outside — but the counter this test
/// needs is the CONSUMER's, i.e. "how many entries did the traversal ask for",
/// and that is measurable through `ScanIter::External`, whose item stream this
/// store owns. (The engine's own laziness is `ScanIter`'s and is covered by
/// `seek_scan_test`; what is under test here is the traversal above it.)
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    calls: Calls,
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
        self.calls.scans.set(self.calls.scans.get() + 1);
        let items: Vec<Result<(Vec<u8>, Vec<u8>)>> = self.inner.scan_range(begin, end).collect();
        ScanIter::External(ExternalScan::new(Box::new(Counted {
            items: items.into_iter(),
            pulled: self.calls.pulled.clone(),
        })))
    }
}

// ================= a key set as a cursor =================

/// Entry keys already in hand, ordered, seekable by binary search — the second
/// side of a set operator when the keys did not come from a scan.
struct KeySet {
    keys: Vec<Vec<u8>>,
    at: usize,
}

impl KeySet {
    fn new(mut keys: Vec<Vec<u8>>) -> Self {
        keys.sort();
        Self { keys, at: 0 }
    }
}

impl Seekable for KeySet {
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

/// A cursor's whole key space, read through [`Seekable`] — the traversal yields
/// STEPS, so the positions are what a comparison needs.
fn keys_of<S: Seekable<Key = Vec<u8>>>(mut cursor: S) -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    if let Some(key) = cursor.key() {
        keys.push(key.clone());
    }
    while cursor.step() {
        if let Some(key) = cursor.key() {
            keys.push(key.clone());
        }
    }
    keys
}

// ================= the suite =================

fn verify(store: TestStore) {
    let name = store.name();
    let calls = Calls::default();
    let counted = Counting {
        inner: store,
        calls: calls.clone(),
    };
    let mut g: Graph<_, Employment> = Graph::new(counted);

    // One hub (person 1) with WORKS + ADVISES out-edges, one person pointing at
    // the first org, and a second out-edge from that org's other endpoint so the
    // in-face has something to mirror.
    let mut decls: Vec<Decl<Employment>> = Vec::new();
    for id in 1..=WORKS {
        decls.push(Decl::edge(head(person(1), org(id), "works_at")));
    }
    for id in 100..100 + ADVISES {
        decls.push(Decl::edge(head(person(1), org(id), "advises")));
    }
    decls.push(Decl::edge(head(person(2), org(1), "works_at")));
    let report = g
        .incorporate(Fragment::new(decls))
        .unwrap_or_else(|e| panic!("{name}: commit: {e}"));
    let keys: Vec<EdgeKey> = report.edges;
    assert_eq!(keys.len() as u64, WORKS + ADVISES + 1);
    let works: Vec<EdgeKey> = keys[..WORKS as usize].to_vec();
    let advises: Vec<EdgeKey> = keys[WORKS as usize..(WORKS + ADVISES) as usize].to_vec();
    let bridge = keys[keys.len() - 1].clone();
    let works_pred = g
        .predicate_id("works_at")
        .expect("predicate probe")
        .expect("the predicate was interned");
    calls.reset();

    // ---- 1. one edge, both directions: the same head, mirrored far ends ----
    let out_step = g
        .out(&person(1))
        .find(|step| step.key() == works[0])
        .expect("the hub's first out-edge");
    let in_step = g
        .inn(&org(1))
        .find(|step| step.key() == works[0])
        .expect("the same edge from the far side");
    assert_eq!(
        (
            out_step.far.clone(),
            out_step.head.src.clone(),
            out_step.head.dst.clone()
        ),
        (org(1), person(1), org(1)),
        "{name}: an out hop's far end is the destination"
    );
    assert_eq!(
        (in_step.far.clone(), in_step.head.dst.clone()),
        (person(1), org(1)),
        "{name}: the in hop's far end is the source"
    );
    assert_eq!(
        out_step.head, in_step.head,
        "{name}: one edge states one head, whichever face read it"
    );
    assert_eq!(
        out_step.head.predicate, works_pred,
        "{name}: the untyped face states the predicate too — it is in the key, not in a row \
         (ADR-0067)"
    );

    // ... and the in face is not a re-read: the second person's edge mirrors too.
    let mirrored = g
        .inn(&org(1))
        .find(|step| step.key() == bridge)
        .expect("the bridge edge arrives at org 1");
    assert_eq!(
        mirrored.far,
        person(2),
        "{name}: a second source on the in face"
    );

    // ---- 2. the far endpoint comes from the KEY: no row is read ----
    calls.reset();
    let walked = g.out(&person(1)).count();
    assert_eq!(walked as u64, WORKS + ADVISES, "{name}: the whole face");
    assert_eq!(
        calls.gets.get(),
        0,
        "{name}: a hop reads no primary row — the entry names both endpoints"
    );

    // ---- 3. predicate pushdown: same set, fewer entries ----
    let untyped_keys = keys_of(g.out(&person(1)));
    let untyped: Vec<EdgeKey> = g.out(&person(1)).map(|step| step.key()).collect();
    // Ordered by the face, i.e. by destination — the identities are not the order.
    let mut untyped_sorted = untyped.clone();
    untyped_sorted.sort();

    // The manual filter the pushdown replaces: walk the untyped face and keep
    // the entries whose key states the predicate. It reads no row — the head is
    // the key (ADR-0067) — so what pushdown buys is ENTRIES, not reads.
    calls.reset();
    let manual: Vec<EdgeKey> = g
        .out(&person(1))
        .filter(|step| step.head.predicate == works_pred)
        .map(|step| step.key())
        .collect();
    let manual_pulled = calls.pulled();
    let manual_gets = calls.gets.get();

    // Pushed down: the 0x08 face's `[predicate][src]` prefix, one scan.
    let narrowed = g.out(&person(1));
    calls.reset();
    let pushed = narrowed
        .by("works_at")
        .expect("predicate id")
        .map(|step| step.key())
        .collect::<Vec<_>>();
    let pushed_pulled = calls.pulled();
    let mut pushed_sorted = pushed.clone();
    pushed_sorted.sort();

    assert_eq!(
        pushed_sorted,
        {
            let mut want = works.clone();
            want.sort();
            want
        },
        "{name}: the pushed-down stream answers exactly the predicate's edges"
    );
    assert_eq!(
        pushed_sorted,
        {
            let mut want = manual.clone();
            want.sort();
            want
        },
        "{name}: ... and it is the same set the key-filter answers"
    );
    assert!(
        pushed_pulled < manual_pulled,
        "{name}: pushdown reads fewer entries ({pushed_pulled} vs {manual_pulled} over a \
         {}-entry face)",
        WORKS + ADVISES
    );
    assert_eq!(
        manual_gets, 0,
        "{name}: the untyped face states the predicate, so filtering it costs no row read \
         ({manual_gets})"
    );
    assert_eq!(
        pushed.len() as u64,
        WORKS,
        "{name}: one entry per works_at edge, no discards"
    );

    // A predicate nobody interned has no face at all — an empty answer, not a
    // scan that filters everything out.
    calls.reset();
    let probe = g.out(&person(1));
    let opened = calls.scans.get();
    let unknown = probe.by("never_written").expect("a probe, not a fault");
    assert_eq!(
        unknown.count(),
        0,
        "{name}: an unknown predicate answers empty"
    );
    assert_eq!(
        calls.scans.get(),
        opened,
        "{name}: and opens no further scan to read and throw away"
    );

    // ---- 4. laziness: the answer's position bounds the reads ----
    calls.reset();
    let found = g
        .out(&person(1))
        .find(|step| step.far == org(1))
        .expect("org 1 is the first destination");
    assert_eq!(found.key(), works[0]);
    let find_pulled = calls.pulled();
    assert!(
        find_pulled <= 3,
        "{name}: `find` must stop at the answer, pulled {find_pulled} of {}",
        WORKS + ADVISES
    );

    calls.reset();
    let head_of_face: Vec<EdgeKey> = g.out(&person(1)).take(3).map(|step| step.key()).collect();
    assert_eq!(head_of_face.len(), 3);
    let take_pulled = calls.pulled();
    assert!(
        take_pulled <= 4,
        "{name}: `take(3)` reads three entries plus the one position it keeps on top, \
         pulled {take_pulled}"
    );

    calls.reset();
    let prefix_run: Vec<EdgeKey> = g
        .out(&person(1))
        .take_while(|step| step.far.ns_id() == Org::NS)
        .map(|step| step.key())
        .collect();
    let while_pulled = calls.pulled();
    assert_eq!(prefix_run.len() as u64, WORKS + ADVISES);
    assert!(
        while_pulled <= WORKS as usize + ADVISES as usize + 1,
        "{name}: `take_while` pays one entry past its run at most, pulled {while_pulled}"
    );

    // ---- 5. Seekable: a seek forward skips a range, the rest still reads ----
    let mut cursor = g.out(&person(1));
    assert_eq!(
        cursor.key(),
        Some(&untyped_keys[0]),
        "{name}: a fresh cursor sits on the first entry"
    );
    let target = untyped_keys[WORKS as usize + 4].clone();
    assert!(cursor.seek(&target), "{name}: seek into the face");
    assert_eq!(
        cursor.key(),
        Some(&target),
        "{name}: the seek landed on the bound, unconsumed"
    );
    // Behind the position is a no-op (forward only).
    assert!(
        cursor.seek(&untyped_keys[0]),
        "{name}: a bound behind the position keeps it"
    );
    assert_eq!(cursor.key(), Some(&target), "{name}: position kept");
    // The items after the seek are the face's own, in order.
    let rest: Vec<Vec<u8>> = keys_of(cursor);
    assert_eq!(
        rest,
        untyped_keys[WORKS as usize + 4..].to_vec(),
        "{name}: the entries after a seek are the face's, unskipped"
    );
    // Past the end: false, spent, and no position to keep.
    let mut past = untyped_keys[untyped_keys.len() - 1].clone();
    past.push(0x00);
    let mut spent = g.out(&person(1));
    assert!(!spent.seek(&past), "{name}: past the end answers false");
    assert_eq!(spent.key(), None, "{name}: ... and the cursor is spent");
    assert!(
        spent.error().is_none(),
        "{name}: running out is not a fault"
    );

    // ---- 6. the set operators over two traversal streams ----
    let hub = keys_of(g.out(&person(1)));
    let leaf = keys_of(g.out(&person(2)));
    assert_eq!(hub.len() as u64, WORKS + ADVISES);
    assert_eq!(leaf.len(), 1, "{name}: person 2 has one out-edge");

    // Equal pair: the face against itself.
    let same: Vec<Vec<u8>> = intersect_seek(g.out(&person(1)), g.out(&person(1))).collect();
    assert_eq!(
        same, hub,
        "{name}: intersecting a face with itself is the face"
    );

    // Disjoint pair: two nodes' out faces share no entry key — the source ref
    // leads the key, so no entry can be in both.
    let none: Vec<Vec<u8>> = intersect_seek(g.out(&person(1)), g.out(&person(2))).collect();
    assert!(none.is_empty(), "{name}: different sources share no entry");
    assert_eq!(
        difference(g.out(&person(1)), g.out(&person(2))).collect::<Vec<_>>(),
        hub,
        "{name}: subtracting a disjoint face changes nothing"
    );
    assert!(
        difference(g.out(&person(1)), g.out(&person(1)))
            .next()
            .is_none(),
        "{name}: a face minus itself is empty"
    );
    let mut merged: Vec<Vec<u8>> = hub.clone();
    merged.extend(leaf.clone());
    merged.sort();
    assert_eq!(
        union(g.out(&person(1)), g.out(&person(2))).collect::<Vec<_>>(),
        merged,
        "{name}: union interleaves the two faces in key order"
    );
    assert_eq!(
        union(g.out(&person(1)), g.out(&person(2)))
            .take(1)
            .collect::<Vec<_>>(),
        vec![hub[0].clone()],
        "{name}: a union answers its head without draining either side"
    );

    // A sparse second side: `difference` must skip the shared keys, not the
    // entries (set semantics — the operator's own contract).
    let every_fifth: Vec<Vec<u8>> = hub.iter().step_by(5).cloned().collect();
    let expected: Vec<Vec<u8>> = hub
        .iter()
        .filter(|key| !every_fifth.contains(key))
        .cloned()
        .collect();
    assert_eq!(
        difference(g.out(&person(1)), KeySet::new(every_fifth.clone())).collect::<Vec<_>>(),
        expected,
        "{name}: difference answers the left keys with no match on the right"
    );
    assert_eq!(
        intersect_seek(KeySet::new(every_fifth.clone()), g.out(&person(1))).collect::<Vec<_>>(),
        every_fifth,
        "{name}: the seek intersection answers the key set exactly"
    );

    // ---- 7. a hop chain is `flat_map`, and `walk` is `out` ----
    assert_eq!(
        g.walk(&person(2)).map(|step| step.far).collect::<Vec<_>>(),
        vec![org(1)],
        "{name}: `walk` is the out face, one hop"
    );
    let deeper: Vec<NodeRef> = g
        .walk(&person(1))
        .flat_map(|step| g.walk(&step.far))
        .map(|step| step.far)
        .collect();
    assert!(
        deeper.is_empty(),
        "{name}: orgs have no out-edges, so the second hop is empty — and nothing \
         collected a visited set to learn it"
    );
    let reverses = g.inn(&org(1)).map(|step| step.far).collect::<Vec<_>>();
    assert_eq!(
        reverses,
        vec![person(1), person(2)],
        "{name}: the in face answers both sources, in key order"
    );

    // Sanity on the face sizes the claims used.
    assert_eq!(advises.len() as u64, ADVISES);
}

#[test]
fn engine_matrix_graph_walk() {
    for (_, store) in TestStore::matrix() {
        verify(store);
    }
}
