//! A generation change as a mechanism (ADR-0075 §5–§7): verdict, copy, catch-up, flip.
//!
//! The three legs the ADR names, in the order that keeps a failed run harmless:
//!
//! 1. **Verdict.** The store's record says which generation is live; the caller names the one
//!    it wants. A move that does not change the parity is refused here, because two
//!    generations at one address is the state the bit exists to make impossible.
//! 2. **Copy.** Every row of the old range is decoded *through the old declaration* and written
//!    through the new one. This is a re-encode, not a byte move: the two generations differ in
//!    offsets, and reading one with the other is the misparse attach refuses.
//! 3. **Catch-up and flip.** Rows written while the copy ran are the log's tail — replayed
//!    after the mark the copy started from — and then one `put` of the new record is the flip.
//!
//! What is deliberately *not* here: the vacuum. A migrated table keeps its old range and its
//! old record until the vacuum (ADR-0075 §"a migrated table keeps its old range"), so a failed
//! verification costs a re-run rather than the data, and a reader that is still on the old
//! generation keeps working through its window.

use std::path::Path;

use crate::model::document::IDENTITY_MASK;
use crate::model::dynamic_collection::DynamicCollection;
use crate::schema::CollectionSchema;
use crate::storage::VirtualStorage;

/// What a migration did, in the terms the operator can check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// The generation the store was on.
    pub from: u8,
    /// The generation it is on now.
    pub to: u8,
    /// The address the old range sits at (kept until the vacuum).
    pub from_address: Vec<u8>,
    /// The address the new range sits at.
    pub to_address: Vec<u8>,
    /// Rows copied out of the old range.
    pub copied: usize,
    /// Segments replayed on top of the copy — the tail the writer produced while it ran.
    pub replayed: usize,
}

/// Migrate one table to `target`'s generation (ADR-0075 §5–§7).
///
/// `table` is the header the store records for this table — the address, generation included —
/// and `root` is the log's root directory, which is where the tail comes from. `root: None`
/// skips the catch-up, which is only honest when nothing was writing during the copy: the
/// caller that hands `None` is a caller that quiesced the table itself.
pub fn migrate_table<S>(
    store: &S,
    root: Option<&Path>,
    table: &[u8],
    target: &CollectionSchema,
) -> Result<Migration, String>
where
    S: VirtualStorage + Clone,
{
    let (from, from_address, row) = crate::model::meta::current(store, table, 0)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{}: the store records no table at this address — attach it first (ADR-0074)",
                hex(table)
            )
        })?;
    // The old declaration is not decoded here on purpose: the reader below reads it from the
    // record itself (ADR-0074 §5), so the copy cannot drift from what the store says the old
    // generation is.
    if row.get(crate::model::meta::SCHEMA).is_none() {
        return Err(format!(
            "{}: the record names no schema, so there is no old layout to read the rows with",
            hex(&from_address)
        ));
    }

    // ---- the verdict ----
    let to = target.layout_version;
    if to != from.saturating_add(1) {
        return Err(format!(
            "layout v{from} → v{to}: a generation moves by one, because that is what flips the \
             parity of the address (ADR-0075 §2). Two generations at one address cannot be told \
             apart, and the store refuses to serve one as the other"
        ));
    }
    let ns = identity(&from_address);
    if identity(&target.ns_prefix) != ns {
        return Err(format!(
            "the target describes ns {:#06x}, not {:#06x}: a migration moves the generation of \
             one table, never the table (ADR-0075 §2)",
            identity(&target.ns_prefix),
            ns
        ));
    }
    let to_address = crate::model::table_plan::address_of(target);

    // ---- the copy ----
    // The reader is opened BEFORE the flip, so it addresses the old range: `open` takes the
    // larger version, and the larger one is still the old generation's until the flip.
    let reader = DynamicCollection::open(store.clone(), ns)?;
    let writer = DynamicCollection::new(store.clone(), ns, target.clone(), Vec::new());
    // Where the copy starts: everything the log has sealed by now is already in the rows being
    // copied, so only segments after this ordinal are the tail.
    let mark = match root {
        Some(root) => last_ordinal(&crate::changelog::table_dir(root, &from_address))?,
        None => 0,
    };

    let mut copied = 0usize;
    for pkey in reader.scan_document_keys()? {
        let Some(row) = reader.fetch(&pkey)? else {
            continue;
        };
        writer.put(&pkey, &row)?;
        copied += 1;
    }

    // ---- the tail, then the flip ----
    let mut replayed = 0usize;
    if let Some(root) = root {
        let dir = crate::changelog::table_dir(root, &from_address);
        let report = crate::changelog::replay::replay_after(&dir, mark, &writer)?;
        replayed = report.segments;
    }
    // One put of the record: from here on the store says this table is on the new generation,
    // and the old range is history that still answers a reader holding the old declaration.
    writer.ensure_layout()?;

    Ok(Migration {
        from,
        to,
        from_address,
        to_address,
        copied,
        replayed,
    })
}

/// The table a header names, as a number: its ns with the generation bit off.
fn identity(header: &[u8]) -> u16 {
    match header {
        [.., hi, lo] => u16::from_be_bytes([*hi, *lo]) & IDENTITY_MASK,
        _ => 0,
    }
}

/// The highest sealed segment number in a directory (`0` when none has been sealed).
fn last_ordinal(dir: &Path) -> Result<u64, String> {
    let mut highest = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A table that has never drained has no directory yet: nothing is sealed, so the mark
        // is zero and the whole log is the tail.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((ordinal, extension)) = name.split_once('.') else {
            continue;
        };
        if extension != "segment" {
            continue;
        }
        if let Ok(n) = ordinal.parse::<u64>() {
            highest = highest.max(n);
        }
    }
    Ok(highest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
