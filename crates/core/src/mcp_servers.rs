//! Persistence for `config_mcp_servers` (Phase 8c).
//!
//! Each row is one configured MCP server endpoint. The connection
//! manager in `execlaw-server` reads these on boot, opens a
//! connection per enabled row, and reflects the discovered tool list
//! into `config_tool_access` rows tagged with `source = "mcp"` and
//! `source_id = <this id>`.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Transport family. `Stdio` is the only fully-wired option in 8c;
/// `StreamableHttp` rows persist but the connection manager logs a
/// "deferred" warning and skips them until 8c-follow-up adds the
/// HTTP transport to the mcp-client crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

impl McpTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable_http",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stdio" => Some(Self::Stdio),
            "streamable_http" => Some(Self::StreamableHttp),
            _ => None,
        }
    }
}

/// Connection state surface. The Settings page renders this directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerStatus {
    /// Never connected since the last boot.
    Idle,
    /// Connected and the initialise handshake completed.
    Connected,
    /// Disconnected due to a clean shutdown or operator disable.
    Disconnected,
    /// Last connect attempt failed; `last_error` carries the message.
    Error,
}

impl McpServerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "connected" => Some(Self::Connected),
            "disconnected" => Some(Self::Disconnected),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// Full row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRow {
    pub id: String,
    pub display_name: String,
    pub transport: McpTransport,
    /// Stdio-only: command + args + env + cwd.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    /// HTTP-only:
    pub url: Option<String>,
    pub auth_secret_ref: Option<String>,
    /// Shared:
    pub enabled: bool,
    pub default_allowed_classes: Vec<String>,
    pub status: McpServerStatus,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Insert payload (operator-supplied; no status / last_error yet).
#[derive(Debug, Clone)]
pub struct McpServerInsert {
    pub id: String,
    pub display_name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub auth_secret_ref: Option<String>,
    pub enabled: bool,
    pub default_allowed_classes: Vec<String>,
}

/// Slug constraints. Used as both the PK and the `mcp:<id>:<tool>`
/// prefix, so we keep it conservative — alphanumeric + hyphen +
/// underscore, 2-32 chars, no leading/trailing hyphen.
pub const MCP_ID_MIN: usize = 2;
pub const MCP_ID_MAX: usize = 32;

pub fn validate_id(s: &str) -> Result<(), &'static str> {
    let len = s.chars().count();
    if len < MCP_ID_MIN {
        return Err("mcp server id is too short");
    }
    if len > MCP_ID_MAX {
        return Err("mcp server id is too long");
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err("mcp server id may not start or end with '-'");
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("mcp server id may only contain alphanumeric, underscore, hyphen");
    }
    Ok(())
}

pub struct McpServerStore<'db> {
    db: &'db Database,
}

impl<'db> McpServerStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    pub fn insert(&self, row: &McpServerInsert, now: i64) -> Result<(), DbError> {
        validate_id(&row.id).map_err(|e| DbError::Config(e.into()))?;
        let args_json =
            serde_json::to_string(&row.args).map_err(|e| DbError::Serde(e.to_string()))?;
        let env_json =
            serde_json::to_string(&row.env).map_err(|e| DbError::Serde(e.to_string()))?;
        let allowed_json = serde_json::to_string(&row.default_allowed_classes)
            .map_err(|e| DbError::Serde(e.to_string()))?;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_mcp_servers \
                   (id, display_name, transport, command, args_json, env_json, cwd, \
                    url, auth_secret_ref, enabled, default_allowed_classes, \
                    status, last_error, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'idle', NULL, ?12, ?12)",
                params![
                    row.id,
                    row.display_name,
                    row.transport.as_str(),
                    row.command,
                    args_json,
                    env_json,
                    row.cwd,
                    row.url,
                    row.auth_secret_ref,
                    row.enabled as i64,
                    allowed_json,
                    now,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<McpServerRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT id, display_name, transport, command, args_json, env_json, cwd, \
                            url, auth_secret_ref, enabled, default_allowed_classes, \
                            status, last_error, created_at, updated_at \
                     FROM config_mcp_servers WHERE id = ?1",
                    params![id],
                    row_to_server,
                )
                .ok();
            Ok(got)
        })
    }

    pub fn list_all(&self) -> Result<Vec<McpServerRow>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, display_name, transport, command, args_json, env_json, cwd, \
                        url, auth_secret_ref, enabled, default_allowed_classes, \
                        status, last_error, created_at, updated_at \
                 FROM config_mcp_servers \
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt
                .query_map([], row_to_server)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Operator-driven update — replaces every column the form
    /// might touch. Status / last_error are NOT changed here; those
    /// are connection-manager territory.
    pub fn update(&self, id: &str, row: &McpServerInsert, now: i64) -> Result<bool, DbError> {
        let args_json =
            serde_json::to_string(&row.args).map_err(|e| DbError::Serde(e.to_string()))?;
        let env_json =
            serde_json::to_string(&row.env).map_err(|e| DbError::Serde(e.to_string()))?;
        let allowed_json = serde_json::to_string(&row.default_allowed_classes)
            .map_err(|e| DbError::Serde(e.to_string()))?;
        self.db.with_conn(|c| {
            let n = c.execute(
                "UPDATE config_mcp_servers SET \
                    display_name = ?2, transport = ?3, command = ?4, args_json = ?5, \
                    env_json = ?6, cwd = ?7, url = ?8, auth_secret_ref = ?9, \
                    enabled = ?10, default_allowed_classes = ?11, updated_at = ?12 \
                 WHERE id = ?1",
                params![
                    id,
                    row.display_name,
                    row.transport.as_str(),
                    row.command,
                    args_json,
                    env_json,
                    row.cwd,
                    row.url,
                    row.auth_secret_ref,
                    row.enabled as i64,
                    allowed_json,
                    now,
                ],
            )?;
            Ok(n > 0)
        })
    }

    /// Connection-manager update for runtime status. Doesn't touch
    /// the operator-set columns.
    pub fn set_status(
        &self,
        id: &str,
        status: McpServerStatus,
        last_error: Option<&str>,
        now: i64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE config_mcp_servers \
                 SET status = ?2, last_error = ?3, updated_at = ?4 \
                 WHERE id = ?1",
                params![id, status.as_str(), last_error, now],
            )?;
            Ok(())
        })
    }

    pub fn delete(&self, id: &str) -> Result<bool, DbError> {
        self.db.with_conn(|c| {
            let n = c.execute("DELETE FROM config_mcp_servers WHERE id = ?1", params![id])?;
            Ok(n > 0)
        })
    }
}

fn row_to_server(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRow> {
    let transport_str: String = row.get(2)?;
    let transport = McpTransport::parse(&transport_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown transport: {transport_str}"),
            )),
        )
    })?;
    let args_json: Option<String> = row.get(4)?;
    let args: Vec<String> = args_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .unwrap_or_default()
        .unwrap_or_default();
    let env_json: Option<String> = row.get(5)?;
    let env: HashMap<String, String> = env_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .unwrap_or_default()
        .unwrap_or_default();
    let allowed_json: String = row.get(10)?;
    let default_allowed_classes: Vec<String> =
        serde_json::from_str(&allowed_json).unwrap_or_default();
    let status_str: Option<String> = row.get(11)?;
    let status = status_str
        .as_deref()
        .and_then(McpServerStatus::parse)
        .unwrap_or(McpServerStatus::Idle);
    Ok(McpServerRow {
        id: row.get(0)?,
        display_name: row.get(1)?,
        transport,
        command: row.get(3)?,
        args,
        env,
        cwd: row.get(6)?,
        url: row.get(7)?,
        auth_secret_ref: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        default_allowed_classes,
        status,
        last_error: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn stdio_seed(id: &str) -> McpServerInsert {
        McpServerInsert {
            id: id.into(),
            display_name: format!("Mock {id}"),
            transport: McpTransport::Stdio,
            command: Some("/usr/bin/mock-mcp".into()),
            args: vec!["--port".into(), "0".into()],
            env: HashMap::from([("MOCK_TOKEN".into(), "abc".into())]),
            cwd: None,
            url: None,
            auth_secret_ref: None,
            enabled: true,
            default_allowed_classes: vec!["Controller".into()],
        }
    }

    #[test]
    fn insert_and_roundtrip() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("github"), 100).unwrap();
        let row = store.get("github").unwrap().unwrap();
        assert_eq!(row.transport, McpTransport::Stdio);
        assert_eq!(row.command.as_deref(), Some("/usr/bin/mock-mcp"));
        assert_eq!(row.args, vec!["--port", "0"]);
        assert_eq!(row.env.get("MOCK_TOKEN").map(String::as_str), Some("abc"));
        assert_eq!(row.status, McpServerStatus::Idle);
    }

    #[test]
    fn list_all_orders_by_created_at_then_id() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("github"), 200).unwrap();
        store.insert(&stdio_seed("slack"), 100).unwrap();
        let names: Vec<String> = store
            .list_all()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(names, vec!["slack", "github"]);
    }

    #[test]
    fn duplicate_id_rejected_at_pk() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("svc"), 100).unwrap();
        assert!(store.insert(&stdio_seed("svc"), 200).is_err());
    }

    #[test]
    fn validate_id_rejects_bad_shapes() {
        assert!(validate_id("a").is_err());
        assert!(validate_id("-bad").is_err());
        assert!(validate_id("bad-").is_err());
        assert!(validate_id("ok-id").is_ok());
        assert!(validate_id("ok_id_2").is_ok());
        assert!(validate_id(&"x".repeat(33)).is_err());
        assert!(validate_id("has space").is_err());
    }

    #[test]
    fn update_replaces_operator_columns_but_not_status() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("svc"), 100).unwrap();
        store
            .set_status("svc", McpServerStatus::Connected, None, 110)
            .unwrap();
        let mut updated = stdio_seed("svc");
        updated.display_name = "renamed".into();
        updated.enabled = false;
        store.update("svc", &updated, 200).unwrap();
        let row = store.get("svc").unwrap().unwrap();
        assert_eq!(row.display_name, "renamed");
        assert!(!row.enabled);
        assert_eq!(row.status, McpServerStatus::Connected);
        assert_eq!(row.updated_at, 200);
    }

    #[test]
    fn delete_returns_true_then_false() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("svc"), 100).unwrap();
        assert!(store.delete("svc").unwrap());
        assert!(!store.delete("svc").unwrap());
    }

    #[test]
    fn set_status_records_last_error() {
        let db = fresh_db();
        let store = McpServerStore::new(&db);
        store.insert(&stdio_seed("svc"), 100).unwrap();
        store
            .set_status("svc", McpServerStatus::Error, Some("connection refused"), 110)
            .unwrap();
        let row = store.get("svc").unwrap().unwrap();
        assert_eq!(row.status, McpServerStatus::Error);
        assert_eq!(row.last_error.as_deref(), Some("connection refused"));
    }
}
