//! Exports to the outside world: columnar (Arrow/Parquet) bridges and
//! the JSON-schema/snapshot tooling. These read the model layer's
//! exported constants (`FIELDS`, `PAYLOAD_FIELDS`, `CollectionSchema`) —
//! no derive cooperation needed.

#[cfg(feature = "arrow")]
pub mod arrow_bridge;
/// Parquet METADATA (manifests, `v1.metadata.json`), so it is a `parquet`
/// feature: `arrow` alone must build without the writer crate (a consumer that
/// only builds columns should not pay for the format).
#[cfg(feature = "parquet")]
pub mod iceberg;
pub mod tooling;
