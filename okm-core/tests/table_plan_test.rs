//! The compiled plan is the log's whole judgment (ADR-0074 §3): it must answer exactly what
//! the rule answers, at every slot, and it must answer "not mine" for a key that belongs to
//! another table. Both are asserted against the rule itself rather than against a table of
//! expected answers, so a change to either side shows up here.

use okm_core::changelog::{Side, side_of};
use okm_core::model::table_plan::TablePlan;
use okm_core::schema::CollectionSchema;
use okm_core::{DocumentEncode, KeyEncode};

#[derive(KeyEncode, Clone, Default)]
struct PlanKey {
    id: u64,
}

#[derive(DocumentEncode, Clone)]
#[ok_ref(PlanKey)]
#[ok_ns(9)]
struct PlanRow {
    level: u32,
    name: String,
}

#[test]
fn the_compiled_table_answers_what_the_rule_answers() {
    let schema = CollectionSchema::of::<PlanKey, PlanRow>();
    let plan = TablePlan::compile(&schema);
    // The bitmap covers 0x00..=0xFF and the rule covers the rest; the seam is the place a
    // hand-written table would disagree, so the sweep crosses it deliberately.
    for slot in 0..=0x3000u16 {
        let expected = side_of(&schema.slots, slot) == Side::Truth;
        assert_eq!(plan.carries(slot), expected, "slot {slot:#04x}");
    }
}

/// The typed path fills its static from the declaration; the dynamic path builds
/// one from schema bytes at run time. They must be the same answer, or a store written through
/// one would be read back differently by the other.
#[test]
fn the_declared_plan_and_the_compiled_one_agree() {
    let schema = CollectionSchema::of::<PlanKey, PlanRow>();
    let compiled = TablePlan::compile(&schema);
    // The static the derive emitted for this table, filled here the way attach fills it.
    let declared = *<PlanRow as okm_core::Document>::PLAN
        .get_or_init(|| TablePlan::compile(&CollectionSchema::of::<PlanKey, PlanRow>()));
    for slot in 0..=0x3000u16 {
        assert_eq!(
            declared.carries(slot),
            compiled.carries(slot),
            "slot {slot:#04x}"
        );
    }
    assert_eq!(declared.header(), compiled.header());
}

#[test]
fn the_plan_carries_its_own_table_and_nothing_else() {
    let schema = CollectionSchema::of::<PlanKey, PlanRow>();
    let plan = TablePlan::compile(&schema);

    let key = PlanKey { id: 7 }.encode();
    let mut own = schema.ns_prefix.clone();
    own.extend_from_slice(&key);
    own.push(0); // the primary face
    assert!(plan.carries_key(&own), "a row of this table is carried");

    let mut other = vec![0x00, 0x63];
    other.extend_from_slice(&key);
    other.push(0);
    assert!(!plan.carries_key(&other), "another table's key is not");
}

// ---------- the shared-ns shape: a static with no module-level name ----------

#[derive(KeyEncode, Clone, Default)]
struct SharedAKey {
    id: u64,
}

#[derive(DocumentEncode, Clone)]
#[ok_ref(SharedAKey)]
#[ok_ns(11)]
#[ok_shared_ns_in_tests]
struct SharedARow {
    level: u32,
}

#[derive(KeyEncode, Clone, Default)]
struct SharedBKey {
    id: u64,
}

#[derive(DocumentEncode, Clone)]
#[ok_ref(SharedBKey)]
#[ok_ns(11)]
#[ok_shared_ns_in_tests]
struct SharedBRow {
    level: u64,
}

fn plan_of<R>() -> &'static TablePlan
where
    R: okm_core::Document,
{
    R::PLAN.get_or_init(|| TablePlan::compile(&CollectionSchema::of::<R::Key, R>()))
}

/// `#[ok_shared_ns_in_tests]` declares no module-level marker, so its `PLAN` is the address of a
/// `static` declared inside the const's own block. A block item is one item, not a copy per
/// evaluation — if it were a copy, two reads would land on different addresses and the plan
/// would be recompiled forever.
#[test]
fn a_shared_declaration_still_has_one_plan_per_type() {
    let first = plan_of::<SharedARow>();
    let again = plan_of::<SharedARow>();
    let other = plan_of::<SharedBRow>();
    assert_eq!(
        std::ptr::from_ref(first),
        std::ptr::from_ref(again),
        "one static, however many times it is read"
    );
    assert_ne!(
        std::ptr::from_ref(first),
        std::ptr::from_ref(other),
        "two declarations, two statics — the shared ns is not a shared plan"
    );
    assert_eq!(first.header(), other.header(), "the ns is what they share");
}
