//! Deprecated index declarations (ADR-0005 slot discipline): a
//! `#[kv_index(name { ... }, deprecated)]` declaration still occupies its
//! slot (declaration order is a persistent contract — removing it would
//! shift every later slot onto stale data), but generates NO write path
//! (no marker struct, no `index_entries` contribution) and NO scan
//! surface. Its stale entries stay in the engine until
//! `Table::prune_deprecated_slots` deletes them by prefix.

use okm_core::{
    KeyEncode, MockStore, Row, RowEncode, Table, VirtualStorage,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

/// Live declaration AFTER the deprecated one — its slot must NOT shift
/// (that is the entire point of the placeholder).
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_ns(9)]
#[kv_index(by_old { fields(legacy) }, deprecated)]
#[kv_index(by_level { fields(level) })]
pub struct User {
    pub legacy: u32,
    pub level: u32,
}

#[test]
fn deprecated_slot_is_reserved_and_not_written() {
    let mut t: Table<MockStore, UserKey, User> = Table::new(MockStore::default());
    t.put(&UserKey { id: 1 }, &User { legacy: 99, level: 7 });

    // Two physical index-bearing entries were written? No: only the LIVE
    // index (slot 2) writes — the deprecated slot 1 produces nothing.
    let slot1 = t.store().scan_suffix(&[0, 9, 1]);
    let slot2 = t.store().scan_suffix(&[0, 9, 2]);
    assert!(slot1.is_empty(), "deprecated slot must not receive writes");
    assert_eq!(slot2.len(), 1, "live index after the deprecated one keeps its slot");
    assert_eq!(t.store().scan_suffix(&[0, 9]).len(), 2, "primary + one live index entry");
}

#[test]
fn prune_deletes_only_deprecated_prefix() {
    // A legacy database had slot-1 entries from before the declaration
    // was deprecated; simulate one by writing directly at slot 1 — on a
    // store handed to the Table AFTER seeding (MockStore::clone is a deep
    // copy, so the seeded entry lands in the table's own engine).
    let mut store = MockStore::default();
    let stale = [
        vec![0u8, 9, 1],
        99u32.to_be_bytes().to_vec(),
        1u64.to_be_bytes().to_vec(),
    ]
    .concat();
    store.put(stale, Vec::new());
    let mut t: Table<MockStore, UserKey, User> = Table::new(store);
    t.put(&UserKey { id: 1 }, &User { legacy: 99, level: 7 });
    t.put(&UserKey { id: 2 }, &User { legacy: 50, level: 8 });

    let pruned = t.prune_deprecated_slots();
    assert_eq!(pruned, 1, "the simulated stale slot-1 entry is pruned");
    assert!(t.store().scan_suffix(&[0, 9, 1]).is_empty());
    // Live entries untouched.
    assert_eq!(t.store().scan_suffix(&[0, 9, 2]).len(), 2);
    assert_eq!(t.get(&UserKey { id: 1 }).unwrap().level, 7);
    assert_eq!(t.get(&UserKey { id: 2 }).unwrap().level, 8);
}

#[test]
fn no_deprecated_declarations_prunes_nothing() {
    #[derive(RowEncode, Clone, PartialEq, Debug)]
    #[kv_ref(UserKey)]
    #[kv_ns(10)]
    #[kv_index(by_only { fields(level) })]
    pub struct PlainUser {
        pub level: u32,
    }

    let mut t: Table<MockStore, UserKey, PlainUser> = Table::new(MockStore::default());
    t.put(&UserKey { id: 1 }, &PlainUser { level: 3 });
    assert_eq!(t.prune_deprecated_slots(), 0);
}
