//! Replay across a generation change (ADR-0056 §5): the directory never changes, so the
//! history it holds spans several builds and each segment is decoded under the declaration
//! the bytes were written with.
//!
//! The two declarations below are the same table with their fields in a DIFFERENT ORDER —
//! same names, same widths, different offsets. That is the sharpest probe available: a
//! segment decoded under the wrong declaration does not fail, it answers with the other
//! field's value, which is exactly the silent corruption a per-directory declaration caused.

#![cfg(feature = "schema-serde")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::replay::replay_table;
use okm_core::changelog::{ChangelogConfig, ChangelogStore, table_dir};
use okm_core::model::dynamic_collection::DynamicCollection;
use okm_core::schema::CollectionSchema;
use okm_core::testing::TestStore;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_value::Variant;

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
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(98)]
#[ok_layout(version = 2)]
#[ok_name("generation_mix")]
pub struct Gen2 {
    pub score: u32,
    pub level: u32,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(TestStore::default(), ChangelogConfig::new(root))
}

#[test]
fn a_mixed_directory_replays_each_segment_under_its_own_declaration() {
    let log = tempfile::tempdir().expect("root");
    {
        let c: Collection<_, K, Gen1> = Collection::open(store(log.path())).expect("assemble");
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
        let c: Collection<_, K, Gen2> = Collection::open(store(log.path())).expect("assemble");
        c.put(
            &K { id: 2 },
            &Gen2 {
                score: 80,
                level: 8,
            },
        )
        .expect("put");
    }

    // One directory per ADDRESS (ADR-0075 §2), so a directory is ONE generation: a replay
    // reads it into a store declared with that generation. Carrying generation 1's bytes into
    // a generation-2 store is not a replay — it is the migration (ADR-0075 §5), which
    // re-encodes through the old declaration instead of reinterpreting its offsets.
    let dir_one = table_dir(log.path(), <Gen1 as Document>::NS_PREFIX);
    let dir_two = table_dir(log.path(), <Gen2 as Document>::NS_PREFIX);
    assert_ne!(
        dir_one, dir_two,
        "one bit of the address is one directory per generation"
    );

    // Each generation's log is replayed into a handle declared with THAT generation — the
    // handle's ns is its address, bit included.
    let target_one = DynamicCollection::new(
        TestStore::default(),
        u16::from_be_bytes([
            <Gen1 as Document>::NS_PREFIX[0],
            <Gen1 as Document>::NS_PREFIX[1],
        ]),
        CollectionSchema::of::<K, Gen1>(),
        Vec::new(),
    );
    let first = replay_table(&dir_one, &target_one).expect("replay");
    assert_eq!(first.segments, 1, "this directory is one generation");
    assert_eq!(first.puts, 1);

    let target_two = DynamicCollection::new(
        TestStore::default(),
        u16::from_be_bytes([
            <Gen2 as Document>::NS_PREFIX[0],
            <Gen2 as Document>::NS_PREFIX[1],
        ]),
        CollectionSchema::of::<K, Gen2>(),
        Vec::new(),
    );
    let second = replay_table(&dir_two, &target_two).expect("replay");
    assert_eq!(second.segments, 1);
    assert_eq!(second.puts, 1);

    let one = target_one
        .fetch(&K { id: 1 }.encode())
        .expect("fetch")
        .expect("the generation 1 row");
    assert_eq!(
        one.get("level"),
        Some(&Variant::U32(7)),
        "generation 1's offsets, not generation 2's: {one:?}"
    );
    assert_eq!(one.get("score"), Some(&Variant::U32(70)));

    let two = target_two
        .fetch(&K { id: 2 }.encode())
        .expect("fetch")
        .expect("the generation 2 row");
    assert_eq!(two.get("level"), Some(&Variant::U32(8)));
    assert_eq!(two.get("score"), Some(&Variant::U32(80)));
}
