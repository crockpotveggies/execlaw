//! Sanity-check that the shipped `plugins/finance-yahoo/plugin.toml`
//! parses through the SDK's manifest reader without errors.
//!
//! Mirrors `open_meteo_manifest.rs`. Catches typo'd field names,
//! mistyped enums, or missing required fields that wouldn't trip the
//! Rhai parser but would block install-time hook registration.

use execlaw_plugin_sdk::PluginManifest;
use std::path::PathBuf;

#[test]
fn finance_yahoo_manifest_parses_with_expected_tools() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/finance-yahoo/plugin.toml");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let manifest: PluginManifest =
        toml::from_str(&source).unwrap_or_else(|e| panic!("plugin.toml must parse: {e}"));

    assert_eq!(manifest.plugin.id, "finance-yahoo");
    let tool_names: Vec<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    let want = [
        "yahoo_finance.quote",
        "yahoo_finance.index_quote",
        "yahoo_finance.crypto_quote",
        "yahoo_finance.fx_quote",
        "yahoo_finance.historical_candles",
        "yahoo_finance.symbol_search",
        "yahoo_finance.market_news",
        "yahoo_finance.quote_summary",
        "yahoo_finance.health",
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

    // Trust floor must be KnownLimited on every tool — read-only data
    // is safe for any known principal, same posture as Open-Meteo.
    for t in &manifest.tools {
        assert_eq!(
            t.trust_floor.as_deref(),
            Some("KnownLimited"),
            "tool {} must declare trust_floor=KnownLimited, got {:?}",
            t.name,
            t.trust_floor
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
