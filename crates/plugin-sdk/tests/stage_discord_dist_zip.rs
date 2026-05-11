//! Smoke test for the operator-installable Discord plugin zip at
//! `dist/discord-<version>.zip`. Round-trips the file through
//! `stage_zip` so a packaging mistake (wrong layout, missing
//! file, manifest typo) surfaces at `cargo test` time instead of
//! at the operator's first install attempt.
//!
//! Mirrors `stage_signal_dist_zip.rs` — same shape, Discord-specific
//! invariants.

use execlaw_plugin_sdk::zip_stage::stage_zip;
use std::fs::File;
use std::io::BufReader;

#[test]
fn dist_discord_zip_stages_cleanly() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let zip_path = workspace_root.join("dist").join("discord-0.1.0.zip");
    if !zip_path.exists() {
        // Allow CI / fresh clones to skip. The zip is rebuilt
        // by the operator's packaging step before publishing —
        // not regenerated automatically on every `cargo build`.
        eprintln!(
            "dist/discord-0.1.0.zip not present at {}; skipping",
            zip_path.display()
        );
        return;
    }
    let f = File::open(&zip_path).expect("open dist zip");
    let staged = stage_zip(BufReader::new(f)).expect("stage_zip must accept the dist zip");

    assert_eq!(staged.manifest.plugin.id, "discord");
    assert_eq!(staged.manifest.plugin.version, "0.1.0");

    // Script-tier: every tool lives in main.rhai, none host-implemented.
    assert!(
        staged.manifest.tools.iter().all(|t| !t.host_implemented),
        "every discord tool must be script-tier in v0.1 (host_implemented = false)"
    );
    // 3 agent-callable + 3 host-internal convention tools.
    assert_eq!(staged.manifest.tools.len(), 6);

    // Admin routes — status / GET config / POST config / POST test.
    assert_eq!(staged.manifest.admin_routes.len(), 4);

    // No sidecar — Discord's gateway is public WSS, hit directly
    // via ws_subscribe_bidi + ws_set_keepalive.
    assert!(
        staged.manifest.services.is_empty(),
        "discord plugin must not declare any [[services]] in v0.1"
    );

    // Transport shape.
    let tr = staged
        .manifest
        .transport
        .as_ref()
        .expect("[transport] must be declared");
    assert_eq!(tr.transport_id, "discord");
    assert!(tr.supports_groups);
    assert!(tr.supports_attachments);

    // main.rhai must be present on disk — script-tier dispatch
    // requires it.
    assert!(staged.root().join("main.rhai").exists());
    // Schema for send_message must ship — the tool dispatcher
    // validates against it before invoking tool_call.
    assert!(
        staged
            .root()
            .join("schemas")
            .join("discord.send_message.json")
            .exists()
    );
}
