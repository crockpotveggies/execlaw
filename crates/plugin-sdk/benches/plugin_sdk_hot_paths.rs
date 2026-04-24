//! Microbenchmarks for plugin-sdk hot paths (§0 axiom #14).
//!
//! `PluginManifest::parse` runs once per plugin-install, so absolute
//! latency here is less load-bearing than `evaluate_turn` — but a 10x
//! regression would still block. Budget ≤ 1ms p99 per typical manifest.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use execlaw_plugin_sdk::PluginManifest;

const TINY_MANIFEST: &str = r#"
[plugin]
id = "p1"
name = "p1"
version = "0.1.0"
"#;

const REALISTIC_MANIFEST: &str = r#"
[plugin]
id = "signal-transport"
name = "Signal"
version = "2.3.1"
description = "Transport plugin for signal-cli"

[transport]
transport_id = "signal"
supports_attachments = true
supports_groups = true

[identity_provider]
resolves = ["phone", "signal_uuid"]
trust_hint_default = "Contact"

[[tools]]
name = "signal.send_attachment"
schema = "schemas/send_attachment.json"
latency = "medium"
required_capabilities = ["transport.send", "attachments.write"]

[[tools]]
name = "signal.get_group_members"
schema = "schemas/get_members.json"
latency = "low"
required_capabilities = ["transport.read"]

[[ui_panels]]
mount = "/plugins/signal"
entry = "index.js"

[[event_subscriptions]]
on = "conversation.message_inbound"
handler = "handlers/inbound.js"

[[event_subscriptions]]
on = "conversation.attachment_received"
handler = "handlers/attachment.js"

[[alert_sources]]
fingerprint_prefix = "signal."
"#;

fn bench_manifest_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("manifest_parse");
    group.bench_function("tiny", |b| {
        b.iter(|| PluginManifest::parse(black_box(TINY_MANIFEST)).unwrap())
    });
    group.bench_function("realistic", |b| {
        b.iter(|| PluginManifest::parse(black_box(REALISTIC_MANIFEST)).unwrap())
    });
    group.finish();
}

criterion_group!(benches, bench_manifest_parse);
criterion_main!(benches);
