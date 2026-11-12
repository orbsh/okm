//! Lead faces (ADR-0054): a face whose sort segment is sliced out of the KEY's
//! own encoding, so it orders a collection by identity instead of by a payload
//! copy of it.
//!
//! Locks three things: the segment IS the key's bytes (hex), the tail decides
//! whether a scan hands back rows (`taken == KEY_LEN`) or keys to continue with,
//! and the face is maintained by the ordinary write path (`put` / `purge` /
//! `rebuild_faces`) with no new verb.

#![allow(clippy::unwrap_used, clippy::panic)]

use okm_async::TestStore;
use okm_core::engine::storage::{Batch, VirtualStorage, prefix_end};
use okm_core::parts;
use okm_core::schema::CollectionSchema;
use okm_core::{Collection, Document, DocumentEncode, KeyEncode, KvIndex, model::faces::FaceKind};

#[derive(KeyEncode, Clone, PartialEq, Eq, Debug)]
pub struct MembershipKey {
    pub channel: u64,
    pub member: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MembershipKey)]
#[ok_ns(210)]
#[ok_lead(by_member { fields(member) })]
#[ok_lead(by_member_channel { fields(member, channel), key(channel) })]
pub struct Membership {
    pub nickname: String,
    pub since_ms: u64,
}

fn key(channel: u64, member: u64) -> MembershipKey {
    MembershipKey { channel, member }
}

fn row(nickname: &str) -> Membership {
    Membership {
        nickname: nickname.to_string(),
        since_ms: 0,
    }
}

fn seeded() -> Collection<TestStore, MembershipKey, Membership> {
    let col: Collection<_, MembershipKey, Membership> = Collection::new(TestStore::default());
    for (channel, member, nickname) in
        [(1u64, 10u64, "a"), (1, 11, "b"), (2, 10, "c"), (2, 11, "d")]
    {
        col.put(&key(channel, member), &row(nickname)).unwrap();
    }
    col
}

/// Every entry key of one face, suffix only (the header is the prefix).
fn entries(col: &Collection<TestStore, MembershipKey, Membership>, slot: u16) -> Vec<Vec<u8>> {
    let prefix = [0u8, 210u8, (slot >> 8) as u8, (slot & 0xff) as u8];
    let end = prefix_end(&prefix);
    let mut out: Vec<Vec<u8>> = col
        .store()
        .scan_range_iter(&prefix, end.as_deref())
        .map(|item| item.unwrap().0[prefix.len()..].to_vec())
        .collect();
    out.sort();
    out
}

#[test]
fn slots_come_from_the_lead_family() {
    assert_eq!(
        <__OkmLead_Membership_by_member as KvIndex>::SLOT,
        0x5001,
        "the lead family starts after reduces; declaration order numbers it"
    );
    assert_eq!(
        <__OkmLead_Membership_by_member_channel as KvIndex>::SLOT,
        0x5002
    );
    // Faces are still faces: both declarations are in the fingerprint table.
    let decls = <Membership as Document>::INDEX_DECLS;
    assert_eq!(decls.len(), 2);
    assert_eq!(decls[0].slot, 0x5001);
    assert_eq!(decls[0].fields, &["member"]);
    assert!(decls[0].includes.is_empty() && decls[0].func.is_empty());
}

#[test]
fn the_entry_is_the_key_reslotted() {
    let col = seeded();
    let keys = entries(&col, 0x5001);
    assert_eq!(keys.len(), 4);
    // channel 1 / member 10, full tail: [member][channel][member].
    let mut expect = Vec::new();
    expect.extend_from_slice(&10u64.to_be_bytes());
    expect.extend_from_slice(&1u64.to_be_bytes());
    expect.extend_from_slice(&10u64.to_be_bytes());
    assert!(
        keys.contains(&expect),
        "segment is the key's own member bytes, tail is the whole key"
    );
    assert!(keys.iter().all(|k| k.len() == 24));
}

#[test]
fn a_full_tail_scan_hands_back_rows_in_identity_order() {
    let col = seeded();
    let probe = __OkmLead_Membership_by_member::prefix()
        .member(10u64)
        .finish();
    let hits = col
        .scan::<__OkmLead_Membership_by_member, (parts::Fields, parts::Key, parts::Row)>(&probe)
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(hits.len(), 2, "members 10 in channels 1 and 2");
    assert_eq!(
        hits.iter()
            .map(|(_, pk, _)| pk.decoded.channel)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "inside the equality prefix the tails ARE the key bytes, in order"
    );
    assert!(
        hits.iter()
            .all(|(_, pk, doc)| pk.taken == 16 && doc.is_some())
    );
    assert_eq!(hits[0].2.as_ref().unwrap().nickname, "a");
}

#[test]
fn a_truncated_tail_hands_back_keys_to_continue_with() {
    let col = seeded();
    let probe = __OkmLead_Membership_by_member_channel::prefix()
        .member(10u64)
        .finish();
    let hits = col
        .scan::<__OkmLead_Membership_by_member_channel, (parts::Fields, parts::Key, parts::Row)>(
            &probe,
        )
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter()
            .all(|(_, pk, doc)| pk.taken == 8 && doc.is_none()),
        "key(channel): the tail is one identity, so there is no row to fetch back"
    );
    // The prefix fields past the boundary are garbage by contract (ADR-0005);
    // only the channel — the field the tail carries — is trustworthy.
    assert_eq!(
        hits.iter()
            .map(|(_, pk, _)| pk.decoded.channel)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn purge_and_rebuild_maintain_the_face() {
    let col = seeded();
    col.purge(&key(1, 10)).unwrap();
    assert_eq!(entries(&col, 0x5001).len(), 3, "purge takes the entry away");

    let report = col.rebuild_faces(64).unwrap();
    assert_eq!(report.entries_deleted, 3 + 3, "both faces swept first");
    assert_eq!(
        report.entries_written,
        3 + 3,
        "and re-derived from the rows"
    );
    assert_eq!(entries(&col, 0x5001).len(), 3);

    let probe = __OkmLead_Membership_by_member::prefix()
        .member(11u64)
        .finish();
    let hits = col
        .scan::<__OkmLead_Membership_by_member, (parts::Fields, parts::Key, parts::Row)>(&probe)
        .collect::<okm_core::error::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(hits.len(), 2, "member 11 is still in both channels");
}

/// Primary-table keys, the same way the collection finds them.
fn primary_keys(col: &Collection<TestStore, MembershipKey, Membership>) -> Vec<Vec<u8>> {
    let prefix = [0u8, 210u8, 0x00, 0x00];
    let end = prefix_end(&prefix);
    col.store()
        .scan_range_iter(&prefix, end.as_deref())
        .map(|item| item.unwrap().0)
        .collect()
}

#[test]
fn the_export_says_which_kind_a_face_is() {
    let schema = CollectionSchema::of::<MembershipKey, Membership>();
    assert_eq!(schema.indexes.len(), 2);
    assert_eq!(schema.indexes[0].kind(), Some(FaceKind::Lead));
    assert_eq!(schema.indexes[1].kind(), Some(FaceKind::Lead));
    assert_eq!(
        schema.slots.declared_lead_base, 0x5001,
        "the export names the family base, so a host never hardcodes it"
    );
    assert_eq!(
        schema.indexes[0].fields,
        vec!["member".to_string()],
        "a lead face's `fields` are KEY fields — the kind is what says so"
    );
}

#[test]
fn the_key_only_half_is_the_same_half_the_write_path_uses() {
    let ns = [0u8, 210u8];
    let k = key(1, 10);
    let key_only = Membership::key_entries(&k, &ns);
    assert_eq!(key_only.len(), 2, "both declared faces are payload-free");

    let full = Membership::index_entries(&k, &row("a"), &ns);
    assert_eq!(
        full, key_only,
        "with no payload face declared, index_entries IS key_entries — one source"
    );
}

#[test]
fn a_lead_only_collection_rebuilds_without_decoding_a_row() {
    let col = seeded();
    // Overwrite every stored row with bytes that are not a payload: a decoder
    // reading `[version][hot_len u16]` would take hot_len = 0xffff and slice out
    // of bounds. A lead face never looks, so the rebuild must still work.
    let mut batch = Batch::default();
    for k in primary_keys(&col) {
        batch.put(k, vec![0xffu8; 5]);
    }
    col.store().commit_batch(batch).unwrap();

    let report = col.rebuild_faces(64).unwrap();
    assert_eq!(report.documents, 4, "the rows are found by key alone");
    assert_eq!(report.entries_written, 8, "two lead faces per row");
    assert_eq!(entries(&col, 0x5001).len(), 4);
    assert_eq!(entries(&col, 0x5002).len(), 4);
}
