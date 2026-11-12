//! The binary end to end on a local lake: the drain writes, `status` reads, `condense`
//! folds, `vacuum` reclaims — every verb given its lake as a URL, no library call in between.
//!
//! The bucket form is not exercised here (a checkout has no bucket): what this holds is the
//! ADDRESSING, which is the same code for both stores, and `store::LakeUrl`'s unit tests
//! cover the parsing that picks the arm.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::{Collection, DocumentEncode, KeyEncode};
use std::process::{Command, Output};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(91)]
#[ok_name("cli_rows")]
pub struct R {
    pub level: u32,
}

const K1: K = K { org: 1, id: 10 };
const K2: K = K { org: 1, id: 11 };

fn collection(root: &std::path::Path) -> Collection<ChangelogStore<TestStore>, K, R> {
    Collection::captured(ChangelogStore::new(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root),
    ))
    .expect("the log opens")
}

fn write(root: &std::path::Path, key: &K, level: u32) {
    let c = collection(root);
    c.put(key, &R { level }).expect("put");
    // Drop seals the segment: a clean exit is what makes the window drainable.
}

fn okm_lake(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_okm-lake"))
        .args(args)
        .output()
        .expect("the binary runs")
}

fn run(args: &[&str]) -> String {
    let out = okm_lake(args);
    assert!(
        out.status.success(),
        "okm-lake {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn every_verb_takes_a_lake_url_and_they_agree_on_what_is_there() {
    let root = tempfile::tempdir().expect("root");
    let lake = tempfile::tempdir().expect("lake");
    let root_arg = root.path().display().to_string();
    let lake_arg = lake.path().display().to_string();

    // Window 1: one row. Window 2: the SAME key again, so the two files share a key and the
    // fold has something to resolve.
    write(root.path(), &K1, 1);
    let first = run(&["--root", &root_arg, "--out", &lake_arg]);
    assert_eq!(
        first.lines().filter(|l| l.ends_with(".parquet")).count(),
        1,
        "one window, one file: {first}"
    );
    write(root.path(), &K1, 2);
    write(root.path(), &K2, 3);
    let second = run(&["--root", &root_arg, "--out", &lake_arg]);
    let key = second
        .lines()
        .find(|l| l.ends_with(".parquet"))
        .expect("the window's key")
        .to_string();
    assert!(
        key.contains("/data/"),
        "a key inside the table's data/: {key}"
    );
    assert!(
        lake.path().join(&key).is_file(),
        "the key a bucket would store is a path under a local root: {key}"
    );

    // `status`, through the same URL spelled as a `file://` url this time.
    let status = run(&["status", "--store", &format!("file://{lake_arg}"), "--rows"]);
    assert!(
        status.contains("v1: 2 live file(s)") && status.contains("current state: 2 row(s)"),
        "{status}"
    );

    // `condense` folds the two change files into one current-state file.
    let folded = run(&["condense", "--out", &lake_arg]);
    assert!(folded.contains("folded 1"), "{folded}");
    let after = run(&["status", "--out", &lake_arg]);
    assert!(
        after.contains("v2: 1 live file(s)") && after.contains("folded"),
        "{after}"
    );

    // `vacuum` reclaims what the fold replaced, and a dry run only says so.
    let dry = run(&[
        "vacuum",
        "--out",
        &lake_arg,
        "--keep-versions",
        "0",
        "--dry-run",
    ]);
    assert!(dry.contains("would delete 2 file(s)"), "{dry}");
    let swept = run(&["vacuum", "--out", &lake_arg, "--keep-versions", "0"]);
    assert!(swept.contains("deleted 2 file(s)"), "{swept}");
    // Idempotent: the log still names them, the directory does not.
    let again = run(&["vacuum", "--out", &lake_arg, "--keep-versions", "0"]);
    assert!(again.contains("deleted 0 file(s)"), "{again}");
}

#[test]
fn one_table_can_be_named_and_a_bad_lake_is_refused() {
    let root = tempfile::tempdir().expect("root");
    let lake = tempfile::tempdir().expect("lake");
    write(root.path(), &K1, 1);
    let root_arg = root.path().display().to_string();
    let lake_arg = lake.path().display().to_string();
    run(&["--root", &root_arg, "--out", &lake_arg]);

    // The table's name under the lake root IS its header in hex, which is what `--table`
    // takes for every verb.
    let table = std::fs::read_dir(lake.path())
        .expect("the lake lists")
        .next()
        .expect("one table")
        .expect("an entry")
        .file_name()
        .to_string_lossy()
        .to_string();
    let status = run(&["status", "--out", &lake_arg, "--table", &table]);
    assert!(status.starts_with(&table), "{status}");
    let other = run(&["status", "--out", &lake_arg, "--table", "ffff"]);
    assert!(
        other.is_empty(),
        "an unknown table matches nothing: {other}"
    );

    // A url that names no bucket is refused by the parser, before anything is opened.
    let out = okm_lake(&["status", "--out", "s3:///lake"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("names no bucket"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
