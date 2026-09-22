//! Bare shard nest (ADR-0010 §5, `NestStorage::bare`): NO prefix —
//! frames execute byte-identical on the engine. The sender's keyspace IS
//! the engine's keyspace; sharding and routing belong to the
//! orchestrator. Prerequisite: every OKM instance pointing at the bare
//! host shares one domain model (one ns dictionary, one encoding) — the
//! same binary deployed per shard makes this automatic. Bare and hosted
//! hosts may coexist on one engine as long as the orchestrator keeps
//! bare instances' ns numbers off the hosted segments' numbers (an
//! allocation duty, not a runtime check).

use okm_core::{KeyEncode, NestStorage, RemoteStore, DocumentEncode, Collection, TestStore, VirtualStorage};

#[test]
fn bare_host_executes_frames_byte_identical() {
    let handle = NestStorage::bare(TestStore::default());
    let mut s: RemoteStore = handle.open();

    // Keys land exactly as sent — no prefix prepended (compare: hosted
    // hosts turn `user:1` into `[0,21]user:1`).
    s.put(b"user:1".to_vec(), b"one".to_vec());
    for _ in 0..200 {
        if s.get(b"user:1").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(s.get(b"user:1").as_deref(), Some(b"one".as_slice()));
    assert_eq!(s.scan_suffix(b"user:"), vec![b"1".to_vec()]);
}

// The shard's OKM instance models its own domain (ns 1 = its table) —
// through a bare host those ns bytes land on the engine untouched, so
// the shard's table key `[1][payload]` occupies engine segment `[1]`
// directly.
#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct ShardDocKey {
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(ShardDocKey)]
#[ok_ns(1)]
pub struct ShardDoc {
    pub title: u64,
}

#[test]
fn shard_table_via_bare_host_lands_on_its_ns_segment() {
    let handle = NestStorage::bare(TestStore::default());
    let mut t: Collection<RemoteStore, ShardDocKey, ShardDoc> = Collection::new(handle.open());

    t.put(&ShardDocKey { id: 5 }, &ShardDoc { title: 3 });
    for _ in 0..200 {
        if t.get(&ShardDocKey { id: 5 }).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Layout: the shard's own `[ns 2B][slot][key]` bytes pass through
    // the bare host untouched — the table's keys scan back exactly as a
    // local Collection on this engine would have written them. (The engine's
    // map itself is unreachable by design: hosts own the only handles,
    // and the sender-side view is the contract.)
    let keys = t.scan_keys();
    assert_eq!(keys, vec![ShardDocKey { id: 5 }]);
}

/// Bare and hosted hosts may coexist on one engine: the orchestrator
/// routes shard traffic to the bare host and app traffic to the hosted
/// hosts; the allocation duty is keeping bare instances' ns numbers off
/// the hosted segments' numbers (here the shard uses ns 1-2, the hosted
/// app sits at segment [0, 30]).
#[test]
fn bare_and_hosted_coexist_with_allocation_discipline() {
    #[derive(okm_core::NestStorage)]
    #[ok_ns(30)]
    pub struct AppBStorage;

    let engine = TestStore::default(); // handle-clone = shared engine
    let bare = NestStorage::bare(engine.clone());
    let hosted = AppBStorage::serve(engine);
    let mut bs: RemoteStore = bare.open();
    let mut hs = hosted.open();

    // Shard writes ns-1 keys; hosted app B's segment is [0,30].
    bs.put([1u8, 5].to_vec(), b"shard-document".to_vec());
    hs.put(b"appkey".to_vec(), b"app-b".to_vec());

    for _ in 0..200 {
        if bs.get(&[1u8, 5]).is_some() && hs.get(b"appkey").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(bs.get(&[1u8, 5]).as_deref(), Some(b"shard-document".as_slice()));
    assert_eq!(hs.get(b"appkey").as_deref(), Some(b"app-b".as_slice()));
}
