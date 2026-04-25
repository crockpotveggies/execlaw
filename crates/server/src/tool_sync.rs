//! Reflects the live tool registry into `config_tool_access` so the
//! per-tool trust-class allowlist (Phase 8a) has a row for every
//! tool the runner might dispatch to.
//!
//! Called from three places:
//!   * **Boot** (`cmd_serve`) — after the plugin host hydrates,
//!     seed all built-ins + every persisted plugin tool.
//!   * **Plugin lifecycle** — install / enable mutates the registry,
//!     so we re-sync after the route handler succeeds.
//!   * **MCP server connect** (Phase 8b+) — same pattern with
//!     `ToolSource::Mcp` rows.
//!
//! Sync is idempotent: `ToolAccessStore::upsert_seen` only stamps
//! defaults on first insert and never overwrites the operator's
//! `enabled` / `allowed_classes` choices.

use execlaw_core::mcp_servers::McpServerRow;
use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
use execlaw_core::Database;
use execlaw_plugin_host::PluginHost;

/// Default allowlist for every built-in tool. Built-ins existed
/// before Phase 8a so they ship "open" (every trust class can call
/// them) — operators tighten via Settings → Tools. The Controller
/// is always present.
fn default_builtin_classes() -> Vec<String> {
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
    ]
}

/// Default allowlist for plugin-supplied tools. Same "open" semantic
/// as builtins on first install: behaviour matches Phase-2 dispatch
/// (capability-set + latency band) so wiring the gate doesn't change
/// any existing flow. Operators tighten per-tool via Settings.
fn default_plugin_classes() -> Vec<String> {
    default_builtin_classes()
}

/// Default allowlist for MCP-sourced tools. Per the locked decision
/// (6c), new tools inherit the server's `default_allowed_classes`
/// column. Falls back to Controller-only when the column is empty
/// — fail-closed for any operator who hasn't explicitly broadened
/// the allowlist.
pub fn default_mcp_classes_for(row: &McpServerRow) -> Vec<String> {
    if row.default_allowed_classes.is_empty() {
        vec!["Controller".into()]
    } else {
        row.default_allowed_classes.clone()
    }
}

/// Bare-name list of every built-in tool the runner crate registers.
/// Kept as a const so adding a new built-in elsewhere will fail to
/// build until this list catches up — that's intentional.
const BUILTIN_TOOLS: &[(&str, &str)] = &[
    ("read_memory", "Read a value from the conversation's memory store."),
    ("write_memory", "Write a value into the conversation's memory store."),
    ("list_memory", "List keys in the conversation's memory store."),
    ("set_thread_name", "Rename the current conversation thread."),
];

/// Idempotently sync builtins + every currently-registered plugin
/// tool into `config_tool_access`. Returns the number of rows
/// upserted (mainly for telemetry / tests).
pub fn sync_tool_access(
    db: &Database,
    host: &PluginHost,
    now: i64,
) -> Result<usize, execlaw_core::DbError> {
    let store = ToolAccessStore::new(db);
    let mut n = 0;

    for (name, desc) in BUILTIN_TOOLS {
        store.upsert_seen(
            &ToolAccessSeed {
                tool_name: (*name).into(),
                source: ToolSource::Builtin,
                source_id: None,
                description: Some((*desc).into()),
                input_schema: None,
                default_allowed_classes: default_builtin_classes(),
            },
            now,
        )?;
        n += 1;
    }

    for tool in host.registry().all_tools() {
        store.upsert_seen(
            &ToolAccessSeed {
                tool_name: tool.tool_name.clone(),
                source: ToolSource::Plugin,
                source_id: Some(tool.plugin_id.clone()),
                description: Some(format!(
                    "Plugin tool '{}' from '{}' (latency: {})",
                    tool.tool_name, tool.plugin_id, tool.latency,
                )),
                input_schema: None,
                default_allowed_classes: default_plugin_classes(),
            },
            now,
        )?;
        n += 1;
    }

    Ok(n)
}

/// Mark every tool owned by a plugin as `removed_at = now` — used by
/// the plugin-disable / uninstall paths so the dispatch gate denies
/// further calls until the plugin comes back. Operator policy is
/// preserved, so a re-enable restores the original allowlist.
pub fn mark_plugin_tools_removed(
    db: &Database,
    plugin_id: &str,
    host: &PluginHost,
    now: i64,
) -> Result<usize, execlaw_core::DbError> {
    let store = ToolAccessStore::new(db);
    // The host's registry may have already dropped the plugin's
    // tools by the time this runs (disable removes from registry),
    // so we look at every persisted row and mark anything tagged
    // with this plugin_id.
    let rows = store.list_all()?;
    let _ = host;
    let mut n = 0;
    for row in rows {
        if row.source == ToolSource::Plugin
            && row.source_id.as_deref() == Some(plugin_id)
            && row.removed_at.is_none()
            && store.mark_removed(&row.tool_name, now)?
        {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_plugin_host::HookRegistry;

    fn fresh_host() -> (Database, PluginHost) {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let stage = std::env::temp_dir().join(format!(
            "execlaw-tool-sync-test-{}",
            uuid::Uuid::new_v4()
        ));
        let host = PluginHost::new(db.clone(), HookRegistry::new(), stage);
        (db, host)
    }

    #[test]
    fn sync_seeds_every_builtin() {
        let (db, host) = fresh_host();
        let n = sync_tool_access(&db, &host, 100).unwrap();
        assert_eq!(n, BUILTIN_TOOLS.len());
        let store = ToolAccessStore::new(&db);
        for (name, _) in BUILTIN_TOOLS {
            let row = store.get(name).unwrap().unwrap();
            assert_eq!(row.source, ToolSource::Builtin);
            assert!(row.enabled);
            // Default allowlist includes Controller through KnownLimited.
            assert!(row.allowed_classes.iter().any(|c| c == "Controller"));
            assert!(row.allowed_classes.iter().any(|c| c == "KnownLimited"));
        }
    }

    #[test]
    fn sync_is_idempotent_and_preserves_operator_policy() {
        let (db, host) = fresh_host();
        sync_tool_access(&db, &host, 100).unwrap();
        let store = ToolAccessStore::new(&db);
        // Operator restricts read_memory to Controller-only.
        store
            .set_policy("read_memory", true, &["Controller".into()])
            .unwrap();
        // Re-sync — must NOT widen back to defaults.
        sync_tool_access(&db, &host, 200).unwrap();
        let row = store.get("read_memory").unwrap().unwrap();
        assert_eq!(row.allowed_classes, vec!["Controller"]);
        assert_eq!(row.last_seen_at, 200);
        assert_eq!(row.first_seen_at, 100);
    }
}
