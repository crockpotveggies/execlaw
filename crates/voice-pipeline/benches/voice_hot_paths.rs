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
        b.iter(|| {
            decide(
                black_box(&cfg),
                black_box(50),
                black_box(""),
                black_box(true),
            )
        })
    });
    c.bench_function("decide_rescind", |b| {
        b.iter(|| {
            decide(
                black_box(&cfg),
                black_box(150),
                black_box("yeah"),
                black_box(false),
            )
        })
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

/// Sentence splitter runs every TTS handoff — budget ≤ 1 µs for
/// typical agent reply lengths (few sentences).
fn bench_sentence_splitter(c: &mut Criterion) {
    use execlaw_voice_pipeline::chunk_at_sentence_boundaries;
    let short = "Hello there. How are you? I am fine!";
    let long = "The weather today is quite pleasant. \
                I think we should go outside. Would you agree? \
                Let me check the forecast. It says seventy-five \
                degrees and partly cloudy. That sounds perfect.";
    let mut group = c.benchmark_group("sentence_splitter");
    group.bench_function("short_3_sentences", |b| {
        b.iter(|| chunk_at_sentence_boundaries(black_box(short)))
    });
    group.bench_function("long_6_sentences", |b| {
        b.iter(|| chunk_at_sentence_boundaries(black_box(long)))
    });
    group.finish();
}

/// `ConversationKind::derive` runs on every chat-route turn (the
/// chat path calls `refresh_conversation_kind` before policy
/// evaluation). Budget ≤ 100 ns p99 — sub-microsecond is the bar
/// for the per-turn pre-flight.
fn bench_conversation_kind_derive(c: &mut Criterion) {
    use execlaw_core::conversation::ConversationKind;
    let mut group = c.benchmark_group("conversation_kind_derive");
    group.bench_function("controller_dm", |b| {
        b.iter(|| ConversationKind::derive(black_box(&["Controller"])))
    });
    group.bench_function("group_with_controller_present", |b| {
        b.iter(|| {
            ConversationKind::derive(black_box(&["Controller", "KnownTrusted", "KnownTrusted"]))
        })
    });
    group.bench_function("mixed_trust", |b| {
        b.iter(|| {
            ConversationKind::derive(black_box(&["Controller", "KnownLimited", "KnownTrusted"]))
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_endpointer,
    bench_bargein,
    bench_sentence_splitter,
    bench_conversation_kind_derive,
);
criterion_main!(benches);
