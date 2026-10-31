#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! What belongs to the log, what belongs to the part, and what belongs to a block.
//!
//! `log.meta` pins three things and refuses an open that disagrees (`precision`,
//! `bucket_seconds`, `partition_tag` — they decide the directory layout, so a reader cannot
//! guess). Everything else in `Config` is a writer setting, and these tests hold the line:
//! a block records its own codec, so codec and block size may differ between opens, and a
//! part carries its dictionary BESIDE it, so the file decides whether codes are in play —
//! not the config the next caller happens to pass.

use okm_series::{Body, Codec, Config, DictConfig, Error, Log, Precision, Value};

fn records() -> Vec<Body> {
    (0..500u64)
        .map(|index| {
            let mut body = Body::new();
            // A closed set of three strings, repeated enough times to earn a code.
            body.push(
                1,
                Value::Str(["checkout", "catalog", "search"][(index % 3) as usize].to_string()),
            );
            body.push(2, Value::U64(index));
            body
        })
        .collect()
}

#[test]
fn a_part_reads_back_its_dictionary_even_when_the_caller_asked_for_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let with_dict = dir.path().join("with");
    let without_dict = dir.path().join("without");

    let mut log = Log::open(
        &with_dict,
        Config {
            precision: Precision::Ms,
            dict: Some(DictConfig::default()),
            // Raw blocks, on both sides: zstd would collapse three repeated strings so
            // thoroughly that the code-versus-inline difference disappears into the ratio.
            block_codec: Codec::Raw,
            ..Config::default()
        },
    )
    .expect("open with a dictionary");
    for body in records() {
        log.append(&body).expect("append");
    }
    log.seal().expect("seal");
    drop(log);

    let mut plain = Log::open(
        &without_dict,
        Config {
            precision: Precision::Ms,
            dict: None,
            block_codec: Codec::Raw,
            ..Config::default()
        },
    )
    .expect("open without a dictionary");
    for body in records() {
        plain.append(&body).expect("append");
    }
    plain.seal().expect("seal");
    drop(plain);

    // The dictionary earned its place on this shape: the same records take fewer bytes with
    // it, which is only possible if strings went in as codes.
    let coded = part_bytes(&with_dict);
    let inline = part_bytes(&without_dict);
    assert!(
        coded < inline,
        "the dictionary has to actually code something: {coded} vs {inline} bytes"
    );

    // Now the point: reopen the coded log with `dict: None`, which says nothing about
    // whether the FILE has a dictionary, and read it anyway.
    let reader = Log::open(
        &with_dict,
        Config {
            precision: Precision::Ms,
            dict: None,
            ..Config::default()
        },
    )
    .expect("reopen without a dictionary");
    let rows = reader.read(0, 1_000).expect("read a coded part");
    assert_eq!(rows.len(), 500);
    for (index, (_, _, body)) in rows.iter().enumerate() {
        let expected = ["checkout", "catalog", "search"][index % 3];
        assert_eq!(
            body.get(1),
            Some(&Value::Str(expected.to_string())),
            "record {index} decoded from a code the caller never asked to load"
        );
        assert_eq!(body.get(2), Some(&Value::U64(index as u64)));
    }
}

#[test]
fn block_settings_are_the_parts_own() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");

    let mut log = Log::open(
        &root,
        Config {
            precision: Precision::Ms,
            block_bytes: 4 * 1024,
            block_codec: Codec::Zstd,
            zstd_level: 3,
            ..Config::default()
        },
    )
    .expect("open with small zstd blocks");
    for body in records() {
        log.append(&body).expect("append");
    }
    log.flush().expect("flush");
    drop(log);

    // A different codec, level and block size. `log.meta` does not pin these because a
    // block header carries its own — so this open must succeed AND read the old blocks.
    let mut reader = Log::open(
        &root,
        Config {
            precision: Precision::Ms,
            block_bytes: 64 * 1024,
            block_codec: Codec::Raw,
            zstd_level: 1,
            ..Config::default()
        },
    )
    .expect("a block setting is not a log setting");
    let rows = reader
        .read(0, 10_000)
        .expect("read blocks written by another config");
    assert_eq!(rows.len(), 500, "every record, whatever codec wrote it");

    // And the new settings take effect for what comes next, which is the whole point of
    // them being per block.
    let mut tail = Body::new();
    tail.push(2, Value::U64(9_999));
    reader.append(&tail).expect("append under the new settings");
    reader.flush().expect("flush");
    assert_eq!(reader.read(0, 10_000).expect("read again").len(), 501);
}

#[test]
fn the_pinned_trio_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");
    let _log = Log::open(
        &root,
        Config {
            precision: Precision::Ms,
            bucket_seconds: 60,
            partition_tag: Some(9),
            ..Config::default()
        },
    )
    .expect("open");

    // Each disagreement names the field, because a caller reading the error has to know
    // which of the three it got wrong. `precision` first: it is checked first.
    for (config, field) in [
        (
            Config {
                precision: Precision::S,
                bucket_seconds: 60,
                partition_tag: Some(9),
                ..Config::default()
            },
            "precision",
        ),
        (
            Config {
                precision: Precision::Ms,
                bucket_seconds: 3600,
                partition_tag: Some(9),
                ..Config::default()
            },
            "bucket",
        ),
        (
            Config {
                precision: Precision::Ms,
                bucket_seconds: 60,
                partition_tag: None,
                ..Config::default()
            },
            "partition tag",
        ),
    ] {
        match Log::open(&root, config) {
            Err(Error::ConfigMismatch(what)) => assert!(
                what.contains(field),
                "the refusal must name {field}: {what}"
            ),
            other => panic!("{field} drift must be refused, got {other:?}"),
        }
    }
}

/// The bytes of the one sealed part under a root — the comparison the dictionary test makes.
fn part_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0;
    for bucket in std::fs::read_dir(root).expect("read root").flatten() {
        if !bucket.path().is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(bucket.path())
            .expect("read bucket")
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("part-") && name.ends_with(".log") {
                total += entry.metadata().expect("metadata").len();
            }
        }
    }
    total
}
