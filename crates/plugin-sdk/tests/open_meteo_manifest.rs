//! Sanity-check that the shipped `plugins/open-meteo/plugin.toml`
//! parses through the SDK's manifest reader without errors.
//!
//! Catches a class of issues (typo'd field name, mistyped enum,
//! missing required field) that wouldn't fail the Rhai parser but
//! would block the install-time hook registration. Same convention
//! used for `dist_signal_zip_stages_cleanly`.

use execlaw_plugin_sdk::PluginManifest;
use std::path::PathBuf;

#[test]
fn open_meteo_manifest_parses_with_expected_tools() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/open-meteo/plugin.toml");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let manifest: PluginManifest = toml::from_str(&source)
        .unwrap_or_else(|e| panic!("plugin.toml must parse: {e}"));

    assert_eq!(manifest.plugin.id, "open-meteo");
    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    let want = [
        "open_meteo.geocode",
        "open_meteo.elevation",
        "open_meteo.forecast",
        "open_meteo.historical",
        "open_meteo.marine",
        "open_meteo.air_quality",
        "open_meteo.seasonal",
        "open_meteo.ensemble",
        "open_meteo.flood",
        "open_meteo.climate",
        "open_meteo.render_chart",
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

    // Trust floor must be KnownLimited on every tool — operator's
    // standing rule for this slice.
    for t in &manifest.tools {
        assert_eq!(
            t.trust_floor.as_deref(),
            Some("KnownLimited"),
            "tool {} must declare trust_floor=KnownLimited, got {:?}",
            t.name,
            t.trust_floor
        );
    }

    // Three chat-component kinds wired up.
    let kinds: Vec<&str> = manifest
        .chat_components
        .iter()
        .map(|c| c.kind.as_str())
        .collect();
    for k in ["weather_current", "weather_daily", "chart"] {
        assert!(
            kinds.contains(&k),
            "manifest missing chat_component kind `{k}`; got {kinds:?}"
        );
    }

    // Admin routes: GET /config, POST /config, POST /test.
    let routes: Vec<(String, String)> = manifest
        .admin_routes
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    assert!(routes.contains(&("GET".into(), "/config".into())));
    assert!(routes.contains(&("POST".into(), "/config".into())));
    assert!(routes.contains(&("POST".into(), "/test".into())));

    // Script runtime, source file present.
    let rt = manifest.runtime.expect("runtime required for tool plugin");
    assert_eq!(rt.tier.as_str(), "script");
}
