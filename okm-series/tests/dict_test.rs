#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! The dictionary: thresholds by width, a code the reader resolves, and a chain that
//! widens without ever patching a byte.

use okm_series::{Body, Config, DictConfig, Log, Precision, Value};

fn config() -> Config {
    Config {
        precision: Precision::Ms,
        bucket_seconds: 3600,
        block_bytes: 4096,
        part_bytes: 64 * 1024 * 1024,
        zstd_level: 1,
        block_codec: okm_series::Codec::Zstd,
        dict: Some(DictConfig::default()),
        partition_tag: None,
    }
}

fn one_string(text: &str) -> Body {
    let mut body = Body::new();
    body.push(1, Value::Str(text.to_string()));
    body
}

fn read_dict(log: &Log) -> Vec<u8> {
    let part = log.parts().unwrap().remove(0);
    std::fs::read(part.path.with_extension("dict")).unwrap_or_default()
}

fn dict_bytes(log: &Log) -> u64 {
    log.parts()
        .unwrap()
        .iter()
        .map(|part| {
            let path = part.path.with_extension("dict");
            std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
        })
        .sum()
}

#[test]
fn a_long_string_earns_a_code_at_its_second_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let text = "GET /v1/instances/42/events?since=1700000000000 HTTP/1.1";
    let base = 1_700_000_000_000u64;
    for step in 0..3u64 {
        log.append(base + step, &one_string(text)).unwrap();
    }
    log.append_durable(base + 3, &one_string(text)).unwrap();

    assert!(
        dict_bytes(&log) > 0,
        "a 50-byte string is past the widest tier, so it is interned"
    );
    let rows = log.read(0, 10).unwrap();
    assert_eq!(rows.len(), 4);
    for row in &rows {
        assert_eq!(row.2.get(1).unwrap().as_str().unwrap(), text);
    }
}

#[test]
fn a_string_no_longer_than_its_code_is_never_interned() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    for step in 0..200u64 {
        // Two bytes against a two-byte id: interning it could only grow the file.
        log.append(base + step, &one_string("AB")).unwrap();
    }
    log.append_durable(base + 200, &one_string("AB")).unwrap();

    assert_eq!(count_entries(&read_dict(&log)), 0, "nothing earned a code");
    let rows = log.read(0, 500).unwrap();
    assert_eq!(rows.len(), 201);
    assert_eq!(rows[0].2.get(1).unwrap().as_str().unwrap(), "AB");
}

#[test]
fn the_first_nine_copies_stay_inline_and_the_tenth_is_coded() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = Log::open(dir.path(), config()).unwrap();
    let base = 1_700_000_000_000u64;
    // Three bytes against a two-byte code lands in the `W..4W` tier: ten occurrences.
    let text = "abc";
    for step in 0..12u64 {
        log.append(base + step, &one_string(text)).unwrap();
    }
    log.append_durable(base + 12, &one_string(text)).unwrap();

    // The dictionary holds one entry, and every copy still reads back as itself.
    let rows = log.read(0, 20).unwrap();
    assert_eq!(rows.len(), 13);
    for row in &rows {
        assert_eq!(row.2.get(1).unwrap().as_str().unwrap(), text);
    }
    let part = log.parts().unwrap().remove(0);
    let dict = std::fs::read(part.path.with_extension("dict")).unwrap();
    assert!(dict.len() > 8, "the entry is on disk");
    assert_eq!(
        count_entries(&dict),
        1,
        "twelve occurrences of one string earn exactly one entry"
    );
}

#[test]
fn a_full_block_widens_the_chain_and_the_reader_still_resolves_old_codes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config.dict = Some(DictConfig {
        tiers: DictConfig::default().tiers,
        block_entries: 3,
    });
    let mut log = Log::open(dir.path(), config).unwrap();
    let base = 1_700_000_000_000u64;
    let mut texts = Vec::new();
    for index in 0..8u64 {
        let text = format!("a message long enough to earn a code, number {index:04}");
        texts.push(text.clone());
        log.append(base + index * 2, &one_string(&text)).unwrap();
        log.append(base + index * 2 + 1, &one_string(&text))
            .unwrap();
    }
    log.seal().unwrap();

    let rows = log.read(0, 100).unwrap();
    assert_eq!(rows.len(), 16);
    for row in &rows {
        let expected = &texts[row.1 as usize / 2];
        assert_eq!(row.2.get(1).unwrap().as_str().unwrap(), expected);
    }
    let part = log.parts().unwrap().remove(0);
    let dict = std::fs::read(part.path.with_extension("dict")).unwrap();
    assert!(
        count_blocks(&dict) >= 2,
        "eight strings with three slots per block need more than one block"
    );
}

#[test]
fn codes_survive_a_reopen_and_a_new_block_can_still_be_coded() {
    let dir = tempfile::tempdir().unwrap();
    let base = 1_700_000_000_000u64;
    let text = "a long enough message that the dictionary takes it on the second sight";
    {
        let mut log = Log::open(dir.path(), config()).unwrap();
        for step in 0..4u64 {
            log.append(base + step, &one_string(text)).unwrap();
        }
        log.append_durable(base + 4, &one_string(text)).unwrap();
    }

    let mut log = Log::open(dir.path(), config()).unwrap();
    let rows = log.read(0, 10).unwrap();
    assert_eq!(
        rows.len(),
        5,
        "the codes written before the reopen still resolve"
    );
    assert_eq!(rows[4].2.get(1).unwrap().as_str().unwrap(), text);

    // The adopted dictionary knows the string already, so the next record is coded
    // without a second entry.
    let offset = log.append_durable(base + 5, &one_string(text)).unwrap();
    assert_eq!(offset, 5);
    let rows = log.read(0, 10).unwrap();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[5].2.get(1).unwrap().as_str().unwrap(), text);

    let part = log.parts().unwrap().remove(0);
    let dict = std::fs::read(part.path.with_extension("dict")).unwrap();
    assert_eq!(
        count_entries(&dict),
        1,
        "one string, one entry, across a reopen"
    );
}

fn count_blocks(dict: &[u8]) -> usize {
    let mut at = 8usize;
    let mut blocks = 0;
    while at + 32 <= dict.len() {
        if &dict[at..at + 4] != b"OKMK" {
            break;
        }
        let len_c = u32::from_be_bytes(dict[at + 24..at + 28].try_into().unwrap()) as usize;
        blocks += 1;
        at += 32 + len_c;
    }
    blocks
}

fn count_entries(dict: &[u8]) -> usize {
    let mut total = 0;
    let mut at = 8usize;
    while at + 32 <= dict.len() {
        if &dict[at..at + 4] != b"OKMK" {
            break;
        }
        let count = u32::from_be_bytes(dict[at + 8..at + 12].try_into().unwrap()) as usize;
        let len_c = u32::from_be_bytes(dict[at + 24..at + 28].try_into().unwrap()) as usize;
        total += count;
        at += 32 + len_c;
    }
    total
}
