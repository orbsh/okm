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

use okm_async::TestStore;
use okm_core::changelog::{
    ChangelogConfig, ChangelogStore, PARTIAL_EXT, read_open_tail, read_sealed, sealed_segments,
};
use okm_core::{Batch, Collection, Document, DocumentEncode, KeyEncode, OP_DELETE, OP_PUT, Vocab};
use okm_value::{Dict, Variant};

/// Assemble a collection over a CAPTURING store (ADR-0056 §2) — the one form of
/// capture the crate has: the store owns the switch, so there is no second way to
/// record a write and no assembly site that can forget to.
fn captured<S: okm_core::VirtualStorage, K: KeyEncode, R: Document<Key = K>>(
    store: S,
    config: ChangelogConfig,
) -> okm_core::error::Result<Collection<ChangelogStore<S>, K, R>> {
    Collection::captured(ChangelogStore::new(store, config))
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct LogKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(LogKey)]
#[ok_ns(57)]
#[ok_name("log_rows")]
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
    frames_in(root, LOG_NS)
}

/// The frames of one table, read after everything that wrote it was dropped.
fn frames_in(root: &std::path::Path, header: &[u8]) -> Vec<CapturedFrame> {
    let segments = sealed_segments(root, header).expect("list segments");
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
    let app = captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
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
    let app = captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
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

    let mut named = Dict::new();
    named.insert(
        "title".to_string(),
        okm_value::Variant::Str("second".into()),
    );
    app.put(&key, &Variant::Map(named)).expect("merge");

    let mut whole = Dict::new();
    whole.insert("author".to_string(), okm_value::Variant::U64(9));
    whole.insert("title".to_string(), okm_value::Variant::Str("third".into()));
    app.put(&key, (&Variant::Map(whole),)).expect("replace");

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
    let app = captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
        .expect("changelog opens");

    app.put(
        &LogKey { id: 1 },
        &LogRow {
            author: 1,
            title: "x".into(),
        },
    )
    .expect("row put");
    let tag_app: Collection<ChangelogStore<TestStore>, TagKey, TagRow> =
        captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
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
    let app = captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()))
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

    // A copy still named `.partial`, with three bytes of a torn prefix. A real
    // partial has no footer — take the header and the body, then tear the body.
    let info = okm_core::changelog::segment_info(sealed).expect("info");
    let name_len = info.meta.name.len();
    let header_len = 1 + 1 + 1 + 1 + info.header.len() + 2 + name_len + 16;
    let torn = dir.path().join("torn").join("00000000.x");
    std::fs::create_dir_all(torn.parent().expect("parent")).expect("mkdir");
    let mut partial = good[..header_len].to_vec();
    partial.extend_from_slice(&good[header_len..good.len() - 28]);
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
    let app = captured(TestStore::slatedb_mem(), config).expect("open");

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
    let app: Collection<ChangelogStore<TestStore>, LogKey, LogRow> =
        captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path())).expect("open");

    let key = LogKey { id: 42 };
    let row = LogRow {
        author: 3,
        title: "batched".into(),
    };
    let mut batch = Batch::default();
    app.save_with_prev_into(&mut batch, &key, &row, None)
        .unwrap();
    app.commit_batch(batch).expect("publish");

    assert_eq!(app.get_row(&key).expect("get"), Some(row));
    let frames = drain(app, dir.path());
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].len(),
        1,
        "primary only — the index face is derived"
    );
    assert_truth_only(LOG_NS, &frames[0]);
}

/// The writer opens on the first CAPTURED commit, so before that there is nothing
/// to owe. A collection over a plain store has no log at all — a type fact now,
/// not a runtime `None`.
#[test]
fn status_reports_a_log_that_never_took_a_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path()));
    store.register::<LogKey, LogRow>().expect("register");
    assert!(
        store.status::<LogRow>().is_none(),
        "no writer is open before the first write"
    );

    let app: Collection<_, LogKey, LogRow> = Collection::new(&store);
    app.put(
        &LogKey { id: 1 },
        &LogRow {
            author: 1,
            title: "x".into(),
        },
    )
    .expect("put");

    let status = store.status::<LogRow>().expect("the writer is open");
    assert_eq!(status.segment, 0);
    assert_eq!(status.segment_written, 0, "the frame is still buffered");
    assert_eq!(status.records_dropped, 0);
    assert!(status.last_error.is_none());
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
    let app: Collection<ChangelogStore<TestStore>, LogKey, LogRow> =
        captured(TestStore::slatedb_mem(), ChangelogConfig::new(dir.path())).expect("open");

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

/// A batch that spans two tables, committed ONCE: the store splits it by key
/// header, so each table's log takes its own frame out of the same commit. This
/// used to need a stage/flush pair per collection, and a caller who forgot them
/// silently lost the other table's writes.
#[test]
fn a_cross_table_batch_captures_each_tables_own_share() {
    let root = tempfile::tempdir().expect("root");
    let (pa, pb);
    {
        // ONE decorator for both tables: a second one over the same directory
        // would be a second writer on one segment number.
        let store =
            ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root.path()));
        store.register::<LogKey, LogRow>().expect("register a");
        store.register::<LogKey, OtherRow>().expect("register b");
        let a: Collection<_, LogKey, LogRow> = Collection::new(&store);
        let b: Collection<_, LogKey, OtherRow> = Collection::new(&store);

        // One batch carrying both tables' keys.
        let mut batch = Batch::default();
        let ka = LogKey { id: 1 };
        let kb = LogKey { id: 2 };
        a.save_with_prev_into(
            &mut batch,
            &ka,
            &LogRow {
                author: 7,
                title: "a".into(),
            },
            None,
        )
        .unwrap();
        b.save_with_prev_into(
            &mut batch,
            &kb,
            &OtherRow {
                author: 8,
                title: "b".into(),
            },
            None,
        )
        .unwrap();
        pa = a.primary_key(&ka);
        pb = b.primary_key(&kb);

        a.commit_batch(batch).expect("one commit");
    }

    let frames_a = frames_in(root.path(), LOG_NS);
    let frames_b = frames_in(root.path(), OTHER_NS);
    assert_eq!(frames_a.len(), 1, "a: one committed batch, one frame");
    assert_eq!(frames_a[0].len(), 1, "a: only its own row");
    assert_eq!(frames_a[0][0].1, pa, "a's key, in a's log");
    assert_eq!(frames_b.len(), 1, "b: one frame out of the same commit");
    assert_eq!(frames_b[0].len(), 1, "b: only its own row");
    assert_eq!(frames_b[0][0].1, pb, "b's key, in b's log");
}

/// The capture rule covers the WHOLE numbering space instead of a whitelist
/// (ADR-0056 §2): every slot value is classified, so a new face cannot be
/// silently dropped — adding one means adding it to this expectation table.
#[test]
fn every_slot_is_classified_and_only_the_truth_half_is_carried() {
    use okm_core::changelog::{Side, side_of};
    use okm_core::schema::SlotMap;

    let slots = SlotMap::standard();

    // The fixed faces an export carries, by number: primary row, dynamic
    // segment, field-name vocabulary both ways, predicate vocabulary both ways,
    // and the cluster band's authority half — the cluster row (0x0A) and its
    // edge members (0x0B), which are assertions the edge rows cannot reproduce
    // (ADR-0066). Everything else in 0x00–0x0F is derived — the graph's
    // traversal faces (0x06–0x09), the cluster's child index (0x0C), the vector
    // snapshot (0x0E), the table meta row (0x0F) — and 0x0D is free again, so it
    // is classified derived only because the whole unclaimed band is.
    let fixed_truth = [0x00u16, 0x01, 0x02, 0x03, 0x04, 0x05, 0x0A, 0x0B];
    for slot in 0u16..=0x0F {
        let want = if fixed_truth.contains(&slot) {
            Side::Truth
        } else {
            Side::Derived
        };
        assert_eq!(side_of(&slots, slot), want, "fixed face 0x{slot:04X}");
    }

    // Counting families: labels ride (0x1nnn / 0x2nnn) because the payload holds
    // ids; indexes, reduces and lead faces are derived state.
    // Counting families (0x1nnn upward — a two-byte slot always starts above
    // 0x0F, so family 0 is the fixed faces and is not part of this loop).
    for family in 0x1u16..=0xF {
        for count in [0u16, 1, 0xFFF] {
            let slot = (family << 12) | count;
            let want = if matches!(family, 0x1 | 0x2) {
                Side::Truth
            } else {
                Side::Derived
            };
            assert_eq!(side_of(&slots, slot), want, "slot 0x{slot:04X}");
        }
    }

    // The six numbers the declaration exports ARE the fixed truth set — the
    // filter reads them from the map, so the two can never drift apart.
    for named in [
        slots.primary,
        slots.dynamic,
        slots.field_vocab_id,
        slots.field_vocab_name,
        slots.predicate_vocab_id,
        slots.predicate_vocab_name,
    ] {
        assert_eq!(side_of(&slots, named), Side::Truth, "0x{named:04X}");
    }

    // Total function: no panics, no gaps, over the whole space.
    for slot in 0u16..=u16::MAX {
        let _ = side_of(&slots, slot);
    }
}

/// The table's declared name travels in the sidecar (ADR-0056 §7) — it is the
/// only thing that can name a lake table, and the only thing a segment header
/// can carry once the directory it was written in is gone.
#[test]
fn the_sidecar_carries_the_tables_declared_name() {
    use okm_core::schema::CollectionSchema;

    let root = tempfile::tempdir().expect("root");
    {
        let c: Collection<ChangelogStore<TestStore>, LogKey, LogRow> =
            captured(TestStore::slatedb_mem(), ChangelogConfig::new(root.path()))
                .expect("the log opens");
        c.put(
            &LogKey { id: 1 },
            &LogRow {
                author: 7,
                title: "t".into(),
            },
        )
        .expect("put");
    }
    let dir = std::fs::read_dir(root.path())
        .expect("root dirs")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");
    let path = dir.join(format!(
        "schema-v{}.json",
        <LogRow as Document>::LAYOUT_VERSION
    ));
    let json = std::fs::read_to_string(&path).expect("the assembly wrote a sidecar");

    assert!(
        json.contains("\"name\": \"log_rows\""),
        "the declaration's name is in the file: {json}"
    );
    let parsed: CollectionSchema = serde_json::from_str(&json).expect("parses");
    assert_eq!(parsed.name, <LogRow as Document>::TABLE_NAME);
    assert_eq!(parsed.name, "log_rows");

    // A sidecar written before the name travelled still loads, and reads as
    // unnamed rather than failing — the export is what refuses an empty name.
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("value");
    let object = value.as_object_mut().expect("an object");
    object.remove("name");
    let older: CollectionSchema =
        serde_json::from_str(&value.to_string()).expect("an older sidecar loads");
    assert_eq!(older.name, "");
    assert_eq!(older.ns_prefix, parsed.ns_prefix, "nothing else moved");
}

/// Seal one row into a table's log and hand back the sealed file's path.
fn sealed_one(root: &std::path::Path) -> std::path::PathBuf {
    {
        let app: Collection<ChangelogStore<TestStore>, LogKey, LogRow> =
            captured(TestStore::slatedb_mem(), ChangelogConfig::new(root)).expect("the log opens");
        app.put(
            &LogKey { id: 7 },
            &LogRow {
                author: 1,
                title: "x".into(),
            },
        )
        .expect("put");
    }
    let segments = sealed_segments(root, LOG_NS).expect("list segments");
    assert_eq!(segments.len(), 1, "a clean drop seals what it wrote");
    segments[0].clone()
}

/// A format-2 segment says which table, which shape and which kind of stream it
/// is — the three facts that let a reader refuse it (ADR-0056 §7) instead of
/// trusting the directory name it happens to sit in.
#[test]
fn a_segment_says_which_table_and_shape_it_belongs_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = sealed_one(dir.path());

    let info = okm_core::changelog::segment_info(&path).expect("info");
    let meta = info.meta.clone();
    assert_eq!(meta.kind, okm_core::changelog::KIND_DELTA);
    assert_eq!(
        meta.layout_version,
        <LogRow as Document>::LAYOUT_VERSION,
        "the shape the bytes were written under"
    );
    assert_eq!(meta.name, "log_rows", "the declared name travels");
    assert_eq!(
        meta.fingerprint,
        <LogRow as Document>::FACES,
        "the face fingerprint ties the file to its sidecar"
    );
    assert_eq!(info.header, LOG_NS, "the file carries its own table header");
    assert_eq!(info.frames, 1, "one commit, one frame");
    assert_eq!(info.ordinal, 0, "the first segment of this table");
}

/// A sealed segment is immutable and checksummed: a flipped byte is corruption,
/// refused outright rather than replayed as a slightly different row.
#[test]
fn a_flipped_byte_in_a_sealed_segment_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = sealed_one(dir.path());

    let mut bytes = std::fs::read(&path).expect("read");
    let at = bytes.len() - 29; // the last body byte, just before the 28-byte footer
    bytes[at] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write");

    let err = read_sealed(&path).expect_err("the checksum catches it");
    assert!(err.to_string().contains("checksum"), "got: {err}");
}
