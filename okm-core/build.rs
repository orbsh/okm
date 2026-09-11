//! Build script — collect every `#[kv_subscribe(Enum::Variant)]` row in
//! this crate and generate the event enum + its channel module
//! (`::okm_subscribe`) into OUT_DIR. Reruns whenever any source file
//! changes (`rerun-if-changed=src/` is the Cargo contract that makes the
//! collection reliable — the structural fix proc-macro file exchange
//! cannot provide; see PLAN event-enum decision).

use std::fs;
use std::path::{Path, PathBuf};

use quote::ToTokens;

fn collect_subscribe_rows(dir: &Path, acc: &mut Vec<(String, String, String, String)>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("build.rs: read src/")
        .map(|e| e.expect("build.rs: dir entry").path())
        .collect();
    entries.sort(); // deterministic generation order
    for path in entries {
        if path.is_dir() {
            collect_subscribe_rows(&path, acc);
        } else if path.extension().is_some_and(|e| e == "rs") {
            scan_file(&path, acc);
        }
    }
}

/// Extract the key type from `#[kv_ref(KeyTy)]` (the derive's proxy-key
/// annotation) — the generated enum variant needs a concrete
/// `Event<KeyTy, Row>` payload type, and key/row live in the consuming
/// crate's own scope, so bare names are emitted.
fn key_type_of(s: &syn::ItemStruct) -> Option<String> {
    s.attrs
        .iter()
        .filter(|a| a.path().is_ident("kv_ref"))
        .filter_map(|a| a.parse_args::<syn::ExprPath>().ok())
        .next()
        .map(|p| p.to_token_stream().to_string().replace(' ', ""))
}

/// One pass with syn: find `#[derive(... RowEncode ...)]` structs carrying
/// `#[kv_subscribe]` (bare only). The variant IS the row type name; the
/// enum name comes from `#[kv_event_enum(Alias)]` (default `RowEvent`).
fn scan_file(path: &Path, acc: &mut Vec<(String, String, String, String)>) {
    let txt = fs::read_to_string(path).unwrap_or_else(|e| panic!("build.rs: read {path:?}: {e}"));
    let Ok(ast) = syn::parse_file(&txt) else {
        // Not parseable standalone (e.g. a fixture snippet) — skip; the
        // derive itself will fail with a proper span if it is real code.
        return;
    };
    for item in &ast.items {
        let syn::Item::Struct(s) = item else { continue };
        let has_row_encode = s
            .attrs
            .iter()
            .filter(|a| a.path().is_ident("derive"))
            .any(|a| a.to_token_stream().to_string().contains("RowEncode"));
        if !has_row_encode {
            continue;
        }
        let mut sub = false;
        let mut enum_name: Option<String> = None;
        for a in &s.attrs {
            if a.path().is_ident("kv_subscribe") {
                // Bare form only: any parenthesized argument is a user
                // error (variant paths are the unsupported hand-mapped
                // shape — the derive emits the send, so nothing to map).
                if a.parse_args::<syn::ExprPath>().is_ok() {
                    panic!(
                        "kv_subscribe[{}]: variant paths are not supported — declare \
                         `#[kv_subscribe]` (bare); the variant is the row type name",
                        s.ident
                    );
                }
                sub = true;
            }
            if a.path().is_ident("kv_event_enum") {
                let body = a
                    .parse_args::<syn::ExprPath>()
                    .ok()
                    .map(|p| p.to_token_stream().to_string().replace(' ', ""))
                    .unwrap_or_else(|| {
                        panic!(
                            "kv_event_enum[{}]: expected an enum name, e.g. `#[kv_event_enum(MyEvents)]`",
                            s.ident
                        )
                    });
                if body.split("::").count() != 1 {
                    panic!("kv_event_enum[{}]: expected a bare enum name, got `{body}`", s.ident);
                }
                enum_name = Some(body);
            }
        }
        if sub {
            let key_ty = key_type_of(s).unwrap_or_else(|| {
                panic!("kv_subscribe[{}]: no `#[kv_ref(KeyTy)]` found", s.ident)
            });
            let en = enum_name.unwrap_or_else(|| "RowEvent".to_string());
            // (enum name, variant = row name, row name, key type)
            acc.push((en, s.ident.to_string(), s.ident.to_string(), key_ty));
        }
    }
}

/// The static runtime half of the generated module — cell type shared by
/// every enum, defined once here rather than in okm_core::subscribe because it
/// is enum-form specific (carries the variant directly, not Event<K, R>).
const RUNTIME: &str = r#"
/// Typed sink cell for one event enum: producer side calls `emit`,
/// consumer registers the transport once via `register` (closure or any
/// `EventSink` impl — tokio mpsc, crossbeam, no-op; transport is an
/// assembly-site decision, same discipline as engine choice).
pub struct EnumChannel<E> {
    sink: std::sync::RwLock<Option<std::sync::Arc<dyn okm_core::subscribe::EventSink<E>>>>,
}

impl<E: 'static> EnumChannel<E> {
    pub const fn new() -> Self {
        Self { sink: std::sync::RwLock::new(None) }
    }

    /// Register the transport; a later registration replaces the
    /// previous sink (one stream, one consumer — fan-out is the
    /// consumer's job: register a sink that broadcasts).
    pub fn register(&self, sink: impl okm_core::subscribe::EventSink<E> + 'static) {
        *self.sink.write().unwrap() = Some(std::sync::Arc::new(sink));
    }

    pub fn has_sink(&self) -> bool {
        self.sink.read().unwrap().is_some()
    }

    /// Best-effort: `false` when no sink is registered or the sink
    /// rejected the event. Never blocks, never fails the write.
    pub fn emit(&self, event: E) -> bool {
        match &*self.sink.read().unwrap() {
            Some(s) => s.try_send(event),
            None => false,
        }
    }
}
"#;

fn main() {
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=tests/");
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    if Path::new("src").exists() {
        collect_subscribe_rows(Path::new("src"), &mut rows);
    }
    if Path::new("tests").exists() {
        collect_subscribe_rows(Path::new("tests"), &mut rows);
    }

    // Group by enum name (multi-enum support is free at this granularity).
    // Variants ARE the row type names — unique within a crate by
    // construction (two structs cannot share one type name), so no
    // duplicate check is needed here.
    type EnumGroup = Vec<(String, String, String)>; // (variant=row, row, key)
    let mut enums: Vec<(String, EnumGroup)> = Vec::new();
    for (en, var, row, key) in rows {
        match enums.iter_mut().find(|(e, _)| *e == en) {
            Some((_, v)) => v.push((var, row, key)),
            None => enums.push((en, vec![(var, row, key)])),
        }
    }

    let mut code = String::from(
        "// Generated by build.rs — DO NOT EDIT.\n\
         // Event enum(s) + channel cells collected from `#[kv_subscribe]` rows.\n\
         use okm_core::subscribe::Event;\n",
    );
    code.push_str(RUNTIME);
    for (en, variants) in &enums {
        let mut decl = format!(
            "\n/// Event enum `{en}` — one variant per subscribed row type.\n#[allow(dead_code)] // variants a test declares but never destructures\npub enum {en} {{\n"
        );
        for (var, row, key) in variants {
            decl.push_str(&format!(
                "    {var}(Event<super::{key}, super::{row}>),\n"
            ));
        }
        decl.push_str("}\n");
        code.push_str(&decl);
        let cell = format!("CHANNEL_{}", en.to_uppercase());
        code.push_str(&format!(
            "\npub static {cell}: EnumChannel<{en}> = EnumChannel::new();\n"
        ));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let dest = Path::new(&out_dir).join("okm_subscribe.rs");
    fs::write(&dest, code).expect("build.rs: write generated module");
}
