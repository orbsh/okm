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
/// `#[kv_subscribe(Enum::Variant)]`.
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
        let mut sub: Option<(String, String)> = None;
        for a in &s.attrs {
            if a.path().is_ident("kv_subscribe") {
                // Bare form (no parens) → parse_args fails → variant None.
                // Annotated form: one `Enum::Variant` path.
                let body = a
                    .parse_args::<syn::ExprPath>()
                    .ok()
                    .map(|p| p.to_token_stream().to_string().replace(' ', ""));
                if let Some(p) = body {
                    let mut segs = p.split("::").filter(|s| !s.is_empty());
                    match (segs.next(), segs.next()) {
                        (Some(e), Some(v)) if segs.next().is_none() => {
                            sub = Some((e.to_string(), v.to_string()));
                        }
                        _ => panic!(
                            "kv_subscribe[{}]: expected `Enum::Variant`, got `{p}`",
                            s.ident
                        ),
                    }
                }
            }
        }
        if let Some((en, var)) = sub {
            let key_ty = key_type_of(s).unwrap_or_else(|| {
                panic!("kv_subscribe[{}]: no `#[kv_ref(KeyTy)]` found", s.ident)
            });
            acc.push((en, var, s.ident.to_string(), key_ty));
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
    let mut enums: Vec<(String, Vec<(String, String, String)>)> = Vec::new();
    for (en, var, row, key) in rows {
        match enums.iter_mut().find(|(e, _)| *e == en) {
            Some((_, v)) => v.push((var, row, key)),
            None => enums.push((en, vec![(var, row, key)])),
        }
    }
    if enums.iter().any(|(_, v)| v.len() > 1) {
        panic!("build.rs: duplicate variant within one event enum");
    }

    let mut code = String::from(
        "// Generated by build.rs — DO NOT EDIT.\n\
         // Event enum(s) + channel cells collected from `#[kv_subscribe]` rows.\n\
         use okm_core::subscribe::Event;\n",
    );
    code.push_str(RUNTIME);
    for (en, variants) in &enums {
        let mut decl = format!(
            "\n/// Event enum `{en}` — one variant per subscribed row type.\npub enum {en} {{\n"
        );
        for (var, row, key) in variants {
            decl.push_str(&format!(
                "    {var}(Event<super::{key}, super::{row}>),\n"
            ));
        }
        decl.push_str("}\n");
        code.push_str(&decl);
        code.push_str(&format!(
            "\npub static CHANNEL: EnumChannel<{en}> = EnumChannel::new();\n"
        ));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let dest = Path::new(&out_dir).join("okm_subscribe.rs");
    fs::write(&dest, code).expect("build.rs: write generated module");
}
