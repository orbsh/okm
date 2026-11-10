//! The smallest end-to-end lake export: a collection with the change log on writes
//! three rows plus a dynamic field, the log seals on clean shutdown, and the drainer
//! turns it into a Delta table.
//!
//!     cargo run -p okm-parquet --example lake_demo -- /tmp/okm-log /tmp/okm-lake
//!
//! then, with no okm in the path — the DIRECTORY is the table, nothing is registered:
//!
//!     duckdb -c "LOAD delta; select * from delta_scan('/tmp/okm-lake/<table>')"
//!     duckdb -c "LOAD delta; select variant_extract(variant, 'note') \
//!                from delta_scan('/tmp/okm-lake/<table>')"

#![allow(clippy::expect_used, clippy::unwrap_used)]

use okm_core::changelog::{ChangelogConfig, ChangelogStore};
use okm_core::model::obj_dynamic::DynamicValue;
use okm_core::{Collection, DocumentEncode, KeyEncode, TestStore, Vocab};
use okm_parquet::{DrainConfig, drain};

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct K {
    pub org: u32,
    pub id: u64,
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(K)]
#[ok_ns(91)]
#[ok_name("demo_rows")]
pub struct R {
    pub state: Vocab<u8>,
    pub amount: u64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let log = std::path::PathBuf::from(args.next().expect("usage: lake_demo <log> <lake>"));
    let lake = std::path::PathBuf::from(args.next().expect("usage: lake_demo <log> <lake>"));

    {
        let c: Collection<_, K, R> = Collection::captured(ChangelogStore::new(
            TestStore::slatedb_mem(),
            ChangelogConfig::new(&log),
        ))
        .expect("the log opens");
        for (id, state, amount) in [(1u64, "paid", 30u64), (2, "open", 10), (3, "paid", 20)] {
            c.put(
                &K { org: 7, id },
                &R {
                    state: Vocab::of(state),
                    amount,
                },
            )
            .expect("put");
        }
        // A field the declaration does not have: the segment is truth, so the lake
        // carries it as a `variant` column.
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("note".to_string(), DynamicValue::Str("first".to_string()));
        c.merge(&K { org: 7, id: 1 }, &extra).expect("merge");
    } // Drop seals the segment: a clean exit is what makes the window readable.

    let report = drain(&log, &DrainConfig::new(&lake, "demo")).expect("drain");
    println!(
        "tables {} rows {} files {:?} metadata {}",
        report.tables, report.rows, report.files, report.metadata
    );
}
