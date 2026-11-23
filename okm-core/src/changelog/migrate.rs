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
//! The vacuum is its own function and its own verb, never a step of the migration run: a
//! migrated table keeps its old range and its old record until someone asks for the vacuum
//! (ADR-0075 §"a migrated table keeps its old range"), so a failed verification costs a re-run
//! rather than the data, and a reader still on the old generation keeps working through its
//! window.

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
    /// Rows the verification compared on both sides before the flip.
    pub verified: usize,
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
    // ---- the verification, before anything is flipped ----
    // Nothing in the store has changed yet as far as readers are concerned: no record names the
    // new generation, so a failure here costs a re-run and never a byte of the table.
    let verified = compare_generations(&reader, &writer)?;

    // One put of the record: from here on the store says this table is on the new generation,
    // and the old range is history that still answers a reader holding the old declaration.
    writer.ensure_layout()?;

    // ... and the record describes the declaration it flipped to (ADR-0074 §2). `ensure_layout`
    // seeds the name and the bytes only for a schema that carries a name, and a schema exported
    // from a build may not — so the one place that holds the target declaration at flip time
    // writes it down. A store that cannot say which declaration wrote it is a store a dynamic
    // reader cannot open, and a vacuum cannot verify.
    let mut flipped = crate::model::meta::read(store, &to_address)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{}: the flip wrote no record", hex(&to_address)))?;
    if flipped.get(crate::model::meta::SCHEMA).is_none() {
        flipped.set(crate::model::meta::SCHEMA, target.encode());
        crate::model::meta::write(store, &to_address, &flipped).map_err(|e| e.to_string())?;
    }

    Ok(Migration {
        from,
        to,
        from_address,
        to_address,
        copied,
        replayed,
        verified: verified.rows,
    })
}

/// What a verification compared, and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    /// Rows present in both ranges.
    pub rows: usize,
    /// Rows only the new range has: a row the writer created while the copy ran, or a row a
    /// caller added to the new generation on purpose. Never a failure — a migration may add
    /// rows, it may not lose them.
    pub added: usize,
    /// Fields the new declaration declares and the old one did not, counted across the rows.
    pub new_fields: usize,
}

/// Compare two ranges row by row, each through its own declaration.
///
/// The comparison is on DECODED rows, not on bytes: the two generations disagree about offsets
/// by construction, so equal values at different offsets is the normal case and equal bytes
/// never is. Three things are refused, and each is a way a copy goes wrong rather than a way it
/// merely changes: a row the old range has and the new one does not (a copy that stopped early,
/// or a tail replayed into the wrong range), a field the old row carries whose value differs
/// (wrong bytes for that row), and a field the new declaration dropped (a read that would lose
/// data the store still holds).
fn compare_generations<S: VirtualStorage>(
    old: &DynamicCollection<S>,
    new: &DynamicCollection<S>,
) -> Result<Verification, String> {
    let old_keys: std::collections::BTreeSet<Vec<u8>> =
        old.scan_document_keys()?.into_iter().collect();
    let new_keys: std::collections::BTreeSet<Vec<u8>> =
        new.scan_document_keys()?.into_iter().collect();

    let missing: Vec<&Vec<u8>> = old_keys.difference(&new_keys).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} of {} rows are in the old range and not in the new one — first {}; nothing has \
             been flipped, so the old range is still the table and a re-run is safe",
            missing.len(),
            old_keys.len(),
            missing.first().map(|k| hex(k)).unwrap_or_default()
        ));
    }

    let mut rows = 0usize;
    let mut new_fields: std::collections::BTreeSet<String> = Default::default();
    for pkey in &old_keys {
        let Some(from) = old.fetch(pkey)? else {
            continue;
        };
        let Some(to) = new.fetch(pkey)? else {
            return Err(format!(
                "{}: the row reads in the old range and as nothing in the new one",
                hex(pkey)
            ));
        };
        for (name, value) in from.iter() {
            match to.get(name) {
                Some(other) if other == value => {}
                Some(other) => {
                    return Err(format!(
                        "{}: field `{name}` decodes to {value:?} in the old range and {other:?} in \
                         the new one — the copy wrote the wrong bytes for this row",
                        hex(pkey)
                    ));
                }
                None => {
                    return Err(format!(
                        "{}: field `{name}` is in the old declaration and not in the new one — a \
                         generation may add fields, not drop what the store still holds",
                        hex(pkey)
                    ));
                }
            }
        }
        new_fields.extend(to.keys().filter(|k| !from.contains_key(*k)).cloned());
        rows += 1;
    }

    Ok(Verification {
        rows,
        added: new_keys.len() - rows,
        new_fields: new_fields.len(),
    })
}

/// What a vacuum took away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vacuum {
    /// The generation that stays.
    pub kept: u8,
    /// The generation whose range was removed — `0` when there was nothing older to remove,
    /// which makes a second run the same answer as a finished first one.
    pub removed: u8,
    /// Rows deleted from the old range (data, index entries and the old record together).
    pub keys: usize,
    /// Sealed segments whose directory was retired, when a log root was given.
    pub log_segments: usize,
    /// Rows the gate compared before anything was deleted.
    pub verified: usize,
}

/// What a vacuum would do — the same gate, the same counts, and not one key deleted.
///
/// This is the pre-flight: an operator reads it before the run that removes data, and it
/// answers the only two questions that matter in that order — does the gate pass, and what
/// exactly would go.
pub fn vacuum_preview<S: VirtualStorage + Clone>(
    store: &S,
    root: Option<&Path>,
    table: &[u8],
) -> Result<Vacuum, String> {
    vacuum_inner(store, root, table, false)
}

/// Retire the older generation a migration left behind (ADR-0075 §"a migrated table keeps its
/// old range and record until the vacuum").
pub fn vacuum_generation<S: VirtualStorage + Clone>(
    store: &S,
    root: Option<&Path>,
    table: &[u8],
) -> Result<Vacuum, String> {
    vacuum_inner(store, root, table, true)
}

/// The body both verbs share: the gate, then the deletion the caller asked for.
///
/// Two things go, and only when the store says which one is older: every key under the old
/// address — the data, the index entries and the old record are all in that range — and, when a
/// log root is given, the old generation's directory. The second is not cosmetic: a log left
/// behind would be replayed into a range that has no record, which is a table nobody can open.
fn vacuum_inner<S: VirtualStorage + Clone>(
    store: &S,
    root: Option<&Path>,
    table: &[u8],
    delete: bool,
) -> Result<Vacuum, String> {
    let (kept, kept_address, kept_row) = crate::model::meta::current(store, table, 0)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "{}: the store records no table at this address — there is no generation to keep",
                hex(table)
            )
        })?;
    let old_address = crate::model::meta::other_parity(&kept_address);
    let older = crate::model::meta::read(store, &old_address).map_err(|e| e.to_string())?;
    let Some(older_row) = older else {
        // Already vacuumed, or never migrated: one answer, so a re-run is harmless.
        return Ok(Vacuum {
            kept,
            removed: 0,
            keys: 0,
            log_segments: 0,
            verified: 0,
        });
    };
    let removed = older_row.layout_version().ok_or_else(|| {
        format!(
            "{}: the other generation's record names no version, so which one is older is not \
             a question the store can answer",
            hex(&old_address)
        )
    })?;
    if removed >= kept {
        return Err(format!(
            "v{removed} is not older than v{kept}: the store's current record is the one to keep, \
             and the other address is not a generation behind it (ADR-0075 §3)"
        ));
    }

    // ---- the gate ----
    // Nothing is deleted until the two ranges agree row for row: the old range is the only copy
    // of anything the new one lacks, so a vacuum in front of this check would be the one
    // irreversible way to lose data (ADR-0075 §"a failed verification costs a re-run"). Both
    // declarations come from the records themselves, so the gate reads the same bytes the store
    // would hand a reader.
    let old_schema =
        CollectionSchema::decode(older_row.get(crate::model::meta::SCHEMA).ok_or_else(|| {
            format!(
                "{}: the older record names no schema, so its rows cannot be read to verify them",
                hex(&old_address)
            )
        })?);
    let kept_schema =
        CollectionSchema::decode(kept_row.get(crate::model::meta::SCHEMA).ok_or_else(|| {
            format!(
                "{}: the current record names no schema, so its rows cannot be read to verify them",
                hex(&kept_address)
            )
        })?);
    let ns = identity(&kept_address);
    let old_handle = DynamicCollection::new(store.clone(), ns, old_schema, Vec::new());
    let new_handle = DynamicCollection::new(store.clone(), ns, kept_schema, Vec::new());
    let verified = compare_generations(&old_handle, &new_handle).map_err(|e| {
        format!("vacuum refused: {e}. The old range is still the only copy of what it lacks")
    })?;

    // One pass to collect, one to delete: an engine iterator must not move under its own
    // deletions.
    let end = crate::storage::prefix_end(&old_address);
    let doomed: Vec<Vec<u8>> = store
        .scan_range(&old_address, end.as_deref())
        .map(|item| item.map(|(key, _)| key))
        .collect::<crate::error::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    if delete {
        for key in &doomed {
            store.del(key).map_err(|e| e.to_string())?;
        }
    }
    // The record row sits inside that range, but a store that recorded it elsewhere would leave
    // a record pointing at nothing — so it is named once, explicitly.
    if delete {
        store
            .del(&crate::model::meta::meta_key(&old_address))
            .map_err(|e| e.to_string())?;
    }

    let mut log_segments = 0usize;
    if let Some(root) = root {
        let dir = crate::changelog::table_dir(root, &old_address);
        if dir.exists() {
            log_segments = std::fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| {
                            e.path()
                                .extension()
                                .and_then(|x| x.to_str())
                                .is_some_and(|x| x == "segment")
                        })
                        .count()
                })
                .unwrap_or(0);
            if delete {
                std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
        }
    }

    Ok(Vacuum {
        kept,
        removed,
        keys: doomed.len(),
        log_segments,
        verified: verified.rows,
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
