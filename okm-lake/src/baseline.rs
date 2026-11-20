//! Whether a log directory's lake reference is true (ADR-0071 §1, §4).
//!
//! A compaction against the lake writes `(table, version, covers)` into the log directory and
//! then deletes every segment up to `covers`. That is a CLAIM about another medium, and core
//! cannot check it: an object store is not its to list. This module is the check, and it is
//! deliberately in the lake crate — the crate that owns the medium — so the claim is verified
//! by the side that can see the evidence.
//!
//! Three things are asked, and a refusal names what was found instead:
//!
//! 1. the table has a log in the lake, and the table name is the one the pointer names;
//! 2. the version the pointer names exists, and the live files are read AT that version — a
//!    lake that has drained more windows since must not be what the answer comes from;
//! 3. every live window's manifest covers segments, and the highest of them is at least
//!    `covers`. A window without a manifest is a refusal, not a skip: a file whose coverage is
//!    unknown could hold exactly the segment the pointer claims to have landed.

use std::path::Path;

use okm_core::changelog::LakeBaseline;

use crate::delta;
use crate::status::manifest_at;
use crate::store::LakeStore;

/// What the lake actually holds, as opposed to what the pointer claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LakeCoverage {
    /// The version the pointer named, whose own state the answer was read from.
    pub version: u64,
    /// The highest segment ordinal found in the live windows' manifests.
    pub covers: u64,
    /// How many live data files the answer was assembled from.
    pub files: usize,
}

/// Verify a pointer against a lake table.
pub fn verify(
    store: &dyn LakeStore,
    table: &str,
    pointer: &LakeBaseline,
) -> Result<LakeCoverage, String> {
    if pointer.table != table {
        return Err(format!(
            "the pointer names table {:?}, this is {table:?}",
            pointer.table
        ));
    }
    let next = delta::next_version(store, table)?;
    if next == 0 {
        return Err(format!(
            "{table}: the lake has no log, so it cannot hold a baseline"
        ));
    }
    let latest = next - 1;
    if pointer.version > latest {
        return Err(format!(
            "the pointer names version {}, the lake's newest is {latest}",
            pointer.version
        ));
    }
    // Read V's own state: the lake may have drained past it, and a pointer is about the
    // version it names, not about where the lake happens to be now.
    let live = delta::files_in_order_at_version(store, table, pointer.version)?;
    if live.is_empty() {
        return Err(format!(
            "{table}: version {} has no live data files, so there is no state to point at",
            pointer.version
        ));
    }

    let mut covers = 0u64;
    let mut files = 0usize;
    for relative in live {
        let Some(manifest) = manifest_at(store, table, &relative) else {
            return Err(format!(
                "{table}/{relative}: no manifest, so its segment coverage cannot be checked"
            ));
        };
        files += 1;
        for segment in &manifest.segments {
            let ordinal = segment
                .split('.')
                .next()
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| format!("{table}/{relative}: segment name {segment:?}"))?;
            covers = covers.max(ordinal);
        }
    }
    if covers < pointer.covers {
        return Err(format!(
            "the live windows cover segments up to {covers}, the pointer claims {}",
            pointer.covers
        ));
    }
    Ok(LakeCoverage {
        version: pointer.version,
        covers,
        files,
    })
}

/// [`verify`] for a table inside a local lake directory.
pub fn verify_at(table_dir: &Path, pointer: &LakeBaseline) -> Result<LakeCoverage, String> {
    let (store, table) = delta::lake_of(table_dir)?;
    verify(&store, &table, pointer)
}
