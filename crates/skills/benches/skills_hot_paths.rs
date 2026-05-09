//! Criterion benches for the skill subsystem's hot paths.
//!
//! Budgets locked 2026-05-03 (axiom #14). Regressions > 10% block
//! merge. The discovery-layer paths sit inside the prompt-cache
//! prefix and add to per-turn fixed overhead, so blowing the budget
//! directly slows every agent call.
//!
//! | Path                              | Budget (median) | Rationale                              |
//! |-----------------------------------|-----------------|----------------------------------------|
//! | scan_clean_body                   |   < 50 µs       | Every write runs the scanner first.    |
//! | scan_body_with_known_pattern      |   < 100 µs      | Same, with a regex hit added.          |
//! | scan_large_body (~1.4 KB)         |   < 250 µs      | Bound on operator-pasted long bodies.  |
//! | list_index_10                     |   < 100 µs      | Hot path — runs every session start.   |
//! | list_index_100                    |   < 500 µs      | Same, mid catalog.                     |
//! | list_index_1000                   |   < 5 ms        | Same, large catalog.                   |
//! | view_one_skill                    |   < 1 ms        | Activation event. Not per-turn.        |
//! | search_selective_query            |   < 5 ms        | Realistic LLM search.                  |
//! | search_every_doc_matches          |   < 50 ms       | Worst case (informational, perms).     |

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use execlaw_core::db::{Database, DbConfig};
use execlaw_core::migrations::MigrationRunner;
use execlaw_skills::model::{NewSkill, NewSkillVersion, RegistrationKind};
use execlaw_skills::sanitizer::{SanitizationReport, sanitize_step};
use execlaw_skills::scanner::{ScanInput, Strictness, scan};
use execlaw_skills::store::SkillStore;
use serde_json::json;

fn fresh_store() -> SkillStore {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    SkillStore::new(db)
}

/// A small bag of topical words so seeded skills don't all share a
/// vocabulary. A search for one of these terms hits ~1/N skills,
/// matching a realistic selective query rather than the degenerate
/// every-doc-matches case.
const TOPICS: &[&str] = &[
    "research",
    "scaffold",
    "deploy",
    "debug",
    "refactor",
    "review",
    "migrate",
    "benchmark",
    "audit",
    "rollback",
    "snapshot",
    "trace",
    "lint",
    "fuzz",
    "profile",
    "schema",
    "encrypt",
    "compose",
];

fn seed_n(store: &SkillStore, n: usize) {
    for i in 0..n {
        let topic = TOPICS[i % TOPICS.len()];
        let name = format!("bench/{topic}-{:04}", i);
        let body = format!(
            "Step 1: prepare the {topic} workspace. Step 2: execute the {topic} pipeline. \
             Step 3: validate the result. Skill instance {i}."
        );
        store
            .create(
                NewSkill {
                    name,
                    source: "bench".into(),
                    registration_kind: RegistrationKind::Authored,
                    owning_plugin_id: None,
                    initial_version: NewSkillVersion {
                        description: format!(
                            "Use this when the user asks about {topic} workflow {i}"
                        ),
                        body_md: body,
                        frontmatter_json: r#"{"tags":["bench"]}"#.into(),
                        authored_by: "bench".into(),
                        promotion_notes: None,
                    },
                    resources: vec![],
                },
                Strictness::Strict,
                1000 + i as i64,
            )
            .unwrap();
    }
}

fn bench_scanner(c: &mut Criterion) {
    let mut group = c.benchmark_group("skill_scanner");
    let clean_body = "This is a perfectly ordinary skill body describing how to scaffold a new Rust crate \
         using cargo new and conventional layout choices the team has standardized on. \
         The description goes on for a couple of sentences to give the entropy heuristic \
         realistic input rather than a tiny string.";
    let dirty_body = format!(
        "{}\n\nuse sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 to authenticate",
        clean_body
    );
    let big_body = clean_body.repeat(20);

    group.throughput(Throughput::Bytes(clean_body.len() as u64));
    group.bench_function("scan_clean_body", |b| {
        b.iter(|| {
            let input = ScanInput {
                body_md: black_box(clean_body),
                description: black_box("ordinary skill"),
                frontmatter_json: black_box("{}"),
                resources: black_box(&[]),
            };
            scan(&input, Strictness::Strict)
        })
    });

    group.bench_function("scan_body_with_known_pattern", |b| {
        b.iter(|| {
            let input = ScanInput {
                body_md: black_box(&dirty_body),
                description: black_box("dirty"),
                frontmatter_json: black_box("{}"),
                resources: black_box(&[]),
            };
            scan(&input, Strictness::Strict)
        })
    });

    group.throughput(Throughput::Bytes(big_body.len() as u64));
    group.bench_function("scan_large_body", |b| {
        b.iter(|| {
            let input = ScanInput {
                body_md: black_box(&big_body),
                description: black_box("ordinary skill"),
                frontmatter_json: black_box("{}"),
                resources: black_box(&[]),
            };
            scan(&input, Strictness::Strict)
        })
    });

    group.finish();
}

fn bench_list_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("skill_list_index");
    for n in [10, 100, 1000usize] {
        let store = fresh_store();
        seed_n(&store, n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let v = store.list_index().unwrap();
                black_box(v);
            })
        });
    }
    group.finish();
}

fn bench_view(c: &mut Criterion) {
    let store = fresh_store();
    seed_n(&store, 1000);
    let mut group = c.benchmark_group("skill_view");
    group.bench_function("view_one_skill", |b| {
        b.iter(|| {
            let v = store.view(black_box("bench/skill-0500")).unwrap();
            black_box(v);
        })
    });
    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let store = fresh_store();
    seed_n(&store, 1000);
    let mut group = c.benchmark_group("skill_search");
    // Selective query — matches ~1/N skills (one of TOPICS).
    // Representative of how the LLM would use search: hunting for a
    // specific topic, not querying terms shared by every skill.
    group.bench_function("search_selective_query", |b| {
        b.iter(|| {
            let hits = store.search(black_box("research workflow"), 10).unwrap();
            black_box(hits);
        })
    });
    // Worst-case: a term every skill body shares ("step"). Every doc
    // matches; FTS5 must rank all 1000. Tracked separately so we
    // notice if it regresses but the budget is permissive.
    group.bench_function("search_every_doc_matches", |b| {
        b.iter(|| {
            let hits = store.search(black_box("step pipeline"), 10).unwrap();
            black_box(hits);
        })
    });
    group.finish();
}

/// Phase C — sanitizer bench. Runs on every captured turn, so its
/// cost is on the worker's hot path (not the chat handler's, but
/// still latency-sensitive because operators eventually want
/// proposals to land within seconds).
///
/// Budget: a 5-tool-call trajectory with realistic args/results
/// must sanitize in < 1 ms total (≈200 µs per step).
fn bench_sanitizer(c: &mut Criterion) {
    let mut group = c.benchmark_group("skill_sanitizer");
    let clean_args = json!({
        "user": "alice",
        "intent": "find recent emails about the Q3 budget review",
        "limit": 10
    });
    let clean_result = json!({
        "messages": [
            {"id": "m1", "subject": "Q3 budget review draft", "from": "bob"},
            {"id": "m2", "subject": "Re: Q3 budget review draft", "from": "carol"}
        ]
    });
    group.bench_function("sanitize_clean_step", |b| {
        b.iter(|| {
            let mut r = SanitizationReport::default();
            let s = sanitize_step(
                black_box(1),
                black_box("email_search"),
                black_box(&clean_args),
                &Ok(clean_result.clone()),
                &mut r,
            );
            black_box(s);
        })
    });

    let dirty_args = json!({
        "Authorization": "Bearer ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890",
        "endpoint": "https://api.example.com/v1/items",
        "body": "use sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz to authenticate"
    });
    group.bench_function("sanitize_dirty_step", |b| {
        b.iter(|| {
            let mut r = SanitizationReport::default();
            let s = sanitize_step(
                black_box(1),
                black_box("http_post"),
                black_box(&dirty_args),
                &Ok(json!({"status": 200})),
                &mut r,
            );
            black_box(s);
        })
    });

    // 5-step trajectory like a realistic agent run.
    let trajectory: Vec<(serde_json::Value, serde_json::Value)> = (0..5)
        .map(|i| {
            (
                json!({"step": i, "intent": "do a thing", "data": clean_args.clone()}),
                json!({"ok": true, "step": i, "result": clean_result.clone()}),
            )
        })
        .collect();
    group.bench_function("sanitize_5_step_trajectory", |b| {
        b.iter(|| {
            let mut r = SanitizationReport::default();
            for (i, (args, result)) in trajectory.iter().enumerate() {
                let s = sanitize_step(
                    i as u32,
                    "tool",
                    black_box(args),
                    &Ok(result.clone()),
                    &mut r,
                );
                black_box(s);
            }
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_scanner,
    bench_list_index,
    bench_view,
    bench_search,
    bench_sanitizer
);
criterion_main!(benches);
