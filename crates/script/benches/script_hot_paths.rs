//! Microbenchmarks for the script-tier hot paths (axiom #14).
//!
//! What we're measuring + budgets:
//!
//! - **Engine construction** — `ScriptEngine::build_for_plugin`
//!   runs once per plugin enable. Budget: ≤ 1 ms p99.
//! - **Script parse** — `ScriptPlugin::from_source` runs once per
//!   plugin enable too. Budget: ≤ 5 ms for a ~200-line script.
//! - **Dispatch (no HTTP)** — `identity_resolve` returning the
//!   null-match short-circuit (no token). Pure interpreter cost
//!   per host call. Budget: ≤ 100 µs p99 — this is the per-RPC
//!   overhead a caller pays even when the plugin no-ops.
//! - **JSON ↔ Rhai conversion** — round-tripping a typical
//!   identity_resolve params + result map. Budget: ≤ 10 µs each
//!   direction for a small map.
//!
//! These benches run with no network and no real OAuth to keep
//! the numbers meaningful — every variation in HTTP latency
//! would dwarf the interpreter overhead anyway. The purpose is
//! catching script-tier regressions, not measuring a real
//! plugin's wall-clock budget.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_script::{ScriptEngine, ScriptPlugin};
use std::path::PathBuf;

/// Tiny script that exercises the realistic shape of an
/// identity_resolve dispatch (top-level fn definition, map
/// indexing, conditional return, normalisation primitives) but
/// short-circuits before any HTTP.
const NULL_MATCH_SCRIPT: &str = r#"
fn identity_resolve(transport, handle, oauth) {
    if oauth["controller"] == () {
        return #{ "match": () };
    }
    if transport != "email" {
        return #{ "match": () };
    }
    #{ "match": () }
}
"#;

fn bench_engine_construct(c: &mut Criterion) {
    c.bench_function("script_engine_build_for_plugin", |b| {
        let factory = ScriptEngine::new();
        b.iter(|| {
            let _ = factory.build_for_plugin(black_box("p"));
        });
    });
}

fn bench_script_parse(c: &mut Criterion) {
    let factory = ScriptEngine::new();
    c.bench_function("script_plugin_parse_small", |b| {
        b.iter(|| {
            let _ = ScriptPlugin::from_source(
                black_box("p"),
                black_box(NULL_MATCH_SCRIPT),
                &factory,
            )
            .unwrap();
        });
    });

    // Real shipped plugins — the budget should comfortably
    // contain a 200-line .rhai parse.
    let workspace_root = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    };
    for (name, rel) in [
        ("google_contacts", "plugins/google-contacts/main.rhai"),
        ("google_calendar", "plugins/google-calendar/main.rhai"),
    ] {
        let path = workspace_root.join(rel);
        if let Ok(real) = std::fs::read_to_string(&path) {
            let bench_name = format!("script_plugin_parse_{name}");
            c.bench_function(&bench_name, |b| {
                b.iter(|| {
                    let _ = ScriptPlugin::from_source(
                        black_box(name),
                        black_box(&real),
                        &factory,
                    )
                    .unwrap();
                });
            });
        }
    }
}

fn bench_dispatch_null_match(c: &mut Criterion) {
    // One Rhai engine constructed once; per-call dispatch only.
    // Drives a tokio runtime since ScriptPlugin::identity_resolve
    // is async (spawn_blocking under the hood).
    let factory = ScriptEngine::new();
    let plugin = ScriptPlugin::from_source("p", NULL_MATCH_SCRIPT, &factory).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    c.bench_function("script_dispatch_identity_resolve_null", |b| {
        let plugin = plugin.clone();
        b.iter(|| {
            rt.block_on(async {
                let _ = plugin
                    .identity_resolve(
                        black_box("email"),
                        black_box("x@x.com"),
                        serde_json::Map::new(),
                    )
                    .await
                    .unwrap();
            });
        });
    });
}

criterion_group!(
    benches,
    bench_engine_construct,
    bench_script_parse,
    bench_dispatch_null_match,
);
criterion_main!(benches);
