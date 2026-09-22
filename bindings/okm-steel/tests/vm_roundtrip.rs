//! Steel VM end-to-end: register the okm functions, run a scheme program
//! that parses the schema, encodes a payload, decodes it back, and checks
//! the values — mirroring the PyO3 verify.py round trip.
use okm_core::{KeyEncode, DocumentEncode, Document, schema::CollectionSchema};
use steel::steel_vm::engine::Engine;

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
    #[ok_default(3)]
    pub tier: u8,
    #[ok_default("eu".to_string())]
    pub region: String,
}

fn hex(h: &str) -> Vec<u8> {
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn steel_vm_roundtrip() {
    let schema = CollectionSchema::of::<UserKey, UserV3>();
    let schema_json = serde_json::to_string(&schema).unwrap();
    let row = UserV3 { level: 9, score: 500, name: "alice".into(), tier: 2, region: "us".into() };
    let key = UserKey { org_id: 1, user_id: 2 };
    let payload_hex: String = row.encode_payload().iter().map(|b| format!("{b:02x}")).collect();
    let key_hex: String = key.encode().iter().map(|b| format!("{b:02x}")).collect();

    let mut vm = Engine::new();
    okm_steel::register(&mut vm);

    let program = format!(
        r#"
(define (chunks n s) (if (<= (string-length s) n) (list s) (cons (substring s 0 n) (chunks n (substring s n (string-length s))))))
(define schema (okm-schema-from-json! "{schema_json}"))
(assert! (= (okm-schema-version! schema) 3))
;; Rust wrote; Steel reads.
(define payload (list->vector (map (lambda (h) (string->number h 16)) (chunks 2 "{payload_hex}"))))
(define key (list->vector (map (lambda (h) (string->number h 16)) (chunks 2 "{key_hex}"))))
(define vals (okm-decode-payload! schema payload))
(assert! (= (hash-ref vals "level") 9))
(assert! (= (hash-ref vals "score") 500))
(assert! (= (hash-ref vals "tier") 2))
(assert! (string=? (hash-ref vals "name") "alice"))
(assert! (string=? (hash-ref vals "region") "us"))
;; Steel writes; byte equality with the Rust derive.
(define enc (okm-encode-payload! schema (hash "level" 9 "score" 500 "name" "alice" "tier" 2 "region" "us")))
(assert! (equal? (vector->list enc) (vector->list payload)))
(define enc-key (okm-encode-key! schema (hash "org_id" 1 "user_id" 2)))
(assert! (equal? (vector->list enc-key) (vector->list key)))
'done
"#,
        schema_json = schema_json.replace('"', "\\\""),
        payload_hex = payload_hex,
        key_hex = key_hex,
    );

    let result = vm.run(program).expect("steel program failed");
    assert!(
        result
            .last()
            .map(|v| matches!(v, steel::SteelVal::SymbolV(s) if s.as_str() == "done"))
            .unwrap_or(false),
        "program did not complete: {result:?}"
    );
}
