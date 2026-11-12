//! A scan as a REPOSITIONABLE cursor (`Collection::seek_scan`) on the real
//! engine matrix, and the operator that uses it (`okm-query::intersect_seek`).
//!
//! Three claims, each asserted on the engines a build actually carries rather
//! than on a test double:
//!
//! 1. **Equivalence.** The repositioning operator answers exactly what the
//!    stream `intersect` answers over the same two scans: same entry keys, same
//!    order.
//! 2. **The path.** The trailing side JUMPS: operator-level steps and seeks are
//!    proportional to the SMALL side, so the entries in between were never
//!    pulled. (Steps are the only way an entry reaches the caller, so the step
//!    count IS the read count.)
//! 3. **The store.** Two range scans opened, no row fetch during the
//!    intersection, and one fetch per answered key afterwards — nothing outside
//!    the answer's neighbourhood is read.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use okm_async::TestStore;
use okm_core::engine::storage::ScanIter;
use okm_core::{
    Collection, DocumentEncode, KeyEncode, Seekable, SharedVirtualStorage, VirtualStorage,
};
use okm_query::{OrderedSets, intersect_seek};

/// 2 000 rows, `tag` = its zero-padded id: the index sorts by the tag, so the
/// entry order is the id order and a tag PREFIX is a contiguous entry range.
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct RowId {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RowId)]
#[ok_ns(77)]
#[ok_index(by_tag { fields(tag) })]
pub struct Row {
    pub tag: String,
    pub n: u32,
}

const ROWS: u64 = 2_000;
/// The narrow prefix: tags `01900..=01999`, i.e. every row whose id starts at
/// the 1 900 mark. A hundred entries out of two thousand — the 20:1 skew the
/// operator is about.
const PREFIX: &[u8] = b"019";
const SMALL: usize = 100;
const SMALL_FROM: u64 = ROWS - SMALL as u64;

fn tag_of(id: u64) -> String {
    format!("{id:05}")
}

fn row_of(id: u64) -> Row {
    Row {
        tag: tag_of(id),
        n: id as u32,
    }
}

// ================= the counting store =================

/// What the store was asked to do, shared with the test after the collection
/// has taken the handle by value.
#[derive(Clone, Default)]
struct Calls {
    /// `scan_range` calls: one per scan opened. A seek that reopened the range
    /// through the STORE would show up here; a reposition inside the engine's
    /// own cursor does not, which is the point.
    scans: Rc<Cell<usize>>,
    /// `get` calls, and which keys they asked for.
    gets: Rc<Cell<usize>>,
    got: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl Calls {
    fn reset(&self) {
        self.scans.set(0);
        self.gets.set(0);
        self.got.borrow_mut().clear();
    }

    fn got_keys(&self) -> Vec<Vec<u8>> {
        self.got.borrow().clone()
    }
}

/// A store that forwards everything and records the two calls that mean "read
/// something": an ordered scan, and a point fetch.
#[derive(Clone)]
struct Counting<S> {
    inner: S,
    calls: Calls,
}

impl<S: VirtualStorage> VirtualStorage for Counting<S> {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> okm_core::error::Result<()> {
        self.inner.put(key, value)
    }

    fn get(&self, key: &[u8]) -> okm_core::error::Result<Option<Vec<u8>>> {
        self.calls.gets.set(self.calls.gets.get() + 1);
        self.calls.got.borrow_mut().push(key.to_vec());
        self.inner.get(key)
    }

    fn del(&self, key: &[u8]) -> okm_core::error::Result<()> {
        self.inner.del(key)
    }

    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> ScanIter {
        self.calls.scans.set(self.calls.scans.get() + 1);
        self.inner.scan_range(begin, end)
    }
}

impl<S: SharedVirtualStorage> SharedVirtualStorage for Counting<S> {
    fn shared_handle(&self) -> Self {
        Self {
            inner: self.inner.shared_handle(),
            calls: self.calls.clone(),
        }
    }
}

// ================= an in-memory key set as a cursor =================

/// The caller-sided cursor: entry keys already in hand (a join's surviving keys),
/// ordered, seekable by binary search. This is the second pairing the operator
/// documents — one broad scan against a small key set — and it is what a caller
/// writes when the small side did not come from a scan.
struct KeySet {
    keys: Vec<Vec<u8>>,
    at: usize,
}

impl KeySet {
    fn new(keys: Vec<Vec<u8>>) -> Self {
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
        // Forward-only: the search starts at the current position, so a bound
        // behind it lands on the position itself (a no-op, as the trait says).
        let base = self.at;
        self.at += self.keys[base..].partition_point(|k| k < lower);
        self.key().is_some()
    }
}

// ================= the counting cursor =================

/// A `Seekable` that counts what the operator asked of the cursor.
#[derive(Clone, Default)]
struct Meter {
    steps: Rc<Cell<usize>>,
    seeks: Rc<Cell<usize>>,
}

struct Metered<S> {
    inner: S,
    meter: Meter,
}

impl<S> Metered<S> {
    fn of(inner: S, meter: &Meter) -> Self {
        Self {
            inner,
            meter: meter.clone(),
        }
    }
}

impl<S: Seekable> Seekable for Metered<S> {
    type Key = S::Key;

    fn key(&self) -> Option<&S::Key> {
        self.inner.key()
    }

    fn step(&mut self) -> bool {
        self.meter.steps.set(self.meter.steps.get() + 1);
        self.inner.step()
    }

    fn seek(&mut self, lower: &S::Key) -> bool {
        self.meter.seeks.set(self.meter.seeks.get() + 1);
        self.inner.seek(lower)
    }
}

// ================= the suite =================

fn verify(store: TestStore) {
    let name = store.name();
    let calls = Calls::default();
    let counted = Counting {
        inner: store,
        calls: calls.clone(),
    };
    let col: Collection<_, RowId, Row> = Collection::new(counted);

    for id in 0..ROWS {
        let row = Row {
            tag: tag_of(id),
            n: id as u32,
        };
        col.put(&RowId { id }, &row).unwrap();
    }
    // Writes are irrelevant to the claims below, and `put` reads the meta entry.
    calls.reset();

    // ---- 1. equivalence: the same answer as the stream intersect ----
    let seeked: Vec<Vec<u8>> = intersect_seek(
        col.seek_scan::<__OkmIndex_Row_by_tag>(&[]),
        col.seek_scan::<__OkmIndex_Row_by_tag>(PREFIX),
    )
    .collect();
    let scans_used = calls.scans.get();
    let gets_used = calls.gets.get();
    assert_eq!(
        scans_used, 2,
        "{name}: the intersection must open one scan per side, opened {scans_used}"
    );
    assert_eq!(
        gets_used, 0,
        "{name}: a cursor answers positions, so the intersection reads no rows, read {gets_used}"
    );

    let stream: Vec<Vec<u8>> = col
        .seek_scan::<__OkmIndex_Row_by_tag>(&[])
        .intersect(col.seek_scan::<__OkmIndex_Row_by_tag>(PREFIX))
        .collect();
    assert_eq!(
        seeked, stream,
        "{name}: the seek form must answer the stream form, entry for entry"
    );

    // ... and both are exactly the entries of the narrow scan, built from the
    // index's own encoder (not from a scan): the answer is the CONTRACT's.
    let mut expected: Vec<Vec<u8>> = (SMALL_FROM..ROWS)
        .map(|id| {
            col.index_key::<__OkmIndex_Row_by_tag>(
                &RowId { id },
                &Row {
                    tag: tag_of(id),
                    n: id as u32,
                },
            )
        })
        .collect();
    expected.sort();
    assert_eq!(
        seeked, expected,
        "{name}: the entries the narrow scan covers"
    );

    // The order is the index order: tags ascending, one entry each.
    for pair in seeked.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{name}: the answer must ascend by entry key"
        );
    }

    // ---- 2. the path: one jump replaced a 1 900-entry walk ----
    //
    // A prefix probe is a CONTIGUOUS block by construction, so this pair has one
    // long gap and then adjacency — which is exactly the shape a narrow range
    // has, and the reason the big side pays a single reopen here.
    let (long_meter, short_meter) = (Meter::default(), Meter::default());
    let jumps: Vec<Vec<u8>> = intersect_seek(
        Metered::of(col.seek_scan::<__OkmIndex_Row_by_tag>(&[]), &long_meter),
        Metered::of(col.seek_scan::<__OkmIndex_Row_by_tag>(PREFIX), &short_meter),
    )
    .collect();
    assert_eq!(jumps, expected, "{name}: the metered run answers the same");

    let long_steps = long_meter.steps.get();
    let long_seeks = long_meter.seeks.get();
    assert_eq!(
        long_seeks, 1,
        "{name}: one reopen for the one gap, got {long_seeks}"
    );
    // Budget plus one drain step per answer: the 1 900 skipped entries cost the
    // budget, not themselves.
    assert_eq!(
        long_steps,
        okm_query::SEEK_BUDGET + SMALL,
        "{name}: the skipped range must cost the budget, not a step per entry"
    );
    let skipped = (ROWS as usize) - SMALL;
    assert!(
        long_steps * 2 < skipped,
        "{name}: {long_steps} steps over {skipped} skipped entries means it walked, not jumped"
    );
    assert_eq!(
        short_meter.seeks.get(),
        0,
        "{name}: the narrow side is the leader, it never repositions"
    );
    assert_eq!(
        short_meter.steps.get(),
        SMALL,
        "{name}: the narrow side steps once per answer"
    );

    // ---- 3. the shape a caller's KEY SET makes: scattered answers ----
    //
    // The other documented pairing: one broad scan against entry keys a previous
    // step already produced (a join's surviving keys, assembled with the index's
    // own encoder). Answers every 20th entry, so every gap is a real gap.
    let scattered: Vec<Vec<u8>> = (0..ROWS)
        .step_by(20)
        .map(|id| col.index_key::<__OkmIndex_Row_by_tag>(&RowId { id }, &row_of(id)))
        .collect();
    assert_eq!(scattered.len(), SMALL);

    calls.reset();
    let (big_meter, set_meter) = (Meter::default(), Meter::default());
    let seeked_scatter: Vec<Vec<u8>> = intersect_seek(
        Metered::of(col.seek_scan::<__OkmIndex_Row_by_tag>(&[]), &big_meter),
        Metered::of(KeySet::new(scattered.clone()), &set_meter),
    )
    .collect();
    assert_eq!(
        seeked_scatter, scattered,
        "{name}: the scattered answer is the key set itself"
    );
    assert_eq!(
        big_meter.seeks.get(),
        SMALL - 1,
        "{name}: one reopen per GAP — the first key sits under both cursors, so it \
         costs no chase; got {}",
        big_meter.seeks.get()
    );
    // Budget + one drain step per answer, and no chase for the first key: 892
    // steps for a 2 000-entry scan, where a walk would be one step per entry.
    assert_eq!(
        big_meter.steps.get(),
        (SMALL - 1) * okm_query::SEEK_BUDGET + SMALL,
        "{name}: the walk must stay inside the budget"
    );
    assert!(
        big_meter.steps.get() * 2 < ROWS as usize,
        "{name}: {} steps on a {ROWS}-entry scan is not a jump",
        big_meter.steps.get()
    );
    assert_eq!(
        (set_meter.steps.get(), set_meter.seeks.get()),
        (SMALL, 0),
        "{name}: the in-memory key set is the leader"
    );
    assert_eq!(
        calls.scans.get(),
        1,
        "{name}: one scan opened for the run, got {}",
        calls.scans.get()
    );
    assert_eq!(
        calls.gets.get(),
        0,
        "{name}: the intersection itself reads no rows"
    );

    // ---- 4. the store: one fetch per answer, none for a skipped entry ----
    let mut fetched = 0usize;
    for entry in &seeked_scatter {
        let tail = &entry[entry.len() - <RowId as KeyEncode>::KEY_LEN..];
        let id = RowId::decode(tail);
        assert_eq!(
            id.id % 20,
            0,
            "{name}: an answered entry outside the key set"
        );
        let row = col
            .get_row(&id)
            .unwrap()
            .expect("an answered key has a row");
        assert_eq!(row.tag, tag_of(id.id), "{name}: the row behind the key");
        fetched += 1;
    }
    assert_eq!(fetched, SMALL);
    assert_eq!(
        calls.gets.get(),
        SMALL,
        "{name}: exactly one row read per answered key"
    );
    assert_eq!(
        calls.scans.get(),
        1,
        "{name}: answering must not open another scan"
    );
    // Every key the store was actually asked for is inside the answer's
    // neighbourhood — the 1 900 other rows were never fetched.
    let got = calls.got_keys();
    assert_eq!(got.len(), SMALL, "{name}: one fetch per answer, no more");
    for key in &got {
        let id = RowId::decode(&key[key.len() - <RowId as KeyEncode>::KEY_LEN..]);
        assert!(
            id.id % 20 == 0,
            "{name}: a row was read outside the answered neighbourhood: {id:?}"
        );
    }
}

/// The trait's contract on the real cursor, engine by engine: a seek lands on
/// the first entry at or after the bound, does NOT consume it, keeps the
/// position on a bound at or behind it, and answers `false` past the end.
///
/// The operator's correctness rests on this, and it is cheap to state where the
/// cursor is a real one.
fn verify_cursor(store: TestStore) {
    let name = store.name();
    let col: Collection<_, RowId, Row> = Collection::new(store);
    for id in 0..50u64 {
        let row = Row {
            tag: tag_of(id),
            n: id as u32,
        };
        col.put(&RowId { id }, &row).unwrap();
    }

    let key_of = |id: u64| {
        col.index_key::<__OkmIndex_Row_by_tag>(
            &RowId { id },
            &Row {
                tag: tag_of(id),
                n: id as u32,
            },
        )
    };

    let mut cursor = col.seek_scan::<__OkmIndex_Row_by_tag>(&[]);
    // Position 0: the first entry.
    assert_eq!(
        cursor.key(),
        Some(&key_of(0)),
        "{name}: a fresh cursor sits on the first entry"
    );

    // Seek to an existing key: lands on it, does not consume it.
    assert!(cursor.seek(&key_of(20)), "{name}: seek to a live key");
    assert_eq!(cursor.key(), Some(&key_of(20)), "{name}: the seek landed");
    assert!(
        cursor.step(),
        "{name}: the landed entry is still there to take"
    );
    assert_eq!(
        cursor.key(),
        Some(&key_of(21)),
        "{name}: step moved past it"
    );

    // Seek between two entries: lands on the first one at or after the bound.
    let mut between = key_of(37);
    between.push(0x00);
    assert!(cursor.seek(&between), "{name}: seek between entries");
    assert_eq!(
        cursor.key(),
        Some(&key_of(38)),
        "{name}: a bound between entries lands on the NEXT one"
    );

    // Backwards (or same position): a no-op that keeps the position.
    assert!(
        cursor.seek(&key_of(10)),
        "{name}: a behind bound is a no-op"
    );
    assert_eq!(
        cursor.key(),
        Some(&key_of(38)),
        "{name}: a behind bound must not move the cursor"
    );
    assert!(
        cursor.seek(&key_of(38)),
        "{name}: the same key is a no-op too"
    );
    assert_eq!(cursor.key(), Some(&key_of(38)), "{name}: position kept");

    // Past the end: false, and nothing at or after the bound.
    let mut past = key_of(49);
    past.push(0x01);
    assert!(!cursor.seek(&past), "{name}: past the end answers false");
    assert_eq!(cursor.key(), None, "{name}: ... and the cursor is spent");
    assert!(!cursor.step(), "{name}: a spent cursor stays spent");
    assert!(
        cursor.error().is_none(),
        "{name}: running out is not a fault"
    );
}

#[test]
fn engine_matrix_seek_scan() {
    // Two fresh engines per matrix entry: the operator suite fills its store, and
    // the contract suite needs a keyspace of its own to seek past the end of.
    let pairs: Vec<_> = TestStore::matrix()
        .into_iter()
        .zip(TestStore::matrix())
        .collect();
    for ((_, store), (_, cursor_store)) in pairs {
        verify(store);
        verify_cursor(cursor_store);
    }
}
