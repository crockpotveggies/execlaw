//! MCP connection manager (Phase 8c).
//!
//! Owns one tokio task per row in `config_mcp_servers` (enabled =
//! true, transport = stdio). Each task:
//!   1. Spawns the stdio child via `execlaw_mcp_client::McpClient::stdio`.
//!   2. On successful initialise, fetches `tools/list` and reflects
//!      every tool into `config_tool_access` with the canonical
//!      `mcp:<server_id>:<remote_name>` prefix.
//!   3. Subscribes to `tools/list_changed` notifications and
//!      re-syncs whenever the server signals a change.
//!   4. On EOF / error, marks the row `error`, sleeps an
//!      exponentially-growing backoff (capped at 60s), and reconnects.
//!
//! Public surface:
//!   * `McpHost::start(db)` — spawns the supervisor; returns a
//!     handle the dispatch chain (Phase 8d) can call.
//!   * `McpHost::call_tool(prefixed_name, args)` — routes to the
//!     right server actor, awaits the JSON result, returns it as
//!     a single text-content array (the runner expects `Value`).
//!   * `McpHost::reconcile()` — re-reads `config_mcp_servers` and
//!     starts/stops actors as the operator adds, edits, or
//!     disables servers.

use crate::mcp_http_client::HttpMcpClient;
use crate::tool_sync::default_mcp_classes_for;
use dashmap::DashMap;
use execlaw_core::Database;
use execlaw_core::mcp_servers::{McpServerRow, McpServerStatus, McpServerStore, McpTransport};
use execlaw_core::tool_access::{ToolAccessSeed, ToolAccessStore, ToolSource};
use execlaw_core::vault_row::VaultRowStore;
use execlaw_mcp_client::{McpClient, McpError, McpNotification, McpResult, McpTool, StdioSpec};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// Connected MCP client — branches by transport. Both arms expose
/// the same `list_tools` / `call_tool` shape so `mcp_host` can
/// dispatch uniformly.
#[derive(Clone)]
enum ConnectedClient {
    Stdio(Arc<McpClient>),
    Http(HttpMcpClient),
}

impl ConnectedClient {
    async fn list_tools(&self) -> McpResult<Vec<McpTool>> {
        match self {
            Self::Stdio(c) => c.list_tools().await,
            Self::Http(c) => c.list_tools().await,
        }
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> McpResult<execlaw_mcp_client::CallToolResult> {
        match self {
            Self::Stdio(c) => c.call_tool(name, args).await,
            Self::Http(c) => c.call_tool(name, args).await,
        }
    }
}

/// Tool-name prefix all MCP-sourced tools use in the registry. Lets
/// the dispatch chain route by string-prefix instead of looking up
/// the source out-of-band.
pub const MCP_TOOL_PREFIX: &str = "mcp:";

/// Reconnect backoff bounds.
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// One running MCP server actor's handle.
struct ServerHandle {
    client: Mutex<Option<ConnectedClient>>,
    shutdown: Arc<Notify>,
}

#[derive(Clone)]
pub struct McpHost {
    inner: Arc<Inner>,
}

struct Inner {
    db: Database,
    /// server_id → handle.
    servers: DashMap<String, Arc<ServerHandle>>,
    /// Global stop signal — flipping this drains every actor.
    global_stop: Arc<Notify>,
}

impl McpHost {
    /// Build an empty host. Call `reconcile()` after construction to
    /// spin up the configured servers.
    pub fn new(db: Database) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                servers: DashMap::new(),
                global_stop: Arc::new(Notify::new()),
            }),
        }
    }

    /// Re-read `config_mcp_servers` and start / stop actors so the
    /// running set matches the persisted set. Idempotent.
    pub async fn reconcile(&self) {
        let store = McpServerStore::new(&self.inner.db);
        let rows = match store.list_all() {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "mcp reconcile: failed to list servers");
                return;
            }
        };

        // Start / restart actors for every enabled row. Both
        // stdio and streamable_http are now wired.
        for row in &rows {
            if !row.enabled {
                self.stop_one(&row.id).await;
                continue;
            }
            if !self.inner.servers.contains_key(&row.id) {
                self.start_one(row.clone());
            }
        }

        // Stop actors whose row was deleted.
        let live_ids: std::collections::HashSet<String> =
            rows.iter().map(|r| r.id.clone()).collect();
        let stale: Vec<String> = self
            .inner
            .servers
            .iter()
            .filter_map(|kv| {
                if live_ids.contains(kv.key()) {
                    None
                } else {
                    Some(kv.key().clone())
                }
            })
            .collect();
        for id in stale {
            self.stop_one(&id).await;
        }
    }

    /// Stop the supervisor for a single server. Best-effort — if the
    /// actor is mid-reconnect it'll observe the shutdown on its next
    /// loop iteration.
    pub async fn stop_one(&self, id: &str) {
        if let Some((_, handle)) = self.inner.servers.remove(id) {
            handle.shutdown.notify_waiters();
            handle.shutdown.notify_one();
            // Drop our client clone so the actor's only owner goes away.
            *handle.client.lock().await = None;
        }
    }

    /// Stop every actor. Called from the server's tokio shutdown path.
    pub async fn shutdown_all(&self) {
        self.inner.global_stop.notify_waiters();
        for kv in self.inner.servers.iter() {
            kv.value().shutdown.notify_waiters();
            kv.value().shutdown.notify_one();
        }
        self.inner.servers.clear();
    }

    /// Dispatch a prefixed tool name (`mcp:<id>:<remote_name>`) to
    /// the right server actor. Returns the raw JSON value of the
    /// `tools/call` result so the dispatch chain can hand it back as
    /// a tool_result payload.
    pub async fn call_tool(&self, prefixed: &str, args: Value) -> Result<Value, String> {
        let (server_id, remote_name) = parse_prefixed_tool_name(prefixed)
            .ok_or_else(|| format!("not an MCP tool name: '{prefixed}'"))?;
        let handle = self
            .inner
            .servers
            .get(server_id)
            .map(|kv| kv.value().clone())
            .ok_or_else(|| format!("MCP server '{server_id}' not connected"))?;
        let client =
            handle.client.lock().await.clone().ok_or_else(|| {
                format!("MCP server '{server_id}' has no live connection right now")
            })?;
        let r = client
            .call_tool(remote_name, args)
            .await
            .map_err(|e| format!("MCP call failed: {e}"))?;
        Ok(serde_json::json!({
            "content": r.content,
            "isError": r.is_error,
        }))
    }

    fn start_one(&self, row: McpServerRow) {
        let shutdown = Arc::new(Notify::new());
        let handle = Arc::new(ServerHandle {
            client: Mutex::new(None),
            shutdown: shutdown.clone(),
        });
        self.inner.servers.insert(row.id.clone(), handle.clone());

        let db = self.inner.db.clone();
        let global_stop = self.inner.global_stop.clone();
        tokio::spawn(async move {
            match row.transport {
                McpTransport::Stdio => {
                    stdio_actor_loop(db, row, handle, shutdown, global_stop).await
                }
                McpTransport::StreamableHttp => {
                    http_actor_loop(db, row, handle, shutdown, global_stop).await
                }
            }
        });
    }
}

/// Per-server stdio supervisor: spawns the child, runs the sync,
/// drives reconnects with exponential backoff. Exits when its
/// `shutdown` notify fires.
async fn stdio_actor_loop(
    db: Database,
    row: McpServerRow,
    handle: Arc<ServerHandle>,
    shutdown: Arc<Notify>,
    global_stop: Arc<Notify>,
) {
    let mut backoff = RECONNECT_INITIAL;
    loop {
        let spec = StdioSpec {
            command: row.command.clone().unwrap_or_default(),
            args: row.args.clone(),
            env: row.env.clone(),
            cwd: row.cwd.as_ref().map(std::path::PathBuf::from),
        };
        let connect_shutdown = Arc::new(Notify::new());
        let client_result = McpClient::stdio(&spec, connect_shutdown.clone()).await;
        match client_result {
            Ok(client) => {
                let client = Arc::new(client);
                *handle.client.lock().await = Some(ConnectedClient::Stdio(client.clone()));
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Connected,
                    None,
                    now,
                );
                info!(server = %row.id, "MCP server connected");
                if let Err(e) = sync_tools(&db, &row, &ConnectedClient::Stdio(client.clone())).await
                {
                    warn!(server = %row.id, error = %e, "initial tool sync failed");
                }

                // Watch for notifications + shutdown.
                let mut notif_rx = client.subscribe_notifications();
                let stop_local = shutdown.clone();
                let stop_global = global_stop.clone();
                let connect_shutdown_inner = connect_shutdown.clone();
                let client_for_loop = client.clone();
                let row_for_loop = row.clone();
                let db_for_loop = db.clone();
                let exit = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = stop_local.notified() => {
                                debug!(server = %row_for_loop.id, "actor: local stop");
                                connect_shutdown_inner.notify_one();
                                return;
                            }
                            _ = stop_global.notified() => {
                                debug!(server = %row_for_loop.id, "actor: global stop");
                                connect_shutdown_inner.notify_one();
                                return;
                            }
                            recv = notif_rx.recv() => {
                                match recv {
                                    Ok(McpNotification::ToolsListChanged) => {
                                        let cc = ConnectedClient::Stdio(client_for_loop.clone());
                                        if let Err(e) = sync_tools(&db_for_loop, &row_for_loop, &cc).await {
                                            warn!(server = %row_for_loop.id, error = %e, "list_changed re-sync failed");
                                        }
                                    }
                                    Ok(McpNotification::ResourcesListChanged) => {
                                        debug!(server = %row_for_loop.id, "resources list changed (no-op for tool sync)");
                                    }
                                    Err(_) => {
                                        // sender dropped — connection actor is gone.
                                        return;
                                    }
                                }
                            }
                        }
                    }
                });
                let _ = exit.await;
                // Wake the inner client task so it shuts down.
                connect_shutdown.notify_one();
                *handle.client.lock().await = None;
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Disconnected,
                    None,
                    now,
                );
                info!(server = %row.id, "MCP server disconnected (operator stop or EOF)");
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(
                    server = %row.id,
                    error = %msg,
                    backoff_ms = backoff.as_millis(),
                    "MCP connect failed; will retry"
                );
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Error,
                    Some(&msg),
                    now,
                );
            }
        }
        // Sleep with respect to shutdown / global stop.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.notified() => {
                return;
            }
            _ = global_stop.notified() => {
                return;
            }
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Per-server HTTP supervisor. Connects via streamable HTTP, runs
/// the initial tool sync, then sits idle until shutdown. HTTP MCP
/// is connection-per-request under the hood (the reqwest client
/// handles pooling) so there's no long-lived socket to babysit —
/// failures surface on the next `call_tool` rather than out-of-band
/// disconnect events.
async fn http_actor_loop(
    db: Database,
    row: McpServerRow,
    handle: Arc<ServerHandle>,
    shutdown: Arc<Notify>,
    global_stop: Arc<Notify>,
) {
    let mut backoff = RECONNECT_INITIAL;
    loop {
        let url = match row.url.as_deref() {
            Some(u) if !u.is_empty() => u.to_owned(),
            _ => {
                let msg = "streamable_http server has no url configured";
                warn!(server = %row.id, "{msg}");
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Error,
                    Some(msg),
                    now,
                );
                return;
            }
        };
        // auth_secret_ref points at a vault row with the bearer
        // token. None / empty = unauthenticated server.
        let bearer = if let Some(name) = row.auth_secret_ref.as_deref().filter(|s| !s.is_empty()) {
            match VaultRowStore::new(&db).get(None, name) {
                Ok(Some(bytes)) => match String::from_utf8(bytes) {
                    Ok(s) => Some(s),
                    Err(_) => {
                        warn!(server = %row.id, "auth_secret_ref vault row is not utf-8; treating as no auth");
                        None
                    }
                },
                Ok(None) => {
                    warn!(server = %row.id, name, "auth_secret_ref points at a missing vault row; treating as no auth");
                    None
                }
                Err(e) => {
                    warn!(server = %row.id, error = %e, "vault read failed; treating as no auth");
                    None
                }
            }
        } else {
            None
        };

        match HttpMcpClient::connect(&url, bearer.as_deref()).await {
            Ok(client) => {
                let cc = ConnectedClient::Http(client);
                *handle.client.lock().await = Some(cc.clone());
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Connected,
                    None,
                    now,
                );
                info!(server = %row.id, url = %url, "MCP server connected (http)");
                if let Err(e) = sync_tools(&db, &row, &cc).await {
                    warn!(server = %row.id, error = %e, "initial tool sync failed");
                }
                // No notification stream in v1 — just wait for
                // shutdown. Re-syncs happen on `reconcile()`.
                tokio::select! {
                    _ = shutdown.notified() => {
                        debug!(server = %row.id, "http actor: local stop");
                    }
                    _ = global_stop.notified() => {
                        debug!(server = %row.id, "http actor: global stop");
                    }
                }
                *handle.client.lock().await = None;
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Disconnected,
                    None,
                    now,
                );
                info!(server = %row.id, "MCP server disconnected (http)");
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(
                    server = %row.id,
                    error = %msg,
                    backoff_ms = backoff.as_millis(),
                    "MCP http connect failed; will retry"
                );
                let now = chrono::Utc::now().timestamp();
                let _ = McpServerStore::new(&db).set_status(
                    &row.id,
                    McpServerStatus::Error,
                    Some(&msg),
                    now,
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.notified() => return,
            _ = global_stop.notified() => return,
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Reflect a server's `tools/list` response into `config_tool_access`.
/// Transport-agnostic — works for both stdio and streamable_http
/// because both arms of `ConnectedClient` expose the same surface.
async fn sync_tools(
    db: &Database,
    row: &McpServerRow,
    client: &ConnectedClient,
) -> McpResult<usize> {
    let tools: Vec<McpTool> = client.list_tools().await?;
    let store = ToolAccessStore::new(db);
    let now = chrono::Utc::now().timestamp();
    let mut n = 0;
    for t in &tools {
        let prefixed = format!("{MCP_TOOL_PREFIX}{}:{}", row.id, t.name);
        let input_schema_str = t
            .input_schema
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        store
            .upsert_seen(
                &ToolAccessSeed {
                    tool_name: prefixed,
                    source: ToolSource::Mcp,
                    source_id: Some(row.id.clone()),
                    description: t.description.clone(),
                    input_schema: input_schema_str,
                    default_allowed_classes: default_mcp_classes_for(row),
                },
                now,
            )
            .map_err(|e| McpError::Protocol(format!("tool_access upsert: {e}")))?;
        n += 1;
    }
    // Mark any previously-seen MCP tool from this server that the
    // current tools/list omitted as `removed_at = now`. Operator
    // policy stays put so a brief disappearance + reappearance
    // restores the same allowlist.
    let live: std::collections::HashSet<String> = tools
        .iter()
        .map(|t| format!("{MCP_TOOL_PREFIX}{}:{}", row.id, t.name))
        .collect();
    let all = store
        .list_all()
        .map_err(|e| McpError::Protocol(format!("tool_access list: {e}")))?;
    for existing in all {
        if existing.source == ToolSource::Mcp
            && existing.source_id.as_deref() == Some(row.id.as_str())
            && existing.removed_at.is_none()
            && !live.contains(&existing.tool_name)
        {
            let _ = store.mark_removed(&existing.tool_name, now);
        }
    }
    info!(server = %row.id, tools = n, "MCP tool sync complete");
    Ok(n)
}

/// Split a prefixed tool name into `(server_id, remote_name)`. Returns
/// `None` if the prefix is missing or either half is empty.
pub fn parse_prefixed_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once(':')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prefixed_tool_name_round_trips() {
        let (server, tool) = parse_prefixed_tool_name("mcp:github:create_pr").unwrap();
        assert_eq!(server, "github");
        assert_eq!(tool, "create_pr");
    }

    #[test]
    fn parse_prefixed_tool_name_rejects_non_mcp() {
        assert!(parse_prefixed_tool_name("set_thread_name").is_none());
        assert!(parse_prefixed_tool_name("mcp:").is_none());
        assert!(parse_prefixed_tool_name("mcp:srv:").is_none());
        assert!(parse_prefixed_tool_name("mcp::tool").is_none());
    }
}
