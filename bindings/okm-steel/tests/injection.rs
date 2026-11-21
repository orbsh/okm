//! The steel injection face end to end (ADR-0037 4.16b): a HOST-owned
//! engine behind the byte face + a raw interface_schema entry → the VM
//! script writes through the Collection fns (zero translation at the
//! method face) → the bytes land in the HOST engine (not an orphan
//! store), the declared index sweeps, the count preset folds — and a
//! SECOND registry over the same engine+ns reads the same rows back
//! (the cross-check shape of aura's `py_injection.rs` lock).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::schema::CollectionSchema;
use okm_core::{DocumentEncode, KeyEncode, NodeRef};
use okm_core::model::dynamic_cluster::{Decl, DynFragment};
use okm_core::model::dynamic_graph::DynEdge;
use okm_value::Dict;
use okm_core::model::dynamic_graph::DynGraph as Graph;
use okm_steel::collection::StorageRegistry;
use okm_steel::hop::GraphRegistry;
use okm_steel::keyset::KeySetRegistry;
use okm_steel::okm_entry::{Engine, EngineBox};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use steel::SteelVal;

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
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), String> {
        self.0.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn del(&self, key: &[u8]) -> Result<(), String> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>, String> {
        let rows = self.0.lock().unwrap();
        let start = begin.to_vec();
        let iter: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match end {
            Some(e) => Box::new(rows.range(start..e.to_vec())),
            None => Box::new(rows.range(start..)),
        };
        Ok(iter.map(|(k, _)| k.clone()).collect())
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
        "indexes": [{ "name": "by_level", "slot": 16385, "fields": ["level"], "kind": "plain" }],
        "reduces": [{ "name": "total", "slot": 20481, "group": [], "kind": "count" }],
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
    reg.inject(engine.clone(), "Counters", &entry_json(), 41)
        .unwrap();
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
(define read (collection-fetch! "Counters" pkey))
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
    reg2.inject(engine.clone(), "Counters", &entry_json(), 41)
        .unwrap();
    let mut vm2 = build_vm(&reg2);
    let out2 = vm2
        .run(format!(
            r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(define read (collection-fetch! "Counters" pkey))
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
(define rows (collection-scan! "Counters" 16385 (list->vector (list 0 0 0 9))))
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
        .run(
            r#"
(define acc (collection-reduce-get! "Counters" (hash)))
(assert! (vector? acc))
(assert! (= (vector-length acc) 8))
'done
"#,
        )
        .expect("reduce read failed");
    assert_eq!(last_sym(out4), "done");
    let stored = engine.0.lock().unwrap();
    let acc_rows: Vec<_> = stored
        .iter()
        .filter(|(k, v)| k.starts_with(&[0u8, 41]) && **v == 1u64.to_be_bytes().to_vec())
        .collect();
    assert!(
        !acc_rows.is_empty(),
        "the folded count acc lives in the host engine"
    );
    drop(stored);

    // Delete sweeps the index entry and removes the primary row.
    vm2.run(format!(
        r#"
(define schema (okm-schema-from-json! "{sj}"))
(define pkey (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(collection-purge! "Counters" pkey)
'done
"#
    ))
    .expect("delete failed");
    let rows = engine.0.lock().unwrap();
    let primary_gone = !rows
        .keys()
        .any(|k| k.starts_with(&[0u8, 41, 0u8]) && k.len() == 3 + 12);
    assert!(primary_gone, "the primary row left the host engine");
    let index_gone = !rows.keys().any(|k| k.starts_with(&[0u8, 41, 64u8, 1u8])); // slot 16385 = 0x4001
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
(void? (collection-fetch! "Counters" pkey))
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
        .run(r#"(collection-fetch! "Ghost" (list->vector (list 0 0 0 0 0 0 0 0 0 0 0 0)))"#)
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
    GraphRegistry::register_stubs(&mut vm);
    vm.run(r#"(define (handler ev) (collection-put! "Counters" (list->vector (list 0)) (hash)))"#)
        .expect("handler referencing the stubbed fns must load under stubs");
    let err = vm
        .run(r#"(handler 1)"#)
        .expect_err("the stub arm names itself");
    assert!(format!("{err:?}").contains("introspection only"), "{err:?}");
    // The hop verbs carry the same trap (a traversal handler references them at
    // load time), so they carry the same stubs.
    vm.run(r#"(define (hop ev) (graph-hop-open "G" (list 16 (list->vector (list 0))) 'out #f))"#)
        .expect("a handler referencing the hop fns must load under stubs");
    let err = vm.run(r#"(hop 1)"#).expect_err("the hop stub names itself");
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
        "indexes": [{ "slot": 16385, "fields": [], "kind": "func" }],
    });
    let err = StorageRegistry::default()
        .inject(engine, "Bad", &entry, 41)
        .unwrap_err();
    assert!(err.contains("host callable"), "{err}");
}

#[test]
fn script_stamps_the_table_and_records_its_own_name() {
    // ADR-0043 at the script face: the record the typed and dynamic paths
    // write, reached through the collection's declared name. `void?` before the
    // stamp is the honest answer (nothing describes this store yet), and the
    // second registry proves the row lives in the HOST engine rather than in
    // the first registry's memory.
    let engine = Arc::new(MemEngine(Mutex::new(Default::default())));
    let reg = StorageRegistry::default();
    reg.inject(engine.clone(), "Counters", &entry_json(), 41)
        .unwrap();
    let mut vm = build_vm(&reg);

    let out = vm
        .run(
            r#"
(assert! (void? (collection-meta! "Counters")))
(define rec (collection-ensure-layout! "Counters"))
(define v (hash-ref rec "version"))
(assert! (vector? v))
(assert! (= (vector-length v) 1))
(assert! (= (vector-ref v 0) 3))
(assert! (equal? (collection-meta! "Counters") rec))
(define rec2 (collection-set-meta! "Counters" "build" (list->vector (list 1 2))))
(assert! (equal? (hash-ref rec2 "build") (list->vector (list 1 2))))
(assert! (= (vector-ref (hash-ref rec2 "version") 0) 3))
(define rec3 (collection-set-meta! "Counters" "version" (list->vector (list 9))))
(assert! (= (vector-ref (hash-ref rec3 "version") 0) 3))
'done
"#,
        )
        .expect("meta verbs failed");
    assert_eq!(last_sym(out), "done");

    let reg2 = StorageRegistry::default();
    reg2.inject(engine, "Counters", &entry_json(), 41).unwrap();
    let mut vm2 = build_vm(&reg2);
    let out2 = vm2
        .run(
            r#"
(define rec (collection-meta! "Counters"))
(assert! (equal? (hash-ref rec "build") (list->vector (list 1 2))))
(assert! (= (vector-ref (hash-ref rec "version") 0) 3))
'done
"#,
        )
        .expect("cross-check read failed");
    assert_eq!(last_sym(out2), "done");
}

// ---------------------------------------------------------------------------
// The graph hop cursor — a pull shape, because Steel cannot apply a closure
// ---------------------------------------------------------------------------

/// The graph table's own ns — a second keyspace, beside the collection's.
const GRAPH_NS: u16 = 0x0190;
/// The member tables: the endpoints an edge names.
const PERSON_NS: u16 = 16;
const ORG_NS: u16 = 17;

fn node(ns: u16, key: u64) -> NodeRef {
    NodeRef::new(&ns.to_be_bytes(), &key.to_be_bytes())
}

/// A graph declaration as an export carries it. The member width table is
/// declared even though a READ never consults it: an edge key is parsed by it
/// (ADR-0067), so a declaration that cannot say how wide its endpoints are
/// could not parse one of its own rows.
fn graph_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "edges",
        "key_len": 8,
        "key_fields": [{"name": "id", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
        "layout_version": 1,
        "hot_width": 8,
        "hot_fields": [{"name": "at", "ty": "U64", "width": 8, "offset": 0, "tag": 0}],
        "cold_fields": [],
        "members": [16, 17],
        "member_widths": [[16, 8], [17, 8]],
        "indexes": [],
        "slots": {"primary": 0, "dynamic": 1, "field_vocab_id": 2, "field_vocab_name": 3,
                  "declared_index_base": 16384, "declared_reduce_base": 20480,
                  "junction_base": 12288}
    })
}

/// An engine that counts its POINT READS, so "early stop is real" is a number
/// rather than a claim: a walk that answers one entry must not pay for the
/// entries it never answers.
struct CountingEngine {
    rows: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
    gets: std::sync::atomic::AtomicUsize,
}

impl CountingEngine {
    fn new() -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            gets: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn reset(&self) {
        self.gets.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Engine for CountingEngine {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), String> {
        self.rows.lock().unwrap().insert(key, value);
        Ok(())
    }
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// Write edges the HOST-side way: the binding is a read face for traversal, and
/// `incorporate` is the dynamic graph's single write entry point. This is also the
/// proof that the carrier and the script share one keyspace — the walk below
/// reads rows this call wrote behind the same `Arc`.
fn write_edges(engine: &Arc<CountingEngine>, edges: &[(NodeRef, &str, NodeRef)]) {
    let schema: CollectionSchema = serde_json::from_value(graph_schema()).unwrap();
    let mut g: Graph<EngineBox> = Graph::new(EngineBox::Injected(engine.clone()), GRAPH_NS, schema);
    let decls = edges.iter().map(|(src, predicate, dst)| {
        Decl::edge(DynEdge {
            src: src.clone(),
            dst: dst.clone(),
            predicate: (*predicate).to_string(),
            attrs: Dict::new(),
        })
    });
    g.incorporate(DynFragment::new(decls))
        .unwrap_or_else(|e| panic!("incorporate: {e}"));
}

/// The session's VM: the codec fns plus a registry holding one injected graph,
/// `"G"`, at the graph declaration's own ns.
fn graph_vm(engine: &Arc<CountingEngine>) -> (GraphRegistry, steel::steel_vm::engine::Engine) {
    let reg = GraphRegistry::default();
    reg.inject(
        engine.clone(),
        "G",
        &serde_json::json!({ "schema": graph_schema() }),
        GRAPH_NS,
    )
    .expect("the graph injects");
    let mut vm = steel::steel_vm::engine::Engine::new();
    okm_steel::register(&mut vm);
    reg.register_into(&mut vm);
    (reg, vm)
}

fn last_int(out: &[SteelVal]) -> isize {
    match out.last() {
        Some(SteelVal::IntV(i)) => *i,
        other => panic!("expected an integer answer, got {other:?}"),
    }
}

/// The node literal for person 1, as a script writes it: `(ns key)`, the key
/// being the pkey byte vector the collection verbs hand back.
const PERSON_1: &str = "(list 16 (list->vector (list 0 0 0 0 0 0 0 1)))";

/// The same graph three ways out of person 1 — depth the early-stop claim needs.
fn three_out_edges(engine: &Arc<CountingEngine>) {
    write_edges(
        engine,
        &[
            (node(PERSON_NS, 1), "works_at", node(ORG_NS, 1)),
            (node(PERSON_NS, 1), "works_at", node(ORG_NS, 2)),
            (node(PERSON_NS, 1), "works_at", node(ORG_NS, 3)),
        ],
    );
}

#[test]
fn hop_cursor_pulls_one_entry_and_stops_the_walk() {
    // The task's core claim: early stop is REAL. The script opens a walk over a
    // node with three out-edges, pulls ONE hop, closes — and pays for exactly
    // one entry's resolution. Nothing is materialised into a list, and no entry
    // is decoded ahead of the one answered (a read-ahead cursor would make the
    // two read counts equal).
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (_reg, mut vm) = graph_vm(&engine);

    engine.reset();
    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'out #f))
(define e (graph-hop-next! c))
(assert! (= (car e) 17))
(assert! (= (vector-length (cadr e)) 8))
(assert! (string=? (caddr e) "works_at"))
(assert! (graph-hop-close! c))
(if e 1 0)
"#
        ))
        .expect("the one-hop script");
    assert_eq!(last_int(&out), 1, "the script saw exactly one entry");
    let one = engine.gets();

    engine.reset();
    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define (drain c n) (if (graph-hop-next! c) (drain c (+ n 1)) n))
(define c (graph-hop-open "G" n 'out #f))
(define total (drain c 0))
(graph-hop-close! c)
total
"#
        ))
        .expect("the drain script");
    assert_eq!(last_int(&out), 3, "the same walk read to the end");
    let all = engine.gets();

    assert!(
        one < all,
        "stopping after one entry must cost less than walking three: {one} vs {all} reads"
    );
    // Two reads per ANSWERED entry — the far endpoint through the registry and
    // the predicate's vocabulary name. The scanned prefix is shared, so the two
    // entries the script never asked for cost it nothing.
    assert_eq!(
        all - one,
        4,
        "each entry the script did not answer costs the binding 2 reads, so 2 \
         extra entries is 4; anything else means the cursor decoded ahead \
         ({one} reads for one entry, {all} for three)"
    );
}

#[test]
fn hop_cursor_drains_every_entry_then_answers_false_twice() {
    // Exhaustion is an ANSWER, not a fault: the walk hands back all three
    // entries and then `#f` — twice, with no panic and no restart on the
    // second call (a cursor that reopened on a spent scan would answer 3 again
    // and the recursion below would never end).
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (_reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define (drain c n) (if (graph-hop-next! c) (drain c (+ n 1)) n))
(define c (graph-hop-open "G" n 'out #f))
(define total (drain c 0))
(assert! (equal? (graph-hop-next! c) #f))
(assert! (equal? (graph-hop-next! c) #f))
(graph-hop-close! c)
total
"#
        ))
        .expect("the drain script");
    assert_eq!(last_int(&out), 3);

    // Two more pulls past the end, driven from Rust: still `#f`, still no panic.
    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'out #f))
(define (drain c n) (if (graph-hop-next! c) (drain c (+ n 1)) n))
(define total (drain c 0))
(list total (if (graph-hop-next! c) 1 0) (if (graph-hop-next! c) 1 0))
"#
        ))
        .expect("the over-drain script");
    let Some(SteelVal::ListV(tail)) = out.last() else {
        panic!(
            "expected the (total after after) list, got {:?}",
            out.last()
        );
    };
    let reads: Vec<isize> = tail
        .iter()
        .map(|v| match v {
            SteelVal::IntV(i) => *i,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(reads, vec![3, 0, 0], "3 entries, then #f, then #f again");
}

#[test]
fn hop_cursor_walks_in_and_out() {
    // `direction` is the only difference between the two faces: `walk` is
    // `out`'s chainable alias (a hop is a hop), `in` is the mirror, so the same
    // node answers its successors and its predecessors.
    let engine = Arc::new(CountingEngine::new());
    write_edges(
        &engine,
        &[
            (node(PERSON_NS, 1), "works_at", node(ORG_NS, 1)),
            (node(PERSON_NS, 2), "knows", node(PERSON_NS, 1)),
        ],
    );
    let (_reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'walk #f))
(define e (graph-hop-next! c))
(assert! (= (car e) 17))
(assert! (equal? (graph-hop-next! c) #f))
(graph-hop-close! c)
(define back (graph-hop-open "G" n 'in #f))
(define p (graph-hop-next! back))
(assert! (= (car p) 16))
(assert! (= (vector-ref (cadr p) 7) 2))
(assert! (string=? (caddr p) "knows"))
(assert! (equal? (graph-hop-next! back) #f))
(graph-hop-close! back)
(if (and e p) 1 0)
"#
        ))
        .expect("the two-direction script");
    assert_eq!(
        last_int(&out),
        1,
        "out found the successor, in found the predecessor"
    );
}

#[test]
fn hop_cursor_pushes_a_named_predicate_down_and_false_means_any() {
    // A named predicate narrows the walk to that predicate's own face; `#f` is
    // "any", and a name nobody interned is an EMPTY walk rather than an error
    // (no edge can carry a predicate nobody wrote).
    let engine = Arc::new(CountingEngine::new());
    write_edges(
        &engine,
        &[
            (node(PERSON_NS, 1), "works_at", node(ORG_NS, 1)),
            (node(PERSON_NS, 1), "knows", node(PERSON_NS, 2)),
        ],
    );
    let (_reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define typed (graph-hop-open "G" n 'out "works_at"))
(define e (graph-hop-next! typed))
(assert! (= (car e) 17))
(assert! (equal? (graph-hop-next! typed) #f))
(graph-hop-close! typed)
(define (drain c n) (if (graph-hop-next! c) (drain c (+ n 1)) n))
(define any (graph-hop-open "G" n 'out #f))
(define total (drain any 0))
(graph-hop-close! any)
(define none (graph-hop-open "G" n 'out "nobody"))
(assert! (equal? (graph-hop-next! none) #f))
(graph-hop-close! none)
total
"#
        ))
        .expect("the predicate script");
    assert_eq!(last_int(&out), 2, "#f walked both predicates");
}

#[test]
fn hop_cursor_close_is_idempotent_and_next_after_close_is_an_error() {
    // `close!` answers `#t` for a live walk and `#f` for one already closed —
    // the same call twice is fine, so a script can close in a cleanup path
    // without tracking state. A `next!` on a closed handle is a loud error
    // naming the handle, never a panic and never a silent `#f` (which would
    // read as "no more neighbours").
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (_reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'out #f))
(assert! (graph-hop-close! c))
(assert! (not (graph-hop-close! c)))
'closed
"#
        ))
        .expect("close twice is fine");
    assert_eq!(last_sym(out), "closed");

    let err = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'out #f))
(graph-hop-close! c)
(graph-hop-next! c)
"#
        ))
        .expect_err("a closed cursor must not answer");
    let text = format!("{err:?}");
    assert!(text.contains("not open"), "{text}");
}

#[test]
fn a_cursor_never_closed_is_released_with_the_session() {
    // The table entry IS the scan, so the walk nobody closed is still a walk
    // the registry owns: the carrier's teardown releases it, and releasing
    // twice is a no-op. Nothing is buffered, so there is no materialised run
    // left behind either (the engine below still answers afterwards).
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(format!(
            r#"
(define n {PERSON_1})
(define c (graph-hop-open "G" n 'out #f))
(if (graph-hop-next! c) 1 0)
"#
        ))
        .expect("the abandoned script");
    assert_eq!(last_int(&out), 1);
    drop(vm);

    assert_eq!(reg.close_all(), 1, "the open walk was still owned");
    assert_eq!(reg.close_all(), 0, "and releasing it again is a no-op");
    // The store is untouched by all of that: a second session over the same
    // engine still walks the same edges.
    let (_reg2, mut vm2) = graph_vm(&engine);
    let out = vm2
        .run(format!(
            r#"
(define n {PERSON_1})
(define (drain c n) (if (graph-hop-next! c) (drain c (+ n 1)) n))
(define c (graph-hop-open "G" n 'out #f))
(define total (drain c 0))
(graph-hop-close! c)
total
"#
        ))
        .expect("a fresh session walks the same graph");
    assert_eq!(last_int(&out), 3);
}

#[test]
fn a_node_the_graph_never_saw_is_an_empty_walk() {
    // A node with no handle has no face at all — an empty walk, not a fault
    // (nothing names it, so no edge can touch it). The same is true of a graph
    // name nobody injected, except that one is a loud error: a typo in the
    // declaration is not an empty answer.
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (_reg, mut vm) = graph_vm(&engine);

    let out = vm
        .run(
            r#"
(define n (list 16 (list->vector (list 0 0 0 0 0 0 0 99))))
(define c (graph-hop-open "G" n 'out #f))
(assert! (equal? (graph-hop-next! c) #f))
(graph-hop-close! c)
'done
"#,
        )
        .expect("an unseen node walks empty");
    assert_eq!(last_sym(out), "done");

    let err = vm
        .run(r#"(graph-hop-open "Ghost" (list 16 (list->vector (list 0))) 'out #f)"#)
        .expect_err("an undeclared graph must fail");
    assert!(format!("{err:?}").contains("Ghost"), "{err:?}");
}

// ---------------------------------------------------------------------------
// The key set face — set algebra across the boundary, one fetch after
// ---------------------------------------------------------------------------

/// The session a key-set script needs: a graph AND a collection over one host
/// engine (so a site name resolves to either), plus the set table. Every piece
/// is per-VM, exactly as a carrier builds them.
fn keyset_vm(
    engine: &Arc<CountingEngine>,
) -> (
    GraphRegistry,
    KeySetRegistry,
    steel::steel_vm::engine::Engine,
) {
    let graphs = GraphRegistry::default();
    graphs
        .inject(
            engine.clone(),
            "G",
            &serde_json::json!({ "schema": graph_schema() }),
            GRAPH_NS,
        )
        .expect("the graph injects");
    let collections = StorageRegistry::default();
    collections
        .inject(engine.clone(), "Counters", &entry_json(), 41)
        .expect("the collection injects");
    let sets = KeySetRegistry::default();
    let mut vm = steel::steel_vm::engine::Engine::new();
    okm_steel::register(&mut vm);
    collections.register_into(&mut vm);
    graphs.register_into(&mut vm);
    sets.register_into(&mut vm, &collections, &graphs);
    (graphs, sets, vm)
}

/// The byte vector a script wrote (a `[key row]` pair's side).
fn bytes_of(v: &SteelVal) -> Vec<u8> {
    let SteelVal::VectorV(vec) = v else {
        panic!("expected a byte vector, got {v:?}");
    };
    vec.iter()
        .map(|item| match item {
            SteelVal::IntV(i) => u8::try_from(*i).expect("a byte"),
            other => panic!("expected a byte, got {other:?}"),
        })
        .collect()
}

/// A list or vector of key byte vectors — the shape `collection-keys` answers
/// and the shape `key-set-of` takes.
fn key_list(v: &SteelVal) -> Vec<Vec<u8>> {
    let items: Vec<SteelVal> = match v {
        SteelVal::ListV(items) => items.iter().cloned().collect(),
        SteelVal::VectorV(items) => items.iter().cloned().collect(),
        other => panic!("expected a list or vector of key byte vectors, got {other:?}"),
    };
    items.iter().map(bytes_of).collect()
}

/// A `key-fetch!` answer parsed back: `(key, row)` per surviving key, the row
/// `None` for the `void` a missing entry answers.
fn rows_of(v: &SteelVal) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let SteelVal::VectorV(rows) = v else {
        panic!("expected a vector of [key row] pairs, got {v:?}");
    };
    rows.iter()
        .map(|row| {
            let SteelVal::VectorV(pair) = row else {
                panic!("expected a [key row] pair, got {row:?}");
            };
            assert_eq!(pair.len(), 2, "a row is a pair");
            let value = match &pair[1] {
                SteelVal::Void => None,
                other => Some(bytes_of(other)),
            };
            (bytes_of(&pair[0]), value)
        })
        .collect()
}

/// The two-step semantics, across the script boundary and in TWO calls: two sets
/// drained off REAL walks, intersected/differenced inside one call each, then one
/// `key-fetch!` that pays exactly one point read per surviving key.
///
/// The counter is the claim. A per-key loop in the script would pay a read per
/// key of BOTH sides (five, here, plus the two the algebra keeps); the two-step
/// form pays two — one per SURVIVOR — and the algebra itself reads nothing.
#[test]
fn key_sets_intersect_then_fetch_in_one_call() {
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (_graphs, _sets, mut vm) = keyset_vm(&engine);

    // Stage one: the sets, off two walks of the same face. The second walk is
    // pulled one entry then drained, so it holds the LAST TWO of the three keys
    // — a genuine partial overlap, not two copies of one answer.
    vm.run(format!(
        r#"
(define n {PERSON_1})
(define all (key-collect! (graph-hop-open "G" n 'out #f)))
(define c (graph-hop-open "G" n 'out #f))
(define first (graph-hop-next! c))
(define rest (key-collect! c))
(define both (key-set-and! all rest))
(define only (key-set-minus! all rest))
(define either (key-set-or! both only))
(if first 'walked 'empty)
"#
    ))
    .expect("the set-building script");

    // Stage two: ONE call turns the survivors into rows, and it costs one read
    // per survivor — two for the intersection.
    engine.reset();
    let out = vm
        .run(r#"(key-fetch! both "G")"#)
        .expect("the intersection fetch");
    let both_rows = rows_of(out.last().expect("a fetch answer"));
    assert_eq!(both_rows.len(), 2, "two of the three keys are shared");
    assert_eq!(
        engine.gets(),
        2,
        "one point read per SURVIVING key: the algebra reads nothing, and the \
         keys it dropped are never read at all"
    );

    // Every row is the store's own row at that key — same key, same bytes.
    {
        let stored = engine.rows.lock().unwrap();
        for (key, value) in &both_rows {
            assert_eq!(
                stored.get(key),
                value.as_ref(),
                "the fetched row is the entry the key names"
            );
            assert!(value.is_some(), "a live edge entry carries its value");
        }
    }

    // The difference is the entry the second walk never reached; the union of
    // the two halves is the whole face again (sets are values, so `all` still
    // answers three keys after two operators have read it).
    engine.reset();
    let out = vm
        .run(r#"(list (key-fetch! only "G") (key-fetch! either "G") (key-fetch! all "G"))"#)
        .expect("the difference / union / re-read script");
    let Some(SteelVal::ListV(answers)) = out.last() else {
        panic!("expected the three fetches, got {:?}", out.last());
    };
    let answers: Vec<SteelVal> = answers.iter().cloned().collect();
    let only_rows = rows_of(&answers[0]);
    let either_rows = rows_of(&answers[1]);
    let all_rows = rows_of(&answers[2]);

    assert_eq!(only_rows.len(), 1, "one key of the three is in `all` alone");
    assert_eq!(
        either_rows.len(),
        3,
        "both halves together are the whole face"
    );
    assert_eq!(
        all_rows.len(),
        3,
        "the inputs survived the operators: a set is a value, not a cursor"
    );

    let intersection_keys: Vec<Vec<u8>> = both_rows.iter().map(|(k, _)| k.clone()).collect();
    let difference_keys: Vec<Vec<u8>> = only_rows.iter().map(|(k, _)| k.clone()).collect();
    assert!(
        difference_keys
            .iter()
            .all(|key| !intersection_keys.contains(key)),
        "the difference and the intersection do not overlap"
    );
    let mut rebuilt: Vec<Vec<u8>> = intersection_keys.clone();
    rebuilt.extend(difference_keys);
    rebuilt.sort_unstable();
    let all_keys: Vec<Vec<u8>> = all_rows.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(
        rebuilt, all_keys,
        "the intersection plus the difference is the left side, key for key"
    );
    // The union answered in KEY ORDER, which is the container's ordering
    // discipline crossing the boundary intact.
    let either_keys: Vec<Vec<u8>> = either_rows.iter().map(|(k, _)| k.clone()).collect();
    let mut sorted = either_keys.clone();
    sorted.sort_unstable();
    assert_eq!(either_keys, sorted, "a set iterates in ascending key order");
}

/// The whole collection-side flow, end to end: documents written through the
/// script verb, their keys taken from a REAL scan (`collection-keys` — the
/// script writes no key bytes of its own), narrowed against another set, and
/// the survivors fetched as documents.
///
/// Then the same flow from the OTHER producer: a declared index face answers
/// its raw entry keys (`collection-index-keys`), so a set can be built from
/// what an access method says, narrowed against the document-key list and
/// fetched — the index scan decodes no document, and only the survivors are
/// read.
///
/// Three claims are counted or compared, not narrated: the store holds two
/// slots per document while the key list answers ONE key each (the scan merges
/// the row and the segment); the scanned list is the encoded pkeys, key for
/// key; and the fetch pays the document's two slots per key it reads, so the
/// three keys fetched across the intersection and the difference cost six.
#[test]
fn key_set_of_fetches_documents_and_a_released_handle_is_refused() {
    let engine = Arc::new(CountingEngine::new());
    let (_graphs, sets, mut vm) = keyset_vm(&engine);
    let sj = quote(&schema_json());

    // Stage one: three documents through the one write verb. The script never
    // writes a key itself — it encodes the pkeys it will narrow against, and the
    // table's own key list supplies the rest.
    vm.run(format!(
        r#"
(define schema (okm-schema-from-json! "{sj}"))
(define alice (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(define bob (okm-encode-key! schema (hash "org_id" 1 "user_id" 3)))
(define carol (okm-encode-key! schema (hash "org_id" 1 "user_id" 4)))
(collection-put! "Counters" alice (hash "org_id" 1 "user_id" 2 "level" 9 "score" 500 "name" "alice"))
(collection-put! "Counters" bob (hash "org_id" 1 "user_id" 3 "level" 4 "score" 100 "name" "bob"))
(collection-put! "Counters" carol (hash "org_id" 1 "user_id" 4 "level" 9 "score" 250 "name" "carol"))
'written
"#
    ))
    .expect("the three writes");

    // Stage two: the KEY LIST. A put leaves two entries per document — the
    // primary row and the dynamic segment — and the list answers ONE key each:
    // the two slots of one document are one key, merged by the scan.
    //
    // The scan is KEY-ONLY: no document is decoded to learn its key. The host
    // engine's face answers a scan as keys, so the pair iterator behind it reads
    // each value back and throws it away — that cost belongs to the boundary,
    // not to this verb, and it is why only the ANSWER's shape is asserted here.
    let stored_slots = {
        let rows = engine.rows.lock().unwrap();
        rows.keys()
            .filter(|k| k.starts_with(&[0, 41, 0]) || k.starts_with(&[0, 41, 1]))
            .count()
    };
    assert_eq!(stored_slots, 6, "three documents, two slots each");

    vm.run(r#"(define keys (collection-keys "Counters"))"#)
        .expect("the key list");
    let out = vm.run(r#"(vector-length keys)"#).expect("the key count");
    assert_eq!(
        out.last(),
        Some(&SteelVal::IntV(3)),
        "six entries, three keys: the scan merges one document's two slots"
    );

    // Stage three: the scanned list, the intersection with a hand-built set,
    // and the difference — then ONE fetch per set over the survivors.
    engine.reset();
    let out = vm
        .run(
            r#"
(define all (key-set-of keys))
(define mine (key-set-of (list bob)))
(define both (key-set-and! all mine))
(define rest (key-set-minus! all mine))
(define rows (key-fetch! both "Counters"))
(assert! (= (vector-length keys) 3))
(assert! (= (vector-length rows) 1))
(assert! (equal? (vector-ref (vector-ref rows 0) 0) bob))
(assert! (string=? (hash-ref (vector-ref (vector-ref rows 0) 1) "name") "bob"))
(assert! (= (hash-ref (vector-ref (vector-ref rows 0) 1) "level") 4))
(assert! (= (vector-length (key-fetch! rest "Counters")) 2))
'done
"#,
        )
        .expect("the scanned set, the algebra and the fetch");
    assert_eq!(last_sym(out), "done");
    assert_eq!(
        engine.gets(),
        6,
        "two point reads per FETCHED key — the row and its dynamic segment are \
         one document — so the intersection's one key plus the difference's two \
         cost six, while the algebra, which touched all three, cost nothing"
    );

    // Stage four: the OTHER producer — an index face. `collection-index-keys`
    // answers the face's raw ENTRY keys (`[ns][slot][field values][pkey]`), so
    // a set can be built from what an access method says without decoding a
    // document. The identity a face names is the key's TAIL, which is why the
    // script takes the last twelve bytes before building the set: entry keys
    // and pkeys are different key spaces, and intersecting them directly
    // answers nothing (asserted, so the difference stays visible).
    //
    // One read here, and it is the BOUNDARY's: a host `Engine` answers a scan
    // as keys only, so the adapter reads each entry's value back and drops it
    // (the same cost `collection-keys` pays). No document is decoded, and the
    // algebra over the sets reads nothing at all.
    engine.reset();
    let out = vm
        .run(
            r#"
(define tail-of (lambda (k) (vector-copy k (- (vector-length k) 12) (vector-length k))))
(define ekeys (collection-index-keys "Counters" 16385 (list->vector (list 0 0 0 4))))
(define ek-set (key-set-of ekeys))
(define raw (key-set-and! ek-set all))
(define idx (key-set-of (map tail-of ekeys)))
(define from-index (key-set-and! idx all))
(assert! (= (vector-length ekeys) 1))
(assert! (= (vector-length (vector-ref ekeys 0)) 20))
(assert! (= (vector-length (key-fetch! raw "Counters")) 0))
(key-set-close! raw)
(key-set-close! ek-set)
'produced
"#,
        )
        .expect("the index-side producer and its set");
    assert_eq!(last_sym(out), "produced");
    assert_eq!(
        engine.gets(),
        1,
        "the index scan's one entry costs the boundary's read-back and nothing \
         else; the algebra and the disjoint fetch read nothing"
    );

    // ... and the survivors of the intersection are ordinary documents.
    engine.reset();
    let out = vm
        .run(
            r#"
(define index-rows (key-fetch! from-index "Counters"))
(assert! (= (vector-length index-rows) 1))
(assert! (equal? (vector-ref (vector-ref index-rows 0) 0) bob))
(assert! (string=? (hash-ref (vector-ref (vector-ref index-rows 0) 1) "name") "bob"))
(key-set-close! idx)
(key-set-close! from-index)
'fetched
"#,
        )
        .expect("the index-derived fetch");
    assert_eq!(last_sym(out), "fetched");
    assert_eq!(
        engine.gets(),
        2,
        "one point read per surviving key: the row and its dynamic segment are \
         one document, and the index side of the flow fetched no document at all"
    );

    // The entry key is the documented layout, byte for byte: the face prefix
    // `[ns 0x00 0x29][slot 0x40 0x01]`, the declared field `level` (u32 BE), the
    // pkey tail — which is bob's own `collection-keys` bytes.
    let out = vm
        .run(r#"(list ekeys bob)"#)
        .expect("the entry keys and the pkey");
    let Some(SteelVal::ListV(halves)) = out.last() else {
        panic!("expected the entry keys and the pkey, got {:?}", out.last());
    };
    let halves: Vec<SteelVal> = halves.iter().cloned().collect();
    let entry_keys = key_list(&halves[0]);
    let bob_key = bytes_of(&halves[1]);
    assert_eq!(
        entry_keys,
        vec![{
            let mut expected = vec![0u8, 41, 0x40, 0x01, 0, 0, 0, 4];
            expected.extend_from_slice(&bob_key);
            expected
        }],
        "the entry key is [ns][slot][level u32][pkey]: the face prefix, the \
         declared field, then the document's own key bytes"
    );

    // The scan answered exactly the pkeys the codec produced — no index entry,
    // no ledger key, no meta record leaked into the list, and none missing.
    let out = vm
        .run(r#"(list keys (list alice bob carol))"#)
        .expect("both key lists");
    let Some(SteelVal::ListV(halves)) = out.last() else {
        panic!("expected the two key lists, got {:?}", out.last());
    };
    let halves: Vec<SteelVal> = halves.iter().cloned().collect();
    let mut scanned = key_list(&halves[0]);
    let mut written = key_list(&halves[1]);
    assert_eq!(scanned.len(), 3, "one key per document");
    scanned.sort_unstable();
    written.sort_unstable();
    assert_eq!(
        scanned, written,
        "the scanned key set is the collection's documents, key for key"
    );

    // A set is a set: input order and duplicates do not survive, and a key the
    // store does not hold answers `void` rather than dropping out of the answer.
    let out = vm
        .run(
            r#"
(define s (key-set-of (list (list->vector (list 9 9)) (list->vector (list 1 1)) (list->vector (list 9 9)))))
(define rows (key-fetch! s "G"))
(list (vector-length rows)
      (equal? (vector-ref (vector-ref rows 0) 0) (list->vector (list 1 1)))
      (void? (vector-ref (vector-ref rows 0) 1))
      (equal? (vector-ref (vector-ref rows 1) 0) (list->vector (list 9 9))))
"#,
        )
        .expect("the byte-key set");
    let Some(SteelVal::ListV(checks)) = out.last() else {
        panic!("expected the checks, got {:?}", out.last());
    };
    let checks: Vec<SteelVal> = checks.iter().cloned().collect();
    assert_eq!(
        checks[0],
        SteelVal::IntV(2),
        "two distinct keys, twice written, once each"
    );
    assert_eq!(checks[1], SteelVal::BoolV(true), "ascending key order");
    assert_eq!(
        checks[2],
        SteelVal::BoolV(true),
        "a key with no entry answers void, in its own row"
    );
    assert_eq!(checks[3], SteelVal::BoolV(true));

    // Release: `#t` for a live set, `#f` for one already released, and every
    // verb that takes a handle refuses the spent one by name.
    let out = vm
        .run(
            r#"(define k (key-set-of (list)))
(assert! (key-set-close! k))
(assert! (not (key-set-close! k)))
'closed"#,
        )
        .expect("close twice is fine");
    assert_eq!(last_sym(out), "closed");

    let err = vm
        .run(r#"(key-set-and! k k)"#)
        .expect_err("a released set must not be an operand");
    assert!(
        format!("{err:?}").contains("not open"),
        "the refusal names the handle: {err:?}"
    );
    let err = vm
        .run(r#"(key-fetch! k "G")"#)
        .expect_err("a released set must not be fetched");
    assert!(format!("{err:?}").contains("not open"), "{err:?}");

    // ... and the session teardown owns whatever a script never released: the
    // four sets the scanned flow built (the key list's, the single-key one, the
    // intersection and the difference), the byte-key set, and nothing of `k`'s
    // (which the script released itself).
    assert_eq!(sets.close_all(), 5, "the five sets the script left live");
    assert_eq!(sets.close_all(), 0, "releasing again is a no-op");
}

/// The SITE name is the store a fetch reads, and it is resolved, never
/// inherited: an unknown name and a name that means two different tables are
/// both loud errors rather than a set of `void` rows.
#[test]
fn key_fetch_refuses_an_unknown_or_ambiguous_site() {
    let engine = Arc::new(CountingEngine::new());
    three_out_edges(&engine);
    let (graphs, _sets, mut vm) = keyset_vm(&engine);
    // A graph under the collection's name: two tables, one name.
    graphs
        .inject(
            engine.clone(),
            "Counters",
            &serde_json::json!({ "schema": graph_schema() }),
            GRAPH_NS,
        )
        .expect("the twin graph injects");

    vm.run(r#"(define k (key-set-of (list)))"#)
        .expect("an empty set");

    let err = vm
        .run(r#"(key-fetch! k "Ghost")"#)
        .expect_err("an unknown site must fail");
    assert!(format!("{err:?}").contains("Ghost"), "{err:?}");

    let err = vm
        .run(r#"(key-fetch! k "Counters")"#)
        .expect_err("an ambiguous site must fail");
    let text = format!("{err:?}");
    assert!(
        text.contains("both a collection and a graph"),
        "the refusal says which ambiguity it is: {text}"
    );
}
