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

use execlaw_core::Database;
use execlaw_core::mcp_servers::McpServerRow;
use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
use execlaw_plugin_host::PluginHost;

/// Default allowlist for plugin-supplied tools. Plugins ship "open"
/// on first install (every trust class can call them) — operators
/// tighten per-tool via Settings. Built-ins now carry their own
/// `default_allowed_classes` on the descriptor so they no longer
/// need a hard-coded helper here.
fn default_plugin_classes() -> Vec<String> {
    vec![
        "Controller".into(),
        "Delegated".into(),
        "KnownTrusted".into(),
        "KnownLimited".into(),
    ]
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

/// Idempotently sync builtins + every currently-registered plugin
/// tool into `config_tool_access`. Returns the number of rows
/// upserted (mainly for telemetry / tests).
///
/// Built-ins are pulled live from the registry's `all_builtins()`
/// rather than a hard-coded list — the descriptor on each tool is
/// the single source of truth for name, description, schema, and
/// default trust-class allowlist. Adding a new built-in is one
/// `register_builtin` call and the row appears here automatically.
pub fn sync_tool_access(
    db: &Database,
    host: &PluginHost,
    now: i64,
) -> Result<usize, execlaw_core::DbError> {
    let store = ToolAccessStore::new(db);
    let mut n = 0;

    for tool in host.registry().all_builtins() {
        let d = tool.descriptor();
        store.upsert_seen(
            &ToolAccessSeed {
                tool_name: d.name.clone(),
                source: ToolSource::Builtin,
                source_id: None,
                description: Some(d.description.clone()),
                input_schema: Some(d.schema.to_string()),
                default_allowed_classes: d.default_allowed_classes.clone(),
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

    /// Helper that returns a fresh DB + plugin host with the core
    /// trait-based built-ins already registered. `register_now`
    /// becomes the `first_seen_at` value the registrar stamps; tests
    /// that exercise idempotency pass a marker timestamp here so
    /// follow-on sync calls can prove they preserved it.
    fn fresh_host_with_core_builtins(register_now: i64) -> (Database, PluginHost, usize) {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let stage =
            std::env::temp_dir().join(format!("execlaw-tool-sync-test-{}", uuid::Uuid::new_v4()));
        let host = PluginHost::new(db.clone(), HookRegistry::new(), stage);
        let landed =
            execlaw_plugin_host::register_core_builtins(host.registry(), &db, register_now)
                .unwrap();
        let count = landed.len();
        (db, host, count)
    }

    #[test]
    fn sync_seeds_every_builtin_from_registry() {
        let (db, host, expected) = fresh_host_with_core_builtins(100);
        let n = sync_tool_access(&db, &host, 100).unwrap();
        assert_eq!(n, expected);
        let store = ToolAccessStore::new(&db);
        for tool in host.registry().all_builtins() {
            let row = store.get(&tool.descriptor().name).unwrap().unwrap();
            assert_eq!(row.source, ToolSource::Builtin);
            assert!(row.enabled);
            assert!(!row.allowed_classes.is_empty());
            assert!(row.allowed_classes.iter().any(|c| c == "Controller"));
        }
    }

    #[test]
    fn sync_is_idempotent_and_preserves_operator_policy() {
        let (db, host, _) = fresh_host_with_core_builtins(100);
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

    /// New built-ins added to the core catalog (via
    /// `register_core_builtins`) appear in the sync output without a
    /// code change here — single source of truth verified.
    #[test]
    fn sync_picks_up_every_descriptor_field_from_registry() {
        let (db, host, _) = fresh_host_with_core_builtins(100);
        sync_tool_access(&db, &host, 100).unwrap();
        let store = ToolAccessStore::new(&db);
        // Spot-check that the descriptor's input_schema landed —
        // proving the new path is using the descriptor, not a stub.
        let row = store.get("read_memory").unwrap().unwrap();
        assert!(row.input_schema.is_some());
        let schema_str = row.input_schema.as_ref().unwrap();
        assert!(schema_str.contains("scope") && schema_str.contains("key"));
    }
}
