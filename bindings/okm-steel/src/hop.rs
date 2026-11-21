//! The host-injected graph HOP face — the steel counterpart of
//! okm-python's `EdgeStream` pyclass (ADR-0037 §1's injected face, applied
//! to ADR-0067's traversal faces). Same three laws the python cursor
//! keeps, and the same reason it is a cursor at all: a
//! `DynEdgeStream<'a, S>` borrows the store, and a script's call cannot
//! hold that borrow, so the binding OWNS the engine handle and the scan
//! it opened and asks the dynamic graph module what each entry means
//! (`edge_face_prefix`, `edge_step`) — the layout stays in one place,
//! only the position lives here.
//!
//! **A cursor, not a callback.** Steel has no iterator protocol, so the
//! obvious steel-shaped surface is "call this closure per hop" — and it
//! cannot be built. `register_fn` hands a native fn only its declared
//! args (`move |args: &[SteelVal]|`), `SteelVal::Closure(Gc<ByteCodeLambda>)`
//! has no invoke method, `SteelThread::call_function` is `pub(crate)`
//! behind `Engine::call_function_with_args`, capturing an `Engine` would
//! demand `Send + Sync + 'static` which `Engine` is not, `BoxedDynFunction::new`
//! is `pub(crate)`, and `stream-cons` wants a `SteelVal::Closure` thunk.
//! So the pull shape is the script's loop: `open`, `next!` until `#f`,
//! `close!`.
//!
//! **Early stop survives, because nothing is buffered.** One `next!`
//! pulls exactly ONE entry off the scan and resolves it (one registry
//! point read for the far endpoint, one vocabulary read for the
//! predicate name); there is no lookahead slot and no `Vec` of entries
//! anywhere. A script that breaks after the first hop stops the walk
//! where it stands — the remaining entries are never decoded, never
//! resolved, and never read again.
//!
//! **Handles stop at the boundary.** The face reports a node's HANDLE
//! (ADR-0067), so `graph-hop-next!` resolves it to `(far-ns far-key
//! predicate)` inside the binding; a script never sees a handle, and an
//! edge has no id to see (its identity is the tuple).
//!
//! Cursor state lives in a PER-VM table (`Arc<Mutex<HashMap<..>>>` held
//! by the registry the closures share), not in a `thread_local`: a
//! session's VM can run on another worker thread than the one that built
//! the bindings, exactly the reason `StorageRegistry` is per-VM. The
//! table entry IS the scan, so `close!` (or the table dying with the
//! session) releases the engine cursor at once — a script that walks
//! away from a handle leaves one spent entry behind, never a live scan
//! nobody can reach.

use okm_core::storage::ScanIter;
use okm_core::VirtualStorage;
use okm_core::{Direction, Handle, Handles, NodeRef};
use okm_entry::EngineBox;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;
use steel::SteelVal;

/// A graph a script can open a walk over: the engine, and the graph's
/// OWN ns (its edge faces live there — member ns numbers are the
/// endpoints' business, never the walk's).
#[derive(Clone)]
struct Site {
    store: EngineBox,
    ns: u16,
}

/// One open walk: the position, and nothing else.
///
/// There is deliberately no "entry on top" slot — [`DynEdgeStream`] needs
/// one because an `Iterator` must answer before it can pull; a
/// `next!` verb can pull exactly the entry it is about to answer.
///
/// [`DynEdgeStream`]: okm_core::model::dynamic_graph::DynEdgeStream
struct Hop {
    store: EngineBox,
    ns: u16,
    dir: Direction,
    /// The node's HANDLE — the scanned prefix, resolved ONCE at the open.
    /// [`Handle::NONE`] means the graph never saw the node as an endpoint,
    /// which is an empty walk, not a fault.
    handle: Handle,
    /// The pushed-down predicate id, or `None` for the untyped face.
    filter: Option<u16>,
    /// The face cursor; `None` means spent (or never opened — a predicate
    /// nobody interned has no face to walk).
    scan: Option<ScanIter>,
    /// Bytes of key before the entry's own segment.
    suffix_at: usize,
}

impl Hop {
    /// A walk over one node's edges in one direction. The node's handle is
    /// resolved once, here; the predicate, when named, is pushed down onto
    /// the `0x08`/`0x09` prefix so not one entry of the whole face is read
    /// and thrown away (an unknown name is an empty walk — no edge can
    /// carry a predicate nobody wrote).
    fn open(
        site: Site,
        dir: Direction,
        node: &NodeRef,
        predicate: Option<&str>,
    ) -> Result<Self, String> {
        let ns_bytes = site.ns.to_be_bytes();
        let mut hop = Hop {
            store: site.store,
            ns: site.ns,
            dir,
            handle: Handle::NONE,
            filter: None,
            scan: None,
            suffix_at: 0,
        };
        let handle = Handles::new(&hop.store, ns_bytes)
            .handle_of(node)
            .map_err(|e| e.to_string())?;
        // A node the graph never saw has no face at all: an empty walk.
        let Some(handle) = handle else {
            return Ok(hop);
        };
        hop.handle = handle;
        let filter = match predicate {
            Some(name) => {
                match okm_core::model::dynamic_graph::predicate_id_in(&hop.store, &ns_bytes, name)
                    .map_err(|e| e.to_string())?
                {
                    Some(id) => Some(id),
                    // Nobody interned the name: no face to walk.
                    None => return Ok(hop),
                }
            }
            None => None,
        };
        hop.reopen(filter);
        Ok(hop)
    }

    /// Where this walk reads: `[ns][slot][predicate?][near_h]`.
    fn prefix(&self) -> Vec<u8> {
        okm_core::model::dynamic_graph::edge_face_prefix(self.ns.to_be_bytes(), self.dir, self.filter, self.handle)
    }

    /// Point the cursor at (possibly) another face and open its scan. A
    /// filter change is a different FACE, not a narrower range on the same
    /// one, so the position does not survive it.
    fn reopen(&mut self, filter: Option<u16>) {
        self.filter = filter;
        let prefix = self.prefix();
        self.suffix_at = prefix.len();
        let end = okm_core::storage::prefix_end(&prefix);
        self.scan = Some(self.store.scan_range(&prefix, end.as_deref()));
    }

    /// The next hop, RESOLVED at the boundary — or `None` when the face is
    /// spent.
    ///
    /// Exactly one entry is pulled per call. An entry whose key this face
    /// cannot carry (some other layout's) is skipped without being counted
    /// as a hop, and a fault mid-walk is an `Err` rather than "no more
    /// neighbours": the distinction a caller needs before it trusts an
    /// empty answer.
    fn next_entry(&mut self) -> Result<Option<(NodeRef, String)>, String> {
        loop {
            let next = match self.scan.as_mut() {
                Some(scan) => scan.next(),
                None => return Ok(None),
            };
            match next {
                None => {
                    self.scan = None;
                    return Ok(None);
                }
                Some(Err(e)) => {
                    self.scan = None;
                    return Err(format!("graph hop: {e}"));
                }
                Some(Ok((key, _))) => {
                    let Some(step) = okm_core::model::dynamic_graph::edge_step(
                        &key,
                        self.suffix_at,
                        self.dir,
                        self.handle,
                        self.filter,
                    ) else {
                        continue;
                    };
                    let far = handle_node(&self.store, self.ns, step.far_handle)?;
                    let predicate = predicate_name(&self.store, self.ns, step.head.predicate)?;
                    return Ok(Some((far, predicate)));
                }
            }
        }
    }

    /// Every remaining ENTRY KEY off this face, in walk order — the raw bytes a
    /// key set holds, and the drain side of `key-collect!`.
    ///
    /// Nothing is RESOLVED: no registry read for the far endpoint and no
    /// vocabulary read for the predicate name, because a caller collecting keys
    /// is going to look the rows up by key anyway. What the face's own rule
    /// still does is decide whether an entry belongs here at all (`edge_step`'s
    /// shape check, exactly as [`next_entry`](Self::next_entry) does), so a
    /// neighbouring layout's entries cannot leak into the set.
    ///
    /// The cursor is DRAINED: whatever it had left is what this answers, and a
    /// second call answers nothing (a spent walk, not a restart).
    fn drain_keys(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let mut keys = Vec::new();
        loop {
            let next = match self.scan.as_mut() {
                Some(scan) => scan.next(),
                None => return Ok(keys),
            };
            match next {
                None => {
                    self.scan = None;
                    return Ok(keys);
                }
                Some(Err(e)) => {
                    self.scan = None;
                    return Err(format!("graph hop: {e}"));
                }
                Some(Ok((key, _))) => {
                    if okm_core::model::dynamic_graph::edge_step(
                        &key,
                        self.suffix_at,
                        self.dir,
                        self.handle,
                        self.filter,
                    )
                    .is_some()
                    {
                        keys.push(key);
                    }
                }
            }
        }
    }
}

/// The per-VM registry: the carrier builds ONE per session, injects the
/// declared graphs into it, and registers the three script fns over a
/// clone. Drop the session, drop its graphs AND its cursors — no
/// process-wide id accretion.
#[derive(Clone, Default)]
pub struct GraphRegistry {
    sites: Arc<Mutex<HashMap<String, Site>>>,
    /// The open walks, by handle. Per-VM (see the module doc), and the
    /// entry owns the scan, so removing it is the release.
    cursors: Arc<Mutex<HashMap<u64, Hop>>>,
    next_id: Arc<AtomicU64>,
}

impl GraphRegistry {
    /// Declare a graph under a name from its RAW interface_schema graph
    /// entry (`{ "schema": <CollectionSchema> }`, or the bare schema): the
    /// graph's ns rides BESIDE the declaration (the DSL rule — it binds at
    /// construction, per binding) and a schema that names a different ns is
    /// refused rather than overridden. Construction power lives host-side,
    /// never in the script (the isolation stance the collections keep).
    ///
    /// Traversal needs the PAIR and nothing else: the faces are keyed
    /// `[ns][slot][handle]`, so the width table (`member_widths`) is the
    /// writer's boundary, not the walk's.
    pub fn inject(
        &self,
        engine: Arc<dyn okm_entry::Engine>,
        name: &str,
        entry: &serde_json::Value,
        ns: u16,
    ) -> Result<(), String> {
        let raw = entry
            .get("schema")
            .cloned()
            .unwrap_or_else(|| entry.clone());
        let schema: okm_core::schema::CollectionSchema =
            serde_json::from_value(raw).map_err(|e| format!("bad graph schema: {e}"))?;
        if !schema.ns_prefix.is_empty() && schema.ns_prefix != ns.to_be_bytes() {
            return Err(format!(
                "graph `{name}`: the declaration says ns {:?} but the injection says {ns} — the pair comes from one declaration, never from two",
                schema.ns_prefix
            ));
        }
        self.sites
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                name.to_string(),
                Site {
                    store: EngineBox::Injected(engine),
                    ns,
                },
            );
        Ok(())
    }

    fn site(&self, name: &str) -> Result<Site, String> {
        self.sites
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
            .ok_or_else(|| format!("okm: no graph `{name}` injected for this session"))
    }

    /// The engine a registered graph reads and writes — what `key-fetch!`
    /// resolves a SITE NAME to when the site is this graph. `None` for a name
    /// nobody injected (the caller decides whether that is the collection
    /// table's business or a typo).
    pub fn store_of(&self, name: &str) -> Option<EngineBox> {
        self.sites
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|site| site.store.clone())
    }

    /// Drain an open walk to its raw ENTRY KEYS, in walk order — the collector
    /// `key-collect!` hands a key set.
    ///
    /// A clone of the handle is not an open walk: the table owns the scan, so
    /// collecting from a handle that was closed (or never opened here) is a
    /// loud error naming it, exactly as `graph-hop-next!` is. The walk is left
    /// OPEN but spent — the handle still exists, it simply has nothing left to
    /// answer, which is what "drained" means for a cursor a script may still
    /// `close!`.
    pub fn drain_keys(&self, id: u64) -> Result<Vec<Vec<u8>>, String> {
        let mut cursors = self.cursors.lock().unwrap_or_else(PoisonError::into_inner);
        let hop = cursors.get_mut(&id).ok_or_else(|| {
            format!("okm: cursor {id} is not open (closed, or never opened here)")
        })?;
        hop.drain_keys()
    }

    /// Install the three script-facing functions over THIS registry:
    /// - `(graph-hop-open name node direction pred)` → a cursor handle.
    ///   `node` is `(ns key)` with `key` the pkey byte vector the
    ///   collection verbs hand back; `direction` is `'out` / `'in` /
    ///   `'walk` (or the strings), where `walk` is `out`'s chainable
    ///   spelling — the same alias `Graph::walk` is; `pred` is `#f` for
    ///   any predicate or a name to push down.
    /// - `(graph-hop-next! cursor)` → `(far-ns far-key predicate)`, or
    ///   `#f` when the walk is done. One call, one entry.
    /// - `(graph-hop-close! cursor)` → `#t` when it released a live walk,
    ///   `#f` when the handle was already closed (idempotent).
    pub fn register_into(&self, vm: &mut Engine) {
        let reg = self.clone();
        vm.register_fn("graph-hop-open", {
            let reg = reg.clone();
            move |name: String,
                  node: SteelVal,
                  direction: SteelVal,
                  pred: SteelVal|
                  -> Result<SteelVal, String> {
                let site = reg.site(&name)?;
                let node = node_of(&node)?;
                let dir = direction_of(&direction)?;
                let predicate = predicate_of(&pred)?;
                let hop = Hop::open(site, dir, &node, predicate.as_deref())?;
                let id = reg.next_id.fetch_add(1, Ordering::Relaxed) + 1;
                reg.cursors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(id, hop);
                Ok(SteelVal::IntV(id as isize))
            }
        });
        vm.register_fn("graph-hop-next!", {
            let reg = reg.clone();
            move |cursor: SteelVal| -> Result<SteelVal, String> {
                let id = handle_of(&cursor)?;
                let mut cursors = reg.cursors.lock().unwrap_or_else(PoisonError::into_inner);
                let hop = cursors.get_mut(&id).ok_or_else(|| {
                    format!("okm: cursor {id} is not open (closed, or never opened here)")
                })?;
                Ok(match hop.next_entry()? {
                    Some((far, predicate)) => entry_to_steel(&far, &predicate),
                    None => SteelVal::BoolV(false),
                })
            }
        });
        vm.register_fn("graph-hop-close!", {
            let reg = reg.clone();
            move |cursor: SteelVal| -> Result<SteelVal, String> {
                let id = handle_of(&cursor)?;
                let released = reg
                    .cursors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id)
                    .is_some();
                Ok(SteelVal::BoolV(released))
            }
        });
    }

    /// The session-teardown hook: release every open walk and answer how
    /// many there were. A VM stops running before its registry dies, so
    /// the carrier calls this when a session ends — the scans go away at
    /// that moment rather than whenever the last `Arc` drops.
    pub fn close_all(&self) -> usize {
        let mut cursors = self.cursors.lock().unwrap_or_else(PoisonError::into_inner);
        let open = cursors.len();
        cursors.clear();
        open
    }

    /// Stub arms for the introspection throwaway engine ONLY — the same
    /// define-compile trap the collection stubs close: a handler calling
    /// a hop verb references the name at load time, and an unregistered
    /// name drops the declaration silently. Introspection never calls a
    /// handler; if it ever does, the error names the shape.
    pub fn register_stubs(vm: &mut Engine) {
        vm.register_fn(
            "graph-hop-open",
            |_: String, _: SteelVal, _: SteelVal, _: SteelVal| -> Result<SteelVal, String> {
                Err("okm: graph-hop-open called outside a session — introspection only".to_string())
            },
        );
        vm.register_fn(
            "graph-hop-next!",
            |_: SteelVal| -> Result<SteelVal, String> {
                Err(
                    "okm: graph-hop-next! called outside a session — introspection only"
                        .to_string(),
                )
            },
        );
        vm.register_fn(
            "graph-hop-close!",
            |_: SteelVal| -> Result<SteelVal, String> {
                Err(
                    "okm: graph-hop-close! called outside a session — introspection only"
                        .to_string(),
                )
            },
        );
    }
}

/// A node's far endpoint through the graph's registry — ONE point read,
/// the boundary a walk defers resolution to.
fn handle_node(store: &EngineBox, ns: u16, handle: Handle) -> Result<NodeRef, String> {
    Handles::new(store, ns.to_be_bytes())
        .node(handle)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "okm: an edge names handle {} but the graph's registry has no node for it",
                handle.id()
            )
        })
}

/// A predicate id → its name, through okm-core's own vocabulary door (the
/// key shape is core's, never re-derived here). A predicate with no name
/// is a graph that lost its vocabulary under itself: an error, never a
/// blank label.
fn predicate_name(store: &EngineBox, ns: u16, id: u16) -> Result<String, String> {
    okm_core::obj_vocab::vocab_label(
        store,
        &ns.to_be_bytes(),
        okm_core::obj_vocab::PREDICATE_VOCAB,
        &id.to_be_bytes(),
    )
    .map_err(|e| e.to_string())?
    .ok_or_else(|| {
        format!(
            "okm: predicate {id} has no vocabulary name — the graph's vocabulary was lost under it"
        )
    })
}

/// `(ns key)` → the ref the dynamic graph takes. The key is the pkey byte vector
/// the collection verbs hand back (a vector of integers 0-255); a list or
/// a vector holds the pair, so `(list 16 bytes)` and `(vector 16 bytes)`
/// both name a node.
fn node_of(v: &SteelVal) -> Result<NodeRef, String> {
    let items: Vec<SteelVal> = match v {
        SteelVal::ListV(l) => l.iter().cloned().collect(),
        SteelVal::VectorV(vec) => vec.iter().cloned().collect(),
        other => {
            return Err(format!(
                "okm: a node is (ns key) — a 2-element list or vector, got {other:?}"
            ));
        }
    };
    if items.len() != 2 {
        return Err(format!(
            "okm: a node is (ns key), got {} element(s)",
            items.len()
        ));
    }
    let SteelVal::IntV(raw_ns) = &items[0] else {
        return Err("okm: a node's ns is an integer".to_string());
    };
    let ns = u16::try_from(*raw_ns).map_err(|_| format!("okm: ns {raw_ns} is not a u16"))?;
    let key = crate::collection::steel_bytes_to_vec(&items[1])?;
    Ok(NodeRef::new(&ns.to_be_bytes(), &key))
}

/// `'out` / `'in` / `'walk` (or the strings). `walk` is `out`'s chainable
/// spelling, the same alias `Graph::walk` is: a hop is a hop, and the depth
/// lives in the caller's loop.
fn direction_of(v: &SteelVal) -> Result<Direction, String> {
    let name = match v {
        SteelVal::SymbolV(s) | SteelVal::StringV(s) => s.to_string(),
        other => {
            return Err(format!(
                "okm: direction is 'out, 'in or 'walk (or the strings), got {other:?}"
            ));
        }
    };
    match name.as_str() {
        "out" | "walk" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        other => Err(format!(
            "okm: direction is 'out, 'in or 'walk (or the strings), got `{other}`"
        )),
    }
}

/// `#f` means any predicate; a symbol or a string names one to push down.
fn predicate_of(v: &SteelVal) -> Result<Option<String>, String> {
    match v {
        SteelVal::BoolV(false) => Ok(None),
        SteelVal::SymbolV(s) | SteelVal::StringV(s) => Ok(Some(s.to_string())),
        other => Err(format!(
            "okm: pred is a name (symbol or string) or #f for any, got {other:?}"
        )),
    }
}

/// A cursor handle: the integer `graph-hop-open` answered.
fn handle_of(v: &SteelVal) -> Result<u64, String> {
    let SteelVal::IntV(id) = v else {
        return Err(format!("okm: a cursor handle is an integer, got {v:?}"));
    };
    u64::try_from(*id).map_err(|_| format!("okm: cursor handle {id} is not a cursor"))
}

/// One hop → `(far-ns far-key predicate)`: the far endpoint as ns + key
/// bytes (never a handle), the predicate by NAME (never an id).
fn entry_to_steel(far: &NodeRef, predicate: &str) -> SteelVal {
    SteelVal::ListV(
        vec![
            SteelVal::IntV(far.ns_id() as isize),
            crate::collection::vec_to_steel(far.pkey.clone()),
            SteelVal::StringV(predicate.to_string().into()),
        ]
        .into(),
    )
}
