//! Typed key prefixes: the derive-emitted probe builder, the lazy primary
//! prefix scan it feeds, and the lazy index prefix scan beside it.
//!
//! Locks three things the raw byte layout allows but nothing typed reached
//! before: a composite primary key answers "every child of this parent" with
//! one prefix scan; the probe names fields, so only declaration order
//! compiles; and both prefix forms are lazy — `.take` is a LIMIT, `.rev()`
//! walks the same window backwards.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_async::TestStore;
use okm_core::{Collection, DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, PartialEq, Eq, Debug)]
pub struct DeviceKey {
    pub org: u32,
    pub user: u64,
    pub device: u16,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DeviceKey)]
#[ok_ns(24)]
#[ok_index(by_label { fields(label) })]
#[ok_index(by_tag_label { fields(tag, label) })]
pub struct Device {
    pub tag: u16,
    pub label: u32,
}

/// 7 devices: org 1 holds 5 (users 10 and 11), org 2 holds 2.
fn seeded() -> Collection<TestStore, DeviceKey, Device> {
    let col: Collection<_, DeviceKey, Device> = Collection::new(TestStore::default());
    let mut label = 0u32;
    for (org, user, count) in [(1u32, 10u64, 3u16), (1, 11, 2), (2, 10, 2)] {
        for device in 0..count {
            col.put(
                &DeviceKey { org, user, device },
                &Device {
                    tag: (label % 3) as u16,
                    label,
                },
            )
            .unwrap();
            label += 1;
        }
    }
    col
}

#[test]
fn prefix_scan_narrows_by_leading_fields() {
    let col = seeded();

    // The empty probe constrains nothing: the whole table, in key order.
    let all: Vec<DeviceKey> = col
        .scan_keys(&DeviceKey::prefix().finish())
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(all.len(), 7);
    for w in all.windows(2) {
        assert!(w[0].encode() < w[1].encode(), "key order");
    }

    // One field: every device of one org.
    let probe = DeviceKey::prefix().org(1).finish();
    assert_eq!(probe.taken(), 4);
    let org1: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    assert_eq!(org1.len(), 5);
    assert!(org1.iter().all(|k| k.org == 1));

    // Two fields: every device of one user.
    let probe = DeviceKey::prefix().org(1).user(10).finish();
    assert_eq!(probe.taken(), 12);
    let user10: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    assert_eq!(user10.len(), 3);
    assert!(user10.iter().all(|k| k.org == 1 && k.user == 10));

    // All three: one full key — the probe is complete, and the caller gets
    // the whole key back (a primary entry carries it, no PrefixKey needed).
    let probe = DeviceKey::prefix().org(1).user(10).device(2).finish();
    assert_eq!(probe.taken(), DeviceKey::KEY_LEN);
    let full: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    assert_eq!(
        full,
        vec![DeviceKey {
            org: 1,
            user: 10,
            device: 2
        }]
    );

    // A prefix nothing carries is empty, not an error.
    let probe = DeviceKey::prefix().org(9).finish();
    assert_eq!(col.scan_keys(&probe).count(), 0);
}

#[test]
fn prefix_scan_is_lazy_reversible_and_pageable() {
    let col = seeded();
    let probe = DeviceKey::prefix().org(1).finish();

    // LIMIT: the iterator stops pulling.
    let head: Vec<DeviceKey> = col.scan_keys(&probe).take(2).map(|r| r.unwrap()).collect();
    assert_eq!(head.len(), 2);

    // OFFSET page.
    let page: Vec<DeviceKey> = col
        .scan_keys(&probe)
        .skip(1)
        .take(1)
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(page, vec![head[1].clone()]);

    // Reverse walks the same window backwards.
    let fwd: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    let mut rev: Vec<DeviceKey> = col.scan_keys(&probe).rev().map(|r| r.unwrap()).collect();
    rev.reverse();
    assert_eq!(fwd, rev);

    // The empty probe streams the same keys the eager enumeration returns.
    let lazy: Vec<DeviceKey> = col
        .scan_keys(&okm_core::PrefixProbe::empty())
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        lazy,
        col.scan_keys(&okm_core::PrefixProbe::empty())
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn document_mode_reaches_both_slots_under_a_prefix() {
    use okm_value::DynamicValue;

    let col = seeded();
    // A document that exists ONLY as its dynamic segment (ADR-0035), inside
    // org 1: invisible to the row mode, present in the document mode.
    let seg_only = DeviceKey {
        org: 1,
        user: 10,
        device: 9,
    };
    let mut patch = std::collections::BTreeMap::new();
    patch.insert("note".to_string(), DynamicValue::Str("no row".into()));
    col.merge(&seg_only, &patch).unwrap();

    let probe = DeviceKey::prefix().org(1).finish();
    let rows: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    assert_eq!(
        rows.len(),
        5,
        "row mode: the segment-only document is not here"
    );

    let docs = col.scan_document_keys(&probe).unwrap();
    assert_eq!(docs.len(), 6, "document mode: the union under the prefix");
    assert!(docs.contains(&seg_only));

    // The prefix still bounds the document mode: org 2 is out of the window.
    let probe = DeviceKey::prefix().org(2).finish();
    assert_eq!(col.scan_document_keys(&probe).unwrap().len(), 2);
}

#[test]
fn index_prefix_iter_matches_the_eager_scan() {
    let col = seeded();

    let eager: Vec<DeviceKey> = col
        .scan::<__OkmIndex_Device_by_label>(&3u32.to_be_bytes())
        .unwrap()
        .into_iter()
        .map(|(_, pk, _doc)| pk.decoded)
        .collect();

    let lazy: Vec<DeviceKey> = col
        .scan_prefix_iter::<__OkmIndex_Device_by_label>(&3u32.to_be_bytes())
        .map(|r| r.unwrap())
        .map(|(_, pk, _doc)| pk.decoded)
        .collect();

    assert_eq!(eager, lazy);
    assert_eq!(eager.len(), 1);

    // A partial prefix over the field segment is the streaming group read.
    let mut group: Vec<DeviceKey> = col
        .scan_prefix_iter::<__OkmIndex_Device_by_label>(&[])
        .map(|r| r.unwrap())
        .map(|(_, pk, _doc)| pk.decoded)
        .collect();
    assert_eq!(group.len(), 7);
    group.dedup();
    assert_eq!(group.len(), 7);

    // Reverse is the mirror of the same window.
    let mut rev: Vec<DeviceKey> = col
        .scan_prefix_iter::<__OkmIndex_Device_by_label>(&[])
        .rev()
        .map(|r| r.unwrap())
        .map(|(_, pk, _doc)| pk.decoded)
        .collect();
    rev.reverse();
    assert_eq!(rev, group);
}

#[test]
fn index_prefix_probe_is_typed_and_byte_identical() {
    let col = seeded(); // tags = label % 3 over labels 0..6

    // Depth 1: the builder emits exactly the bytes a caller would hand-encode,
    // so the typed door and the byte door are one window.
    let probe = __OkmIndex_Device_by_tag_label::prefix().tag(1u16).finish();
    assert_eq!(probe.as_bytes(), 1u16.to_be_bytes());

    let typed: Vec<DeviceKey> = col
        .scan::<__OkmIndex_Device_by_tag_label>(&probe)
        .unwrap()
        .into_iter()
        .map(|(_, pk, _)| pk.decoded)
        .collect();
    let raw: Vec<DeviceKey> = col
        .scan::<__OkmIndex_Device_by_tag_label>(&1u16.to_be_bytes()[..])
        .unwrap()
        .into_iter()
        .map(|(_, pk, _)| pk.decoded)
        .collect();
    assert_eq!(typed, raw);
    assert_eq!(typed.len(), 2, "labels 1 and 4 carry tag 1");

    // Depth 2 closes the segment: one device, decoded from the entry.
    let probe = __OkmIndex_Device_by_tag_label::prefix()
        .tag(1u16)
        .label(1u32)
        .finish();
    let one = col.scan::<__OkmIndex_Device_by_tag_label>(&probe).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        one[0].1.decoded,
        DeviceKey {
            org: 1,
            user: 10,
            device: 1
        }
    );

    // The empty probe is the whole access method.
    let all = col
        .scan::<__OkmIndex_Device_by_tag_label>(&__OkmIndex_Device_by_tag_label::prefix().finish())
        .unwrap();
    assert_eq!(all.len(), 7);

    // The lazy twin takes the same probe, and reverses.
    let lazy: Vec<DeviceKey> = col
        .scan_prefix_iter::<__OkmIndex_Device_by_tag_label>(
            &__OkmIndex_Device_by_tag_label::prefix().tag(1u16).finish(),
        )
        .map(|r| r.unwrap())
        .map(|(_, pk, _)| pk.decoded)
        .collect();
    assert_eq!(lazy, typed);
}

#[cfg(feature = "fjall")]
#[test]
fn prefix_scan_reverses_on_a_native_backend() {
    let col: Collection<_, DeviceKey, Device> = Collection::new(TestStore::fjall_tmp());
    for device in 0..20u16 {
        col.put(
            &DeviceKey {
                org: 1,
                user: 7,
                device,
            },
            &Device {
                tag: 0,
                label: device as u32,
            },
        )
        .unwrap();
    }
    let probe = DeviceKey::prefix().org(1).user(7).finish();
    let fwd: Vec<DeviceKey> = col.scan_keys(&probe).map(|r| r.unwrap()).collect();
    assert_eq!(fwd.len(), 20);
    // fjall iterates backwards natively — no buffer fallback, same answer.
    let back: Vec<DeviceKey> = col.scan_keys(&probe).rev().map(|r| r.unwrap()).collect();
    let mut back = back;
    back.reverse();
    assert_eq!(fwd, back);

    // `.take` on a prefix window pays only the reads performed.
    assert_eq!(col.scan_keys(&probe).take(3).count(), 3);
}
