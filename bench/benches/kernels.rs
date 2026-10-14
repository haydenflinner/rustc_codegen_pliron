use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;

mod chars_alpha;
mod chars_ascii;
mod chars_mixed;
mod contains_rng;
mod eqbug;
mod fib;
mod hashmap;
mod hmget;
mod iterw;
mod matmul;
mod nbody;
mod parse_only;
mod sieve;
mod sortw;
mod strwork;
mod tokenize;

fn bench_chars_alpha(c: &mut Criterion) {
    c.bench_function("chars_alpha", |b| {
        let s = chars_alpha::setup();
        b.iter(|| chars_alpha::run(black_box(&s)))
    });
}

fn bench_chars_ascii(c: &mut Criterion) {
    c.bench_function("chars_ascii", |b| {
        let s = chars_ascii::setup();
        b.iter(|| chars_ascii::run(black_box(&s)))
    });
}

fn bench_chars_mixed(c: &mut Criterion) {
    c.bench_function("chars_mixed", |b| {
        let s = chars_mixed::setup();
        b.iter(|| chars_mixed::run(black_box(&s)))
    });
}

fn bench_contains_rng(c: &mut Criterion) {
    c.bench_function("contains_rng", |b| {
        let v = contains_rng::setup();
        b.iter(|| contains_rng::run(black_box(&v), black_box(100), black_box(60_000)))
    });
}

fn bench_eqbug(c: &mut Criterion) {
    c.bench_function("eqbug", |b| b.iter(eqbug::run));
}

fn bench_fib(c: &mut Criterion) {
    c.bench_function("fib", |b| b.iter(fib::run));
}

fn bench_hashmap(c: &mut Criterion) {
    c.bench_function("hashmap", |b| b.iter(hashmap::run));
}

fn bench_hmget(c: &mut Criterion) {
    c.bench_function("hmget", |b| {
        let m = hmget::setup();
        b.iter(|| hmget::run(black_box(&m)))
    });
}

fn bench_iterw(c: &mut Criterion) {
    c.bench_function("iterw", |b| {
        let a = iterw::setup();
        b.iter(|| iterw::run(black_box(&a)))
    });
}

fn bench_matmul(c: &mut Criterion) {
    c.bench_function("matmul", |b| {
        b.iter_batched(
            matmul::setup,
            |(a, bb, mut c, n)| matmul::run(&a, &bb, &mut c, n),
            BatchSize::LargeInput,
        )
    });
}

fn bench_nbody(c: &mut Criterion) {
    c.bench_function("nbody", |b| {
        b.iter_batched(
            nbody::setup,
            |mut bodies| nbody::run(&mut bodies),
            BatchSize::LargeInput,
        )
    });
}

fn bench_parse_only(c: &mut Criterion) {
    c.bench_function("parse_only", |b| b.iter(parse_only::run));
}

fn bench_sieve(c: &mut Criterion) {
    c.bench_function("sieve", |b| b.iter(sieve::run));
}

fn bench_sortw(c: &mut Criterion) {
    c.bench_function("sortw", |b| {
        b.iter_batched(
            sortw::setup,
            |mut v| sortw::run(&mut v),
            BatchSize::LargeInput,
        )
    });
}

fn bench_strwork(c: &mut Criterion) {
    c.bench_function("strwork", |b| b.iter(strwork::run));
}

fn bench_tokenize(c: &mut Criterion) {
    c.bench_function("tokenize", |b| {
        let s = tokenize::setup();
        b.iter(|| tokenize::run(black_box(&s)))
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    targets = bench_chars_alpha, bench_chars_ascii, bench_chars_mixed,
        bench_contains_rng, bench_eqbug, bench_fib, bench_hashmap, bench_hmget,
        bench_iterw, bench_matmul, bench_nbody, bench_parse_only, bench_sieve,
        bench_sortw, bench_strwork, bench_tokenize
}

criterion_main!(benches);
