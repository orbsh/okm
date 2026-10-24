//! Junction assembly point: [`Junction`]`<S, E>` — engine + junction type =
//! the operation surface of one relationship. Junctions are the
//! node-to-node accessor family; document collections use
//! [`crate::model::document::Collection`] (ADR-0006). Each entry is one-way and
//! lives in its endpoint's own ns (ADR-0015/0016).
//!
//! No macro binds key and junction types: binding needs type parameters,
//! not code generation (see `docs/adr/0003`). The macro layer stays
//! storage-free; engine choice and lifecycle belong to the call site
//! (`Junction::new(store)`).

use crate::model::junction::KvJunction;
use crate::engine::storage::VirtualStorage;
use crate::model::key::{KeyEncode, PrefixKey};

/// Engine `S` + junction `E` = the operation surface of one relationship.
pub struct Junction<S, E> {
    pub store: S,
    _pd: std::marker::PhantomData<E>,
}

impl<S: VirtualStorage, E: KvJunction> Junction<S, E> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            _pd: std::marker::PhantomData,
        }
    }

    /// Atomic double write: one entry in each endpoint's ns.
    pub fn link(&self, a: &<E::A as crate::model::index::Document>::Key, b: &<E::B as crate::model::index::Document>::Key) -> crate::error::Result<()> {
        let mut batch = crate::engine::storage::Batch::default();
        self.save_into(&mut batch, a, b);
        // Both entries ride ONE commit: a half-written junction is a
        // relationship that exists in one direction only.
        self.store.commit_batch(batch)
    }

    /// Encode this junction's double write into an externally owned batch —
    /// no write until commit. The cross-collection atomic path (ADR-0003):
    /// documents and junctions share one batch, one `commit_batch` covers
    /// them all.
    pub fn save_into(
        &self,
        batch: &mut crate::engine::storage::Batch,
        a: &<E::A as crate::model::index::Document>::Key,
        b: &<E::B as crate::model::index::Document>::Key,
    ) {
        let e = E::from_parts(a.clone(), b.clone());
        batch.put(e.a_side_key(), Vec::new());
        batch.put(e.b_side_key(), Vec::new());
    }

    /// Removes both entries.
    pub fn unlink(&self, a: &<E::A as crate::model::index::Document>::Key, b: &<E::B as crate::model::index::Document>::Key) -> crate::error::Result<()> {
        let e = E::from_parts(a.clone(), b.clone());
        let mut batch = crate::engine::storage::Batch::default();
        batch.del(&e.a_side_key());
        batch.del(&e.b_side_key());
        self.store.commit_batch(batch)
    }

    /// A-side scan: Bs linked to `a` (reads A's collection). Requires B to
    /// have full identity (decodable).
    pub fn forward(&self, a: &<E::A as crate::model::index::Document>::Key) -> crate::error::Result<Vec<<E::B as crate::model::index::Document>::Key>> {
        if !E::B_HEAD.is_empty() {
            return Err(crate::error::Error::Corrupt {
                what: "forward requires B full identity to decode back into the key type; \
                       use reverse_prefix when the endpoint is truncated"
                    .into(),
            });
        }
        let p = E::a_side_prefix(a);
        Ok(self
            .store
            .scan_suffix(&p)?
            .iter()
            .map(|suffix| <E::B as crate::model::index::Document>::Key::decode(suffix))
            .collect())
    }

    /// B-side scan, raw prefix bytes (returned for a collection prefix scan
    /// when A's identity is truncated and cannot be decoded).
    pub fn reverse_raw(
        &self,
        b: &<E::B as crate::model::index::Document>::Key,
    ) -> crate::error::Result<Vec<Vec<u8>>> {
        let p = E::b_side_prefix(b);
        self.store.scan_suffix(&p)
    }

    /// B-side scan, decodes back into A's key type when A has full identity.
    pub fn reverse(&self, b: &<E::B as crate::model::index::Document>::Key) -> crate::error::Result<Vec<<E::A as crate::model::index::Document>::Key>> {
        if !E::A_HEAD.is_empty() {
            return Err(crate::error::Error::Corrupt {
                what: "A is a truncated identity and cannot be decoded; use reverse_raw".into(),
            });
        }
        Ok(self
            .reverse_raw(b)?
            .iter()
            .map(|sfx| <E::A as crate::model::index::Document>::Key::decode(sfx))
            .collect())
    }

    /// B-side scan (truncated-identity product): only the first `taken`
    /// bytes are trustworthy.
    pub fn reverse_prefix(
        &self,
        b: &<E::B as crate::model::index::Document>::Key,
    ) -> crate::error::Result<Vec<PrefixKey<<E::A as crate::model::index::Document>::Key>>> {
        let taken = if E::A_HEAD.is_empty() {
            <<E::A as crate::model::index::Document>::Key as KeyEncode>::KEY_LEN
        } else {
            <<E::A as crate::model::index::Document>::Key as KeyEncode>::prefix_width(E::A_HEAD)
        };
        Ok(self
            .reverse_raw(b)?
            .iter()
            .map(|sfx| PrefixKey {
                decoded: <E::A as crate::model::index::Document>::Key::decode(sfx),
                taken,
            })
            .collect())
    }
}
