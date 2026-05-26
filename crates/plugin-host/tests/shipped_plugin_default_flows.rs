//! Phase E (Flows middleware) — integration test: every shipped
//! plugin under `plugins/<id>/` whose manifest declares
//! `[[default_automations]]` must:
//!
//!   1. Parse the manifest cleanly through the SDK reader.
//!   2. Have each declared `flow_path` exist on disk.
//!   3. Parse the flow JSON into an `AutomationDef`.
//!   4. Pass `execlaw_core::automations::validate()`.
//!
//! Catches typos in flow JSONs and stale manifest references BEFORE
//! we build zips + reinstall — install would only WARN-log a bad
//! flow and silently continue, which would hide the bug behind a
//! "tests pass / nothing in DB" surface.

use execlaw_core::automations::{AutomationDef, validate};
use execlaw_plugin_sdk::PluginManifest;
use std::path::PathBuf;

fn workspace_plugins_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p.push("plugins");
    p
}

#[test]
fn every_shipped_default_flow_parses_and_validates() {
    let plugins = workspace_plugins_dir();
    let entries = std::fs::read_dir(&plugins)
        .unwrap_or_else(|e| panic!("could not list {plugins:?}: {e}"));

    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if name == "_shared" {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("could not read {manifest_path:?}: {e}"));
        let manifest: PluginManifest = match PluginManifest::parse(&raw) {
            Ok(m) => m,
            Err(e) => {
                failures.push(format!("[{name}] manifest parse failed: {e}"));
                continue;
            }
        };

        for d in &manifest.default_automations {
            checked += 1;
            let flow_path = path.join(&d.flow_path);
            if !flow_path.is_file() {
                failures.push(format!(
                    "[{name}] default_automations[{}].flow_path = '{}' \
                     does not exist on disk (looked at {:?})",
                    d.name, d.flow_path, flow_path
                ));
                continue;
            }
            let flow_raw = match std::fs::read_to_string(&flow_path) {
                Ok(s) => s,
                Err(e) => {
                    failures.push(format!(
                        "[{name}] reading {flow_path:?} failed: {e}"
                    ));
                    continue;
                }
            };
            let def: AutomationDef = match serde_json::from_str(&flow_raw) {
                Ok(d) => d,
                Err(e) => {
                    failures.push(format!(
                        "[{name}] flow {flow_path:?} did not parse as AutomationDef: {e}"
                    ));
                    continue;
                }
            };
            if let Err(e) = validate(&def) {
                failures.push(format!(
                    "[{name}] flow {flow_path:?} failed validator: {e}"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} shipped default-flow check(s) failed (of {checked} checked):\n  - {}",
        failures.len(),
        failures.join("\n  - "),
    );
    assert!(
        checked > 0,
        "expected at least one plugin to declare [[default_automations]]; \
         found 0 — did the manifest sections get dropped?"
    );
}
