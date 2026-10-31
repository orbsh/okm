// Gate: `cargo test --no-default-features --features host-tests` (the
// default wheel-posture test run skips this file — extension-module
// forbids linking libpython; reading a PyErr or calling the collection
// face needs the interpreter linked, the embedding case). Both flags are
// required: `--features host-tests` alone keeps extension-module on and
// the link fails. On a nix host the interpreter also has to be on the
// linker's path: `RUSTFLAGS=-L native=<python3-env>/lib`.

//! The host-injected face (ADR-0037 §1, Phase 4.16a) end to end: a
//! Collection built over an ENGINE THE HOST OWNS must (a) drive its
//! reads/writes through that engine — the realm's store, not a private
//! orphan — and (b) behave exactly like the embedded face (declared
//! index + preset reduce included). This is what the probe carrier's
//! python injection rides; the byte contract lives in okm-dynamic,
//! already locked there — this test locks the BINDING wiring.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use okm::pyo3_impl::{Collection, Engine};
use okm_dynamic::{Value, ValueMap};

/// A host-owned in-memory engine behind the byte-level face (the realm
/// store's shape at this seam).
struct MemEngine {
    rows: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemEngine {
    fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
        }
    }
    fn len(&self) -> usize {
        self.rows.lock().unwrap().len()
    }
}

impl Engine for MemEngine {
    // The face answers `Result<_, String>`: an engine that can fail says so, and the
    // binding's enum maps the string onto its own error. An in-memory map cannot fail, but
    // a fake that cannot fail is still obliged to speak the trait's language.
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), String> {
        self.rows.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.rows.lock().unwrap().get(key).cloned())
    }
    fn del(&self, key: &[u8]) -> Result<(), String> {
        self.rows.lock().unwrap().remove(key);
        Ok(())
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>, String> {
        let rows = self.rows.lock().unwrap();
        let start = begin.to_vec();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match end {
            Some(e) => Box::new(rows.range(start..e.to_vec())),
            None => Box::new(rows.range(start..)),
        };
        Ok(iter.map(|(k, _)| k.clone()).collect())
    }
}

/// The `counters` shape the aura/nu fixtures declare: key `id` (U64),
/// hot field `count` (U64), one declared index `by_count`, one preset
/// reduce (count over all). Slot bases match the schema's `slots` block.
fn counters_entry() -> serde_json::Value {
    serde_json::json!({
        "schema": {
            "key_len": 8,
            "key_fields": [{"name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
            "layout_version": 1, "hot_width": 8, "payload_header_len": 3,
            "hot_fields": [{"name": "count", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
            "cold_fields": [],
            "slots": {"primary": 0, "dynamic": 1, "field_vocab_id": 2, "field_vocab_name": 3,
                      "declared_index_base": 4096, "declared_reduce_base": 8192,
                      "junction_base": 12288}
        },
        "indexes": [{"name": "by_count", "slot": 4097, "fields": ["count"], "kind": "plain"}],
        "reduces": [{"name": "total", "slot": 8193, "group": [], "kind": "count"}]
    })
}

fn key_bytes(id: u64) -> Vec<u8> {
    id.to_be_bytes().to_vec()
}

fn doc(count: u64) -> ValueMap {
    let mut m = ValueMap::new();
    m.insert("id".into(), Value::U64(count));
    m.insert("count".into(), Value::U64(count));
    m
}

#[test]
fn injected_collection_writes_to_the_host_engine() {
    let engine = Arc::new(MemEngine::new());
    let coll = Collection::with_store(engine.clone(), &counters_entry(), 7).unwrap();

    // The host owns the engine — before any write it is empty; the
    // collection wrote NOTHING to a private store, it wrote HERE.
    assert_eq!(engine.len(), 0, "fresh host engine holds no rows");
    coll.merge_doc(&key_bytes(1), &doc(10)).unwrap();
    assert!(
        engine.len() > 0,
        "the put landed in the HOST engine, not an orphan store"
    );

    // Read-back through the injected engine.
    let back = coll.fetch_doc(&key_bytes(1)).unwrap().unwrap();
    assert_eq!(back.get("count"), Some(&Value::U64(10)));
    assert!(coll.fetch_doc(&key_bytes(999)).unwrap().is_none());

    // Declared index routes through the same engine (plan wrote the
    // index entry, scan reads it): by_count prefix = 10u64 BE.
    let hits = coll.scan_doc(4097, &10u64.to_be_bytes()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("id"), Some(&Value::U64(1)));

    // Overwrite sweeps the stale index entry (10 → 11): the old prefix
    // finds nothing, the new one finds the key.
    coll.merge_doc(&key_bytes(1), &doc(11)).unwrap();
    assert!(coll
        .scan_doc(4097, &10u64.to_be_bytes())
        .unwrap()
        .is_empty());
    assert_eq!(coll.scan_doc(4097, &11u64.to_be_bytes()).unwrap().len(), 1);

    // Delete removes primary + index.
    coll.purge_doc(&key_bytes(1)).unwrap();
    assert!(coll.fetch_doc(&key_bytes(1)).unwrap().is_none());
    assert!(coll
        .scan_doc(4097, &11u64.to_be_bytes())
        .unwrap()
        .is_empty());
}

#[test]
fn injected_reduce_preset_accums_in_the_host_engine() {
    let engine = Arc::new(MemEngine::new());
    let coll = Collection::with_store(engine.clone(), &counters_entry(), 7).unwrap();
    // Two puts fold the count preset once each — the accumulator is an
    // engine row (group bytes = u64 BE count == 2).
    coll.merge_doc(&key_bytes(1), &doc(5)).unwrap();
    coll.merge_doc(&key_bytes(2), &doc(7)).unwrap();
    // The count group entry key: [ns][slot BE][group segment=empty].
    let mut ek = vec![0u8, 7];
    ek.extend_from_slice(&8193u16.to_be_bytes());
    let acc = engine
        .get(&ek)
        .unwrap()
        .expect("the preset reduce wrote its accumulator to the host engine");
    assert_eq!(u64::from_be_bytes(acc.as_slice().try_into().unwrap()), 2);
    // Unfold on delete brings it back to 1 (Count is reversible).
    coll.purge_doc(&key_bytes(1)).unwrap();
    let acc = engine.get(&ek).unwrap().unwrap();
    assert_eq!(u64::from_be_bytes(acc.as_slice().try_into().unwrap()), 1);
}

#[test]
fn bad_declaration_is_an_error_not_a_silent_empty() {
    // Reading a PyErr's message needs the interpreter (Display walks
    // the Python exception) — the only test here that inspects one.
    pyo3::Python::initialize();
    let engine = Arc::new(MemEngine::new());
    // A func index cannot be built from data (needs a host callable).
    let entry = serde_json::json!({
        "schema": counters_entry()["schema"].clone(),
        "indexes": [{"name": "f", "slot": 4097, "fields": [], "kind": "func"}]
    });
    let err = match Collection::with_store(engine, &entry, 7) {
        Err(e) => e,
        Ok(_) => panic!("a func index must not build from data"),
    };
    assert!(err.to_string().contains("host callable"), "{err}");
    // A bad schema JSON is an error value too.
    let garbage = serde_json::json!({"schema": {"nope": 1}});
    assert!(Collection::with_store(Arc::new(MemEngine::new()), &garbage, 7).is_err());
}
