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

use okm_core::changelog::{
    ChangelogStore, Compacted, LakeBaseline, header_hex, last_sealed, table_dir,
};
use okm_core::{Document, VirtualStorage};

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

/// Compact a capturing table against the lake, exporting the uncovered tail first (ADR-0071 §2).
///
/// [`compact_against`] refuses when the lake does not reach the newest sealed segment, and the
/// refusal is right — a pointer covers what it names. What it cannot do is make the lake catch
/// up, so a cutover waits on the drainer's schedule. This does it: the drainer's own writer runs
/// synchronously over whatever this consumer has not taken, and the pointer then names the
/// version that export produced.
///
/// `cfg.out_dir` is the lake root and `cfg.consumer` is the reader whose position the pin
/// advances — the same one the deployment drains with, so the pin takes a window the drainer
/// would have taken rather than writing the same segments a second time. A lake on a bucket
/// needs the store form of `drain_table_to`, which is not here yet; everything else is the path
/// form [`compact_against`] already uses.
pub fn compact_against_exporting<S, R>(
    store: &ChangelogStore<S>,
    cfg: &crate::DrainConfig,
    keep: usize,
) -> Result<(Compacted, crate::Report), String>
where
    S: VirtualStorage,
    R: Document,
{
    let root = store.config().root().ok_or_else(|| {
        "the log has no local directory, so a lake reference has nowhere to be recorded".to_string()
    })?;
    let header = R::NS_PREFIX;
    let dir = table_dir(root, header);
    let sealed = last_sealed(&dir).ok_or_else(|| {
        format!(
            "{}: nothing sealed, so a pointer would cover no segments",
            dir.display()
        )
    })?;
    let covers = sealed
        .split('.')
        .next()
        .and_then(|stem| stem.parse::<u64>().ok())
        .ok_or_else(|| format!("{}: segment name {sealed:?}", dir.display()))?;

    let mut report = crate::Report::default();
    crate::drain_table(&dir, cfg, &mut report).map_err(|e| format!("exporting the tail: {e}"))?;

    let table = header_hex(header);
    let lake_table = cfg.out_dir.join(&table);
    let next = delta::next_version_at(&lake_table)?;
    if next == 0 {
        return Err(format!(
            "{table}: the export produced no lake version, so a pointer would name nothing"
        ));
    }
    let version = next - 1;
    let claim = LakeBaseline {
        table,
        version,
        covers,
    };
    let coverage = verify_at(&lake_table, &claim)?;
    let compacted = store
        .compact_lake::<R>(version, coverage.covers, keep)
        .map_err(|e| e.to_string())?;
    Ok((compacted, report))
}

/// Compact a capturing table against the lake version that holds it (ADR-0071 §1).
///
/// The coverage is READ FROM THE LAKE, never assumed from the log: what V's own manifests say
/// they hold is exactly what may be deleted here. A lake that is behind therefore prunes less
/// and leaves a longer tail — that is the fallback working, not a failure — and a lake with no
/// such version refuses before anything is touched, so a caller that wanted the scanned
/// baseline still can.
pub fn compact_against<S, R>(
    store: &ChangelogStore<S>,
    lake_table_dir: &Path,
    version: u64,
    keep: usize,
) -> Result<Compacted, String>
where
    S: VirtualStorage,
    R: Document,
{
    let root = store.config().root().ok_or_else(|| {
        "the log has no local directory, so a lake reference has nowhere to be recorded".to_string()
    })?;
    let header = R::NS_PREFIX;
    let dir = table_dir(root, header);
    if last_sealed(&dir).is_none() {
        return Err(format!(
            "{}: nothing sealed, so a pointer would cover no segments",
            dir.display()
        ));
    }
    let claim = LakeBaseline {
        table: header_hex(header),
        version,
        covers: 0,
    };
    // Ordinal 0 is a segment too, so "covers 0" is a real answer: the guard against an
    // empty version is verify's own ("no live data files"), not a zero here.
    let coverage = verify_at(lake_table_dir, &claim)?;
    store
        .compact_lake::<R>(version, coverage.covers, keep)
        .map_err(|e| e.to_string())
}
