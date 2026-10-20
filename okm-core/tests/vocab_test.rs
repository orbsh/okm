//! `Vocab<N>` end to end (ADR-0032): labels in memory, an N-byte id on the
//! wire, one data vocabulary per declared field.

use okm_core::{Collection, Document, DocumentEncode, FieldType, KeyEncode, TestStore, Vocab};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct VKey {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(VKey)]
#[ok_ns(11)]
pub struct VRow {
    /// One byte of id: up to 256 labels.
    pub status: Vocab<u8>,
    /// Two bytes: the same label mechanism with a wider id space.
    pub color: Vocab<u16>,
    pub n: u32,
}

fn row(status: &str, color: &str, n: u32) -> VRow {
    VRow {
        status: Vocab::of(status),
        color: Vocab::of(color),
        n,
    }
}

#[test]
fn labels_round_trip_through_per_field_vocabularies() {
    let store = TestStore::slatedb_mem();
    let mut c = Collection::<_, VKey, VRow>::new(store);

    c.put(&VKey { org: 1, id: 1 }, &row("active", "red", 7));
    c.put(&VKey { org: 1, id: 2 }, &row("closed", "red", 8));

    let a = c.get(&VKey { org: 1, id: 1 }).unwrap();
    assert_eq!(a.status.as_str(), Some("active"));
    assert_eq!(a.color.as_str(), Some("red"));
    assert_eq!(a.n, 7);
    // Per-FIELD id spaces: each field's first label is its id 0.
    assert_eq!(a.status.id(), Some(0u8));
    assert_eq!(a.color.id(), Some(0u16));

    let b = c.get(&VKey { org: 1, id: 2 }).unwrap();
    assert_eq!(b.status.as_str(), Some("closed"));
    assert_eq!(b.status.id(), Some(1u8), "second label in the status field");
    assert_eq!(
        b.color.id(),
        Some(0u16),
        "red was already the color field's id 0"
    );

    // The map bridge lifts the label, not the id.
    let m = a.to_map();
    assert!(
        matches!(
            m.get("status"),
            Some(okm_core::obj_dynamic::DynamicValue::Str(s)) if s == "active"
        ),
        "status lifted as a label: {:?}",
        m.get("status")
    );

    // A label the store never saw interns on write, after a from_map round trip.
    let mut d = VRow::from_map(&m);
    d.status = Vocab::of("suspended");
    c.put(&VKey { org: 1, id: 3 }, &d);
    let e = c.get(&VKey { org: 1, id: 3 }).unwrap();
    assert_eq!(e.status.as_str(), Some("suspended"));
    assert_eq!(e.status.id(), Some(2u8));
    assert_eq!(
        e.color.as_str(),
        Some("red"),
        "untouched field keeps its label"
    );

    // Each field's vocabulary is its own face pair in counting families 1/2, and
    // the schema carries the id face so a dynamic reader can resolve labels.
    let faces: Vec<(u16, usize)> = VRow::FIELDS
        .iter()
        .filter_map(|f| match f.ty {
            FieldType::Vocab(slot) => Some((slot, f.width)),
            _ => None,
        })
        .collect();
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[0].0 >> 12, 2, "id → label lives in family 2");
    assert_eq!(faces[1].0 >> 12, 2);
    assert_ne!(faces[0].0, faces[1].0, "one vocabulary per field");
    assert_eq!(
        (faces[0].1, faces[1].1),
        (1, 2),
        "the declaration picked the id widths"
    );
}