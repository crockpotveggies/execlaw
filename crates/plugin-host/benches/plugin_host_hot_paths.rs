//! Microbenchmarks for plugin-host hot paths (§0 axiom #14).
//!
//! `HookRegistry::tool(name)` runs on every single tool call — the
//! TurnExecutor looks up the tool, the host validates capabilities,
//! then dispatches to the subprocess. Budget ≤ 200 ns p99.
//!
//! `call_tool`'s capability check is also on the hot path; it runs
//! before any subprocess IPC. Budget ≤ 100 ns p99.

use async_trait::async_trait;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_core::tool::{
    ToolCtx, ToolDescriptor, ToolImpl, ToolLatency, ToolOutcome, ToolSource,
};
use execlaw_plugin_host::HookRegistry;
use execlaw_plugin_sdk::PluginManifest;
use std::sync::Arc;

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

// -----------------------------------------------------------------
// 2026-04-29 — built-in tool tier microbenchmarks
//
// `HookRegistry::builtin(name)` runs on every dispatch BEFORE the
// plugin tier in the new ChainedToolDispatch. Budget ≤ 200 ns p99
// (same as `tool()`) so the registry-aware dispatch doesn't regress
// turn-loop latency for plugin-only deployments.
//
// `ToolImpl::invoke` through an `Arc<dyn ToolImpl>` is the single
// virtual call between the dispatch layer and the tool body. Budget
// ≤ 1 µs for a no-op invoke (most of which is the async runtime
// overhead, not the trait dispatch itself) — this is the floor that
// guards against accidentally introducing heavyweight setup in the
// trait method.
// -----------------------------------------------------------------

struct NoopBenchTool {
    descriptor: ToolDescriptor,
}

#[async_trait]
impl ToolImpl for NoopBenchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, _ctx: ToolCtx, _args: serde_json::Value) -> ToolOutcome {
        ToolOutcome::ok(serde_json::Value::Null)
    }
}

fn build_noop_tool(name: &str) -> Arc<dyn ToolImpl> {
    Arc::new(NoopBenchTool {
        descriptor: ToolDescriptor {
            name: name.into(),
            description: "noop".into(),
            schema: serde_json::json!({"type": "object"}),
            source: ToolSource::Builtin,
            latency: ToolLatency::Low,
            capabilities: vec![],
            default_allowed_classes: vec!["Controller".into()],
        },
    })
}

fn build_registry_with_n_builtins(n: usize) -> HookRegistry {
    let reg = HookRegistry::new();
    for i in 0..n {
        reg.register_builtin(build_noop_tool(&format!("builtin_{i}")))
            .unwrap();
    }
    reg
}

fn bench_builtin_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("hook_registry_builtin_lookup");
    for n in [4usize, 32, 256] {
        let reg = build_registry_with_n_builtins(n);
        let hit_name = format!("builtin_{}", n / 2);
        group.bench_function(format!("hit_n={n}"), |b| {
            b.iter(|| reg.builtin(black_box(&hit_name)))
        });
        group.bench_function(format!("miss_n={n}"), |b| {
            b.iter(|| reg.builtin(black_box("no_such_builtin")))
        });
    }
    group.finish();
}

fn bench_builtin_invoke_noop(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio rt");
    let tool = build_noop_tool("noop");
    let mut group = c.benchmark_group("builtin_invoke_noop");
    group.bench_function("via_arc_dyn", |b| {
        b.iter(|| {
            rt.block_on(async {
                let ctx = ToolCtx::empty(
                    execlaw_core::ids::ConversationId::from("c"),
                    "Controller",
                    Arc::new(execlaw_core::tool::SystemClock),
                );
                black_box(tool.invoke(ctx, serde_json::json!({})).await);
            });
        });
    });
    group.finish();
}

/// `lookup_any` is the unified lookup path the dispatcher uses to
/// decide between the built-in and plugin tier. Budget ≤ 250 ns —
/// it does at most two map probes.
fn bench_lookup_any(c: &mut Criterion) {
    let reg = HookRegistry::new();
    // Mixed registry: 16 builtins + 16 plugin tools.
    for i in 0..16 {
        reg.register_builtin(build_noop_tool(&format!("builtin_{i}")))
            .unwrap();
    }
    let mut body = String::from(
        "[plugin]\nid = \"bench-mixed\"\nname = \"bench\"\nversion = \"1.0.0\"\n",
    );
    for i in 0..16 {
        body.push_str(&format!(
            "\n[[tools]]\nname = \"plugin_{i}\"\nschema = \"schemas/p_{i}.json\"\nlatency = \"low\"\nrequired_capabilities = []\n"
        ));
    }
    reg.enable(&PluginManifest::parse(&body).unwrap()).unwrap();

    let mut group = c.benchmark_group("hook_registry_lookup_any");
    group.bench_function("hit_builtin", |b| {
        b.iter(|| reg.lookup_any(black_box("builtin_8")))
    });
    group.bench_function("hit_plugin", |b| {
        b.iter(|| reg.lookup_any(black_box("plugin_8")))
    });
    group.bench_function("miss", |b| {
        b.iter(|| reg.lookup_any(black_box("nonexistent")))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_tool_lookup,
    bench_capability_check,
    bench_builtin_lookup,
    bench_builtin_invoke_noop,
    bench_lookup_any,
);
criterion_main!(benches);
