//! Companion to `python_sandbox_manifest.rs` — verifies that every
//! `schema = "..."` path the manifest declares actually exists on
//! disk AND parses as a JSON Schema 2020-12 document (the version
//! the rest of execlaw's JSON-Schema validation pipeline targets).
//!
//! Catches the failure mode where the manifest says
//! `schema = "schemas/python.execute.json"` but the file is missing,
//! malformed JSON, or uses a different draft URI — any of which
//! would make `HookRegistry::enable_with_stage` fail at install time
//! with a much less obvious error than this test produces.

use execlaw_plugin_sdk::PluginManifest;
use serde_json::Value;
use std::path::PathBuf;

#[test]
fn every_python_sandbox_tool_schema_exists_and_parses() {
    let workspace = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    };
    let plugin_root = workspace.join("plugins/python-sandbox");
    let manifest_src = std::fs::read_to_string(plugin_root.join("plugin.toml"))
        .expect("plugin.toml must be readable");
    let manifest: PluginManifest =
        toml::from_str(&manifest_src).expect("plugin.toml must parse");

    // Every tool declares a schema path; each one must exist + parse.
    for tool in &manifest.tools {
        let rel = tool
            .schema
            .as_ref()
            .unwrap_or_else(|| panic!("tool {} declares no schema path", tool.name));
        let path = plugin_root.join(rel);
        assert!(
            path.exists(),
            "tool {} declares schema {} which does not exist on disk at {}",
            tool.name,
            rel,
            path.display()
        );
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("schema {} not readable: {e}", path.display()));
        let json: Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("schema {} is not valid JSON: {e}", path.display()));

        // Every schema we ship targets the same draft so the
        // validator's behavior is consistent across tools.
        let draft = json
            .get("$schema")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "schema {} is missing a top-level `$schema` URI",
                    path.display()
                )
            });
        assert_eq!(
            draft, "https://json-schema.org/draft/2020-12/schema",
            "schema {} targets draft {}; execlaw standardizes on draft/2020-12",
            path.display(),
            draft
        );

        // All four tools are object-typed at the args level; required
        // is allowed to be empty for the no-arg tools (reset / interrupt
        // / list_files) but the field must still be `additionalProperties: false`
        // so a malformed extra arg fails fast rather than silently.
        assert_eq!(
            json.get("type").and_then(Value::as_str),
            Some("object"),
            "schema {} must declare type=object",
            path.display()
        );
        assert_eq!(
            json.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "schema {} must set additionalProperties=false (defense against typo'd arg keys)",
            path.display()
        );
    }
}

#[test]
fn python_execute_schema_enforces_realistic_limits() {
    let workspace = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p
    };
    let schema_path = workspace.join("plugins/python-sandbox/schemas/python.execute.json");
    let body = std::fs::read_to_string(&schema_path).expect("python.execute.json readable");
    let json: Value = serde_json::from_str(&body).expect("python.execute.json valid JSON");

    let props = json.get("properties").expect("properties present");

    // `code` is required and capped — defends against an agent
    // accidentally streaming a 5 MB pile of code into the kernel.
    let required = json
        .get("required")
        .and_then(Value::as_array)
        .expect("required[]");
    assert!(
        required.iter().any(|v| v.as_str() == Some("code")),
        "code must be required"
    );
    let code = props.get("code").expect("code property");
    let max = code.get("maxLength").and_then(Value::as_u64);
    assert!(
        matches!(max, Some(n) if n <= 1_000_000),
        "code.maxLength must be set + reasonable; got {max:?}"
    );

    // `timeout_ms` has a hard ceiling so the agent can't pin a kernel
    // indefinitely. 10 minutes is the policy.
    let timeout = props.get("timeout_ms").expect("timeout_ms property");
    let timeout_max = timeout.get("maximum").and_then(Value::as_u64);
    assert_eq!(
        timeout_max,
        Some(600_000),
        "timeout_ms.maximum must be exactly 600000 (10 min hard cap); got {timeout_max:?}"
    );
}
