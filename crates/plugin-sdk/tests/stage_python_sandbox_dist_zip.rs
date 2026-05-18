//! Smoke test for the operator-installable python-sandbox plugin
//! zip at `dist/python-sandbox-<version>.zip`. Round-trips the
//! file through `stage_zip` so a packaging mistake (wrong layout,
//! missing file, manifest typo) surfaces at `cargo test` time
//! instead of at the operator's first install attempt.
//!
//! Mirrors the `dist_signal_zip_stages_cleanly` / `dist_discord_*`
//! convention.

use execlaw_plugin_sdk::zip_stage::stage_zip;
use std::fs::File;
use std::io::BufReader;

#[test]
fn dist_python_sandbox_zip_stages_cleanly() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let zip_path = workspace_root.join("dist").join("python-sandbox-0.1.0.zip");
    if !zip_path.exists() {
        eprintln!(
            "dist/python-sandbox-0.1.0.zip not present at {}; skipping",
            zip_path.display()
        );
        return;
    }
    let f = File::open(&zip_path).expect("open dist zip");
    let staged = stage_zip(BufReader::new(f)).expect("stage_zip must accept the dist zip");

    assert_eq!(staged.manifest.plugin.id, "python-sandbox");
    assert_eq!(staged.manifest.plugin.version, "0.1.0");

    // Four tools, all host_implemented.
    assert_eq!(staged.manifest.tools.len(), 4);
    assert!(
        staged
            .manifest
            .tools
            .iter()
            .all(|t| t.host_implemented),
        "every python-sandbox tool must be host_implemented = true \
         (dispatch happens in Rust, not in main.rhai)"
    );

    // python.interrupt is host_internal; the others are agent-callable.
    for t in &staged.manifest.tools {
        let want_internal = t.name == "python.interrupt";
        assert_eq!(
            t.host_internal, want_internal,
            "tool {} host_internal expected = {}, got {}",
            t.name, want_internal, t.host_internal
        );
    }

    // Trust floor pinned to Controller.
    for t in &staged.manifest.tools {
        assert_eq!(
            t.trust_floor.as_deref(),
            Some("Controller"),
            "tool {} must declare trust_floor = Controller",
            t.name
        );
    }

    // Kernel-gateway sidecar must be declared with the supervised
    // RPC port + health path matching what wiring::SIDECAR_NAME
    // expects.
    let kg = staged
        .manifest
        .services
        .iter()
        .find(|s| s.name == "kernel-gateway")
        .expect("[[services]] kernel-gateway missing");
    let sidecar = kg.sidecar.as_ref().expect("kernel-gateway must declare [services.sidecar]");
    assert_eq!(sidecar.rpc_port, 8888);
    assert_eq!(sidecar.rpc_health_path, "/api/kernels");

    // state://work mount RW — supervisor populates per-convo subdirs,
    // kernel container reads/writes through the bind mount.
    let mounts = &kg.mounts;
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source, "state://work");
    assert_eq!(mounts[0].target, "/work");
    assert!(
        !mounts[0].read_only,
        "/work must be RW (kernel writes outputs + supervisor writes upload symlinks)"
    );

    // Runtime tier: script with the stub main.rhai. host_implemented
    // tools don't route through Rhai, but the manifest validator
    // requires a runtime field, and the stub is there to fail-loud
    // if anything ever does try to route via Rhai.
    let runtime = staged
        .manifest
        .runtime
        .as_ref()
        .expect("[runtime] must be declared");
    assert_eq!(runtime.tier, "script");
    assert_eq!(runtime.source.as_deref(), Some("main.rhai"));
}
