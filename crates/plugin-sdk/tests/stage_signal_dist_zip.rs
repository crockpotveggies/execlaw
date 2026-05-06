//! Smoke test for the operator-installable Signal plugin zip at
//! `dist/signal-<version>.zip`. Round-trips the file through
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
    let zip_path = workspace_root.join("dist").join("signal-0.4.6.zip");
    if !zip_path.exists() {
        // Allow CI / fresh clones to skip — the zip is rebuilt
        // by `scripts/package-plugins.ps1` (Phase 7) and not
        // checked in until the operator opts to ship it.
        eprintln!(
            "dist/signal-0.4.6.zip not present at {}; skipping",
            zip_path.display()
        );
        return;
    }
    let f = File::open(&zip_path).expect("open dist zip");
    let staged = stage_zip(BufReader::new(f)).expect("stage_zip must accept the dist zip");
    assert_eq!(staged.manifest.plugin.id, "signal");
    assert_eq!(staged.manifest.plugin.version, "0.4.6");
    // Phase B: all six tools are now SCRIPT-tier (main.rhai owns
    // them via sidecar_http_* bindings). Pin so a regression that
    // re-introduces host_implemented gets caught.
    // 6 agent-callable + 3 host-internal convention tools
    // (set_typing, send_with_attachments, fetch_attachment).
    assert_eq!(staged.manifest.tools.len(), 9);
    assert!(
        staged.manifest.tools.iter().all(|t| !t.host_implemented),
        "every signal tool must be script-tier in v0.4.0+ (host_implemented = false)"
    );
    // Two admin routes (status + qrcodelink) — the plugin now owns
    // its own pairing flow.
    assert_eq!(staged.manifest.admin_routes.len(), 3);
    // Sidecar declaration must be present so the supervisor
    // spawns signal-cli on enable.
    let signal_cli = staged
        .manifest
        .services
        .iter()
        .find(|s| s.name == "signal-cli")
        .expect("[[services]] signal-cli missing from dist zip");
    assert!(signal_cli.sidecar.is_some());
    // v0.3 must use json-rpc mode + persistent state mount + the
    // read-receipts wrapper entrypoint. Without any of these the
    // operator's pairing flow either returns a blank QR or signal-cli
    // silently drops read-receipt acks.
    assert_eq!(signal_cli.env.get("MODE").map(|s| s.as_str()), Some("json-rpc"));
    assert!(
        signal_cli
            .mounts
            .iter()
            .any(|m| m.target == "/home/.local/share/signal-cli"),
        "data state mount missing"
    );
    assert!(
        signal_cli
            .mounts
            .iter()
            .any(|m| m.target == "/enable-read-receipts.sh"),
        "read-receipts script mount missing"
    );
    let entry = signal_cli
        .entrypoint
        .as_ref()
        .expect("entrypoint must override bbernhard's default");
    assert!(entry.iter().any(|s| s.contains("enable-read-receipts.sh")));
    // main.rhai must be present on disk too — it's the defensive
    // stub the rhai tier hits if any tool ever leaks down.
    assert!(staged.root().join("main.rhai").exists());
    // The read-receipts wrapper script must ship in the zip — the
    // sidecar mount points at it via stage://.
    assert!(staged.root().join("enable-read-receipts.sh").exists());
}
