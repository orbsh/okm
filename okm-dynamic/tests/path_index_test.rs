//! Path-index acceptance (ADR-0029): declared paths over the schema-free
//! zone, maintained by the dynamic-segment write path.
//!
//! What this locks:
//!
//! - a declared path resolves nested content and produces one entry per
//!   resolved value (`author.name`, `meta.rank`);
//! - an array path fans out (one entry per element: `tags`) and a pinned
//!   index addresses exactly one element (`refs[0].href`);
//! - a missing path and a value whose variant does not match the
//!   declared type produce NO entry and no error;
//! - overwrite sweeps the stale entries, `replace` without dynamic names removes them;
//! - the entry key is `[ns 2B][slot 2B][value][pkey]` — the path itself
//!   never rides the key (the slot is its identity).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;

use okm_core::schema::CollectionSchema;
use okm_core::{DocumentEncode, KeyEncode, TestStore, VirtualStorage};
use okm_dynamic::{AccessMethod, DynamicCollection, FieldType, Path, Value, ValueMap};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct NodeKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(NodeKey)]
#[ok_ns(51)]
pub struct Node {
    pub kind: u32,
}

fn schema() -> CollectionSchema {
    CollectionSchema::of::<NodeKey, Node>()
}

fn key(id: u64) -> Vec<u8> {
    NodeKey { id }.encode()
}

const SLOT_AUTHOR: u16 = 0x3001;
const SLOT_TAGS: u16 = 0x3002;
const SLOT_RANK: u16 = 0x1003;
const SLOT_PINNED: u16 = 0x1004;

fn table(store: TestStore) -> DynamicCollection<TestStore> {
    DynamicCollection::new(
        store,
        51,
        schema(),
        vec![
            AccessMethod::path(
                SLOT_AUTHOR,
                Path::parse("author.name").unwrap(),
                FieldType::Str,
            )
            .unwrap(),
            AccessMethod::path(SLOT_TAGS, Path::parse("tags").unwrap(), FieldType::Str).unwrap(),
            AccessMethod::path(SLOT_RANK, Path::parse("meta.rank").unwrap(), FieldType::U32)
                .unwrap(),
            AccessMethod::path(
                SLOT_PINNED,
                Path::parse("refs[0].href").unwrap(),
                FieldType::Str,
            )
            .unwrap(),
        ],
    )
}

/// The four path indexes are declared on the same table as the primary
/// document: `kind` is the schema-declared field, everything else lives
/// in the dynamic segment.
fn declared(kind: u32) -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("kind".into(), Value::U32(kind));
    m
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Obj(m)
}

fn fields(author: Option<&str>, tags: &[&str], rank: Option<Value>, refs: &[&str]) -> ValueMap {
    let mut m = BTreeMap::new();
    if let Some(a) = author {
        m.insert("author".into(), obj(&[("name", Value::Str(a.into()))]));
    }
    m.insert(
        "tags".into(),
        Value::Array(tags.iter().map(|t| Value::Str((*t).into())).collect()),
    );
    if let Some(r) = rank {
        m.insert("meta".into(), obj(&[("rank", r)]));
    }
    m.insert(
        "refs".into(),
        Value::Array(
            refs.iter()
                .map(|h| obj(&[("href", Value::Str((*h).into()))]))
                .collect(),
        ),
    );
    m
}

/// Primary keys (the `id` field of the decoded key) a scan returns.
fn ids(hits: Vec<ValueMap>) -> Vec<u64> {
    let mut out: Vec<u64> = hits
        .iter()
        .map(|k| match k.get("id") {
            Some(Value::U64(v)) => *v,
            other => panic!("unexpected key decode: {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

/// Every stored key under one index slot (`[ns][slot]` prefix).
fn entry_keys<S: VirtualStorage>(store: &S, slot: u16) -> Vec<Vec<u8>> {
    let mut prefix: Vec<u8> = 51u16.to_be_bytes().to_vec();
    prefix.extend_from_slice(&slot.to_be_bytes());
    store
        .scan_suffix(&prefix)
        .expect("a scan on a working store")
        .into_iter()
        .map(|suffix| {
            let mut full = prefix.clone();
            full.extend_from_slice(&suffix);
            full
        })
        .collect()
}

#[test]
fn nested_and_top_level_paths_resolve_to_entries() {
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(
        &key(1),
        &fields(Some("alice"), &["rust"], Some(Value::U32(4)), &[]),
    )
    .unwrap();
    t.merge(&key(2), &declared(7)).unwrap();
    t.merge(
        &key(2),
        &fields(Some("bob"), &["rust", "db"], Some(Value::U32(9)), &[]),
    )
    .unwrap();

    // Nested field: `author.name`.
    assert_eq!(ids(t.scan(SLOT_AUTHOR, b"alice").unwrap()), vec![1]);
    assert_eq!(ids(t.scan(SLOT_AUTHOR, b"bob").unwrap()), vec![2]);
    assert!(t.scan(SLOT_AUTHOR, b"carol").unwrap().is_empty());

    // Nested fixed-width field: `meta.rank`, byte order == value order.
    assert_eq!(
        ids(t.scan(SLOT_RANK, &4u32.to_be_bytes()).unwrap()),
        vec![1]
    );
    assert_eq!(
        ids(t.scan(SLOT_RANK, &9u32.to_be_bytes()).unwrap()),
        vec![2]
    );

    // The empty prefix is legal for a path index (its segment width is
    // the caller's business, exactly like a func index).
    assert_eq!(t.scan(SLOT_RANK, &[]).unwrap().len(), 2);
}

#[test]
fn exact_scan_ignores_a_probe_completed_by_the_key_tail() {
    // `author.name` is a Str path index, so the value's end and the key tail
    // have no boundary byte between them: id 0x3100…0001 encodes to a key
    // whose first byte is 0x31 ('1'), which lets the probe "ad1" match the
    // stored value "ad" in a raw prefix scan. Values mix letters and digits,
    // so that data is legal — the scan's length test is what drops it.
    let t = table(TestStore::slatedb_mem());
    let trap = 0x3100_0000_0000_0001;
    for (id, name) in [(trap, "ad"), (2, "ad1"), (3, "ad1_to_cart")] {
        t.merge(&key(id), &declared(7)).unwrap();
        t.merge(&key(id), &fields(Some(name), &[], None, &[]))
            .unwrap();
    }

    // The trap is real: the entry's bytes start with the probe's prefix.
    let mut probe: Vec<u8> = 51u16.to_be_bytes().to_vec();
    probe.extend_from_slice(&SLOT_AUTHOR.to_be_bytes());
    probe.extend_from_slice(b"ad1");
    let keys = entry_keys(t.store(), SLOT_AUTHOR);
    let trapped = keys
        .iter()
        .find(|k| k.ends_with(&key(trap)))
        .expect("the trap entry exists");
    assert!(trapped.starts_with(&probe));
    assert_eq!(&trapped[4..], b"ad\x31\x00\x00\x00\x00\x00\x00\x01");

    // Prefix: only values that START with "ad1" (the trap is dropped).
    assert_eq!(ids(t.scan(SLOT_AUTHOR, b"ad1").unwrap()), vec![2, 3]);
    // Exact: only the value that IS "ad1" — no fetch-back validation.
    assert_eq!(ids(t.scan_exact(SLOT_AUTHOR, b"ad1").unwrap()), vec![2]);
    // The short value keeps working: prefix sees all three, exact sees one.
    assert_eq!(ids(t.scan(SLOT_AUTHOR, b"ad").unwrap()), vec![2, 3, trap]);
    assert_eq!(ids(t.scan_exact(SLOT_AUTHOR, b"ad").unwrap()), vec![trap]);
}

#[test]
fn array_path_fans_out_and_pinned_index_selects_one() {
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(
        &key(1),
        &fields(None, &["rust", "db"], None, &["first", "second"]),
    )
    .unwrap();

    // Wildcard: the ABSENCE of an Index component fans out over elements.
    assert_eq!(entry_keys(t.store(), SLOT_TAGS).len(), 2);
    assert_eq!(ids(t.scan(SLOT_TAGS, b"rust").unwrap()), vec![1]);
    assert_eq!(ids(t.scan(SLOT_TAGS, b"db").unwrap()), vec![1]);

    // Pinned: exactly one element, whatever the array holds.
    assert_eq!(entry_keys(t.store(), SLOT_PINNED).len(), 1);
    assert_eq!(ids(t.scan(SLOT_PINNED, b"first").unwrap()), vec![1]);
    assert!(t.scan(SLOT_PINNED, b"second").unwrap().is_empty());
}

#[test]
fn missing_path_and_type_mismatch_produce_no_entry() {
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    // No `author`, and `meta.rank` carries a Str where the declaration
    // says U32 — both are empty results, never errors.
    t.merge(
        &key(1),
        &fields(None, &["rust"], Some(Value::Str("high".into())), &[]),
    )
    .unwrap();

    assert!(entry_keys(t.store(), SLOT_AUTHOR).is_empty());
    assert!(entry_keys(t.store(), SLOT_RANK).is_empty());
    assert_eq!(entry_keys(t.store(), SLOT_TAGS).len(), 1);
    assert!(t.scan(SLOT_AUTHOR, b"").unwrap().is_empty());
}

#[test]
fn entry_layout_is_ns_slot_value_pkey() {
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(3), &declared(7)).unwrap();
    t.merge(
        &key(3),
        &fields(Some("alice"), &[], Some(Value::U32(4)), &[]),
    )
    .unwrap();

    let mut expected: Vec<u8> = 51u16.to_be_bytes().to_vec();
    expected.extend_from_slice(&SLOT_RANK.to_be_bytes());
    expected.extend_from_slice(&4u32.to_be_bytes());
    expected.extend_from_slice(&key(3));
    assert_eq!(entry_keys(t.store(), SLOT_RANK), vec![expected]);
}

#[test]
fn overwrite_sweeps_stale_entries() {
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(
        &key(1),
        &fields(Some("alice"), &["old"], Some(Value::U32(1)), &[]),
    )
    .unwrap();
    t.merge(
        &key(1),
        &fields(Some("bob"), &["new"], Some(Value::U32(2)), &[]),
    )
    .unwrap();

    assert!(t.scan(SLOT_AUTHOR, b"alice").unwrap().is_empty());
    assert_eq!(ids(t.scan(SLOT_AUTHOR, b"bob").unwrap()), vec![1]);
    assert!(t.scan(SLOT_TAGS, b"old").unwrap().is_empty());
    assert_eq!(ids(t.scan(SLOT_TAGS, b"new").unwrap()), vec![1]);
    assert!(t.scan(SLOT_RANK, &1u32.to_be_bytes()).unwrap().is_empty());
    assert_eq!(
        ids(t.scan(SLOT_RANK, &2u32.to_be_bytes()).unwrap()),
        vec![1]
    );
    // One document, one entry per declared path (no accumulation).
    assert_eq!(entry_keys(t.store(), SLOT_AUTHOR).len(), 1);
    assert_eq!(entry_keys(t.store(), SLOT_RANK).len(), 1);
}

#[test]
fn entry_bytes_depend_on_the_stored_number_not_the_writer_s_variant() {
    // The dynamic segment stores an unsigned value as a WIDTH-LESS
    // `UInt` frame (minimal-length BE — the width is dropped), so a
    // read-back map always carries `U64`. Entry bytes must therefore be
    // a function of (declared type, stored number), never of the
    // variant the writer happened to hand in: the overwrite sweep
    // recomputes keys from the read-back map, and a strict variant match
    // would make that recompute land on a DIFFERENT key set than the
    // write built, silently leaving the old entry behind.
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(&key(1), &fields(None, &[], Some(Value::U32(4)), &[]))
        .unwrap();
    t.merge(&key(2), &declared(7)).unwrap();
    t.merge(&key(2), &fields(None, &[], Some(Value::U64(4)), &[]))
        .unwrap();

    // Same probe finds both documents.
    assert_eq!(
        ids(t.scan(SLOT_RANK, &4u32.to_be_bytes()).unwrap()),
        vec![1, 2]
    );

    // …because the indexed segment is byte-identical: same declared
    // width (4 bytes), same BE payload. Strip the pkey tail (8 bytes).
    let keys = entry_keys(t.store(), SLOT_RANK);
    assert_eq!(keys.len(), 2);
    let segments: Vec<Vec<u8>> = keys.iter().map(|k| k[..k.len() - 8].to_vec()).collect();
    assert_eq!(segments[0], segments[1]);
    let mut expected: Vec<u8> = 51u16.to_be_bytes().to_vec();
    expected.extend_from_slice(&SLOT_RANK.to_be_bytes());
    expected.extend_from_slice(&4u32.to_be_bytes());
    assert_eq!(segments[0], expected);
}

#[test]
fn out_of_range_and_non_numeric_values_produce_no_entry() {
    let t = DynamicCollection::new(
        TestStore::slatedb_mem(),
        51,
        schema(),
        vec![
            AccessMethod::path(0x1005, Path::parse("meta.small").unwrap(), FieldType::U8).unwrap(),
        ],
    );
    let mut one = BTreeMap::new();
    one.insert("meta".into(), obj(&[("small", Value::U64(300))]));
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(&key(1), &one).unwrap();
    // 300 does not fit the declared U8 → no entry, no error.
    assert!(entry_keys(t.store(), 0x1005).is_empty());

    let mut fits = BTreeMap::new();
    fits.insert("meta".into(), obj(&[("small", Value::U64(200))]));
    t.merge(&key(1), &fits).unwrap();
    assert_eq!(ids(t.scan(0x1005, &[200u8]).unwrap()), vec![1]);

    let mut wrong_kind = BTreeMap::new();
    wrong_kind.insert("meta".into(), obj(&[("small", Value::Str("big".into()))]));
    t.merge(&key(1), &wrong_kind).unwrap();
    assert!(entry_keys(t.store(), 0x1005).is_empty());
}

#[test]
fn replace_with_the_declared_map_drops_the_entries_and_keeps_the_document() {
    // The segment is no longer independently deletable (ADR-0035 retired the
    // slot-1 verbs): a whole-document `replace` whose map carries no dynamic
    // name empties it, and the path entries ride the sweep.
    let t = table(TestStore::slatedb_mem());
    t.merge(&key(1), &declared(7)).unwrap();
    t.merge(
        &key(1),
        &fields(Some("alice"), &["rust"], Some(Value::U32(4)), &[]),
    )
    .unwrap();
    t.replace(&key(1), &declared(7)).unwrap();

    for slot in [SLOT_AUTHOR, SLOT_TAGS, SLOT_RANK] {
        assert!(
            entry_keys(t.store(), slot).is_empty(),
            "slot {slot} not swept"
        );
    }
    assert!(t.scan(SLOT_AUTHOR, b"").unwrap().is_empty());
    // The declared document survives the replacement.
    assert_eq!(
        t.fetch(&key(1)).unwrap().unwrap().get("kind"),
        Some(&Value::U32(7))
    );
}
