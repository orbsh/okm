//! Set-operator benches: the slice gallop against the stream merge, on the two
//! shapes the shortcut is about — lopsided and balanced.
//!
//! No CI gates: the numbers are for relative comparison (which operator a caller
//! should hand which shape), not thresholds. Run:
//! `cargo bench -p okm-query --bench set_ops`, or one group with
//! `-- --bench set_ops "slice vs stream"`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use okm_query::{OrderedSets, gallop_intersect};

/// `n` keys ascending, then every `step`-th key of them: the lopsided shape
/// (1 000 000 against 1 000 is `step = 997`), and `step = 2` is the balanced one.
fn pair(n: u64, step: usize) -> (Vec<u64>, Vec<u64>) {
    ((0..n).collect(), (0..n).step_by(step).collect())
}

fn slice_vs_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("slice vs stream");
    group.sample_size(20);
    for (name, n, step) in [
        ("skewed 1M/1K", 1_000_000u64, 997usize),
        ("balanced 1M/500K", 1_000_000, 2),
        ("balanced 100K/50K", 100_000, 2),
    ] {
        let (long, short) = pair(n, step);
        group.bench_with_input(
            BenchmarkId::new("gallop (slice)", name),
            &(&long, &short),
            |b, (l, s)| {
                b.iter(|| black_box(gallop_intersect(l, s).count()));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("merge (stream)", name),
            &(&long, &short),
            |b, (l, s)| {
                b.iter(|| black_box(l.iter().intersect(s.iter()).count()));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, slice_vs_stream);
criterion_main!(benches);
