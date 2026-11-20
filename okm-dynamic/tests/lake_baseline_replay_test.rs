//! A baseline that is a REFERENCE (ADR-0071 §1): the state lives in the lake, the log keeps
//! the tail above the version's last segment, and a replay refuses to invent the missing half.
//!
//! The three claims this pins: the reference names `(table, version, covers)`; the segments
//! the version covers are gone from the directory; and `replay_after` applies exactly the
//! deltas that came after it, through the target's own write path.

#![cfg(feature = "replay")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::replay::{replay_after, replay_table};
use okm_core::changelog::{lake_baseline, table_dir, ChangelogConfig, ChangelogStore};
use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode};
use okm_dynamic::DynamicCollection;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(97)]
#[ok_name("lake_rows")]
pub struct LRow {
    pub title: String,
}

fn store(root: &std::path::Path) -> ChangelogStore<TestStore> {
    ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig {
            // One segment per commit: the test needs to see what pruning removed.
            segment_bytes: 1,
            ..ChangelogConfig::new(root)
        },
    )
}

#[test]
fn the_reference_names_the_version_and_a_replay_owes_only_the_tail() {
    let log = tempfile::tempdir().expect("root");
    let header = <LRow as Document>::NS_PREFIX.to_vec();
    let dir = table_dir(log.path(), &header);

    let writer = store(log.path());
    let app: Collection<_, K, LRow> = Collection::captured(writer).expect("assemble");
    for id in 0..8 {
        app.put(
            &K { id },
            &LRow {
                title: format!("row {id}"),
            },
        )
        .expect("put");
    }
    let covers: u64 = okm_core::changelog::last_sealed(&dir)
        .expect("a sealed segment to cover")
        .split('.')
        .next()
        .expect("the ordinal prefix")
        .parse()
        .expect("a number");

    let compacted = app
        .store()
        .compact_lake::<LRow>(5, covers, 0)
        .expect("compact");
    assert!(
        compacted.baseline.is_some(),
        "the reference replaces the copy"
    );
    assert!(
        compacted.freed > 0,
        "the covered segments were reclaimed: {compacted:?}"
    );

    let lake = lake_baseline(log.path(), &header)
        .expect("read the reference")
        .expect("a table compacted against a lake has one");
    assert_eq!(
        lake,
        okm_core::changelog::LakeBaseline {
            table: "lake_rows".to_string(),
            version: 5,
            covers,
        }
    );
    assert_eq!(
        std::fs::read_dir(&dir)
            .expect("the table directory")
            .filter(|e| {
                e.as_ref()
                    .map(|e| e.file_name().to_string_lossy().ends_with(".segment"))
                    .unwrap_or(false)
            })
            .count(),
        0,
        "no segment survives: the state is the lake's"
    );

    let target = DynamicCollection::new(
        TestStore::slatedb_mem(),
        u16::from_be_bytes([header[0], header[1]]),
        CollectionSchema::of::<K, LRow>(),
        Vec::new(),
    );

    let refused = replay_table(&dir, &target).expect_err("a replay cannot start from a reference");
    assert!(
        refused.contains("lake table") && refused.contains("replay_after"),
        "the message names the import and the tail: {refused}"
    );

    // What the log still owes: the writes above the covered segment.
    for id in 100..104 {
        app.put(
            &K { id },
            &LRow {
                title: format!("tail {id}"),
            },
        )
        .expect("put");
    }
    let tail = replay_after(&dir, covers, &target).expect("replay the tail");
    // Everything numbered above the coverage, and nothing below it: the rows the reference
    // covers are the lake's to hand over, not the log's.
    assert_eq!(tail.puts, 4, "every row above the coverage: {tail:?}");
    for covered in [0u64, 7] {
        assert!(
            target.fetch(&K { id: covered }.encode()).expect("fetch").is_none(),
            "row {covered} is covered: the lake hands it over, not the log"
        );
    }
    for owed in 100u64..104 {
        assert!(
            target.fetch(&K { id: owed }.encode()).expect("fetch").is_some(),
            "row {owed} is owed by the log"
        );
    }
    let landed = target
        .fetch(&K { id: 100 }.encode())
        .expect("fetch")
        .expect("the tail row landed");
    assert_eq!(
        landed.get("title"),
        Some(&okm_value::Variant::Str("tail 100".to_string())),
        "the tail landed through the write path"
    );
}
