//! Microbenchmarks for plugin-host hot paths (§0 axiom #14).
//!
//! `HookRegistry::tool(name)` runs on every single tool call — the
//! TurnExecutor looks up the tool, the host validates capabilities,
//! then dispatches to the subprocess. Budget ≤ 200 ns p99.
//!
//! `call_tool`'s capability check is also on the hot path; it runs
//! before any subprocess IPC. Budget ≤ 100 ns p99.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_plugin_host::HookRegistry;
use execlaw_plugin_sdk::PluginManifest;

fn build_registry_with_n_tools(n: usize) -> HookRegistry {
    let mut body = String::from(
        "[plugin]\nid = \"bench-p\"\nname = \"bench\"\nversion = \"1.0.0\"\n",
    );
    for i in 0..n {
        body.push_str(&format!(
            "\n[[tools]]\nname = \"tool_{i}\"\nschema = \"schemas/tool_{i}.json\"\nlatency = \"low\"\nrequired_capabilities = [\"tools.safe\"]\n"
        ));
    }
    let manifest = PluginManifest::parse(&body).unwrap();
    let reg = HookRegistry::new();
    reg.enable(&manifest).unwrap();
    reg
}

fn bench_tool_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("hook_registry_tool_lookup");
    for n in [4usize, 32, 256] {
        let reg = build_registry_with_n_tools(n);
        let hit_name = format!("tool_{}", n / 2);
        group.bench_function(format!("hit_n={n}"), |b| {
            b.iter(|| reg.tool(black_box(&hit_name)))
        });
        group.bench_function(format!("miss_n={n}"), |b| {
            b.iter(|| reg.tool(black_box("no_such_tool")))
        });
    }
    group.finish();
}

/// Capability check — ultra-hot, should be sub-100ns. The common case
/// has the wildcard `"*"` (Controller turns), which short-circuits.
fn bench_capability_check(c: &mut Criterion) {
    // Direct test of the intersection logic. We mimic what
    // `PluginHost::call_tool` does: `has_wildcard || caller.contains(required)`.
    let caller_wildcard: Vec<&str> = vec!["*"];
    let caller_narrow: Vec<&str> = vec!["tools.safe", "memory.read"];
    let required_single = vec!["tools.safe".to_owned()];
    let required_two = vec!["tools.safe".to_owned(), "memory.read".to_owned()];

    let mut group = c.benchmark_group("capability_check");
    group.bench_function("wildcard_short_circuit", |b| {
        b.iter(|| {
            let has_wildcard = black_box(&caller_wildcard).contains(&"*");
            if has_wildcard {
                return true;
            }
            for r in black_box(&required_two) {
                if !caller_wildcard.iter().any(|c| c == r) {
                    return false;
                }
            }
            true
        })
    });
    group.bench_function("narrow_single_required", |b| {
        b.iter(|| {
            let has_wildcard = caller_narrow.contains(&"*");
            if has_wildcard {
                return true;
            }
            for r in black_box(&required_single) {
                if !caller_narrow.iter().any(|c| c == r) {
                    return false;
                }
            }
            true
        })
    });
    group.bench_function("narrow_two_required", |b| {
        b.iter(|| {
            let has_wildcard = caller_narrow.contains(&"*");
            if has_wildcard {
                return true;
            }
            for r in black_box(&required_two) {
                if !caller_narrow.iter().any(|c| c == r) {
                    return false;
                }
            }
            true
        })
    });
    group.finish();
}

criterion_group!(benches, bench_tool_lookup, bench_capability_check);
criterion_main!(benches);
