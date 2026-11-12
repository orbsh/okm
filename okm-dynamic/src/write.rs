//! One write verb, the payload type chooses the path (ADR-0035 as amended) —
//! the runtime-typed mirror of `okm_core::WritePayload`.
//!
//! The typed assembly point binds a document type at compile time, so its
//! payloads are `&R`, a `Variant::Map` and the one-element tuple that
//! marks "the whole thing". This facade has no `R` — a document IS a name-keyed
//! map — so its payloads are the map itself for the partial document and the
//! same one-tuple rule for the whole one. `&Value` is accepted too, so a caller
//! that already holds an `okm_value::Variant` spells the call exactly as
//! it does on the typed side; a non-object value is refused there, because a
//! scalar or an array is a field VALUE and not a document.
//!
//! `merge` and `replace` are gone from the public surface: they were the same
//! op list with a different rule for absent names, which is a statement about
//! the payload, not about the verb.

use okm_core::storage::VirtualStorage;

use crate::Dict;
use crate::collection::DynamicCollection;
use okm_value::Variant;

/// What a `put` writes, and therefore which path it takes.
pub trait WritePayload<S: VirtualStorage> {
    /// Write this payload at `pkey` through `col`'s write path.
    fn write(self, col: &DynamicCollection<S>, pkey: &[u8]) -> Result<(), String>;
}

/// Partial document: a map, i.e. the names this write carries. Every other name
/// in the stored document is left as it was.
impl<S: VirtualStorage> WritePayload<S> for &Dict {
    fn write(self, col: &DynamicCollection<S>, pkey: &[u8]) -> Result<(), String> {
        col.merge_map(pkey, self)
    }
}

/// Partial document, spelled as the typed side spells it. Only an object is a
/// document.
impl<S: VirtualStorage> WritePayload<S> for &Variant {
    fn write(self, col: &DynamicCollection<S>, pkey: &[u8]) -> Result<(), String> {
        match self {
            Variant::Map(object) => col.merge_map(pkey, object),
            other => Err(not_a_document(other)),
        }
    }
}

/// Whole document: the map IS the document, so a name it omits is gone.
impl<S: VirtualStorage> WritePayload<S> for (&Dict,) {
    fn write(self, col: &DynamicCollection<S>, pkey: &[u8]) -> Result<(), String> {
        col.replace_map(pkey, self.0)
    }
}

/// Whole document, spelled as the typed side spells it.
impl<S: VirtualStorage> WritePayload<S> for (&Variant,) {
    fn write(self, col: &DynamicCollection<S>, pkey: &[u8]) -> Result<(), String> {
        match self.0 {
            Variant::Map(object) => col.replace_map(pkey, object),
            other => Err(not_a_document(other)),
        }
    }
}

fn not_a_document(value: &Variant) -> String {
    format!(
        "a whole-document write takes an object; the payload is {}",
        okm_value::dyn_kind(value)
    )
}
