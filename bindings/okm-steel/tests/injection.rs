//! The steel injection face end to end (ADR-0037 4.16b): a HOST-owned
//! engine behind the byte face + a raw interface_schema entry → the VM
//! script writes through the Collection fns (zero translation at the
//! method face) → the bytes land in the HOST engine (not an orphan
//! store), the declared index sweeps, the count preset folds — and a
//! SECOND registry over the same engine+ns reads the same rows back
//! (the cross-check shape of aura's `py_injection.rs` lock).
use okm_core::schema::CollectionSchema;
use okm_core::{DocumentEncode, KeyEncode};
use okm_steel::collection::StorageRegistry;
use steel::SteelVal;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(41)]
#[ok_layout(version = 3)]
pub struct UserV3 {
    pub level: u32,
    pub score: u16,
    pub name: String,
}

/// The host engine behind the byte face — the realm store's shape in
/// miniature (ordered, full-key scan returns the FULL key).
struct MemEngine(Mutex<BTreeMap<Vec<u8>, Vec<u8>>>);

impl okm_steel::okm_entry::Engine for MemEngine {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        self.0.lock().unwrap().insert(key, value);
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn del(&self, key: &[u8]) {
        self.0.lock().unwrap().remove(key);
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>> {
        let rows = self.0.lock().unwrap();
        let start = begin.to_vec();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match end {
            Some(e) => Box::new(rows.range(start..e.to_vec())),
            None => Box::new(rows.range(start..)),
        };
        iter.map(|(k, _)| k.clone()).collect()
    }
}

fn schema_json() -> String {
    serde_json::to_string(&CollectionSchema::of::<UserKey, UserV3>()).unwrap()
}

/// The RAW entry (the interface_schema storage shape): schema + one
/// declared plain index + one count preset. Slots are caller-allocated
/// — the declared-index/reduce base convention (python injection
/// test's shape).
fn entry_json() -> serde_json::Value {
    serde_json::json!({
        "schema": serde_json::to_value(CollectionSchema::of::<UserKey, UserV3>()).unwrap(),
        "indexes": [{ "name": "by_level", "slot": 4097, "fields": ["level"], "kind": "plain" }],
        "reduces": [{ "name": "total", "slot": 8193, "group": [], "kind": "count" }],
    })
}

/// Escape the JSON for a steel string literal (vm_roundtrip's shape).
fn quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn last_sym(vm_out: Vec<SteelVal>) -> String {
    vm_out
        .last()
        .map(|v| match v {
            SteelVal::SymbolV(s) => s.to_string(),
            other => format!("{other:?}"),
        })
        .unwrap_or_default()
}

fn build_vm(reg: &StorageRegistry) -> steel::steel_vm::engine::Engine {
    let mut vm = steel::steel_vm::engine::Engine::new();
    okm_steel::register(&mut vm);
    reg.register_into(&mut vm);
    vm
}

#[test]
fn script_writes_land_in_the_host_engine() {
    let engine = Arc::new(MemEngine(Mutex::new(Default::default())));
    let reg = StorageRegistry::default();
    reg.inject(engine.clone(), "Counters", &entry_json(), 41).unwrap();
    let mut vm = build_vm(&reg);

    let sj = quote(&schema_json());
    let doc = r#"(hash "org_id" 1 "user_id" 2 "level" 9 "score" 500 "name" "alice")"#;

    // The method face: put → get round trip THROUGH THE HOST ENGINE,
    // zero translation at the Collection shell, collection named (not
    // numbered) by the script.
    let out = vm
        .run(format!(
            r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(collection-put! "Counters" pkey {doc})
(define read (collection-get! "Counters" pkey))
(assert! (string=? (hash-ref read "name") "alice"))
(assert! (= (hash-ref read "level") 9))
(assert! (= (hash-ref read "score") 500))
'done
"#
        ))
        .expect("steel program failed");
    assert_eq!(last_sym(out), "done");

    // A SECOND registry over the same engine + ns sees the same row by
    // the same name (the cross-check — the bytes live in the host, not
    // an orphan).
    let reg2 = StorageRegistry::default();
    reg2.inject(engine.clone(), "Counters", &entry_json(), 41).unwrap();
    let mut vm2 = build_vm(&reg2);
    let out2 = vm2
        .run(format!(
            r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(define read (collection-get! "Counters" pkey))
(assert! (string=? (hash-ref read "name") "alice"))
'done
"#
        ))
        .expect("cross-check read failed");
    assert_eq!(last_sym(out2), "done");

    // The declared index swept: a level-prefix scan over the SECOND
    // registry finds the row the FIRST wrote.
    let out3 = vm2
        .run(
            r#"
(define rows (collection-scan! "Counters" 4097 (list->vector (list 0 0 0 9))))
(assert! (= (vector-length rows) 1))
(assert! (= (hash-ref (vector-ref rows 0) "org_id") 1))
(assert! (= (hash-ref (vector-ref rows 0) "user_id") 2))
'done
"#
            .to_string(),
        )
        .expect("index scan failed");
    assert_eq!(last_sym(out3), "done");

    // The count preset folded through the ENGINE (not a cache): the
    // accumulator is a u64 BE in the host store.
    let out4 = vm2
        .run(r#"
(define acc (collection-reduce-get! "Counters" (hash)))
(assert! (vector? acc))
(assert! (= (vector-length acc) 8))
'done
"#)
        .expect("reduce read failed");
    assert_eq!(last_sym(out4), "done");
    let stored = engine.0.lock().unwrap();
    let acc_rows: Vec<_> = stored
        .iter()
        .filter(|(k, v)| k.starts_with(&[0u8, 41]) && **v == 1u64.to_be_bytes().to_vec())
        .collect();
    assert!(!acc_rows.is_empty(), "the folded count acc lives in the host engine");
    drop(stored);

    // Delete sweeps the index entry and removes the primary row.
    vm2.run(format!(
        r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(collection-delete! "Counters" pkey)
'done
"#
    ))
    .expect("delete failed");
    let rows = engine.0.lock().unwrap();
    let primary_gone = !rows
        .keys()
        .any(|k| k.starts_with(&[0u8, 41, 0u8, 0u8]) && k.len() == 4 + 12);
    assert!(primary_gone, "the primary row left the host engine");
    let index_gone = !rows.keys().any(|k| k.starts_with(&[0u8, 41, 16u8, 1u8])); // slot 4097 = 0x1001
    assert!(index_gone, "the index entry was swept");
    drop(rows);
}

#[test]
fn get_absent_document_is_void() {
    // The steel void arm mirrors python's None (the `(void? cur)`
    // reading discipline on the script side).
    let engine = Arc::new(MemEngine(Mutex::new(Default::default())));
    let reg = StorageRegistry::default();
    reg.inject(engine, "Counters", &entry_json(), 41).unwrap();
    let mut vm = build_vm(&reg);
    let sj = quote(&schema_json());
    let out = vm
        .run(format!(
            r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 7 "user_id" 7)))
(void? (collection-get! "Counters" pkey))
"#
        ))
        .unwrap();
    assert!(matches!(out.last(), Some(SteelVal::BoolV(true))));
}

#[test]
fn unknown_collection_names_the_error() {
    // The script names a collection the injection never declared — a
    // loud error, not a silent void (the injection gap the python
    // mirror closed with `with_store`).
    let engine = Arc::new(MemEngine(Mutex::new(Default::default())));
    let reg = StorageRegistry::default();
    reg.inject(engine, "Counters", &entry_json(), 41).unwrap();
    let mut vm = build_vm(&reg);
    let err = vm
        .run(r#"(collection-get! "Ghost" (list->vector (list 0 0 0 0 0 0 0 0 0 0 0 0)))"#)
        .expect_err("unknown collection must fail");
    assert!(format!("{err:?}").contains("Ghost"), "{err:?}");
}

#[test]
fn introspection_stubs_keep_dotted_scripts_loading() {
    // A resident script whose handlers call the collection fns loads in
    // the INTROSPECTION throwaway engine (define-compile resolves the
    // six free names against the stubs — the ctx-stub precedent; the
    // schema half is not silently dropped). The stub arms fail loudly
    // if actually called.
    let mut vm = steel::steel_vm::engine::Engine::new();
    okm_steel::register(&mut vm);
    StorageRegistry::register_stubs(&mut vm);
    vm.run(
        r#"(define (handler ev) (collection-put! "Counters" (list->vector (list 0)) (hash)))"#,
    )
    .expect("handler referencing the stubbed fns must load under stubs");
    let err = vm
        .run(r#"(handler 1)"#)
        .expect_err("the stub arm names itself");
    assert!(format!("{err:?}").contains("introspection only"), "{err:?}");
}

#[test]
fn bad_declaration_names_the_error() {
    // A func index cannot be built from data (the embedded add_*
    // surface owns callables) — the injection names it (the python
    // mirror's rule — same okm-entry parsing, one source).
    let engine = Arc::new(MemEngine(Mutex::new(Default::default())));
    let entry = serde_json::json!({
        "schema": serde_json::to_value(CollectionSchema::of::<UserKey, UserV3>()).unwrap(),
        "indexes": [{ "slot": 4097, "fields": [], "kind": "func" }],
    });
    let err = StorageRegistry::default()
        .inject(engine, "Bad", &entry, 41)
        .unwrap_err();
    assert!(err.contains("host callable"), "{err}");
}
