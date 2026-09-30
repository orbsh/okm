//! The host-injected Collection face (ADR-0037 §1, Phase 4.16b) — the
//! steel mirror of okm-python's `Collection::with_store`. The entry
//! parsing and the engine face come from `okm-entry` (the single
//! source); this module adds only the steel-shaped shell:
//!
//! - A PER-VM registry (`StorageRegistry`), not the thread_local the
//!   codec handles use: a session's VM lives in the realm's Sessions
//!   map and the `unsafe impl Send` means it can run on a different
//!   worker thread than the one that built the bindings — thread_local
//!   would lose the collections across threads. Per-VM also matches
//!   the eviction lifecycle (drop the session, drop its collections —
//!   no process-wide id accretion).
//! - `register_storage_fns` for the script-side surface: SIX global fns
//!   with fixed names, the collection addressed by a NAME STRING —
//!   `(collection-put! "Counters" pkey doc)`. Fixed names exist because
//!   steel resolves free identifiers at DEFINE-COMPILE time: the
//!   introspection throwaway engine must carry the same names (the ctx-
//!   stub precedent — a dotted per-collection shim like `Counters.put`
//!   could never be stubbed there, its names are script content, and
//!   the FreeIdentifier failure silently drops the schema). Scripts
//!   never see handles: the name is the declaration, the registry
//!   binds it at inject time.

use okm_core::storage::VirtualStorage;
use okm_dynamic::ValueMap;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use steel::SteelVal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A registered binding: the shared table handle (the
/// `DynamicCollection` code path is the single executor; the Mutex is
/// the same shape okm-python's pyclass carries), the declared schema,
/// and the bound ns (the DSL rule: ns binds at construction and rides
/// beside the handle).
#[derive(Clone)]
struct Bound {
    table: Arc<Mutex<okm_dynamic::DynamicCollection<okm_entry::EngineBox>>>,
    schema: Arc<okm_core::schema::CollectionSchema>,
    ns: u16,
}

/// The per-VM collection registry: the carrier builds ONE, injects the
/// declared collections into it, and registers the six script fns over
/// a clone. Drop the session, drop the registry.
#[derive(Clone, Default)]
pub struct StorageRegistry {
    inner: Arc<Mutex<HashMap<String, Bound>>>,
}

impl StorageRegistry {
    /// Build a table over the HOST engine from the RAW interface_schema
    /// storage entry (the `okm-entry` parsing — one source with
    /// okm-python) and register it under the collection name. The
    /// carrier's load step calls this per declared entry (never the
    /// script — the isolation stance: construction power lives
    /// host-side).
    pub fn inject(
        &self,
        engine: Arc<dyn okm_entry::Engine>,
        name: &str,
        entry: &serde_json::Value,
        ns: u16,
    ) -> Result<(), String> {
        let (schema, table) =
            okm_entry::collection_from_entry(okm_entry::EngineBox::Injected(engine), entry, ns)?;
        self.inner.lock().unwrap().insert(
            name.to_string(),
            Bound {
                table: Arc::new(Mutex::new(table)),
                schema: Arc::new(schema),
                ns,
            },
        );
        Ok(())
    }

    /// An injected collection's declared schema as JSON (the carrier's
    /// introspection checks read the storage half back).
    pub fn schema_json(&self, name: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .get(name)
            .map(|b| serde_json::to_value(b.schema.as_ref()).unwrap())
            .map(|v| v.to_string())
    }

    fn with_bound<T>(
        &self,
        name: &str,
        f: impl FnOnce(&Bound) -> Result<T, String>,
    ) -> Result<T, String> {
        let bound = self
            .inner
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("okm: no collection `{name}` injected for this session"))?;
        f(&bound)
    }

    /// Install the six script-facing functions over THIS registry into
    /// a VM. All address the collection by its declared name:
    /// - `(collection-put! name pkey doc)` → void
    /// - `(collection-get! name pkey)` → hash? or void (absent document)
    /// - `(collection-delete! name pkey)` → void
    /// - `(collection-scan! name slot prefix)` → vector of hashes
    /// - `(collection-reduce-get! name group-hash)` → acc bytes or void
    /// - `(collection-scan-reduces! name slot)` → vector of `[group acc]`
    pub fn register_into(&self, vm: &mut Engine) {
        let reg = self.clone();
        vm.register_fn("collection-put!", {
            let reg = reg.clone();
            move |name: String, pkey: SteelVal, doc: SteelVal| -> Result<SteelVal, String> {
                let pkey = steel_bytes_to_vec(&pkey)?;
                reg.with_bound(&name, |b| {
                    let map = hash_to_map(&b.schema, &doc)?;
                    b.table.lock().unwrap().put(&pkey, &map)?;
                    Ok(SteelVal::Void)
                })
            }
        });
        vm.register_fn("collection-get!", {
            let reg = reg.clone();
            move |name: String, pkey: SteelVal| -> Result<SteelVal, String> {
                let pkey = steel_bytes_to_vec(&pkey)?;
                reg.with_bound(&name, |b| {
                    Ok(match b.table.lock().unwrap().get(&pkey)? {
                        Some(m) => crate::value_map_to_steel(&m),
                        None => SteelVal::Void,
                    })
                })
            }
        });
        vm.register_fn("collection-delete!", {
            let reg = reg.clone();
            move |name: String, pkey: SteelVal| -> Result<SteelVal, String> {
                let pkey = steel_bytes_to_vec(&pkey)?;
                reg.with_bound(&name, |b| {
                    b.table.lock().unwrap().delete(&pkey)?;
                    Ok(SteelVal::Void)
                })
            }
        });
        vm.register_fn("collection-scan!", {
            let reg = reg.clone();
            move |name: String, slot: i64, prefix: SteelVal| -> Result<SteelVal, String> {
                let prefix = steel_bytes_to_vec(&prefix)?;
                reg.with_bound(&name, |b| {
                    let rows = b.table.lock().unwrap().scan(slot as u16, &prefix)?;
                    Ok(vector_of(
                        rows.iter().map(crate::value_map_to_steel).collect::<Vec<_>>(),
                    ))
                })
            }
        });
        vm.register_fn("collection-reduce-get!", {
            let reg = reg.clone();
            move |name: String, group: SteelVal| -> Result<SteelVal, String> {
                reg.with_bound(&name, |b| {
                    let map = hash_to_map(&b.schema, &group)?;
                    let guard = b.table.lock().unwrap();
                    let ek = guard.reduce_entry_key(b.ns, &map)?;
                    Ok(match guard.store().get(&ek) {
                        Some(bytes) => vec_to_steel(bytes),
                        None => SteelVal::Void,
                    })
                })
            }
        });
        vm.register_fn("collection-scan-reduces!", {
            let reg = reg.clone();
            move |name: String, slot: i64| -> Result<SteelVal, String> {
                reg.with_bound(&name, |b| {
                    let pairs = {
                        let guard = b.table.lock().unwrap();
                        okm_dynamic::scan_reduces(guard.store(), &b.ns.to_be_bytes(), slot as u16)
                    };
                    Ok(vector_of(
                        pairs
                            .into_iter()
                            .map(|(group, acc)| vector_of(vec![vec_to_steel(group), vec_to_steel(acc)]))
                            .collect::<Vec<_>>(),
                    ))
                })
            }
        });
    }

    /// Stub arms for the introspection throwaway engine ONLY (never the
    /// resident session — there the real registry fns must win, the
    /// same-shadowing rule as the ctx stubs). Scripts reference these
    /// six names at define-compile time; introspection never calls a
    /// handler, and if it ever does, the error names the shape.
    pub fn register_stubs(vm: &mut Engine) {
        // Exact arities (steel checks call arity against the builtin —
        // a 2/3-arg call site would ArityMismatch even in a stub).
        // register_fn wants 'static closures, so each arm inlines its
        // own message (the shared `stub` helper would be a borrow).
        vm.register_fn("collection-put!", |_: String, _: SteelVal, _: SteelVal| -> Result<SteelVal, String> {
            Err("okm: collection-put! called outside a session — introspection only".to_string())
        });
        vm.register_fn("collection-get!", |_: String, _: SteelVal| -> Result<SteelVal, String> {
            Err("okm: collection-get! called outside a session — introspection only".to_string())
        });
        vm.register_fn("collection-delete!", |_: String, _: SteelVal| -> Result<SteelVal, String> {
            Err("okm: collection-delete! called outside a session — introspection only".to_string())
        });
        vm.register_fn("collection-scan!", |_: String, _: i64, _: SteelVal| -> Result<SteelVal, String> {
            Err("okm: collection-scan! called outside a session — introspection only".to_string())
        });
        vm.register_fn("collection-reduce-get!", |_: String, _: SteelVal| -> Result<SteelVal, String> {
            Err("okm: collection-reduce-get! called outside a session — introspection only".to_string())
        });
        vm.register_fn("collection-scan-reduces!", |_: String, _: i64| -> Result<SteelVal, String> {
            Err("okm: collection-scan-reduces! called outside a session — introspection only".to_string())
        });
    }
}

/// Codec-fn stubs for the introspection throwaway engine ONLY (the
/// same define-compile trap the collection fns carry: a handler that
/// encodes a pkey references `okm-encode-key!` etc. at load time, the
/// throwaway engine registers neither the real codec NOR these — the
/// FreeIdentifier failure drops the declaration silently). Scripts
/// declaring storage need the codec names to LOAD even when
/// introspection never calls them.
pub fn register_codec_stubs(vm: &mut Engine) {
    vm.register_fn("okm-schema-from-json!", |_: String| -> Result<steel::SteelVal, String> {
        Err("okm: schema functions called outside a session — introspection only".to_string())
    });
    vm.register_fn("okm-schema-version!", |_: i64| -> Result<steel::SteelVal, String> {
        Err("okm: schema functions called outside a session — introspection only".to_string())
    });
    vm.register_fn("okm-encode-key!", |_: i64, _: SteelVal| -> Result<SteelVal, String> {
        Err("okm: codec called outside a session — introspection only".to_string())
    });
    vm.register_fn("okm-encode-payload!", |_: i64, _: SteelVal| -> Result<SteelVal, String> {
        Err("okm: codec called outside a session — introspection only".to_string())
    });
    vm.register_fn("okm-decode-key!", |_: i64, _: SteelVal| -> Result<SteelVal, String> {
        Err("okm: codec called outside a session — introspection only".to_string())
    });
    vm.register_fn("okm-decode-payload!", |_: i64, _: SteelVal| -> Result<SteelVal, String> {
        Err("okm: codec called outside a session — introspection only".to_string())
    });
}

/// SteelVal bytes (vector of integers 0-255) → Vec<u8> — the existing
/// codec-surface byte convention (steel's ByteVector field is private).
fn steel_bytes_to_vec(v: &SteelVal) -> Result<Vec<u8>, String> {
    let SteelVal::VectorV(vec) = v else {
        return Err("bytes must be a vector of integers 0-255".to_string());
    };
    let mut out = Vec::with_capacity(vec.len());
    for item in vec.iter() {
        let SteelVal::IntV(i) = item else {
            return Err("bytes must be a vector of integers 0-255".to_string());
        };
        out.push(u8::try_from(*i).map_err(|_| "bytes must be 0-255".to_string())?);
    }
    Ok(out)
}

fn vec_to_steel(b: Vec<u8>) -> SteelVal {
    SteelVal::VectorV(steel::rvals::SteelVector::from(steel::gc::Gc::new(
        b.into_iter()
            .map(|x| SteelVal::IntV(x as isize))
            .collect::<im_rc::Vector<SteelVal>>(),
    )))
}

fn vector_of(items: Vec<SteelVal>) -> SteelVal {
    SteelVal::VectorV(steel::rvals::SteelVector::from(steel::gc::Gc::new(
        items.into_iter().collect::<im_rc::Vector<SteelVal>>(),
    )))
}

fn hash_to_map(
    schema: &okm_core::schema::CollectionSchema,
    v: &SteelVal,
) -> Result<ValueMap, String> {
    crate::steel_hash_to_map(schema, v)
}
