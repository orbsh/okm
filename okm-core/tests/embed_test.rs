use okm_core::{KeyEncode, DocumentEncode, Document, Embedded, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OwnerKey { pub org_id: u32, pub user_id: u64 }

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AddressKey { pub owner_id: u64, pub kind: u8 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(AddressKey)]
#[ok_ns(42)]
pub struct Address {
    pub city: String,
    pub zip: u32,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(OwnerKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub address: Embedded<Address, AddressKey>,
}

#[test]
fn embed_roundtrip() {
    let mut users = <User as Document>::table(TestStore::slatedb_mem());
    let mut addrs: okm_core::Collection<TestStore, AddressKey, Address> =
        okm_core::Collection::new(users.store().clone());
    let akey = AddressKey { owner_id: 2, kind: 1 };
    let ukey = OwnerKey { org_id: 1, user_id: 2 };
    let addr = Address { city: "delhi".into(), zip: 110001 };
    users.put(&ukey, &User {
        level: 4,
        address: Embedded::own(akey.clone(), addr.clone()),
    });
    // child written at its own key
    assert_eq!(addrs.get(&akey), Some(addr.clone()));
    // deref on read
    let u = users.get(&ukey).expect("user");
    assert_eq!(u.address.value, Some(addr.clone()));
    // reference-only write: new user pointing at the same address
    let ukey2 = OwnerKey { org_id: 1, user_id: 3 };
    users.put(&ukey2, &User { level: 9, address: Embedded::ref_key(akey.clone()) });
    assert_eq!(addrs.get(&akey), Some(addr.clone()));
    let u2 = users.get(&ukey2).expect("user2");
    assert_eq!(u2.address.value, Some(addr.clone()));
    // deleted child reads back as None
    addrs.delete_by_pkey(&akey);
    let u3 = users.get(&ukey2).expect("user3");
    assert_eq!(u3.address.value, None);
}
