//! Smoke test for the operator-installable Signal plugin zip at
//! `dist/signal-0.1.0.zip`. Round-trips the file through
//! `stage_zip` so a packaging mistake (wrong layout, missing
//! file, manifest typo) surfaces at `cargo test` time instead of
//! at the operator's first install attempt.

use execlaw_plugin_sdk::zip_stage::stage_zip;
use std::fs::File;
use std::io::BufReader;

#[test]
fn dist_signal_zip_stages_cleanly() {
    // Locate dist/ relative to the workspace root. CARGO_MANIFEST_DIR
    // points at this crate (plugin-sdk), so step up two levels.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let zip_path = workspace_root.join("dist").join("signal-0.2.0.zip");
    if !zip_path.exists() {
        // Allow CI / fresh clones to skip — the zip is rebuilt
        // by `scripts/package-plugins.ps1` (Phase 7) and not
        // checked in until the operator opts to ship it.
        eprintln!(
            "dist/signal-0.2.0.zip not present at {}; skipping",
            zip_path.display()
        );
        return;
    }
    let f = File::open(&zip_path).expect("open dist zip");
    let staged = stage_zip(BufReader::new(f)).expect("stage_zip must accept the dist zip");
    assert_eq!(staged.manifest.plugin.id, "signal");
    assert_eq!(staged.manifest.plugin.version, "0.2.0");
    // All six tools are host-implemented — pin so a packaging step
    // that accidentally bundled an old manifest gets caught.
    assert_eq!(staged.manifest.tools.len(), 6);
    assert!(
        staged.manifest.tools.iter().all(|t| t.host_implemented),
        "every signal tool must be host_implemented in the dist zip"
    );
    // Sidecar declaration must be present so the supervisor
    // spawns signal-cli on enable.
    let signal_cli = staged
        .manifest
        .services
        .iter()
        .find(|s| s.name == "signal-cli")
        .expect("[[services]] signal-cli missing from dist zip");
    assert!(signal_cli.sidecar.is_some());
    // main.rhai must be present on disk too — it's the defensive
    // stub the rhai tier hits if any tool ever leaks down.
    assert!(staged.root().join("main.rhai").exists());
}
