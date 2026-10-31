//! The change log (ADR-0045): what a committed batch leaves behind.
//!
//! Three locks matter more than the rest. **The truth half only** — a write set
//! that carries a primary entry, an index face and a reduce accumulator leaves
//! ONE op, because the derived half is rebuilt from rows by design. **The whole
//! document verbs too** — `merge` / `replace` / `purge` go through
//! `save_with_prev_into`, which emits no subscribe event (ADR-0031 §5), so a
//! capture hung there would miss them silently. **Labels are truth** — a
//! categorical vocabulary's two directions ride (`0x1nnn`/`0x2nnn`) while its
//! allocation cursor, a bare face prefix, does not.
//!
//! The tail rules: a sealed segment is complete by construction (sealing is a
//! rename), so a short record inside one is corruption; an unsealed `.partial`
//! has a torn tail by definition, so a short record there is just the end.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_core::changelog::{
    ChangelogConfig, PARTIAL_EXT, read_open_tail, read_sealed, sealed_segments,
};
use okm_core::{
    Batch, Collection, Document, DocumentEncode, KeyEncode, OP_DELETE, OP_PUT, TestStore, Vocab,
};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct LogKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LogKey)]
#[ok_ns(57)]
#[ok_index(by_author { fields(author) })]
pub struct LogRow {
    pub author: u64,
    pub title: String,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct TagKey {
    pub id: u64,
}

/// A categorical field: the payload stores the id, so both vocabulary
/// directions are truth and must reach the log (ADR-0032).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TagKey)]
#[ok_ns(58)]
pub struct TagRow {
    pub status: Vocab<u8>,
    pub body: String,
}

/// The second table of the cross-table test: same key type, DIFFERENT ns, so the
/// two collections have different headers — which is what "cross-table" means to
/// a capture filter.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LogKey)]
#[ok_ns(59)]
pub struct OtherRow {
    pub author: u64,
    pub title: String,
}

const OTHER_NS: &[u8] = <OtherRow as Document>::NS_PREFIX;

const LOG_NS: &[u8] = <LogRow as Document>::NS_PREFIX;
const TAG_NS: &[u8] = <TagRow as Document>::NS_PREFIX;

/// One committed batch as the log hands it back: `(tag, key, value)` per op.
type CapturedFrame = Vec<(u8, Vec<u8>, Vec<u8>)>;

fn slot_of(header: &[u8], key: &[u8]) -> u16 {
    let rest = key
        .strip_prefix(header)
        .expect("captured key carries the header");
    let first = *rest.first().expect("captured key has a slot byte");
    if first <= 0x0F {
        u16::from(first)
    } else {
        u16::from_be_bytes([first, *rest.get(1).expect("two-byte slot")])
    }
}

/// Every captured slot is truth: the fixed truth faces, or a vocabulary family.
fn assert_truth_only(header: &[u8], ops: &[(u8, Vec<u8>, Vec<u8>)]) {
    for (_, key, _) in ops {
        let slot = slot_of(header, key);
        let truth = matches!(slot, 0x00..=0x05) || matches!(slot >> 12, 0x1 | 0x2);
        assert!(truth, "captured a derived entry at slot 0x{slot:04X}");
    }
}

/// Drop the collection, then read every sealed segment back in order.
fn drain<S>(collection: Collection<S, LogKey, LogRow>, root: &std::path::Path) -> Vec<CapturedFrame>
where
    S: okm_core::VirtualStorage,
{
    drop(collection);
    let segments = sealed_segments(root, LOG_NS).expect("list segments");
    assert!(!segments.is_empty(), "a clean drop seals what it wrote");
    segments
        .iter()
        .flat_map(|path| read_sealed(path).expect("sealed segment reads"))
        .map(|frame| frame.0)
        .collect()
}

#[test]
fn a_put_leaves_the_row_and_drops_the_derived_faces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("changelog opens");

    let key = LogKey { id: 7 };
    app.put(
        &key,
        &LogRow {
            author: 1,
            title: "hello".into(),
        },
    )
    .expect("put");
    let primary = app.primary_key(&key);

    let frames = drain(app, dir.path());
    assert_eq!(frames.len(), 1, "one committed batch = one frame");
    let ops = &frames[0];
    assert_eq!(
        ops.len(),
        1,
        "the index face is derived and must not be logged"
    );
    let (tag, logged_key, value) = &ops[0];
    assert_eq!(*tag, OP_PUT);
    assert_eq!(logged_key, &primary, "the logged key is the full address");
    assert!(!value.is_empty(), "the row's payload rides");
    assert_truth_only(LOG_NS, ops);
}

#[test]
fn the_whole_document_verbs_are_captured() {
    // ADR-0031 §5: these go through `save_with_prev_into`, which emits no
    // subscribe event. A capture hung on that sink would lose all four.
    let dir = tempfile::tempdir().expect("tempdir");
    let app =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("changelog opens");

    let key = LogKey { id: 1 };
    app.put(
        &key,
        &LogRow {
            author: 1,
            title: "first".into(),
        },
    )
    .expect("put");

    let mut named = std::collections::BTreeMap::new();
    named.insert(
        "title".to_string(),
        okm_core::model::obj_dynamic::DynamicValue::Str("second".into()),
    );
    app.merge(&key, &named).expect("merge");

    let mut whole = std::collections::BTreeMap::new();
    whole.insert(
        "author".to_string(),
        okm_core::model::obj_dynamic::DynamicValue::UInt(9),
    );
    whole.insert(
        "title".to_string(),
        okm_core::model::obj_dynamic::DynamicValue::Str("third".into()),
    );
    app.replace(&key, &whole).expect("replace");

    app.purge(&key).expect("purge");

    let frames = drain(app, dir.path());
    assert_eq!(
        frames.len(),
        4,
        "put, merge, replace, purge — one frame each"
    );
    for (index, ops) in frames.iter().enumerate() {
        assert!(!ops.is_empty(), "frame {index} carries its truth half");
        assert_truth_only(LOG_NS, ops);
        let last = ops.last().expect("non-empty");
        if index == 3 {
            assert_eq!(last.0, OP_DELETE, "purge logs a delete");
        }
    }
}

#[test]
fn categorical_labels_ride_and_their_cursor_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("changelog opens");

    app.put(
        &LogKey { id: 1 },
        &LogRow {
            author: 1,
            title: "x".into(),
        },
    )
    .expect("row put");
    let tag_app: Collection<TestStore, TagKey, TagRow> =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("tag changelog opens");
    tag_app
        .put(
            &TagKey { id: 1 },
            &TagRow {
                status: Vocab::of("active"),
                body: "x".into(),
            },
        )
        .expect("tag put");
    let tag_frames: Vec<_> = {
        drop(tag_app);
        sealed_segments(dir.path(), TAG_NS)
            .expect("list")
            .iter()
            .flat_map(|p| read_sealed(p).expect("read"))
            .collect()
    };

    assert_eq!(tag_frames.len(), 1);
    let ops = &tag_frames[0].0;
    let slots: Vec<u16> = ops.iter().map(|(_, key, _)| slot_of(TAG_NS, key)).collect();
    assert!(
        slots.iter().any(|s| s >> 12 == 0x1),
        "label -> id rides: {slots:04X?}"
    );
    assert!(
        slots.iter().any(|s| s >> 12 == 0x2),
        "id -> label rides: {slots:04X?}"
    );
    // The allocation cursor is the id face's bare prefix: header + slot, no
    // payload. It is derived state and must never reach the log.
    let cursor_len = TAG_NS.len() + 2;
    assert!(
        !ops.iter()
            .any(|(_, key, _)| key.len() == cursor_len && slot_of(TAG_NS, key) >> 12 == 0x2),
        "the vocabulary cursor is a face singleton, not a record"
    );
    assert_truth_only(TAG_NS, ops);
}

#[test]
fn a_torn_tail_ends_a_partial_and_corrupts_a_sealed_segment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("changelog opens");
    app.put(
        &LogKey { id: 1 },
        &LogRow {
            author: 1,
            title: "a".into(),
        },
    )
    .expect("put");
    drop(app);

    let sealed = sealed_segments(dir.path(), LOG_NS).expect("list");
    let sealed = sealed.first().expect("one sealed segment");
    let good = std::fs::read(sealed).expect("read segment");

    // A copy still named `.partial`, with three bytes of a torn prefix.
    let torn = dir.path().join("torn").join("00000000.x");
    std::fs::create_dir_all(torn.parent().expect("parent")).expect("mkdir");
    let mut partial = good.clone();
    partial.extend_from_slice(&[0, 0, 0]);
    let partial_path = torn.with_extension(PARTIAL_EXT);
    std::fs::write(&partial_path, &partial).expect("write partial");
    let tail = read_open_tail(&partial_path).expect("a torn tail is a normal end");
    assert_eq!(tail.len(), 1, "the frames before the tear are good");

    // The same bytes named `.segment`: sealing is a rename, so a short record
    // inside one means the file was damaged after it was complete.
    let corrupt_path = torn.with_extension("segment");
    std::fs::write(&corrupt_path, &partial).expect("write sealed");
    assert!(
        read_sealed(&corrupt_path).is_err(),
        "corruption is an error"
    );
}

#[test]
fn rotation_seals_both_by_bytes_and_by_age() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = ChangelogConfig::new(dir.path());
    config.segment_bytes = 1; // seal on the next commit, whatever the payload
    config.segment_seconds = 0; // and let age do it too, to exercise the branch
    let app = Collection::with_changelog(TestStore::slatedb_mem(), config).expect("open");

    for id in 0..3 {
        app.put(
            &LogKey { id },
            &LogRow {
                author: 1,
                title: "row".into(),
            },
        )
        .expect("put");
    }
    drop(app);

    let segments = sealed_segments(dir.path(), LOG_NS).expect("list");
    assert_eq!(
        segments.len(),
        3,
        "each commit sealed the segment it filled"
    );
    let mut frames = 0;
    for path in &segments {
        frames += read_sealed(path).expect("read").len();
    }
    assert_eq!(frames, 3, "no record lost across rotation");
}

#[test]
fn the_batch_writer_verb_captures_save_into() {
    // `save_into` hands the caller a batch; before this verb the caller reached
    // the store directly and no collection-level hook could see the write.
    let dir = tempfile::tempdir().expect("tempdir");
    let app: Collection<TestStore, LogKey, LogRow> =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("open");

    let key = LogKey { id: 42 };
    let row = LogRow {
        author: 3,
        title: "batched".into(),
    };
    let mut batch = Batch::default();
    app.save_into(&mut batch, &key, &row);
    app.commit_batch(batch).expect("publish");

    assert_eq!(app.get(&key).expect("get"), Some(row));
    let frames = drain(app, dir.path());
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].len(),
        1,
        "primary only — the index face is derived"
    );
    assert_truth_only(LOG_NS, &frames[0]);
}

#[test]
fn status_reports_a_log_that_never_took_a_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app: Collection<TestStore, LogKey, LogRow> =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("open");
    let status = app.changelog_status().expect("a changelog is attached");
    assert_eq!(status.segment, 0);
    assert_eq!(status.segment_written, 0);
    assert_eq!(status.records_dropped, 0);
    assert!(status.last_error.is_none());

    // And a plain collection has no log at all.
    let plain: Collection<TestStore, LogKey, LogRow> = Collection::new(TestStore::slatedb_mem());
    assert!(plain.changelog_status().is_none());
}

/// The sidecar is the half of the declaration the log cannot carry. It is
/// written by `with_changelog` (the one place that knows both the schema and the
/// directory), it round-trips, and a DIFFERENT shape behind the same version is
/// refused rather than overwritten — a version that can mean two shapes has
/// stopped meaning anything (ADR-0042).
#[cfg(feature = "schema-serde")]
#[test]
fn the_sidecar_is_written_once_per_layout_version() {
    use okm_core::changelog::{sidecar_name, table_dir, write_sidecar};
    use okm_core::schema::CollectionSchema;

    let dir = tempfile::tempdir().expect("tempdir");
    let app: Collection<TestStore, LogKey, LogRow> =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
            .expect("open");

    let table = table_dir(dir.path(), LOG_NS);
    let path = table.join(sidecar_name(<LogRow as Document>::LAYOUT_VERSION));
    let json = std::fs::read_to_string(&path).expect("with_changelog stamps the sidecar");
    let parsed: CollectionSchema = serde_json::from_str(&json).expect("sidecar round-trips");
    assert_eq!(parsed, CollectionSchema::of::<LogKey, LogRow>());
    assert_eq!(
        parsed.key_fields.len(),
        1,
        "the sidecar carries the KEY layout, which no log byte does"
    );

    // Same schema again: idempotent, same path, no error.
    assert_eq!(write_sidecar(&table, &parsed).expect("same schema"), path);

    // A different shape behind the same number: refused.
    let mut other = parsed.clone();
    other.key_len += 1;
    assert!(
        write_sidecar(&table, &other).is_err(),
        "one version must not describe two shapes"
    );
    drop(app);
}

/// A batch that spans two tables: each collection stages its OWN share before the
/// one commit and appends after it (the staged/flush split). Without it the
/// committer's log records only its own keys and the other table's writes never
/// reach a lake — the hole this verb pair closes.
#[test]
fn a_cross_table_batch_captures_each_tables_own_share() {
    let root_a = tempfile::tempdir().expect("a");
    let root_b = tempfile::tempdir().expect("b");
    let a: Collection<TestStore, LogKey, LogRow> = Collection::with_changelog(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root_a.path()),
    )
    .expect("open a");
    let b: Collection<TestStore, LogKey, OtherRow> = Collection::with_changelog(
        TestStore::slatedb_mem(),
        ChangelogConfig::new(root_b.path()),
    )
    .expect("open b");

    // One batch carrying both tables' keys.
    let mut batch = Batch::default();
    let ka = LogKey { id: 1 };
    let kb = LogKey { id: 2 };
    a.save_into(
        &mut batch,
        &ka,
        &LogRow {
            author: 7,
            title: "a".into(),
        },
    );
    b.save_into(
        &mut batch,
        &kb,
        &OtherRow {
            author: 8,
            title: "b".into(),
        },
    );
    let pa = a.primary_key(&ka);
    let pb = b.primary_key(&kb);

    // Stage before the commit: each collection takes its own ops and no one
    // else's (a directory belongs to one header).
    assert_eq!(a.stage_capture(&batch.ops), 1, "a takes its own op");
    assert_eq!(b.stage_capture(&batch.ops), 1, "b takes its own op");

    a.commit_batch(batch).expect("one commit");
    a.flush_capture().expect("a appends after the commit");
    b.flush_capture().expect("b appends after the commit");

    let frames_a = drain(a, root_a.path());
    drop(b);
    let frames_b: Vec<CapturedFrame> = sealed_segments(root_b.path(), OTHER_NS)
        .expect("b's segments")
        .iter()
        .flat_map(|path| read_sealed(path).expect("b's frames"))
        .map(|frame| frame.0)
        .collect();
    assert_eq!(frames_a.len(), 1, "a: one committed batch, one frame");
    assert_eq!(frames_a[0].len(), 1, "a: only its own row");
    assert_eq!(frames_a[0][0].1, pa, "a's key, in a's log");
    assert_eq!(frames_b.len(), 1, "b: one frame, from its own stage");
    assert_eq!(frames_b[0].len(), 1, "b: only its own row");
    assert_eq!(frames_b[0][0].1, pb, "b's key, in b's log");
}

/// The repair path: a crash between the commit and the append leaves a log that is
/// visibly incomplete, and the rows that still exist are what can be re-emitted.
#[test]
fn reconcile_re_emits_the_rows_that_still_exist() {
    let root = tempfile::tempdir().expect("root");
    let c: Collection<TestStore, LogKey, LogRow> =
        Collection::with_changelog(TestStore::slatedb_mem(), ChangelogConfig::new(root.path()))
            .expect("open");
    for id in 1..=3u64 {
        c.put(
            &LogKey { id },
            &LogRow {
                author: id,
                title: format!("row-{id}"),
            },
        )
        .expect("put");
    }

    // The whole table: three rows, three frames staged, nothing else picked up.
    assert_eq!(c.reconcile().expect("scan"), 3, "every row is re-emitted");
    c.flush_capture().expect("flush");

    // A slice of it: the bounds are KEY suffixes, so [1, 3) is the first two rows —
    // the header and the primary slot are the collection's to add.
    assert_eq!(
        c.reconcile_range(&1u64.to_be_bytes(), &3u64.to_be_bytes())
            .expect("range scan"),
        2,
        "a range re-emits only what is inside it"
    );
    c.flush_capture().expect("flush");
    assert_eq!(c.reconcile().expect("scan"), 3, "idempotent re-read");

    let pkeys: Vec<Vec<u8>> = (1..=3u64).map(|id| c.primary_key(&LogKey { id })).collect();
    let frames = drain(c, root.path());
    // A window may carry a row more than once, so the frames are three separate
    // batches: the three puts, the table reconcile, the range reconcile.
    assert_eq!(frames.len(), 5, "three puts, then one frame per reconcile");
    let keys: Vec<Vec<u8>> = frames
        .iter()
        .flat_map(|f| f.iter().map(|(_, key, _)| key.clone()))
        .collect();
    assert_eq!(keys.len(), 8, "3 puts + 3 whole-table + 2 in range");
    for want in &pkeys {
        assert!(keys.contains(want), "the row came back");
    }
}
