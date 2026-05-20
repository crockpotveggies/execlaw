//! Sanity-check that the shipped `plugins/python-sandbox/plugin.toml`
//! parses through the SDK's manifest reader without errors AND that
//! the host-implemented contract holds (every tool dispatches into
//! the host's `python_sandbox` Rust module, not into main.rhai).
//!
//! Mirrors the `open_meteo_manifest` / `finance_yahoo_manifest`
//! convention — a typo'd field, missing required field, or a
//! regression that strips `host_implemented` off a tool fails here
//! at `cargo test` time rather than at the operator's first install.

use execlaw_plugin_sdk::PluginManifest;
use std::path::PathBuf;

#[test]
fn python_sandbox_manifest_parses_with_expected_tools() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/python-sandbox/plugin.toml");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let manifest: PluginManifest =
        toml::from_str(&source).unwrap_or_else(|e| panic!("plugin.toml must parse: {e}"));

    assert_eq!(manifest.plugin.id, "python-sandbox");
    assert_eq!(manifest.plugin.version, "0.1.0");

    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    let want = [
        "python.execute",
        "python.reset",
        "python.interrupt",
        "python.list_files",
    ];
    for w in &want {
        assert!(
            tool_names.contains(w),
            "manifest missing tool `{w}`; got {tool_names:?}"
        );
    }
    assert_eq!(
        manifest.tools.len(),
        want.len(),
        "manifest declared an unexpected number of tools"
    );

    // Every tool MUST be host_implemented = true. The python-sandbox
    // module owns dispatch (per-conversation kernel routing, MIME
    // bundle parsing, created_files attachment) — there is no
    // script-tier path that could implement it.
    for t in &manifest.tools {
        assert!(
            t.host_implemented,
            "tool {} must be host_implemented = true (python-sandbox has no script-tier dispatch)",
            t.name
        );
    }

    // python.interrupt is host_internal — agents don't call it
    // directly; only the SPA's stop button does. Others are
    // agent-callable.
    for t in &manifest.tools {
        let want_internal = t.name == "python.interrupt";
        assert_eq!(
            t.host_internal, want_internal,
            "tool {} host_internal expected = {}, got {}",
            t.name, want_internal, t.host_internal
        );
    }

    // Trust floor = Controller across the board. Python execution is
    // high-leverage; outsiders in a mixed-trust thread must not be
    // able to trigger it. Operators relax this per-conversation in
    // settings later (v1.1).
    for t in &manifest.tools {
        assert_eq!(
            t.trust_floor.as_deref(),
            Some("Controller"),
            "tool {} must declare trust_floor=Controller, got {:?}",
            t.name,
            t.trust_floor
        );
    }

    // Single sidecar — the kernel gateway. The supervisor keeps THIS
    // container healthy; per-conversation kernels are managed by the
    // host's python_sandbox module via the gateway's HTTP+WS API,
    // NOT by spawning more containers.
    assert_eq!(manifest.services.len(), 1);
    let kg = &manifest.services[0];
    assert_eq!(kg.name, "kernel-gateway");
    let sidecar = kg
        .sidecar
        .as_ref()
        .expect("[[services]] kernel-gateway must declare [services.sidecar]");
    assert_eq!(sidecar.rpc_port, 8888);
    assert_eq!(sidecar.rpc_health_path, "/api/kernels");

    // Single per-sidecar state mount at /work. Supervisor populates
    // /work/<convo_id>/{uploads,outputs}/ from the host side.
    let mounts = &kg.mounts;
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source, "state://work");
    assert_eq!(mounts[0].target, "/work");
    assert!(
        !mounts[0].read_only,
        "/work must be read-write (kernel writes outputs there + supervisor writes upload symlinks)"
    );

    // Runtime tier is script with the main.rhai stub — every tool is
    // host_implemented = true, so the script's only job is to satisfy
    // the manifest validator and host future admin-route handlers.
    let runtime = manifest
        .runtime
        .as_ref()
        .expect("python-sandbox must declare a [runtime]");
    assert_eq!(runtime.tier, "script");
    assert_eq!(runtime.source.as_deref(), Some("main.rhai"));
}
