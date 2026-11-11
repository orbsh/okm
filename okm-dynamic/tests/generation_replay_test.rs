//! Replay across a generation change (ADR-0056 §5): the directory never changes, so the
//! history it holds spans several builds and each segment is decoded under the declaration
//! the bytes were written with.
//!
//! The two declarations below are the same table with their fields in a DIFFERENT ORDER —
//! same names, same widths, different offsets. That is the sharpest probe available: a
//! segment decoded under the wrong declaration does not fail, it answers with the other
//! field's value, which is exactly the silent corruption a per-directory declaration caused.

#![cfg(feature = "replay")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::replay::replay_table;
use okm_core::changelog::{ChangelogConfig, ChangelogStore, table_dir};
use okm_value::DynamicValue;
use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_dynamic::DynamicCollection;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

/// Authentic generation 1: `level` sits at offset 0.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(98)]
#[ok_name("generation_mix")]
pub struct Gen1 {
    pub level: u32,
    pub score: u32,
}

/// Generation 2: the same two fields, swapped. A payload read under this declaration when
/// it was written under generation 1 gives back `level = 70`.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(98)]
#[ok_layout(version = 2)]
#[ok_name("generation_mix")]
pub struct Gen2 {
    pub score: u32,
    pub level: u32,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root))
}

#[test]
fn a_mixed_directory_replays_each_segment_under_its_own_declaration() {
    let log = tempfile::tempdir().expect("root");
    {
        let c: Collection<_, K, Gen1> = Collection::captured(store(log.path())).expect("assemble");
        c.put(
            &K { id: 1 },
            &Gen1 {
                level: 7,
                score: 70,
            },
        )
        .expect("put");
    }
    {
        let c: Collection<_, K, Gen2> = Collection::captured(store(log.path())).expect("assemble");
        c.put(
            &K { id: 2 },
            &Gen2 {
                score: 80,
                level: 8,
            },
        )
        .expect("put");
    }

    // A store built today is declared with the NEWEST generation — that is the shape it
    // serves. The history below it is read under the shape it was written with.
    let target = DynamicCollection::new(
        TestStore::slatedb_mem(),
        u16::from_be_bytes([
            <Gen1 as Document>::NS_PREFIX[0],
            <Gen1 as Document>::NS_PREFIX[1],
        ]),
        CollectionSchema::of::<K, Gen2>(),
        Vec::new(),
    );
    let dir = table_dir(log.path(), <Gen1 as Document>::NS_PREFIX);
    let report = replay_table(&dir, &target).expect("replay");
    assert_eq!(report.segments, 2, "both generations were read");
    assert_eq!(report.puts, 2, "one row from each");

    let one = target
        .fetch(&K { id: 1 }.encode())
        .expect("fetch")
        .expect("the generation 1 row survived the change of shape");
    assert_eq!(
        one.get("level"),
        Some(&DynamicValue::U32(7)),
        "generation 1's offsets, not generation 2's: {one:?}"
    );
    assert_eq!(one.get("score"), Some(&DynamicValue::U32(70)));

    let two = target
        .fetch(&K { id: 2 }.encode())
        .expect("fetch")
        .expect("the generation 2 row");
    assert_eq!(two.get("level"), Some(&DynamicValue::U32(8)));
    assert_eq!(two.get("score"), Some(&DynamicValue::U32(80)));
}
