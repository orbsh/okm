//! What one write allocates — the ceiling the scratch buffers are for (ADR-0074 §3).
//!
//! A write used to allocate per op (`Vec` per key, `Vec` per value, a fresh batch, a fresh
//! entries list), and each of those was removed by giving the collection buffers that
//! survive between writes. Nothing in the type system says they stay removed: a `clone()`
//! back onto the write path, or a `Vec::new()` inside a helper, costs nothing visible and
//! undoes the work silently. This file is the lock — the counts below are measured, and
//! they are asserted as ceilings, not targets, so a write may get cheaper without an edit
//! here and cannot get more expensive without one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use okm_core::testing::TestStore;
use okm_core::{Collection, DocumentEncode, KeyEncode};

thread_local! {
    /// Allocations made by THIS thread. A count, not a size: the point is how many times
    /// the allocator is entered per write, which is what a per-op `Vec` costs.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump() {
    // A thread tearing down or an allocator re-entry (a lock's own allocation, say) can
    // land without a TLS slot; those allocations are not the write's and are not counted.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            bump();
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            bump();
        }
        out
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// How many times the allocator was entered while `f` ran.
fn allocations<F: FnOnce()>(f: F) -> u64 {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

/// A warm typed row costs at most this many allocations. Measured: 15.
///
/// Was 18 before `header()` stopped building a `Vec` per call (a handle's header is now
/// bytes it carries, so the 26 read sites allocate nothing) and `primary_key` assembled its
/// answer in one buffer. What is left is the payload's own buffer — the one copy the batch
/// must own — plus whatever the engine copies on the way into its chunks.
const TYPED_ROW_CEILING: u64 = 15;

/// A warm map write (`merge`, the read-modify-write path) costs at most this many. Measured:
/// 35 — it compares two entry sets (the new row's faces and the old row's), which used to be
/// two `index_entries` vec-sets and is now two reused buffers. What is left is the `Dict`
/// round trip: the patch arrives as names, the row leaves as fields.
const MAP_MERGE_CEILING: u64 = 35;

/// A warm purge costs at most this many. Measured: 11 — it reads the previous row to sweep
/// its faces, so it pays the decode; its own face list no longer allocates.
const DELETE_CEILING: u64 = 11;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct ItemKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ItemKey)]
#[ok_index(by_name { fields(name) })]
#[ok_index(by_score { fields(score), includes(name) })]
#[ok_ns(0x51)]
pub struct Item {
    pub name: String,
    pub score: u64,
    pub note: String,
}

fn store() -> Collection<TestStore, ItemKey, Item> {
    Collection::open(TestStore::default()).unwrap()
}

fn item(id: u64, name: &str) -> (ItemKey, Item) {
    (
        ItemKey { id },
        Item {
            name: name.to_string(),
            score: id,
            note: String::new(),
        },
    )
}

#[test]
fn a_warm_typed_row_write_is_inside_its_allocation_ceiling() {
    let col = store();
    let (key, doc) = item(1, "first");
    col.put(&key, &doc).unwrap();

    let (key, doc) = item(2, "second");
    let count = allocations(|| col.put(&key, &doc).unwrap());

    assert!(
        count <= TYPED_ROW_CEILING,
        "a warm put allocated {count} times, ceiling is {TYPED_ROW_CEILING}: the entries \
         buffer or the batch blob stopped being reused"
    );
}

#[test]
fn writing_the_same_row_twice_allocates_the_same_way() {
    // The steady state is the one that matters: a reuse that works once and leaks once is
    // the per-op `Vec` this file exists to keep out.
    let col = store();
    let (key, doc) = item(3, "steady");
    col.put(&key, &doc).unwrap();

    let mut counts = Vec::new();
    for round in 0..4 {
        let doc = Item {
            score: round,
            ..doc.clone()
        };
        counts.push(allocations(|| col.put(&key, &doc).unwrap()));
    }

    let first = counts[0];
    for count in &counts {
        assert!(
            *count <= first,
            "put allocated {count} times after a first write of {first}: something grows \
             with the number of writes — counts were {counts:?}"
        );
    }
}

#[test]
fn a_warm_map_merge_is_inside_its_allocation_ceiling() {
    let col = store();
    let (key, doc) = item(4, "merged");
    col.put(&key, &doc).unwrap();

    let mut patch = okm_value::Dict::new();
    patch.insert("score".to_string(), okm_value::Variant::U64(9));
    let patch = okm_value::Variant::Map(patch);
    let count = allocations(|| col.put(&key, &patch).unwrap());

    assert!(
        count <= MAP_MERGE_CEILING,
        "a warm merge allocated {count} times, ceiling is {MAP_MERGE_CEILING}: it decodes the \
         previous row, so it is dearer than a typed put — but the buffers are the same ones"
    );
}

#[test]
fn a_warm_purge_is_inside_its_allocation_ceiling() {
    let col = store();
    let (key, doc) = item(5, "doomed");
    col.put(&key, &doc).unwrap();

    let count = allocations(|| {
        col.purge(&key).unwrap();
    });

    assert!(
        count <= DELETE_CEILING,
        "a warm purge allocated {count} times, ceiling is {DELETE_CEILING}: it reads the \
         previous row, but it must not build an op list of its own"
    );
}

// --- the dynamic plan (ADR-0076 §1) ----------------------------------
//
// A `Parallel` declaration whose two faces are plain: the same two entries a
// typed row writes, produced by the compiled walk instead of a per-entry
// pair list. The ceiling is the lock that keeps the walk wired in — a plan
// that fell back to `index_entries` allocates per entry and blows it.

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct PlanKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PlanKey)]
#[ok_index(by_level { fields(level) })]
#[ok_index(by_level_tag { fields(level), includes(tag) })]
#[ok_ns(0x52)]
pub struct PlanRow {
    pub level: u32,
    pub tag: u64,
    pub name: String,
}

/// A warm `plan_put` costs at most this many allocations. Measured: 8.
///
/// The entry buffer is the collection's, so the faces cost nothing per write — the two plain
/// faces of this declaration append into one `Vec<u8>` that survives between plans. What is
/// left is the plan's own `Batch` (a buffer and an op list), the payload, the primary key, and
/// the accumulator overlay. The old producer spent two allocations PER ENTRY before any of
/// that: a `Vec` for the key and a clone of the includes segment.
const DYNAMIC_PLAN_CEILING: u64 = 8;

/// A warm overwrite costs this many: the same plan plus the sweep. Measured: 16.
///
/// The sweep asks for KEYS (`index_keys`), so the covered face's includes segment is never
/// encoded — the old form built the whole pair list and dropped the values on the floor. Its
/// cost is one `Vec` per key plus the deletes the plan records.
const DYNAMIC_OVERWRITE_CEILING: u64 = 16;

fn plan_table() -> okm_core::model::dynamic_collection::DynamicCollection<TestStore> {
    use okm_core::model::access_method::AccessMethod;
    okm_core::model::dynamic_collection::DynamicCollection::new(
        TestStore::default(),
        0x52,
        okm_core::schema::CollectionSchema::of::<PlanKey, PlanRow>(),
        vec![
            AccessMethod::plain(0x4001, vec!["level".into()], vec![]),
            AccessMethod::plain(0x4002, vec!["level".into()], vec!["tag".into()]),
        ],
    )
}

fn plan_row(id: u64) -> okm_value::Dict {
    let mut map = okm_value::Dict::new();
    map.insert("level".into(), okm_value::Variant::U32(3));
    map.insert("tag".into(), okm_value::Variant::U64(id));
    map.insert("name".into(), okm_value::Variant::Str(format!("row {id}")));
    map
}

#[test]
fn a_warm_dynamic_plan_is_inside_its_allocation_ceiling() {
    let table = plan_table();
    let key = 7u64.to_be_bytes();
    let doc = plan_row(7);

    let plan = table.plan_put(&key, &doc, None, &|_ek| Ok(None)).unwrap();
    assert_eq!(plan.ops.len(), 3, "the row and its two faces");

    let count = allocations(|| {
        table.plan_put(&key, &doc, None, &|_ek| Ok(None)).unwrap();
    });

    assert!(
        count <= DYNAMIC_PLAN_CEILING,
        "a warm dynamic plan allocated {count} times, ceiling is {DYNAMIC_PLAN_CEILING}: the \
         faces are a walk over bytes already built, not a pair list per entry"
    );
}

/// The same plan over an EXISTING row: the sweep removes the old faces. It asks for keys only,
/// so the covered face's includes segment is never encoded — the ceiling is the same as a fresh
/// write's.
#[test]
fn a_warm_dynamic_overwrite_is_inside_its_allocation_ceiling() {
    let table = plan_table();
    let key = 3u64.to_be_bytes();
    let doc = plan_row(3);

    let plan = table.plan_put(&key, &doc, None, &|_ek| Ok(None)).unwrap();
    assert_eq!(plan.ops.len(), 3);

    let count = allocations(|| {
        table
            .plan_put(&key, &doc, Some(&doc), &|_ek| Ok(None))
            .unwrap();
    });

    assert!(
        count <= DYNAMIC_OVERWRITE_CEILING,
        "a warm overwrite allocated {count} times, ceiling is {DYNAMIC_OVERWRITE_CEILING}: the \
         sweep removes the old faces by key, and builds no values for them"
    );
}
