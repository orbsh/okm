use okm_core::{DocumentEncode, KeyEncode, Refs, TestStore};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct RoomKey {
    pub dept_id: u8,
    pub seq: u16,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(RoomKey)]
#[ok_ns(62)]
pub struct Room {
    pub label: String,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DeptKey)]
#[ok_ns(61)]
pub struct Dept {
    pub name: String,
    pub rooms: Refs<Room, RoomKey>, // no attribute needed
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct DeptKey {
    pub dept_id: u8,
}

#[test]
fn list_roundtrip_and_stale_release() {
    let mut depts: okm_core::Collection<TestStore, DeptKey, Dept> =
        okm_core::Collection::new(TestStore::slatedb_mem());
    let mut rooms: okm_core::Collection<TestStore, RoomKey, Room> =
        okm_core::Collection::new(depts.store().clone());

    let dept_key = DeptKey { dept_id: 1 };
    let k1 = RoomKey { dept_id: 1, seq: 1 };
    let k2 = RoomKey { dept_id: 1, seq: 2 };
    let k3 = RoomKey { dept_id: 1, seq: 3 };

    // Own-write: three rooms in one put.
    depts.put(&dept_key, &Dept {
        name: "platform".into(),
        rooms: Refs::own_all(
            vec![k1.clone(), k2.clone(), k3.clone()],
            vec![
                Room { label: "alpha".into() },
                Room { label: "beta".into() },
                Room { label: "gamma".into() },
            ],
        ),
    });
    for k in [&k1, &k2, &k3] {
        assert!(rooms.get(k).is_some(), "child written");
    }
    let d = depts.get(&dept_key).expect("dept");
    assert_eq!(d.rooms.values.len(), 3);
    assert_eq!(d.rooms.values[0].as_ref().unwrap().label, "alpha");
    assert_eq!(d.rooms.values[2].as_ref().unwrap().label, "gamma");

    // Shorten the list: k3 drops out -> stale release deletes it.
    depts.put(&dept_key, &Dept {
        name: "platform".into(),
        rooms: Refs::own_all(
            vec![k1.clone(), k2.clone()],
            vec![
                Room { label: "alpha".into() },
                Room { label: "beta2".into() },
            ],
        ),
    });
    assert!(rooms.get(&k3).is_none(), "stale child released");
    assert_eq!(rooms.get(&k2).unwrap().label, "beta2");

    // Reference-only: list of keys, children untouched.
    depts.put(&dept_key, &Dept {
        name: "platform".into(),
        rooms: Refs::new(vec![k1.clone(), k2.clone()]),
    });
    assert_eq!(rooms.get(&k1).unwrap().label, "alpha");
    assert!(rooms.get(&k2).is_some());

    // Dangling reference reads back as None.
    rooms.delete_by_pkey(&k1);
    let d2 = depts.get(&dept_key).expect("dept2");
    assert!(d2.rooms.values[0].is_none(), "dangling -> None");
    assert!(d2.rooms.values[1].is_some());
}
