use okm_core::{DocumentEncode, Embedded, KeyEncode, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OrgKey {
    pub org_id: u32,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DeptKey {
    pub org_id: u32,
    pub dept_id: u8,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DeptKey)]
#[ok_ns(52)]
pub struct Dept {
    pub name: String,
    // two-level: Org -> Dept -> Floor
    #[allow(dead_code)]
    pub floor_count: u8,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct FloorKey {
    pub org_id: u32,
    pub dept_id: u8,
    pub floor: u8,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(FloorKey)]
#[ok_ns(53)]
pub struct Floor {
    pub room_count: u16,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DeptKey)]
#[ok_ns(52)]
pub struct DeptV2 {
    pub name: String,
    pub floor_count: u8,
    pub main_floor: Embedded<Floor, FloorKey>,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(OrgKey)]
#[ok_ns(51)]
pub struct Org {
    pub name: String,
    pub hq: Embedded<DeptV2, DeptKey>,
}

#[test]
fn two_level_embed_roundtrip() {
    let mut orgs: okm_core::Collection<TestStore, OrgKey, Org> =
        okm_core::Collection::new(TestStore::slatedb_mem());
    let mut depts: okm_core::Collection<TestStore, DeptKey, DeptV2> =
        okm_core::Collection::new(orgs.store().clone());
    let mut floors: okm_core::Collection<TestStore, FloorKey, Floor> =
        okm_core::Collection::new(orgs.store().clone());

    let org_key = OrgKey { org_id: 7 };
    let dept_key = DeptKey { org_id: 7, dept_id: 3 };
    let floor_key = FloorKey { org_id: 7, dept_id: 3, floor: 2 };
    let floor = Floor { room_count: 12 };

    // Write the leaf first, then reference it up the chain.
    floors.put(&floor_key, &floor);
    let dept = DeptV2 {
        name: "platform".into(),
        floor_count: 3,
        main_floor: Embedded::ref_key(floor_key.clone()),
    };
    depts.put(&dept_key, &dept);
    let org = Org {
        name: "acme".into(),
        hq: Embedded::ref_key(dept_key.clone()),
    };
    orgs.put(&org_key, &org);

    // Top-level get dereferences the whole chain.
    let got = orgs.get(&org_key).expect("org");
    let hq = got.hq.value.expect("dept dereferenced");
    assert_eq!(hq.name, "platform");
    let fl = hq.main_floor.value.expect("floor dereferenced");
    assert_eq!(fl.room_count, 12);

    // Owning write: change the floor through the dept's put.
    let dept2 = DeptV2 {
        floor_count: 4,
        main_floor: Embedded::own(floor_key.clone(), Floor { room_count: 20 }),
        ..dept.clone()
    };
    depts.put(&dept_key, &dept2);
    assert_eq!(floors.get(&floor_key), Some(Floor { room_count: 20 }));
    // And the org still dereferences to the updated dept.
    let got2 = orgs.get(&org_key).expect("org2");
    let hq2 = got2.hq.value.expect("dept2");
    assert_eq!(hq2.main_floor.value.expect("floor2").room_count, 20);
}
