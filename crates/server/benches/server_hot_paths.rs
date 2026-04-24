//! Microbenchmarks for execlaw-server hot paths (§0 axiom #14).
//!
//! Capability-token issue/verify runs at runner spawn time (issue) and on
//! every tool-call authorization check (verify). Both are on the turn path
//! — budget ≤ 500µs per issue, ≤ 200µs per verify on typical hardware.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_server::auth::JwtSigner;
use execlaw_server::capability::{issue_capability_token, verify_capability_token};

fn bench_capability_token(c: &mut Criterion) {
    let signer = JwtSigner::generate("execlaw-bench".into());
    let caps = vec![
        "tools.safe".to_string(),
        "memory.read".to_string(),
        "memory.write".to_string(),
    ];

    c.bench_function("issue_capability_token", |b| {
        b.iter(|| {
            issue_capability_token(
                black_box(&signer),
                black_box("pri_ctrl"),
                black_box("conv_bench"),
                black_box(47),
                black_box(caps.clone()),
                black_box(None),
            )
        })
    });

    let token = issue_capability_token(&signer, "pri_ctrl", "conv_bench", 47, caps.clone(), None);
    c.bench_function("verify_capability_token", |b| {
        b.iter(|| {
            verify_capability_token(
                black_box(&signer),
                black_box(&token),
                black_box("conv_bench"),
                black_box(47),
            )
            .unwrap()
        })
    });
}

fn bench_jwt_access(c: &mut Criterion) {
    let signer = JwtSigner::generate("execlaw-bench".into());
    c.bench_function("issue_access_token", |b| {
        b.iter(|| {
            signer
                .issue_access_token(black_box("pri_ctrl"), black_box("sess-1"), black_box(60))
                .unwrap()
        })
    });
    let tok = signer.issue_access_token("pri_ctrl", "sess-1", 60).unwrap();
    c.bench_function("verify_access_token", |b| {
        b.iter(|| signer.verify_access_token(black_box(&tok)).unwrap())
    });
}

/// End-to-end bench of the policy → capability-token pipeline (the
/// pre-turn authorization path). This is what runs on EVERY chat
/// message before the model call — so its budget is tight.
fn bench_policy_to_capability_pipeline(c: &mut Criterion) {
    use execlaw_policy::trust::{TrustLevel, TurnPolicyInput, evaluate_turn};
    let signer = JwtSigner::generate("execlaw-bench".into());
    let mut turn_seq = 0i64;
    c.bench_function("policy_then_issue_capability", |b| {
        b.iter(|| {
            turn_seq += 1;
            let decision = evaluate_turn(TurnPolicyInput {
                effective_trust: TrustLevel::Controller,
                sender_trust: TrustLevel::Controller,
                voice: false,
                accesses_sensitive_data: false,
                produces_external_effect: false,
            });
            let caps: Vec<String> =
                decision.capability_set.iter().map(|s| (*s).to_owned()).collect();
            issue_capability_token(
                &signer,
                "pri_ctrl",
                "conv_bench",
                turn_seq,
                caps,
                None,
            )
        })
    });
}

criterion_group!(
    benches,
    bench_capability_token,
    bench_jwt_access,
    bench_policy_to_capability_pipeline,
);
criterion_main!(benches);
