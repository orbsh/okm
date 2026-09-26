//! `NestStorage` derive (declared via `#[ok_ns]`) — end-to-end (ADR-0010 §4): the
//! annotated empty struct becomes a host bound to its declared prefix;
//! the sender endpoint plugs into a `Collection` whose write path runs
//! entirely over the wire.

use okm_core::{KeyEncode, TestStore, RemoteStore, DocumentEncode, Collection, VirtualStorage};

// Receiver declaration: no data methods, one execution surface.
#[derive(okm_core::NestStorage)]
#[ok_ns(21)]
pub struct AppStorage;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct ItemKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ItemKey)]
#[ok_ns(21)]
#[ok_index(by_kind { fields(kind) })]
pub struct Item {
    pub kind: u32,
}

use __OkmIndex_Item_by_kind as ByKind;

#[test]
fn nest_derive_end_to_end() {
    // NS_PREFIX is the declared prefix, big-endian [ns 2B] — same
    // encoding as Document::NS_PREFIX.
    assert_eq!(AppStorage::NS_PREFIX, &[0, 21]);

    let handle = AppStorage::serve(TestStore::slatedb_mem());
    let remote = handle.open();
    let mut t: Collection<RemoteStore, ItemKey, Item> = Collection::new(remote);

    t.put(&ItemKey { id: 1 }, &Item { kind: 7 });

    // Fire-and-forget write: poll until the round trip sees the document.
    for _ in 0..200 {
        if t.get(&ItemKey { id: 1 }).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let document = t.get(&ItemKey { id: 1 }).expect("document round-tripped");
    assert_eq!(document.kind, 7);

    let hits = t.scan::<ByKind>(&7u32.to_be_bytes());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.as_ref().unwrap().kind, 7);
}

#[test]
fn two_instances_isolated_by_declared_prefix() {
    // One declared NestStorage executor per application — the isolation
    // boundary IS the declared prefix (ADR-0010 §5).
    #[derive(okm_core::NestStorage)]
    #[ok_ns(22)]
    pub struct OtherStorage;

    assert_eq!(OtherStorage::NS_PREFIX, &[0, 22]);

    let h1 = AppStorage::serve(TestStore::slatedb_mem());
    let h2 = OtherStorage::serve(TestStore::slatedb_mem());
    let s1 = h1.open();
    let s2 = h2.open();

    s1.put(b"k".to_vec(), b"one".to_vec());
    s2.put(b"k".to_vec(), b"two".to_vec());

    // Fire-and-forget writes: poll until each round trip sees its value.
    for _ in 0..200 {
        if s1.get(b"k").is_some() && s2.get(b"k").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(s1.get(b"k").as_deref(), Some(b"one".as_slice()));
    assert_eq!(s2.get(b"k").as_deref(), Some(b"two".as_slice()));
}
