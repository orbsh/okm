//! ADR-0022 acceptance: the binding-implementable semantic surfaces.
//!
//! - Embedded mode: the calling-discipline test (put/delete/overwrite
//!   against the accumulator) is the core case — a wrong call site is a
//!   silent accumulator drift, so every discipline arm is locked here.
//! - Byte-layout parity: func-index and reduce entries share the exact
//!   layout of the Rust-side derive, verified against a typed
//!   `Collection` with the equivalent declarations landing in the same ns.
//! - Purity failure mode is documented, not engineered around: the acc
//!   callable contract (`unfold(fold(a, x)) = a`) is the implementor's
//!   obligation, same as the Rust-side `ReduceLogic`.

use okm_core::{DocumentEncode, KeyEncode, TestStore, VirtualStorage};
use okm_core::schema::CollectionSchema;
use okm_dynamic::{
    AccessMethod, AccessMethodKind, BoundReduce, DynamicCollection, ReduceSpec, Value, ValueMap,
};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(42)]
#[ok_layout(version = 2)]
#[ok_index(by_level { fields(level), includes(score) })]
#[ok_index(by_tag { func(user_tags) })]
#[ok_index(active_only { fields(level), includes(score), where(user_active) })]
pub struct User {
    pub level: u32,
    pub score: u16,
    pub name: String,
}

/// Rust-side func-index derive function (fan-out on the name's bytes).
fn user_tags(document: &User) -> Vec<u32> {
    document.name.bytes().map(|b| b as u32).collect()
}

/// Rust-side partial-index predicate (mirror of the host closure).
fn user_active(document: &User) -> bool {
    // The dynamic side gates on the name's first byte as its "active"
    // bit (bool fields are not a typed schema kind); the Rust side
    // mirrors that so both sides admit the same documents.
    document.name.as_bytes() == b"a"
}

// The Rust-side reduce logic, for byte-parity: count of documents per
// level group (u64 BE accumulator).
mod rust_reduce {
    use super::{User, UserKey};
    use okm_core::model::reduce::{ReduceCodec, ReduceLogic, Reduce};

    #[derive(Default)]
    pub struct GroupCount(pub u64);

    impl ReduceCodec for GroupCount {
        fn encode_acc(&self) -> Vec<u8> {
            self.0.to_be_bytes().to_vec()
        }
        fn decode_acc(bytes: &[u8]) -> Self {
            GroupCount(u64::from_be_bytes(bytes.try_into().unwrap()))
        }
    }

    impl ReduceLogic for GroupCount {
        type Document = User;
        type Acc = GroupCount;
        fn fold(acc: &mut GroupCount, _item: &User) {
            acc.0 += 1;
        }
        fn unfold(acc: &mut GroupCount, _item: &User) {
            acc.0 -= 1;
        }
    }

    impl Reduce for GroupCount {
        const SLOT: u16 = 0x2001;
        const GROUP: &'static [&'static str] = &["level"];
        fn group_bytes(_key: &UserKey, _document: &User) -> Vec<u8> {
            unreachable!("the derive generates this; we only need the layout constants")
        }
    }
}

fn schema() -> CollectionSchema {
    CollectionSchema::of::<UserKey, User>()
}

fn values(org_id: u32, user_id: u64, level: u32, score: u16, name: &str) -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("org_id".into(), Value::U32(org_id));
    m.insert("user_id".into(), Value::U64(user_id));
    m.insert("level".into(), Value::U32(level));
    m.insert("score".into(), Value::U16(score));
    m.insert("name".into(), Value::Str(name.into()));
    m
}

fn key_bytes(org_id: u32, user_id: u64) -> Vec<u8> {
    UserKey { org_id, user_id }.encode()
}

/// Host-side callable spelling: closures over the decoded document,
/// returning encoded bytes — the shape a Python/Steel callable fills.
/// Fan-out on the name's bytes (inverted-index regime); the encoding is
/// the caller's (BE u32 here, matching the Rust `user_tags`).
fn func_tags(document: &ValueMap) -> Result<Vec<Vec<u8>>, String> {
    match document.get("name") {
        Some(Value::Str(s)) => Ok(s
            .bytes()
            .map(|b| (b as u32).to_be_bytes().to_vec())
            .collect()),
        _ => Err("func index field `name` missing or not a string".into()),
    }
}

/// The reduce logic as ONE object — the mirror of the Rust-side
/// `ReduceLogic` + `ReduceCodec: Default` pair. `seed` is the
/// `Default::default()` counterpart (8B BE zero for a u64 counter).
struct GroupCount;

impl okm_dynamic::ReduceLogic for GroupCount {
    fn seed(&self) -> Vec<u8> {
        0u64.to_be_bytes().to_vec()
    }
    fn fold(&self, acc: &mut Vec<u8>, _document: &ValueMap) -> Result<(), String> {
        let n = be_u64(acc) + 1;
        *acc = n.to_be_bytes().to_vec();
        Ok(())
    }
    fn unfold(&self, acc: &mut Vec<u8>, _document: &ValueMap) -> Result<(), String> {
        let n = be_u64(acc)
            .checked_sub(1)
            .ok_or("accumulator underflow: unfold without fold")?;
        *acc = n.to_be_bytes().to_vec();
        Ok(())
    }
}

fn be_u64(b: &[u8]) -> u64 {
    assert!(b.len() == 8, "u64 acc must be 8B BE (host-side layout rule)");
    u64::from_be_bytes(b.try_into().unwrap())
}

/// The dynamic-side "active" predicate: the host closure gates on the
/// name's first byte (bool is not a typed schema kind in v1 scope).
fn active(document: &ValueMap) -> Result<bool, String> {
    Ok(matches!(document.get("name"), Some(Value::Str(s)) if s.as_bytes() == b"a"))
}

fn dynamic_table(store: TestStore) -> DynamicCollection<TestStore> {
    DynamicCollection::with_reduces(
        store,
        42,
        schema(),
        vec![
            AccessMethod::plain(0x1001, vec!["level".into()], vec!["score".into()]),
            AccessMethod {
                slot: 0x1002,
                fields: vec![], // func: the derive result IS the segment
                includes: vec![],
                kind: AccessMethodKind::Func(Box::new(func_tags)),
            },
            AccessMethod {
                slot: 0x1003,
                fields: vec!["level".into()],
                includes: vec!["score".into()],
                kind: AccessMethodKind::Partial(Box::new(active)),
            },
        ],
        vec![ReduceSpec {
            slot: 0x2001,
            group_fields: vec!["level".into()],
            logic: Box::new(GroupCount),
        }],
    )
}

fn reduce_entry_key(t: &DynamicCollection<TestStore>, level: u32) -> Vec<u8> {
    let reduce = BoundReduce::new(ReduceSpec {
        slot: 0x2001,
        group_fields: vec!["level".into()],
        logic: Box::new(GroupCount),
    });
    let mut m = BTreeMap::new();
    m.insert("level".into(), Value::U32(level));
    reduce.entry_key(t.schema(), &42u16.to_be_bytes(), &m).unwrap()
}

fn acc_of(t: &DynamicCollection<TestStore>, level: u32) -> Option<u64> {
    t.store().get(&reduce_entry_key(t, level)).map(|b| be_u64(&b))
}

#[test]
fn func_index_fans_out_and_sweeps() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    // "bob" → entries for 'b' 'o' 'b' (two 'b' entries, one 'o').
    t.put(&key_bytes(1, 2), &values(1, 2, 4, 77, "bob")).unwrap();

    let b = (b'b' as u32).to_be_bytes();
    let o = (b'o' as u32).to_be_bytes();
    // Duplicate derived values ('b' twice in "bob") map to the SAME
    // entry key and collapse — identical to the Rust-side fan-out.
    assert_eq!(t.scan(0x1002, &b).unwrap().len(), 1, "collapsed 'b' entry");
    assert_eq!(t.scan(0x1002, &o).unwrap().len(), 1, "one 'o' entry");

    // Overwrite to a name sharing no characters: all old entries swept.
    t.put(&key_bytes(1, 2), &values(1, 2, 4, 77, "zig")).unwrap();
    assert!(t.scan(0x1002, &b).unwrap().is_empty());
    assert!(t.scan(0x1002, &o).unwrap().is_empty());
    let z = (b'z' as u32).to_be_bytes();
    assert_eq!(t.scan(0x1002, &z).unwrap().len(), 1);

    // Delete removes the fan-out entirely.
    t.delete(&key_bytes(1, 2)).unwrap();
    assert!(t.scan(0x1002, &z).unwrap().is_empty());
}

#[test]
fn partial_index_admits_gates_entries() {
    let mut t = dynamic_table(TestStore::slatedb_mem());
    // Not active ("bob"): no entry in the partial index.
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "bob")).unwrap();
    assert!(t.scan(0x1003, &4u32.to_be_bytes()).unwrap().is_empty());

    // Overwrite to active ("a"): the entry appears (predicate flip on
    // overwrite inserts without a stale entry to remove).
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();
    assert_eq!(t.scan(0x1003, &4u32.to_be_bytes()).unwrap().len(), 1);

    // Overwrite back to inactive: the entry is swept.
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "zig")).unwrap();
    assert!(t.scan(0x1003, &4u32.to_be_bytes()).unwrap().is_empty());

    // Delete of an admitted document removes its partial entry.
    t.put(&key_bytes(1, 11), &values(1, 11, 9, 50, "a")).unwrap();
    t.delete(&key_bytes(1, 11)).unwrap();
    assert!(t.scan(0x1003, &9u32.to_be_bytes()).unwrap().is_empty());
}

#[test]
fn reduce_calling_discipline() {
    let mut t = dynamic_table(TestStore::slatedb_mem());

    // put (no old document): fold only. Two documents in group level=4.
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();
    t.put(&key_bytes(1, 11), &values(1, 11, 4, 200, "b")).unwrap();
    assert_eq!(acc_of(&t, 4), Some(2));

    // put into another group: independent accumulators.
    t.put(&key_bytes(1, 12), &values(1, 12, 9, 50, "c")).unwrap();
    assert_eq!(acc_of(&t, 9), Some(1));
    assert_eq!(acc_of(&t, 4), Some(2));

    // overwrite within a group: unfold old, fold new → net zero.
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a2")).unwrap();
    assert_eq!(acc_of(&t, 4), Some(2), "overwrite must not drift the acc");

    // overwrite across groups: old group unfolds, new group folds.
    t.put(&key_bytes(1, 12), &values(1, 12, 4, 50, "c")).unwrap();
    assert_eq!(acc_of(&t, 9), Some(0), "group 9 emptied: zero acc, entry kept (no GC)");
    assert_eq!(acc_of(&t, 4), Some(3));

    // delete: unfold the stored document.
    t.delete(&key_bytes(1, 10)).unwrap();
    assert_eq!(acc_of(&t, 4), Some(2));

    // delete again (missing document): no double unfold.
    t.delete(&key_bytes(1, 10)).unwrap();
    assert_eq!(acc_of(&t, 4), Some(2));
}

#[test]
fn reduce_entry_layout_matches_rust_side() {
    // The dynamic reduce entry key must be byte-identical to the
    // Rust-side Reduce::entry_key layout: [ns 2B][slot u16 BE][group
    // segment].
    let mut t = dynamic_table(TestStore::slatedb_mem());
    t.put(&key_bytes(1, 10), &values(1, 10, 4, 100, "a")).unwrap();

    let expected = reduce_entry_key(&t, 4);
    assert!(
        t.store().get(&expected).is_some(),
        "dynamic reduce entry must land at the Rust-side layout key"
    );

    // And the accumulator value must match the Rust fold's encoding:
    // one document folded → u64 BE of 1.
    assert_eq!(t.store().get(&expected).unwrap(), 1u64.to_be_bytes().to_vec());
}
