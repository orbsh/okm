//! The S3 story end to end, without a bucket: a captured collection seals a segment,
//! the seal is announced after the rename, and the copy lands in an in-memory object
//! store under `{prefix}/{table hex}/{segment name}`.
//!
//! The point of the test is the SEAM, not the transport: what the writer promises is
//! "the finished file, handed over outside the commit", and an in-memory store makes
//! that promise checkable without credentials or a network.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn
)]

use std::sync::Arc;
use std::time::Duration;

use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::s3::SegmentUploader;
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(93)]
#[ok_name("uploaded_rows")]
pub struct R {
    pub level: u32,
}

#[test]
fn a_sealed_segment_reaches_the_object_store() {
    let root = tempfile::tempdir().expect("log");
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let uploader = SegmentUploader::new(Arc::clone(&objects), "logs");

    {
        let c: Collection<_, K, R> = Collection::captured(
            ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root.path()))
                .with_sink(uploader.clone()),
        )
        .expect("the log opens");
        c.put(&K { org: 1, id: 1 }, &R { level: 3 }).expect("put");
        c.put(&K { org: 1, id: 2 }, &R { level: 4 }).expect("put");
    } // Drop seals: the rename happens here, and the hook runs after it.

    assert!(
        uploader.flush(Duration::from_secs(10)),
        "the queue drained"
    );
    assert_eq!(uploader.failures(), 0, "nothing was dropped");
    assert_eq!(uploader.uploaded(), 1, "one sealed segment, one copy");

    // What the log has on disk, and what the bucket has, is the same file.
    let table_dir = std::fs::read_dir(root.path())
        .expect("log root")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one table directory");
    let table = table_dir
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    let segment = std::fs::read_dir(&table_dir)
        .expect("table dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|x| x == "segment"))
        .expect("one sealed segment");
    let name = segment
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();

    let key = ObjectPath::parse(format!("logs/{table}/{name}")).expect("legal key");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let copied = rt.block_on(async {
        objects
            .get(&key)
            .await
            .expect("the object exists")
            .bytes()
            .await
            .expect("bytes")
    });
    let local = std::fs::read(&segment).expect("local segment");
    assert_eq!(copied.as_ref(), local.as_slice(), "byte for byte");
}

/// Two tables upload to two prefixes: the key carries the table's header, because one
/// bucket holds many logs and a segment number alone says nothing about which.
#[test]
fn the_key_carries_the_table() {
    let root = tempfile::tempdir().expect("log");
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let uploader = SegmentUploader::new(Arc::clone(&objects), "logs");
    {
        let c: Collection<_, K, R> = Collection::captured(
            ChangelogStore::new(TestStore::slatedb_mem(), ChangelogConfig::new(root.path()))
                .with_sink(uploader.clone()),
        )
        .expect("the log opens");
        c.put(&K { org: 9, id: 1 }, &R { level: 1 }).expect("put");
    }
    assert!(uploader.flush(Duration::from_secs(10)));
    assert_eq!(uploader.failures(), 0);

    // 93 = 0x005d: the header IS the ns, and the object path says so.
    let expected = ObjectPath::parse("logs/005d/00000000.segment").expect("legal key");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    assert!(
        rt.block_on(objects.head(&expected)).is_ok(),
        "the copy is under the table's header"
    );
}