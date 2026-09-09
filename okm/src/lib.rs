//! OKM — object-keyspace mapping runtime.
//!
//! OKM is the KV counterpart of ORM: derive macros map Rust structs onto
//! binary KV keys, turning schema correctness from a runtime database
//! concern into a compile-time guarantee. The macro layer is deliberately
//! storage-free — every `encode`/`decode` is a pure `Vec<u8>` in/out
//! function, and engine choice belongs to the assembly site.
//!
//! # Architecture
//!
//! ```text
//! KeyEncode / EdgeEncode derive macros   ← compile-time codecs, zero I/O
//!         ↓ expand into pure functions
//! Collection<S, E>                       ← assembly point: engine + edge type
//!         ↓ trait dispatch
//! KvEngine (MockStore / FjallStore / SlatedbStore)  ← real storage lives here
//! ```
//!
//! # Features
//!
//! - `fjall` — sync [`FjallStore`] engine adapter.
//! - `slatedb` — async [`SlatedbStore`] engine adapter (+ [`AsyncCollection`]).
//!
//! See `docs/adr/` for the design decisions (direction-bit niche, namespace
//! dictionary, assembly point, value-side roadmap) and the project README
//! for a full walkthrough.

pub use okm_derive::{EdgeEncode, KeyEncode, RowEncode};

pub use reverse::{Reversible, Reverse};
pub use wrappers::{Enum, EnumTag, Offset, Quant, VarInt, VarIntEnc, offset_decode, offset_encode};

pub mod collection;
pub mod edge;
pub mod engine;
pub mod field;
pub mod index;
pub mod key;
pub mod reverse;
pub mod table;
pub mod wrappers;

#[cfg(feature = "arrow")]
pub mod arrow_bridge;
#[cfg(feature = "fjall")]
pub mod fjall_backend;
#[cfg(feature = "slatedb")]
pub mod slatedb_backend;
pub mod tooling;

pub use collection::EdgeTable;
pub use edge::{KvEdge, head_bytes};
pub use engine::{KvEngine, MockStore};
pub use field::{FieldDesc, FieldType};
pub use index::{IndexFuncResult, KvIndex, PRIMARY_SLOT, Row, scan_index};
pub use key::{KeyEncode, PrefixKey};
pub use table::Table;

#[cfg(feature = "arrow")]
pub use arrow_bridge as arrow_backend;
#[cfg(feature = "parquet")]
pub use tooling::parquet_io;

#[cfg(feature = "fjall")]
pub use fjall_backend::FjallStore;
#[cfg(feature = "slatedb")]
pub use slatedb_backend::{AsyncEdgeTable, KvEngineAsync, SlatedbStore};
