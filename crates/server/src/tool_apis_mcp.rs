//! Concrete `McpAdminApi` implementation. Lives in the server
//! crate (not core) because it needs both `McpServerStore` (core)
//! AND `McpHost::reconcile()` (server) to make a freshly-added
//! row actually connect.

use crate::mcp_host::{McpHost, MCP_TOOL_PREFIX};
use execlaw_core::Database;
use execlaw_core::mcp_servers::{
    McpServerInsert, McpServerRow, McpServerStore, McpTransport,
};
use execlaw_core::tool::{ApiError, McpAdminApi, McpServerSpec, McpServerView};
use execlaw_core::tool_access::{ToolAccessStore, ToolSource};
use execlaw_core::vault_row::VaultRowStore;

#[derive(Clone)]
pub struct DbMcpAdminApi {
    db: Database,
    host: McpHost,
}

impl DbMcpAdminApi {
    pub fn new(db: Database, host: McpHost) -> Self {
        Self { db, host }
    }
}

#[async_trait::async_trait]
impl McpAdminApi for DbMcpAdminApi {
    async fn list_servers(&self) -> Result<Vec<McpServerView>, ApiError> {
        let rows = McpServerStore::new(&self.db)
            .list_all()
            .map_err(|e| ApiError::Storage(format!("mcp list: {e}")))?;
        let access = ToolAccessStore::new(&self.db);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let tool_count = access
                .list_all()
                .map(|all| {
                    all.iter()
                        .filter(|t| {
                            t.source == ToolSource::Mcp
                                && t.source_id.as_deref() == Some(row.id.as_str())
                                && t.removed_at.is_none()
                        })
                        .count() as u32
                })
                .unwrap_or(0);
            out.push(row_to_view(row, tool_count));
        }
        Ok(out)
    }

    async fn add_server(&self, spec: McpServerSpec) -> Result<McpServerView, ApiError> {
        // Validate id slug. Errors here surface to the agent as
        // ApiError::Validation so it can fix the input on retry
        // without bothering the user.
        execlaw_core::mcp_servers::validate_id(&spec.id)
            .map_err(|e| ApiError::Validation(format!("mcp id: {e}")))?;

        let transport = McpTransport::parse(&spec.transport).ok_or_else(|| {
            ApiError::Validation(format!(
                "unknown transport '{}': agent-callable transports are: streamable_http",
                spec.transport
            ))
        })?;

        // SECURITY GATE: agent cannot install stdio servers (would
        // run an arbitrary local binary). Only operators can add
        // stdio via the SPA admin form.
        if transport == McpTransport::Stdio {
            return Err(ApiError::Validation(
                "stdio transport is operator-only — agent-installed servers must use streamable_http. \
                 The user can add stdio servers manually via Settings → MCP."
                    .into(),
            ));
        }

        let url = match transport {
            McpTransport::StreamableHttp => match spec.url.as_deref() {
                Some(u) if !u.is_empty() => u.to_owned(),
                _ => {
                    return Err(ApiError::Validation(
                        "streamable_http requires a `url`".into(),
                    ));
                }
            },
            McpTransport::Stdio => unreachable!(),
        };

        // Persist the bearer token (if any) in the vault under a
        // generated, plugin-scope-less key. The row's
        // auth_secret_ref points at it.
        let auth_secret_ref = if let Some(tok) = spec.auth_token.as_deref().filter(|s| !s.is_empty())
        {
            let key = format!("mcp:{}/auth_token", spec.id);
            let now = chrono::Utc::now().timestamp();
            VaultRowStore::new(&self.db)
                .put(None, &key, tok.as_bytes(), now)
                .map_err(|e| ApiError::Storage(format!("vault put: {e}")))?;
            Some(key)
        } else {
            None
        };

        // Default trust classes: Controller + Delegated. Operators
        // can broaden in Settings → Tools later.
        let default_allowed_classes = vec!["Controller".to_string(), "Delegated".to_string()];

        let insert = McpServerInsert {
            id: spec.id.clone(),
            display_name: spec.display_name,
            transport,
            command: None,
            args: vec![],
            env: std::collections::HashMap::new(),
            cwd: None,
            url: Some(url),
            auth_secret_ref,
            enabled: true,
            default_allowed_classes,
        };
        let now = chrono::Utc::now().timestamp();
        McpServerStore::new(&self.db)
            .insert(&insert, now)
            .map_err(|e| ApiError::Storage(format!("mcp insert: {e}")))?;

        // Kick the connection actor and wait briefly so the agent
        // sees Connected (or Error) status in the returned view.
        self.host.reconcile().await;
        // Give the actor up to 2s to land its first status update.
        // Don't block forever — Rovo's initialize handshake takes
        // ~300-800ms in practice.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(Some(row)) = McpServerStore::new(&self.db).get(&spec.id) {
                if matches!(
                    row.status,
                    execlaw_core::mcp_servers::McpServerStatus::Connected
                        | execlaw_core::mcp_servers::McpServerStatus::Error
                ) {
                    break;
                }
            }
        }

        let row = McpServerStore::new(&self.db)
            .get(&spec.id)
            .map_err(|e| ApiError::Storage(format!("mcp post-insert read: {e}")))?
            .ok_or_else(|| ApiError::Storage("row vanished after insert".into()))?;
        let tool_count = ToolAccessStore::new(&self.db)
            .list_all()
            .map(|all| {
                all.iter()
                    .filter(|t| {
                        t.source == ToolSource::Mcp
                            && t.source_id.as_deref() == Some(spec.id.as_str())
                            && t.removed_at.is_none()
                    })
                    .count() as u32
            })
            .unwrap_or(0);
        Ok(row_to_view(row, tool_count))
    }

    async fn remove_server(&self, id: &str) -> Result<(), ApiError> {
        let store = McpServerStore::new(&self.db);
        let existing = store
            .get(id)
            .map_err(|e| ApiError::Storage(format!("mcp lookup: {e}")))?
            .ok_or_else(|| ApiError::NotFound(format!("mcp server '{id}' not found")))?;

        // Stop the actor first so its sync_tools doesn't race with
        // our delete.
        self.host.stop_one(id).await;

        // Mark every prefixed tool removed so the agent's catalog
        // updates on the next turn. tool_access store handles the
        // bookkeeping; we just supply the prefix.
        let access = ToolAccessStore::new(&self.db);
        let now = chrono::Utc::now().timestamp();
        let prefix = format!("{MCP_TOOL_PREFIX}{id}:");
        if let Ok(all) = access.list_all() {
            for t in all {
                if t.source == ToolSource::Mcp
                    && t.source_id.as_deref() == Some(id)
                    && t.removed_at.is_none()
                    && t.tool_name.starts_with(&prefix)
                {
                    let _ = access.mark_removed(&t.tool_name, now);
                }
            }
        }

        // Drop the row.
        store
            .delete(id)
            .map_err(|e| ApiError::Storage(format!("mcp delete: {e}")))?;

        // Clean up the vault secret if any.
        if let Some(ref secret_key) = existing.auth_secret_ref {
            let _ = VaultRowStore::new(&self.db).delete(None, secret_key);
        }
        Ok(())
    }
}

fn row_to_view(row: McpServerRow, tool_count: u32) -> McpServerView {
    McpServerView {
        id: row.id,
        display_name: row.display_name,
        transport: row.transport.as_str().to_owned(),
        url: row.url,
        command: row.command,
        enabled: row.enabled,
        status: row.status.as_str().to_owned(),
        last_error: row.last_error,
        tool_count,
    }
}
