//! Criterion benchmarks for the hot path.
//!
//! We can't benchmark "real" detect-to-submit end-to-end without a live
//! Geyser stream and Jito block engine — so instead we benchmark the CPU-
//! bound stages that sit between them:
//!
//!   1. Instruction decode (detector)
//!   2. Filter prefilter (regex + allow/blocklist checks)
//!   3. Filter full evaluate (adds threshold checks)
//!   4. Position state machine tick
//!
//! The sum of (1) + (2) + (3) is a reasonable proxy for "how long after a
//! transaction arrives on our socket until we'd hand off to the signer".
//! Signing + HTTP POST to Jito is network-bound and benched separately in
//! integration tests against a staging endpoint.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use solana_nova_sniper::bench_api::{
    bench_decode, bench_evaluate, bench_prefilter, bench_strategy_tick, BenchFixture,
};

fn decode_benchmark(c: &mut Criterion) {
    let fx = BenchFixture::new();
    c.bench_function("detector/decode_pumpfun_create", |b| {
        b.iter(|| {
            let ev = bench_decode(&fx);
            black_box(ev);
        });
    });
}

fn filter_benchmark(c: &mut Criterion) {
    let fx = BenchFixture::new();
    c.bench_function("filter/prefilter", |b| {
        b.iter(|| {
            let v = bench_prefilter(&fx);
            black_box(v);
        });
    });
    c.bench_function("filter/evaluate", |b| {
        b.iter(|| {
            let v = bench_evaluate(&fx);
            black_box(v);
        });
    });
}

fn strategy_benchmark(c: &mut Criterion) {
    let fx = BenchFixture::new();
    c.bench_function("strategy/tick_hold", |b| {
        b.iter(|| {
            let a = bench_strategy_tick(&fx, 1.1, 1);
            black_box(a);
        });
    });
    c.bench_function("strategy/tick_take_profit", |b| {
        b.iter(|| {
            let a = bench_strategy_tick(&fx, 2.5, 1);
            black_box(a);
        });
    });
}

criterion_group!(benches, decode_benchmark, filter_benchmark, strategy_benchmark);
criterion_main!(benches);
