//! Microbenchmarks for execlaw-policy hot paths (§0 axiom #14).
//!
//! `evaluate_turn` runs on every single turn — before any model call — so
//! its budget is ≤ 1µs p99. Spotlight wrap runs on every untrusted input
//! ingested in a turn; input-guard runs on every inbound transport event.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_policy::input_guard::{fold_common_homoglyphs, strip_invisible};
use execlaw_policy::rule_of_two::{RuleOfTwoInput, rule_of_two_verdict};
use execlaw_policy::sideband::{default_sideband_priority, pick_sideband_transport};
use execlaw_policy::spotlighting::Spotlight;
use execlaw_policy::trust::{TrustLevel, TurnPolicyInput, evaluate_turn};

fn bench_evaluate_turn(c: &mut Criterion) {
    let mut group = c.benchmark_group("evaluate_turn");
    for (name, trust) in [
        ("controller", TrustLevel::Controller),
        ("delegated", TrustLevel::Delegated),
        ("known_trusted", TrustLevel::KnownTrusted),
        ("known_limited", TrustLevel::KnownLimited),
        ("unknown_pending", TrustLevel::UnknownPending),
        ("blocked", TrustLevel::Blocked),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                evaluate_turn(black_box(TurnPolicyInput {
                    effective_trust: trust,
                    sender_trust: trust,
                    voice: false,
                    accesses_sensitive_data: true,
                    produces_external_effect: true,
                }))
            })
        });
    }
    group.finish();
}

fn bench_rule_of_two(c: &mut Criterion) {
    c.bench_function("rule_of_two_verdict", |b| {
        b.iter(|| {
            rule_of_two_verdict(black_box(RuleOfTwoInput {
                untrusted_input_in_turn: true,
                accesses_sensitive_data: true,
                produces_external_effect: false,
            }))
        })
    });
}

fn bench_spotlight(c: &mut Criterion) {
    let spot = Spotlight::generate();
    let tiny = "hello world";
    let medium: String = "abcd ".repeat(256); // ~1.3 KB
    let large: String = "xyz ".repeat(4096); // ~16 KB

    let mut group = c.benchmark_group("spotlight_wrap");
    group.bench_function("tiny_11_bytes", |b| {
        b.iter(|| spot.wrap(black_box(tiny)))
    });
    group.bench_function("medium_1_3k", |b| {
        b.iter(|| spot.wrap(black_box(&medium)))
    });
    group.bench_function("large_16k", |b| {
        b.iter(|| spot.wrap(black_box(&large)))
    });
    group.finish();

    c.bench_function("spotlight_generate", |b| {
        b.iter(Spotlight::generate)
    });
}

fn bench_input_guard(c: &mut Criterion) {
    // Representative inbound text: 512 chars of mixed ASCII + a few stowaway
    // invisibles + a couple of homoglyphs, modeling a cleaned-up DM.
    let sample: String = format!(
        "{}hello there\u{200B}everything normal looking {} stuff",
        "a".repeat(200),
        "рaypal.com",
    );
    c.bench_function("strip_invisible_512", |b| {
        b.iter(|| strip_invisible(black_box(&sample)))
    });
    c.bench_function("fold_homoglyphs_512", |b| {
        b.iter(|| fold_common_homoglyphs(black_box(&sample)))
    });
}

fn bench_sideband(c: &mut Criterion) {
    let enabled = ["signal", "email", "webui", "matrix"];
    let priority = default_sideband_priority();
    let prio_refs: Vec<&str> = priority.to_vec();
    c.bench_function("pick_sideband_transport", |b| {
        b.iter(|| {
            pick_sideband_transport(
                black_box(&enabled),
                black_box("signal"),
                black_box(&prio_refs),
            )
        })
    });
}

criterion_group!(
    benches,
    bench_evaluate_turn,
    bench_rule_of_two,
    bench_spotlight,
    bench_input_guard,
    bench_sideband,
);
criterion_main!(benches);
