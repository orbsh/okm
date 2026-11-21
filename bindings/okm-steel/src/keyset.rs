//! The host-injected KEY SET face — the steel counterpart of okm-python's
//! `KeySet` pyclass, and the verb set that keeps a set step a TWO-STEP
//! semantics across the script boundary instead of a per-key loop inside it.
//!
//! **A value in a handle, because a script has nowhere else to put one.** The
//! container is `okm_core::KeySet` (ordered, deduplicated, algebra-free) and the
//! algebra over two of them is `okm_query::HeldSets`; neither is steel-shaped,
//! and a Steel script has no way to hold a Rust value between calls — so the
//! verbs answer an INTEGER handle into a per-VM table and take one back. That is
//! the same shape `hop.rs` uses for its cursors, for the same reasons: the table
//! is per-VM (`Arc<Mutex<HashMap<..>>>`, never a `thread_local`, because a
//! session's VM can run on another worker thread than the one that built the
//! bindings), and `close_all` is the session-teardown hook that empties it.
//!
//! **Sets are values, not cursors.** `key-set-and!` / `-or!` / `-minus!` leave
//! both inputs untouched and answer a NEW handle: `a` after `(key-set-and! a b)`
//! is still every key it had, so a script can ask the same two sets three
//! different questions without re-scanning either. Nothing is re-read from the
//! store by any of them; the algebra runs over keys already in hand.
//!
//! **What a key IS at a site.** A set is key-shaped for the site it is fetched
//! at, because a site's rows are addressed by its own keys:
//!
//! - a GRAPH's keys are face ENTRY keys — what `key-collect!` drains off a walk
//!   cursor, and what that graph's store answers a row for;
//! - a COLLECTION's keys are document PKEYS — the bytes `okm-encode-key!`
//!   answers, read through the table's own reader (which knows the primary row
//!   and the dynamic segment are one document, so a shallow write reads back).
//!
//! `key-fetch!` takes the site NAME rather than reusing whatever store a cursor
//! or a scan happened to hold: a set carries keys and nothing else, and a fetch
//! that silently read another site's store would answer void where the caller
//! expected rows. A name injected as BOTH a collection and a graph is refused —
//! the caller has to say which rows it meant.
//!
//! **Where the keys come from.** Four producers, none of which decodes a
//! document:
//!
//! - `key-collect!` drains a walk cursor — face entry keys;
//! - `key-set-of` takes key bytes the script already holds;
//! - `collection-keys` answers every document pkey in a collection, so a whole
//!   table becomes a set without the script encoding a single key;
//! - `collection-index-keys` answers one declared face's raw ENTRY keys inside
//!   a prefix — the index side of the same idea. Those keys carry the face
//!   prefix and the indexed fields in front of the pkey tail, so they are a key
//!   space of their own (the identity is the tail); the two producers answer
//!   different bytes for the same document by design.
//!
//! `collection-scan!` is NOT a producer: it answers DECODED documents, and a
//! document hash carries no key (`KeySet` is a key container, not a row
//! container). The key list is the separate verb, which is why it exists at all.

use okm_core::KeySet as HeldKeys;
use okm_core::VirtualStorage;
use okm_entry::EngineBox;
use okm_query::HeldSets;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use steel::SteelVal;

use crate::collection::{steel_bytes_to_vec, vec_to_steel, StorageRegistry};
use crate::hop::GraphRegistry;

/// The per-VM key-set table: the container a handle names, and the id counter
/// that hands the handles out. The carrier builds ONE per session and registers
/// the script fns over a clone, exactly as it does for the collection and graph
/// registries.
///
/// Drop the session, drop its sets: nothing here outlives the VM, and a set holds
/// no scan, so there is nothing to spend but memory.
#[derive(Clone, Default)]
pub struct KeySetRegistry {
    /// The sets, by handle. A set is a value, so an entry is the whole state —
    /// removing it is the release.
    sets: Arc<Mutex<HashMap<u64, HeldKeys<Vec<u8>>>>>,
    next_id: Arc<AtomicU64>,
}

impl KeySetRegistry {
    /// Take a set into the table and answer its handle.
    fn insert(&self, keys: HeldKeys<Vec<u8>>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, keys);
        id
    }

    /// The set behind a handle, CLONED out so the table is not held across the
    /// algebra or the fetch. A released (or never handed out) handle is a loud
    /// error naming it — the same discipline `graph-hop-next!` follows, and the
    /// reason a script can trust an empty answer.
    fn get(&self, id: u64) -> Result<HeldKeys<Vec<u8>>, String> {
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("okm: key set {id} is not open (released, or never built here)"))
    }

    /// Install the script-facing verbs over THIS registry into a VM:
    /// - `(key-set-of keys)` → a set handle. `keys` is a list or vector of key
    ///   byte vectors; order and duplicates do not matter. `collection-keys`'s
    ///   answer is already that shape, so a whole table's key list is one call
    ///   away.
    /// - `(key-set-and! a b)` / `(key-set-or! a b)` / `(key-set-minus! a b)` →
    ///   a NEW set handle; both inputs stay usable.
    /// - `(key-collect! cursor)` → a set handle holding every remaining face
    ///   entry key of a `graph-hop-open` walk, in walk order. The cursor is
    ///   DRAINED (its handle stays valid and spent, so `graph-hop-close!` still
    ///   answers `#t`).
    /// - `(key-fetch! set site)` → a vector of `[key row]` pairs, one point read
    ///   per surviving key and all of them inside this call. `site` is a
    ///   registered collection (keys are pkeys, rows are documents) or graph
    ///   (keys are face entry keys, rows are the entries' values); a key whose
    ///   row is gone answers `void`, a live store's normal race rather than a
    ///   fault.
    /// - `(key-set-close! set)` → `#t` when it released a live set, `#f` when
    ///   the handle was already released (idempotent, like `graph-hop-close!`).
    pub fn register_into(
        &self,
        vm: &mut Engine,
        collections: &StorageRegistry,
        graphs: &GraphRegistry,
    ) {
        let reg = self.clone();
        vm.register_fn("key-set-of", {
            let reg = reg.clone();
            move |keys: SteelVal| -> Result<SteelVal, String> {
                Ok(SteelVal::IntV(
                    reg.insert(HeldKeys::new(keys_of(&keys)?)) as isize
                ))
            }
        });
        vm.register_fn("key-set-and!", {
            let reg = reg.clone();
            move |a: SteelVal, b: SteelVal| -> Result<SteelVal, String> {
                let (left, right) = (reg.get(handle_of(&a)?)?, reg.get(handle_of(&b)?)?);
                Ok(SteelVal::IntV(reg.insert(left.and(&right)) as isize))
            }
        });
        vm.register_fn("key-set-or!", {
            let reg = reg.clone();
            move |a: SteelVal, b: SteelVal| -> Result<SteelVal, String> {
                let (left, right) = (reg.get(handle_of(&a)?)?, reg.get(handle_of(&b)?)?);
                Ok(SteelVal::IntV(reg.insert(left.or(&right)) as isize))
            }
        });
        vm.register_fn("key-set-minus!", {
            let reg = reg.clone();
            move |a: SteelVal, b: SteelVal| -> Result<SteelVal, String> {
                let (left, right) = (reg.get(handle_of(&a)?)?, reg.get(handle_of(&b)?)?);
                Ok(SteelVal::IntV(reg.insert(left.minus(&right)) as isize))
            }
        });
        vm.register_fn("key-collect!", {
            let reg = reg.clone();
            let graphs = graphs.clone();
            move |cursor: SteelVal| -> Result<SteelVal, String> {
                let id = cursor_handle_of(&cursor)?;
                let keys = graphs.drain_keys(id)?;
                Ok(SteelVal::IntV(reg.insert(HeldKeys::new(keys)) as isize))
            }
        });
        vm.register_fn("key-fetch!", {
            let reg = reg.clone();
            let collections = collections.clone();
            let graphs = graphs.clone();
            move |set: SteelVal, site: String| -> Result<SteelVal, String> {
                let keys = reg.get(handle_of(&set)?)?;
                let site = site_of(&site, &collections, &graphs)?;
                let mut rows = Vec::with_capacity(keys.len());
                match site {
                    RowSite::Collection(table) => {
                        let table = table.lock().unwrap_or_else(PoisonError::into_inner);
                        for key in keys.iter() {
                            let row = match table.fetch(key)? {
                                Some(document) => crate::value_map_to_steel(&document),
                                None => SteelVal::Void,
                            };
                            rows.push(row_pair(key, row));
                        }
                    }
                    RowSite::Graph(store) => {
                        for key in keys.iter() {
                            let row = match store.get(key).map_err(|e| e.to_string())? {
                                Some(value) => vec_to_steel(value),
                                None => SteelVal::Void,
                            };
                            rows.push(row_pair(key, row));
                        }
                    }
                }
                Ok(crate::collection::vector_of(rows))
            }
        });
        vm.register_fn("key-set-close!", {
            let reg = reg.clone();
            move |set: SteelVal| -> Result<SteelVal, String> {
                let id = handle_of(&set)?;
                let released = reg
                    .sets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id)
                    .is_some();
                Ok(SteelVal::BoolV(released))
            }
        });
    }

    /// The session-teardown hook: release every set and answer how many there
    /// were. A set owns no cursor, so this is memory and nothing else — but a
    /// session that ends should not leave handles a later script could name.
    pub fn close_all(&self) -> usize {
        let mut sets = self.sets.lock().unwrap_or_else(PoisonError::into_inner);
        let open = sets.len();
        sets.clear();
        open
    }

    /// Stub arms for the introspection throwaway engine ONLY — the same
    /// define-compile trap the collection and hop stubs close: a handler calling
    /// `(key-set-and! ..)` references the name at load time, and an unregistered
    /// name drops the declaration silently. Introspection never calls a handler;
    /// if it ever does, the error names the shape.
    pub fn register_stubs(vm: &mut Engine) {
        vm.register_fn("key-set-of", |_: SteelVal| -> Result<SteelVal, String> {
            Err("okm: key-set-of called outside a session — introspection only".to_string())
        });
        vm.register_fn(
            "key-set-and!",
            |_: SteelVal, _: SteelVal| -> Result<SteelVal, String> {
                Err("okm: key-set-and! called outside a session — introspection only".to_string())
            },
        );
        vm.register_fn(
            "key-set-or!",
            |_: SteelVal, _: SteelVal| -> Result<SteelVal, String> {
                Err("okm: key-set-or! called outside a session — introspection only".to_string())
            },
        );
        vm.register_fn(
            "key-set-minus!",
            |_: SteelVal, _: SteelVal| -> Result<SteelVal, String> {
                Err("okm: key-set-minus! called outside a session — introspection only".to_string())
            },
        );
        vm.register_fn("key-collect!", |_: SteelVal| -> Result<SteelVal, String> {
            Err("okm: key-collect! called outside a session — introspection only".to_string())
        });
        vm.register_fn(
            "key-fetch!",
            |_: SteelVal, _: String| -> Result<SteelVal, String> {
                Err("okm: key-fetch! called outside a session — introspection only".to_string())
            },
        );
        vm.register_fn(
            "key-set-close!",
            |_: SteelVal| -> Result<SteelVal, String> {
                Err("okm: key-set-close! called outside a session — introspection only".to_string())
            },
        );
    }
}

/// What a site NAME resolves to: the table that owns the rows, in the shape the
/// fetch needs. Resolved ONCE per `key-fetch!` call, from the two registries the
/// carrier injected — never carried by the set.
enum RowSite {
    /// A collection: keys are document pkeys, rows are decoded documents.
    Collection(Arc<Mutex<okm_core::model::dynamic_collection::DynamicCollection<EngineBox>>>),
    /// A graph: keys are face entry keys, rows are the entries' value bytes.
    Graph(EngineBox),
}

/// The one place a site name is turned into rows-to-read. A name in BOTH tables
/// is refused rather than resolved by precedence: the two answer different rows
/// for the same bytes, and guessing which the caller meant is how a fetch
/// silently answers void.
fn site_of(
    name: &str,
    collections: &StorageRegistry,
    graphs: &GraphRegistry,
) -> Result<RowSite, String> {
    match (collections.table_of(name), graphs.store_of(name)) {
        (Some(table), None) => Ok(RowSite::Collection(table)),
        (None, Some(store)) => Ok(RowSite::Graph(store)),
        (Some(_), Some(_)) => Err(format!(
            "okm: `{name}` is both a collection and a graph — key-fetch! cannot tell which rows you meant"
        )),
        (None, None) => Err(format!(
            "okm: no collection or graph `{name}` injected for this session"
        )),
    }
}

/// One `[key row]` answer, the pair shape `collection-scan-reduces!` already
/// speaks at this face.
fn row_pair(key: &[u8], row: SteelVal) -> SteelVal {
    crate::collection::vector_of(vec![vec_to_steel(key.to_vec()), row])
}

/// A list or vector of key byte vectors → the keys, in the order written (the
/// set sorts and dedupes them).
fn keys_of(v: &SteelVal) -> Result<Vec<Vec<u8>>, String> {
    let items: Vec<SteelVal> = match v {
        SteelVal::ListV(l) => l.iter().cloned().collect(),
        SteelVal::VectorV(vec) => vec.iter().cloned().collect(),
        other => {
            return Err(format!(
                "okm: a key set is a list or vector of key byte vectors, got {other:?}"
            ));
        }
    };
    items
        .iter()
        .map(|item| steel_bytes_to_vec(item).map_err(|e| format!("okm: {e}")))
        .collect()
}

/// A SET handle: the integer `key-set-of` / `key-collect!` / the operators
/// answered.
fn handle_of(v: &SteelVal) -> Result<u64, String> {
    let SteelVal::IntV(id) = v else {
        return Err(format!("okm: a key set handle is an integer, got {v:?}"));
    };
    u64::try_from(*id).map_err(|_| format!("okm: key set handle {id} is not a handle"))
}

/// A CURSOR handle for `key-collect!` — named apart from [`handle_of`] so the
/// error says which kind of integer the verb wanted (`key-set-of`'s list is the
/// other shape a caller reaches for here by mistake).
fn cursor_handle_of(v: &SteelVal) -> Result<u64, String> {
    match v {
        SteelVal::IntV(id) => {
            u64::try_from(*id).map_err(|_| format!("okm: cursor handle {id} is not a cursor"))
        }
        other => Err(format!(
            "okm: key-collect! drains a walk cursor (`graph-hop-open`'s handle), got {other:?} — \
             a list of key bytes is key-set-of's argument, and a `collection-scan!` answer is \
             decoded rows, which carry no key (a collection's key list is `collection-keys`)"
        )),
    }
}
