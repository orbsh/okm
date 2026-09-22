//! ADR-0022 remote-mode acceptance (acceptance 2): the same document
//! mutation planned Python-side and planned Rust-side must land
//! BYTE-IDENTICAL wire frames.
//!
//! Protocol: this binary prints `FRAME_HEX <hex>` lines for a scripted
//! scenario (put fresh → overwrite → delete), each planned through the
//! Rust-side plan surface with the scripted old-document and acc state
//! passed as CLI args. accept_embedded.py drives the same scenario
//! through the Python binding and compares hex-for-hex.
//!
//! Cross-language byte equality, extended from codec bytes to semantic
//! entries (ADR-0022 acceptance).

use okm_core::{KeyEncode, DocumentEncode, TestStore};
use okm_core::schema::CollectionSchema;
use okm_dynamic::{
    AccessMethod, AccessMethodKind, DynamicCollection, ReduceLogic, ReduceSpec, Value, ValueMap,
};
use std::collections::BTreeMap;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(42)]
#[ok_layout(version = 2)]
pub struct User {
    pub level: u32,
    pub score: u16,
    pub name: String,
}

/// Rust-side reduce logic: count per level group (u64 BE acc) — must
/// mirror the Python side's GroupCount byte-for-byte.
struct GroupCount;

impl ReduceLogic for GroupCount {
    fn seed(&self) -> Vec<u8> { 0u64.to_be_bytes().to_vec() }
    fn fold(&self, acc: &mut Vec<u8>, _d: &ValueMap) -> Result<(), String> {
        let n = u64::from_be_bytes(acc.as_slice().try_into().map_err(|_| "acc width")?) + 1;
        *acc = n.to_be_bytes().to_vec();
        Ok(())
    }
    fn unfold(&self, acc: &mut Vec<u8>, _d: &ValueMap) -> Result<(), String> {
        let n = u64::from_be_bytes(acc.as_slice().try_into().map_err(|_| "acc width")?)
            .checked_sub(1)
            .ok_or("underflow")?;
        *acc = n.to_be_bytes().to_vec();
        Ok(())
    }
}

fn schema() -> CollectionSchema {
    CollectionSchema::of::<UserKey, User>()
}

fn dynamic_table() -> DynamicCollection<TestStore> {
    // Indexes/reduce declared identically to the Python side (same slots,
    // same callables' semantics).
    DynamicCollection::with_reduces(
        TestStore::slatedb_mem(),
        42,
        schema(),
        vec![
            AccessMethod {
                slot: 0x1002,
                fields: vec![],
                includes: vec![],
                kind: AccessMethodKind::Func(Box::new(|d| match d.get("name") {
                    Some(Value::Str(s)) => Ok(s
                        .bytes()
                        .map(|b| (b as u32).to_be_bytes().to_vec())
                        .collect()),
                    _ => Err("name missing".into()),
                })),
            },
            AccessMethod {
                slot: 0x1003,
                fields: vec!["level".into()],
                includes: vec![],
                kind: AccessMethodKind::Partial(Box::new(|d| {
                    Ok(matches!(d.get("name"), Some(Value::Str(s)) if s.as_bytes() == b"a"))
                })),
            },
        ],
        vec![ReduceSpec {
            slot: 0x2001,
            group_fields: vec!["level".into()],
            logic: Box::new(GroupCount),
        }],
    )
}

fn values(org: u32, user: u64, level: u32, score: u16, name: &str) -> ValueMap {
    let mut m = BTreeMap::new();
    m.insert("org_id".into(), Value::U32(org));
    m.insert("user_id".into(), Value::U64(user));
    m.insert("level".into(), Value::U32(level));
    m.insert("score".into(), Value::U16(score));
    m.insert("name".into(), Value::Str(name.into()));
    m
}

fn key(org: u32, user: u64) -> Vec<u8> {
    UserKey { org_id: org, user_id: user }.encode()
}

/// Scenario step: put(key, doc) with optional old doc; acc cache passed
/// as hex pairs. Prints FRAME_HEX lines; the acc receipts accumulate.
fn main() {
    let t = dynamic_table();
    let mut accs: std::collections::HashMap<Vec<u8>, Vec<u8>> = std::collections::HashMap::new();

    let show = |label: &str, frame: &[u8], accs: &std::collections::HashMap<Vec<u8>, Vec<u8>>| {
        println!("FRAME_{} {}", label, frame.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let mut pairs: Vec<_> = accs.iter().collect();
        pairs.sort();
        for (k, v) in pairs {
            println!("ACC_{}_{} {} {}", label,
                k.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                v.iter().map(|b| format!("{b:02x}")).collect::<String>(), "");
        }
    };

    // Step 1: put fresh (no old doc, no accs).
    let plan = t.plan_put(&key(1, 10), &values(1, 10, 4, 100, "a"), None, &|ek| accs.get(ek).cloned()).unwrap();
    for (ek, acc) in &plan.new_accs { accs.insert(ek.clone(), acc.clone()); }
    show("PUT1", &okm_wire::OpFrame::write_batch(&plan.ops).encode(), &accs);

    // Step 2: overwrite same key (old = step-1 doc).
    let old = values(1, 10, 4, 100, "a");
    let plan = t.plan_put(&key(1, 10), &values(1, 10, 4, 100, "a2"), Some(&old), &|ek| accs.get(ek).cloned()).unwrap();
    for (ek, acc) in &plan.new_accs { accs.insert(ek.clone(), acc.clone()); }
    show("PUT2", &okm_wire::OpFrame::write_batch(&plan.ops).encode(), &accs);

    // Step 3: put into a second group (moves the count: unfold 4, fold 9
    // via a DIFFERENT key — mirrors the Python scenario's second actor).
    let old3 = values(1, 11, 9, 200, "b");
    let plan = t.plan_put(&key(1, 11), &values(1, 11, 4, 200, "b"), Some(&old3), &|ek| accs.get(ek).cloned()).unwrap();
    for (ek, acc) in &plan.new_accs { accs.insert(ek.clone(), acc.clone()); }
    show("PUT3", &okm_wire::OpFrame::write_batch(&plan.ops).encode(), &accs);

    // Step 4: delete key(1,10).
    let old4 = values(1, 10, 4, 100, "a2");
    let plan = t.plan_delete(&key(1, 10), &old4, &|ek| accs.get(ek).cloned()).unwrap();
    for (ek, acc) in &plan.new_accs { accs.insert(ek.clone(), acc.clone()); }
    show("DEL1", &okm_wire::OpFrame::write_batch(&plan.ops).encode(), &accs);
}
