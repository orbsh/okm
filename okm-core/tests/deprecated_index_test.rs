//! Deprecated index declarations (ADR-0005 slot discipline): a
//! `#[ok_index(name { ... }, deprecated)]` declaration still occupies its
//! slot (declaration order is a persistent contract — removing it would
//! shift every later slot onto stale data), but generates NO write path
//! (no marker struct, no `index_entries` contribution) and NO scan
//! surface. Its stale entries stay in the engine until
//! `Table::prune_deprecated_slots` deletes them by prefix.

use okm_core::{
    KeyEncode, TestStore, ObjEncode, Table, VirtualStorage,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub id: u64,
}

/// Live declaration AFTER the deprecated one — its slot must NOT shift
/// (that is the entire point of the placeholder).
#[derive(ObjEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(9)]
#[ok_index(by_old { fields(legacy) }, deprecated)]
#[ok_index(by_level { fields(level) })]
pub struct User {
    pub legacy: u32,
    pub level: u32,
}

#[test]
fn deprecated_slot_is_reserved_and_not_written() {
    let mut t: Table<TestStore, UserKey, User> = Table::new(TestStore::slatedb_mem());
    t.put(&UserKey { id: 1 }, &User { legacy: 99, level: 7 });

    // Only the LIVE index writes — the deprecated slot (16) produces
    // nothing; the live one (17, declaration order preserved) writes.
    let slot_dep = t.store().scan_suffix(&[0, 9, 16]);
    let slot_live = t.store().scan_suffix(&[0, 9, 17]);
    assert!(slot_dep.is_empty(), "deprecated slot must not receive writes");
    assert_eq!(slot_live.len(), 1, "live index after the deprecated one keeps its slot");
    assert_eq!(t.store().scan_suffix(&[0, 9]).len(), 2, "primary + one live index entry");
}

#[test]
fn prune_deletes_only_deprecated_prefix() {
    // A legacy database had slot-1 entries from before the declaration
    // was deprecated; simulate one by writing directly at slot 1 — on a
    // store handed to the Table AFTER seeding (TestStore::clone is a deep
    // copy, so the seeded entry lands in the table's own engine).
    let mut store = TestStore::slatedb_mem();
    let stale = [
        vec![0u8, 9, 16],
        99u32.to_be_bytes().to_vec(),
        1u64.to_be_bytes().to_vec(),
    ]
    .concat();
    store.put(stale, Vec::new());
    let mut t: Table<TestStore, UserKey, User> = Table::new(store);
    t.put(&UserKey { id: 1 }, &User { legacy: 99, level: 7 });
    t.put(&UserKey { id: 2 }, &User { legacy: 50, level: 8 });

    let pruned = t.prune_deprecated_slots();
    assert_eq!(pruned, 1, "the simulated stale slot-16 entry is pruned");
    assert!(t.store().scan_suffix(&[0, 9, 16]).is_empty());
    // Live entries untouched.
    assert_eq!(t.store().scan_suffix(&[0, 9, 17]).len(), 2);
    assert_eq!(t.get(&UserKey { id: 1 }).unwrap().level, 7);
    assert_eq!(t.get(&UserKey { id: 2 }).unwrap().level, 8);
}

#[test]
fn no_deprecated_declarations_prunes_nothing() {
    #[derive(ObjEncode, Clone, PartialEq, Debug)]
    #[ok_ref(UserKey)]
    #[ok_ns(10)]
    #[ok_index(by_only { fields(level) })]
    pub struct PlainUser {
        pub level: u32,
    }

    let mut t: Table<TestStore, UserKey, PlainUser> = Table::new(TestStore::slatedb_mem());
    t.put(&UserKey { id: 1 }, &PlainUser { level: 3 });
    assert_eq!(t.prune_deprecated_slots(), 0);
}
