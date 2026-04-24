//! Microbenchmarks for voice-pipeline hot paths (§0 axiom #14).
//!
//! Per-audio-chunk decisions: endpointer `classify_tail` fires on every STT
//! partial, `bargein::decide` fires every VAD tick. Both must be <1µs to
//! stay out of the critical audio path.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_voice_pipeline::bargein::{BargeInConfig, decide, is_backchannel};
use execlaw_voice_pipeline::endpointer::{EndpointerConfig, classify_and_window, classify_tail};

fn bench_endpointer(c: &mut Criterion) {
    let cfg = EndpointerConfig::default();
    c.bench_function("classify_tail_terminal", |b| {
        b.iter(|| classify_tail(black_box("Hello there.")))
    });
    c.bench_function("classify_tail_mid", |b| {
        b.iter(|| classify_tail(black_box("let me think,")))
    });
    c.bench_function("classify_and_window", |b| {
        b.iter(|| classify_and_window(black_box("Hello there."), black_box(&cfg)))
    });
}

fn bench_bargein(c: &mut Criterion) {
    let cfg = BargeInConfig::default();
    c.bench_function("is_backchannel_hit", |b| {
        b.iter(|| is_backchannel(black_box("mm-hmm")))
    });
    c.bench_function("is_backchannel_miss", |b| {
        b.iter(|| is_backchannel(black_box("hold on let me check that for you")))
    });
    c.bench_function("decide_wait", |b| {
        b.iter(|| decide(black_box(&cfg), black_box(50), black_box(""), black_box(true)))
    });
    c.bench_function("decide_rescind", |b| {
        b.iter(|| decide(black_box(&cfg), black_box(150), black_box("yeah"), black_box(false)))
    });
    c.bench_function("decide_confirm", |b| {
        b.iter(|| {
            decide(
                black_box(&cfg),
                black_box(500),
                black_box("cancel that"),
                black_box(true),
            )
        })
    });
}

criterion_group!(benches, bench_endpointer, bench_bargein);
criterion_main!(benches);
