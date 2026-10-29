//! The capture tax, measured without criterion.
//!
//! `cargo bench -p okm-core -- write-changelog` hangs on this machine before any
//! measurement starts (a pure-CPU filter and `--list` hang the same way, so it is
//! the harness, not the code — recorded in PLAN). This example times the same two
//! writes with `Instant` and prints the per-write cost, so the number the ADR
//! argues about stops being an estimate.
//!
//! Run: cargo run --release --example changelog_tax -p okm-core --features test-engines

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use okm_core::{Collection, DocumentEncode, KeyEncode, Reverse, Reversible, TestStore, VarInt};

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct TaxKey {
    pub org_id: u32,
    pub user_id: u64,
    pub tag: [u8; 4],
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(TaxKey)]
#[ok_ns(0x61)]
pub struct TaxRow {
    pub level: u32,
    pub score: Reverse<u64>,
    pub visits: VarInt<u64>,
    pub name: String,
}

fn make_key(i: u64) -> TaxKey {
    TaxKey {
        org_id: (i % 1000) as u32,
        user_id: i,
        tag: [(i % 256) as u8, 0, 0, 0],
    }
}

fn make_row(i: u64) -> TaxRow {
    TaxRow {
        level: (i % 100) as u32,
        score: Reverse(i),
        visits: VarInt(i % 1000),
        name: format!("user-{i}"),
    }
}

/// Time `ops` writes of one arm.
fn time_once(ops: u64, mut write: impl FnMut(u64)) -> Duration {
    let start = Instant::now();
    for i in 0..ops {
        write(i);
    }
    black_box(());
    start.elapsed()
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    xs[xs.len() / 2]
}

fn main() {
    const OPS: u64 = 5_000;
    const ROUNDS: usize = 9;

    // `fjall_tmp`, not `slatedb_mem`: the in-memory slatedb test engine stalls past
    // roughly a thousand durable puts (measured: the driver thread parks and never
    // wakes, so a 5k-put loop never returns). That is a defect of that backend, not
    // something a benchmark can wait out — `cargo test --test store_volume_probe`
    // in the session log reproduces it without any of this code.
    let plain: Collection<TestStore, TaxKey, TaxRow> = Collection::new(TestStore::fjall_tmp());
    let root = std::env::temp_dir().join("okm-changelog-tax");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("log dir");
    let captured: Collection<TestStore, TaxKey, TaxRow> = Collection::with_changelog(
        TestStore::fjall_tmp(),
        okm_core::changelog::ChangelogConfig::new(&root),
    )
    .expect("capture");

    // Warm the store and the encoder so the first run is not the slow one.
    for i in 0..500 {
        plain.put(&make_key(i), &make_row(i)).expect("warm plain");
        captured
            .put(&make_key(i), &make_row(i))
            .expect("warm captured");
    }

    // PAIRED rounds, and the order flips every round: the two arms are two fjall
    // stores with their own compaction, so an unpaired median drifts with whichever
    // arm happened to collect garbage — the first run of this printed a NEGATIVE tax
    // for exactly that reason. A ratio per round cancels the drift.
    let mut plain_ns = Vec::with_capacity(ROUNDS);
    let mut captured_ns = Vec::with_capacity(ROUNDS);
    let mut ratios = Vec::with_capacity(ROUNDS);
    let mut deltas = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        let a = || {
            time_once(OPS, |i| {
                let (key, row) = (make_key(i), make_row(i));
                plain.put(&key, &row).expect("plain put");
                black_box(());
            })
        };
        let b = || {
            time_once(OPS, |i| {
                let (key, row) = (make_key(i), make_row(i));
                captured.put(&key, &row).expect("captured put");
                black_box(());
            })
        };
        // Alternate which arm goes first: a slow-down that belongs to the machine
        // should not belong to one arm.
        let (p, c) = if round % 2 == 0 {
            let p = a();
            (p, b())
        } else {
            let c = b();
            (a(), c)
        };
        let (p, c) = (
            p.as_nanos() as f64 / OPS as f64,
            c.as_nanos() as f64 / OPS as f64,
        );
        plain_ns.push(p);
        captured_ns.push(c);
        ratios.push(c / p);
        deltas.push(c - p);
    }

    let plain_med = median(plain_ns);
    let captured_med = median(captured_ns);
    let ratio = median(ratios);
    let delta = median(deltas);
    let status = captured.changelog_status().expect("status");
    println!(
        "key {} B, {} rounds x {OPS} writes",
        TaxKey::KEY_LEN,
        ROUNDS
    );
    println!("put, no log   : {plain_med:8.1} ns/write (median)");
    println!("put, captured : {captured_med:8.1} ns/write (median)");
    println!(
        "capture tax   : {delta:8.1} ns/write ({:+.1}%, medians of the paired rounds)",
        (ratio - 1.0) * 100.0
    );
    println!(
        "log           : segment {} holds {} B, {} B pending, {} dropped",
        status.segment, status.segment_written, status.pending_bytes, status.records_dropped
    );

    let _ = std::fs::remove_dir_all(&root);
}
