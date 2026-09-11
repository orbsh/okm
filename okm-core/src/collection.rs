//! Edge assembly point: [`EdgeTable`]`<S, E>` — engine + edge type = the
//! operation surface of one relationship. Edges are the node-to-node
//! accessor family; row tables use [`crate::table::Table`] (ADR-0006:
//! Collection narrowed to edges, storage bound to the store instance).
//!
//! No `KvRecord` macro exists: binding key and value/edge types needs type
//! parameters, not code generation (see `docs/adr/0003`). The macro layer
//! stays storage-free; engine choice and lifecycle belong to the call site
//! (`EdgeTable::new(store)`).

use crate::edge::KvEdge;
use crate::engine::KvEngine;
use crate::key::{KeyEncode, PrefixKey};

/// Engine `S` + edge `E` = the operation surface of one relationship.
pub struct EdgeTable<S, E> {
    pub store: S,
    _pd: std::marker::PhantomData<E>,
}

impl<S: KvEngine, E: KvEdge> EdgeTable<S, E> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            _pd: std::marker::PhantomData,
        }
    }

    /// Atomic double write: forward + reverse key.
    pub fn link(&mut self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        let fk = e.forward_key();
        let rk = e.reverse_key();
        self.store.put(fk, Vec::new());
        self.store.put(rk, Vec::new());
    }

    /// Encode this edge's double write (forward + reverse) into an
    /// externally owned batch — no write until commit. The
    /// cross-collection atomic path (ADR-0003): rows and edges share one
    /// batch, one `commit_batch` covers them all.
    pub fn save_into(&self, batch: &mut impl crate::engine::KvBatch, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        batch.put(e.forward_key(), Vec::new());
        batch.put(e.reverse_key(), Vec::new());
    }

    /// Removes both directions.
    pub fn unlink(&mut self, a: &E::A, b: &E::B) {
        let e = E::from_parts(a.clone(), b.clone());
        self.store.del(&e.forward_key());
        self.store.del(&e.reverse_key());
    }

    /// Scan prefix = header ++ A·identity (trailing partial bytes of A's
    /// identity are dropped — everything after A·identity belongs to an
    /// arbitrary B and must not participate in matching).
    fn forward_prefix(a: &E::A) -> Vec<u8> {
        let mut p = Vec::with_capacity(2 + E::a_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, false));
        E::encode_a_head(&mut p, a);
        p
    }

    /// A → Bs: forward scan. Requires B to have full identity (decodable).
    pub fn forward(&self, a: &E::A) -> Vec<E::B> {
        assert!(
            E::B_HEAD.is_empty(),
            "forward requires B full identity to decode back into the type"
        );
        let p = Self::forward_prefix(a);
        self.store
            .scan_suffix(&p)
            .iter()
            .map(|suffix| E::B::decode(suffix))
            .collect()
    }

    /// B → As, raw prefix bytes (returned for a main-table prefix scan when
    /// A's identity is truncated and cannot be decoded).
    pub fn reverse_raw(&self, b: &E::B) -> Vec<Vec<u8>> {
        let mut p = Vec::with_capacity(2 + E::b_head_width());
        p.extend_from_slice(&crate::edge::head_bytes(E::NS, true));
        E::encode_b_head(&mut p, b);
        self.store.scan_suffix(&p)
    }

    /// B → As: reverse scan, decodes back into the type when A has full
    /// identity.
    pub fn reverse(&self, b: &E::B) -> Vec<E::A> {
        assert!(
            E::A_HEAD.is_empty(),
            "A is a truncated identity and cannot be decoded; use reverse_raw"
        );
        self.reverse_raw(b)
            .iter()
            .map(|sfx| E::A::decode(sfx))
            .collect()
    }

    /// B → As (truncated-identity product): only the first `taken` bytes are
    /// trustworthy.
    pub fn reverse_prefix(&self, b: &E::B) -> Vec<PrefixKey<E::A>> {
        let taken = if E::A_HEAD.is_empty() {
            E::A::KEY_LEN
        } else {
            E::A::prefix_width(E::A_HEAD)
        };
        self.reverse_raw(b)
            .iter()
            .map(|sfx| PrefixKey {
                decoded: E::A::decode(sfx),
                taken,
            })
            .collect()
    }
}
