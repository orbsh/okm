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

use okm_async::TestStore;
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
/// 44 — the previous row is decoded into a `Dict`, then re-encoded, and the dict round trip
/// owns every one of its strings. The gap against the typed row's 15 is the next thing to
/// look at, not something this lock pretends is fine.
const MAP_MERGE_CEILING: u64 = 44;

/// A warm purge costs at most this many. Measured: 19 — it reads the previous row to sweep
/// its faces, so it pays the typed row's cost plus the decode.
const DELETE_CEILING: u64 = 19;

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
