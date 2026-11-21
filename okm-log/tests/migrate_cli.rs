//! The two verbs driven the way an operator drives them: through the binary, against a store on
//! disk (ADR-0075 §7 — "the driver is the log's subcommand").
//!
//! The unit tests over the mechanism prove the three legs; this proves the wiring an operator
//! actually touches: the header hex a table directory is named by, the schema JSON a build
//! exports, and the order in which the two verbs are run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, FjallStore, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct K {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(200)]
pub struct Gen1 {
    pub level: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_shared_ns_in_tests]
#[ok_ref(K)]
#[ok_ns(200)]
#[ok_layout(version = 2)]
pub struct Gen2 {
    pub level: u32,
    pub score: u32,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_okm-log"))
        .args(args)
        .output()
        .expect("the binary runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn an_operator_migrates_and_then_vacuums() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store_dir = dir.path().join("store");

    // The store as the old build left it: the record and the rows, at generation 1's address.
    let header = {
        let store = FjallStore::open(&store_dir, "okm").expect("open the store");
        let one: Collection<_, K, Gen1> = Collection::open(store.clone()).expect("attach");
        one.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
        one.put(&K { id: 2 }, &Gen1 { level: 8 }).expect("put");
        drop(one);
        // The binary opens the same directory, and a fjall store has one writer: let go first.
        hex(<Gen1 as Document>::NS_PREFIX)
    };

    // The next declaration, as a build exports it.
    let schema_path = dir.path().join("gen2.json");
    std::fs::write(&schema_path, CollectionSchema::of::<K, Gen2>().encode()).expect("write");

    let (ok, stdout, stderr) = run(&[
        "migrate",
        "--store",
        store_dir.to_str().expect("utf-8"),
        "--table",
        &header,
        "--to",
        schema_path.to_str().expect("utf-8"),
    ]);
    assert!(ok, "migrate failed: {stderr}");
    assert!(stdout.contains("layout v1 → v2"), "{stdout}");
    assert!(stdout.contains("2 rows copied"), "{stdout}");
    assert!(
        stdout.contains("assumed the table was quiesced"),
        "a copy with no tail says so: {stdout}"
    );

    // The new generation reads at the new address.
    {
        let store = FjallStore::open(&store_dir, "okm").expect("open the store");
        let two: Collection<_, K, Gen2> =
            Collection::open(store.clone()).expect("attach generation 2");
        assert_eq!(
            two.get_row(&K { id: 1 }).expect("read").expect("the row"),
            Gen2 { level: 7, score: 0 }
        );
        drop(two);
    }

    let (ok, stdout, stderr) = run(&[
        "vacuum",
        "--store",
        store_dir.to_str().expect("utf-8"),
        "--table",
        &header,
    ]);
    assert!(ok, "vacuum failed: {stderr}");
    assert!(stdout.contains("vacuumed layout v1"), "{stdout}");

    // And the table is whole afterwards.
    let store = FjallStore::open(&store_dir, "okm").expect("open the store");
    let two: Collection<_, K, Gen2> = Collection::open(store.clone()).expect("attach generation 2");
    assert_eq!(
        two.get_row(&K { id: 2 }).expect("read").expect("the row"),
        Gen2 { level: 8, score: 0 }
    );
}

#[test]
fn a_second_vacuum_says_there_is_nothing_to_do() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let store_dir = dir.path().join("store");

    let header = {
        let store = FjallStore::open(&store_dir, "okm").expect("open the store");
        let one: Collection<_, K, Gen1> = Collection::open(store.clone()).expect("attach");
        one.put(&K { id: 1 }, &Gen1 { level: 7 }).expect("put");
        drop(one);
        hex(<Gen1 as Document>::NS_PREFIX)
    };

    let (ok, stdout, stderr) = run(&[
        "vacuum",
        "--store",
        store_dir.to_str().expect("utf-8"),
        "--table",
        &header,
    ]);
    assert!(
        ok,
        "vacuum on a single generation is not a failure: {stderr}"
    );
    assert!(
        stdout.contains("nothing to vacuum"),
        "and it says so: {stdout}"
    );
}
