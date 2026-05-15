//! Smoke test for the operator-installable Yahoo Finance plugin zip
//! at `dist/finance-yahoo-<version>.zip`. Round-trips the file through
//! `stage_zip` so a packaging mistake (wrong layout, missing file,
//! manifest typo) surfaces at `cargo test` time instead of at the
//! operator's first install attempt.

use execlaw_plugin_sdk::zip_stage::stage_zip;
use std::fs::File;
use std::io::BufReader;

#[test]
fn dist_finance_yahoo_zip_stages_cleanly() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let zip_path = workspace_root
        .join("dist")
        .join("finance-yahoo-0.1.0.zip");
    if !zip_path.exists() {
        // Fresh clones / CI may not have the zip yet — it's a release
        // artifact rebuilt by the packaging step, not a checked-in
        // dev requirement.
        eprintln!(
            "dist/finance-yahoo-0.1.0.zip not present at {}; skipping",
            zip_path.display()
        );
        return;
    }
    let f = File::open(&zip_path).expect("open dist zip");
    let staged = stage_zip(BufReader::new(f)).expect("stage_zip must accept the dist zip");

    assert_eq!(staged.manifest.plugin.id, "finance-yahoo");
    assert_eq!(staged.manifest.plugin.version, "0.1.0");

    // Pin the v0.1 tool surface — a regression that drops or renames
    // one of these breaks the agent's grounded tool definitions.
    let tool_names: Vec<&str> = staged
        .manifest
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    for want in [
        "yahoo_finance.quote",
        "yahoo_finance.index_quote",
        "yahoo_finance.crypto_quote",
        "yahoo_finance.fx_quote",
        "yahoo_finance.historical_candles",
        "yahoo_finance.symbol_search",
        "yahoo_finance.market_news",
        "yahoo_finance.quote_summary",
        "yahoo_finance.health",
    ] {
        assert!(
            tool_names.contains(&want),
            "manifest missing tool `{want}`; got {tool_names:?}"
        );
    }
    assert_eq!(staged.manifest.tools.len(), 9);

    // All tools must declare trust_floor=KnownLimited — operator's
    // standing rule for read-only data plugins.
    for t in &staged.manifest.tools {
        assert_eq!(
            t.trust_floor.as_deref(),
            Some("KnownLimited"),
            "tool {} trust_floor must be KnownLimited",
            t.name
        );
    }

    // Admin routes: GET /config, POST /config, POST /test.
    let routes: Vec<(String, String)> = staged
        .manifest
        .admin_routes
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    assert!(routes.contains(&("GET".into(), "/config".into())));
    assert!(routes.contains(&("POST".into(), "/config".into())));
    assert!(routes.contains(&("POST".into(), "/test".into())));

    // Files the plugin needs at runtime must be on disk after staging.
    assert!(staged.root().join("main.rhai").exists());
    assert!(staged.root().join("ui/panel.js").exists());
    for schema in [
        "quote.json",
        "index_quote.json",
        "crypto_quote.json",
        "fx_quote.json",
        "historical_candles.json",
        "symbol_search.json",
        "market_news.json",
        "quote_summary.json",
        "health.json",
    ] {
        let p = staged.root().join("schemas").join(schema);
        assert!(p.exists(), "schemas/{schema} missing in staged zip");
    }
}
