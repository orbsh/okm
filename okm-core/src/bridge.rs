//! Exports to the outside world: columnar (Arrow/Parquet) bridges and
//! the JSON-schema/snapshot tooling. These read the model layer's
//! exported constants (`FIELDS`, `PAYLOAD_FIELDS`, `CollectionSchema`) —
//! no derive cooperation needed.

#[cfg(feature = "arrow")]
pub mod arrow_bridge;
pub mod tooling;
