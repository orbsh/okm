//! Multi-tenancy (ADR-0010 §5): one physical engine, several `#[kv_storage]`
//! executors — one declared prefix per application. Isolation is the pure
//! concatenation in `hosted_key`: two hosts on the same engine occupy two
//! disjoint prefix segments, and a sender bound to one prefix cannot reach
//! the other's bytes (it does not hold the prefix). There is no app_id
//! layer, no reserved prefix values — the declared prefix IS the
//! mechanism, which is why this test is all the multi-tenancy code there
//! is.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use okm_core::{KeyEncode, RemoteStore, Row, RowEncode, Table, VirtualStorage};

/// A test engine whose clone SHARES the map (engine-handle semantics, like
/// a real fjall store handle) — the same physical instance handed to two
/// hosts. MockStore itself is a plain deep-copy map, which cannot model
/// "one physical engine, two hosts".
#[derive(Default, Clone)]
struct SharedEngine {
    map: Arc<Mutex<BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl VirtualStorage for SharedEngine {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.map.lock().unwrap().insert(key, value);
    }
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.lock().unwrap().get(key).cloned()
    }
    fn del(&mut self, key: &[u8]) {
        self.map.lock().unwrap().remove(key);
    }
    fn scan_suffix(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        self.map
            .lock()
            .unwrap()
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k[prefix.len()..].to_vec())
            .collect()
    }
}

// One declared executor per application — same physical engine behind all.
#[derive(okm_core::StorageEncode)]
#[kv_ns(21)]
pub struct TenantAStorage;

#[derive(okm_core::StorageEncode)]
#[kv_ns(22)]
pub struct TenantBStorage;

#[derive(okm_core::StorageEncode)]
#[kv_ns(21)]
pub struct AppStorage;

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DocKey {
    pub id: u64,
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(DocKey)]
#[kv_ns(1)]
pub struct Doc {
    pub title: u64,
}

/// Two hosts, ONE engine instance: disjoint prefix segments on the same
/// bytes. Each tenant's sender sees only its own rows; the physical
/// engine's key space splits as [0,21|sender bytes] vs [0,22|sender bytes].
#[test]
fn one_engine_two_tenants_prefix_segments_disjoint() {
    // One physical engine instance (handle-clone shares the map), handed
    // to both hosts. The hosts own the only handles; the sender endpoints
    // are the contract-level view.
    let engine = SharedEngine::default();
    let ha = TenantAStorage::serve(engine.clone());
    let hb = TenantBStorage::serve(engine);
    let mut sa = ha.open();
    let mut sb = hb.open();

    // Same sender key bytes into both tenants.
    sa.put(b"doc:1".to_vec(), b"a".to_vec());
    sb.put(b"doc:1".to_vec(), b"b".to_vec());

    // Wait for both fire-and-forget writes to land (cross reads).
    for _ in 0..200 {
        if sa.get(b"doc:1").is_some() && sb.get(b"doc:1").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Each tenant reads back its own value under the identical sender key.
    assert_eq!(sa.get(b"doc:1").as_deref(), Some(b"a".as_slice()));
    assert_eq!(sb.get(b"doc:1").as_deref(), Some(b"b".as_slice()));

    // Scan segmentation: each sender's scan over the shared sender-key
    // prefix returns exactly its own segment's entry — the host prefix
    // split the engine's key space into disjoint segments.
    assert_eq!(sa.scan_suffix(b"doc:"), vec![b"1".to_vec()]);
    assert_eq!(sb.scan_suffix(b"doc:"), vec![b"1".to_vec()]);
    // Cross-tenant reads are structurally absent: each sender holds only
    // its own host's handle, so there is no expression for "read tenant
    // B's bytes from sender A" — verified by A's view being unchanged
    // after B's write.
    assert_eq!(sa.get(b"doc:1").as_deref(), Some(b"a".as_slice()));
}

/// Tenant partitioning INSIDE one application is a plain key field
/// (business sharding, same modeling for every tenant) — it does not
/// touch ns or the host prefix.
#[test]
fn internal_tenant_sharding_is_a_plain_key_field() {
    let engine = SharedEngine::default();
    let handle = TenantAStorage::serve(engine);
    let mut s = handle.open();

    // tenant_id is the leading key-payload field: sender-side business
    // data, opaque to the receiver.
    s.put(b"t1:doc:1".to_vec(), b"x".to_vec());
    s.put(b"t2:doc:1".to_vec(), b"y".to_vec());

    // Fire-and-forget writes: poll until the round trips see both keys.
    for _ in 0..200 {
        if s.get(b"t1:doc:1").is_some() && s.get(b"t2:doc:1").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Same host, same ns segment — the tenant distinction is sender bytes.
    assert_eq!(s.get(b"t1:doc:1").as_deref(), Some(b"x".as_slice()));
    assert_eq!(s.get(b"t2:doc:1").as_deref(), Some(b"y".as_slice()));
    assert_eq!(s.scan_suffix(b"t1:"), vec![b"doc:1".to_vec()]);
}

/// The row-level write path through a hosted tenant: Table semantics
/// (primary + index entries) land inside the tenant's prefix segment.
#[test]
fn table_write_path_inside_tenant_segment() {
    let handle = AppStorage::serve(SharedEngine::default());
    let t: Table<RemoteStore, DocKey, Doc> = Table::new(handle.open());

    let mut t = t;
    t.put(&DocKey { id: 9 }, &Doc { title: 5 });

    // Both entries (primary slot-0 + index slot-1) are visible through
    // the tenant's sender; the sender's own ns [0,1] rides inside the
    // sender-key bytes, opaque to the receiver. Physical observation of
    // the engine's map is out of reach BY DESIGN (the host owns the only
    // handle) — the sender-side view is the contract.
    for _ in 0..200 {
        if t.get(&DocKey { id: 9 }).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(t.get(&DocKey { id: 9 }).unwrap().title, 5);
    assert_eq!(t.scan_keys().len(), 1);
}
