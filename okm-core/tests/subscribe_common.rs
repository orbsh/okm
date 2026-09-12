//! Shared subscribe fixture: row types, reduce/index impls, and the
//! generated okm_subscribe module. Test binaries that emit or consume
//! events include this module at their crate root (`#[path] mod`), so
//! the build.rs-generated enum's `super::` references and the derive
//! artifacts resolve identically in each binary. build.rs scans tests/
//! recursively and collects these rows once.
//!
//! Convention: every `#[kv_subscribe]` row of the okm-core test family
//! lives HERE — the generated enum spans all of them, so they must share
//! one compilation unit per test binary.

use okm_core::{KeyEncode, ReduceLogic, ReduceCodec, RowEncode};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AccountKey {
    pub id: u64,
}

/// Subscribed row — variant is the row type name, derived by build.rs.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(AccountKey)]
#[kv_subscribe]
#[kv_ns(21)]
pub struct Account {
    pub balance: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct AuditKey {
    pub id: u64,
}

/// Subscribed row routed through the same enum — fan-in shape.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(AuditKey)]
#[kv_subscribe]
#[kv_ns(22)]
pub struct Audit {
    pub note: String,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct GhostKey {
    pub id: u64,
}

/// Subscribed but nobody ever registers the channel — the no-sink
/// case must be tested on a channel no other test can touch (global
/// statics are process-wide; parallel tests would race otherwise).
/// This row carries its own enum alias via `#[kv_event_enum]`, which
/// lands as a second generated enum.
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(GhostKey)]
#[kv_event_enum(ShadowEvents)]
#[kv_subscribe]
#[kv_ns(23)]
pub struct Ghost {
    pub v: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct CounterKey {
    pub id: u64,
}

/// Row with an index + a reduce + a subscription, so tests can verify
/// the put path fired through all three (upsert_with must not bypass
/// any of them).
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(CounterKey)]
#[kv_index(by_bucket { fields(bucket) })]
#[kv_reduce(CounterTotals { group(bucket) })]
#[kv_subscribe]
#[kv_ns(31)]
pub struct Counter {
    pub bucket: u32,
    pub hits: u64,
}

/// 复合 acc：count + sum（均值底座，可逆）。
#[derive(Default, Clone, Debug, PartialEq)]
pub struct CountSum {
    pub count: u64,
    pub sum: u64,
}

impl ReduceCodec for CountSum {
    fn encode_acc(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&self.count.to_be_bytes());
        b.extend_from_slice(&self.sum.to_be_bytes());
        b
    }
    fn decode_acc(bytes: &[u8]) -> Self {
        assert!(bytes.len() == 16, "CountSum acc must be 16 bytes BE");
        CountSum {
            count: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            sum: u64::from_be_bytes(bytes[8..].try_into().unwrap()),
        }
    }
}

pub struct CounterTotals;

impl ReduceLogic for CounterTotals {
    type Row = Counter;
    type Acc = CountSum;
    fn fold(acc: &mut CountSum, item: &Counter) {
        acc.count += 1;
        acc.sum += item.hits;
    }
    fn unfold(acc: &mut CountSum, item: &Counter) {
        acc.count -= 1;
        acc.sum -= item.hits;
    }
}

// build.rs-collected event enum + channel cell, generated into OUT_DIR.
pub mod okm_subscribe {
    include!(concat!(env!("OUT_DIR"), "/okm_subscribe.rs"));
}
