//! Plugin-lifecycle orchestration.
//!
//! `PluginHost` owns the runtime + DB row for each plugin. `SidecarSupervisor`
//! owns Docker containers + on-disk state dirs. Per-plugin DB stores
//! (OAuth, vault, artifacts) live in `execlaw_core`. This module is the
//! single coordinator that knows about all three and chains them in the
//! one ordering that produces a true "clean slate" when a plugin is
//! removed.
//!
//! ## Why this lives in `server` and not `plugin-host`
//!
//! `PluginHost` is layered below `server` in the crate graph and cannot
//! depend on `SidecarSupervisor` (which lives in `server`). Inverting
//! that dependency via a trait was an option, but the orchestration
//! itself is server-flavored (it touches `AppState`, fires UI events,
//! shapes an HTTP response body) so the natural home is here.
//!
//! ## Ordering (load-bearing)
//!
//!   1. `PluginHost::disable(plugin_id)` — fires the plugin's optional
//!      `on_disable` Rhai hook *while the plugin still has access to
//!      its OAuth tokens, vault secrets, and transport bindings*. This
//!      lets a well-behaved plugin send a "going offline" notification,
//!      revoke an upstream OAuth grant, post a Signal "device removed"
//!      farewell, etc. **Order matters**: if we delete the OAuth row
//!      first, the hook can't revoke the upstream token; if we wipe
//!      the sidecar first, the hook can't send a final message on its
//!      transport. So this is step #1 by design.
//!   2. `SidecarSupervisor::remove_for_plugin(plugin_id)` — stop + remove
//!      every docker container the plugin owns AND `rm -rf` its
//!      per-plugin state root (`~/.execlaw/sidecars/<plugin_id>/`).
//!      The state-dir delete is the gap that earlier `stop_all` left
//!      behind: a re-install would silently inherit signal-cli's
//!      keystore, wuzapi's session DB, etc.
//!   3. `OauthTokenStore::delete_for_plugin` then
//!      `OauthClientStore::delete_for_plugin` — wipe the plugin's
//!      stored OAuth grants. Tokens cascade-delete from clients via
//!      the FK, but we run both deletes explicitly so the report
//!      surfaces an accurate per-table count and so manually
//!      modified DBs (older migration data, ops surgery) don't leave
//!      orphan token rows.
//!   4. `AttachmentStore::purge_artifacts_for_plugin` — delete every
//!      `state_artifacts` row owned by the plugin and (refcount-aware)
//!      unlink the underlying blob files on disk. Without this,
//!      `~/.execlaw/plugin_artifacts/<sha>` blobs survive uninstall
//!      and slowly leak disk.
//!   5. `VaultRowStore::delete_for_plugin` — drop the plugin's
//!      `vault_secrets` rows. Core-scope rows (`plugin_id IS NULL`)
//!      are not touched.
//!   6. `PluginHost::uninstall(plugin_id)` — archives plugin-shipped
//!      skills, deletes the `state_plugins` row, and removes the
//!      staged plugin directory at `state_plugins.stage_path`. This
//!      is the final step because steps 1-5 may need to look the
//!      plugin up by `plugin_id` against still-present rows.
//!
//! ## Two callers
//!
//!   * **SPA uninstall** (`DELETE /api/admin/plugins/{id}`) calls
//!     `purge_plugin` directly. One plugin's resources gone, the rest
//!     of the system untouched.
//!   * **Factory reset** (`POST /api/admin/factory-reset`) enumerates
//!     every installed plugin, calls `purge_plugin` for each, then
//!     blows away the entire DB file. The per-plugin purges are
//!     technically redundant for DB-side state (which is about to be
//!     nuked) but are NOT redundant for Docker containers + on-disk
//!     state dirs, which the DB nuke can't reach.
//!
//! ## Failure mode
//!
//! Best-effort. Each step is wrapped to log the error and continue.
//! A failed `docker stop` does not block the OAuth-row delete; a
//! failed `rm -rf` of the state dir does not block the `state_plugins`
//! delete. The returned report records what actually happened so the
//! operator can spot a partial teardown and decide whether to retry,
//! `docker rm` by hand, or accept the residue.

use crate::sidecar_supervisor::SidecarRemovalReport;
use crate::state::AppState;
use execlaw_core::attachments::AttachmentStore;
use execlaw_core::oauth::{OauthClientStore, OauthTokenStore};
use execlaw_core::vault_row::VaultRowStore;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Per-plugin teardown report — one entry per `purge_plugin` call.
/// Surfaced verbatim in HTTP responses (SPA uninstall returns a
/// single report; factory reset returns a Vec<PluginPurgeReport>).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PluginPurgeReport {
    pub plugin_id: String,
    /// `true` when `PluginHost::disable` was called (it always is,
    /// but a fresh-install or already-disabled plugin's disable is a
    /// no-op — the field still flips to `true` for caller visibility).
    pub disabled: bool,
    /// Sidecar containers stopped + removed and state dir cleanup.
    /// Present iff the plugin had any registered sidecars at the
    /// moment of purge; absent for tool-only plugins.
    pub sidecars: Option<SidecarRemovalReport>,
    /// Count of `state_oauth_clients` rows removed. Tokens cascade
    /// from this delete via FK, so the operator's
    /// "Connected accounts" view drops the plugin's entries.
    pub oauth_clients_removed: usize,
    /// Count of `state_oauth_tokens` rows removed *explicitly*. Under
    /// normal flows this equals 0 because the client-delete cascade
    /// already handled them; older / hand-edited DBs may surface a
    /// non-zero count.
    pub oauth_tokens_removed: usize,
    /// Count of `state_artifacts` rows removed. Underlying on-disk
    /// blobs are unlinked when their refcount hits zero (other
    /// plugins' identical-bytes artifacts keep the blob alive).
    pub artifacts_removed: usize,
    /// Count of `vault_secrets` rows removed (plugin-scope only —
    /// core-scope rows are never touched).
    pub vault_rows_removed: usize,
    /// `true` when `PluginHost::uninstall` returned Ok; `false` when
    /// the row was missing (plugin already uninstalled — idempotent
    /// success) OR the call failed (which the `errors` field will
    /// have one entry for).
    pub uninstalled: bool,
    /// Human-readable error messages from any step that failed. Empty
    /// vec is the happy path. Non-empty does NOT mean the purge
    /// aborted — best-effort semantics ensure later steps still ran.
    pub errors: Vec<String>,
}

/// Purge a single plugin's resources end-to-end. See the module-level
/// docstring for the load-bearing ordering and the rationale behind
/// each step.
///
/// Idempotent. Calling twice for the same `plugin_id` is safe — the
/// second call sees no DB rows, no slots, no state dir, and returns
/// `disabled = true, uninstalled = false, errors = []`.
pub async fn purge_plugin(state: &AppState, plugin_id: &str) -> PluginPurgeReport {
    let mut errors: Vec<String> = Vec::new();

    // Step 1 — fire on_disable and tear down hooks/runtime. The
    // plugin's transport bindings + OAuth tokens + vault secrets are
    // all still readable at this point so the hook can do real work.
    if let Err(e) = state.plugin_host.disable(plugin_id).await {
        // `NotInstalled` is the idempotent path — treat it as "nothing
        // to disable" and continue. Anything else is a real error.
        let msg = e.to_string();
        if !msg.contains("not installed") && !msg.contains("NotInstalled") {
            errors.push(format!("disable: {msg}"));
        }
    }

    // Step 2 — sidecar containers + state dirs. Only meaningful when
    // the host actually has a SidecarSupervisor wired (tests + dev
    // builds without docker skip this and return None).
    let sidecars: Option<SidecarRemovalReport> = match &state.sidecar_supervisor {
        Some(sup) => Some(sup.remove_for_plugin(plugin_id).await),
        None => None,
    };

    // Step 3 — OAuth tokens, then clients. Tokens cascade from
    // clients, so the explicit token delete is a defensive
    // safety-net for older DBs / manual edits.
    let oauth_tokens_removed = OauthTokenStore::new(&state.db)
        .delete_for_plugin(plugin_id)
        .map_err(|e| errors.push(format!("oauth_tokens: {e}")))
        .unwrap_or(0);
    let oauth_clients_removed = OauthClientStore::new(&state.db)
        .delete_for_plugin(plugin_id)
        .map_err(|e| errors.push(format!("oauth_clients: {e}")))
        .unwrap_or(0);

    // Step 4 — artifacts (rows + refcount-aware blobs).
    let artifacts_removed = AttachmentStore::new(&state.db)
        .purge_artifacts_for_plugin(plugin_id)
        .map_err(|e| errors.push(format!("artifacts: {e}")))
        .unwrap_or(0);

    // Step 5 — vault rows for this plugin.
    let vault_rows_removed = VaultRowStore::new(&state.db)
        .delete_for_plugin(plugin_id)
        .map_err(|e| errors.push(format!("vault: {e}")))
        .unwrap_or(0);

    // Step 6 — final uninstall (archive skills, delete state_plugins
    // row, rm stage dir). After this returns Ok, the plugin is
    // completely gone from the host's perspective.
    let uninstalled = match state.plugin_host.uninstall(plugin_id).await {
        Ok(()) => {
            // Mirror the SPA uninstall_handler's tool-sync step so
            // every tool the plugin contributed gets `removed_at`
            // set and the dispatch gate stops accepting calls.
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = crate::tool_sync::mark_plugin_tools_removed(
                &state.db,
                plugin_id,
                &state.plugin_host,
                now,
            ) {
                errors.push(format!("mark_plugin_tools_removed: {e}"));
            }
            true
        }
        Err(e) => {
            let msg = e.to_string();
            // `NotInstalled` here is the idempotent path — plugin
            // was already gone (or step 1 found nothing to disable).
            // Don't surface it as an error.
            if msg.contains("not installed") || msg.contains("NotInstalled") {
                false
            } else {
                errors.push(format!("uninstall: {msg}"));
                false
            }
        }
    };

    PluginPurgeReport {
        plugin_id: plugin_id.to_owned(),
        disabled: true,
        sidecars,
        oauth_clients_removed,
        oauth_tokens_removed,
        artifacts_removed,
        vault_rows_removed,
        uninstalled,
        errors,
    }
}

/// Purge every plugin known to `PluginHost` at the moment of call.
/// Used by factory reset to enumerate-then-purge before nuking the
/// DB. Returns one report per plugin. Plugins are processed in the
/// order `PluginHost::list_rows` returns (alphabetic by `plugin_id`
/// per the current sqlite query plan); ordering doesn't affect
/// correctness — every plugin's resources are independent of every
/// other's.
pub async fn purge_all_plugins(state: &AppState) -> Vec<PluginPurgeReport> {
    let rows = match state.plugin_host.list_rows() {
        Ok(r) => r,
        Err(e) => {
            // We can't enumerate plugins — return a single
            // synthetic report so the caller still has something to
            // surface in the HTTP response. The DB wipe that
            // typically follows will still happen and will clean
            // everything DB-side; only sidecar containers + on-disk
            // state dirs would survive a list-failure here.
            tracing::warn!(
                target: "plugin_lifecycle",
                error = %e,
                "purge_all_plugins: plugin enumeration failed — DB wipe will still cover DB-side state",
            );
            return vec![PluginPurgeReport {
                plugin_id: "<enumeration_failed>".into(),
                disabled: false,
                sidecars: None,
                oauth_clients_removed: 0,
                oauth_tokens_removed: 0,
                artifacts_removed: 0,
                vault_rows_removed: 0,
                uninstalled: false,
                errors: vec![format!("list_rows: {e}")],
            }];
        }
    };

    let mut reports = Vec::with_capacity(rows.len());
    for row in rows {
        let report = purge_plugin(state, &row.plugin_id).await;
        tracing::info!(
            target: "plugin_lifecycle",
            plugin_id = %row.plugin_id,
            containers_removed = report.sidecars.as_ref().map(|s| s.containers_removed).unwrap_or(0),
            artifacts_removed = report.artifacts_removed,
            vault_rows_removed = report.vault_rows_removed,
            oauth_clients_removed = report.oauth_clients_removed,
            errors = report.errors.len(),
            "plugin purged",
        );
        reports.push(report);
    }
    reports
}
