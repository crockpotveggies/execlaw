//! Registered event-kind + reply-handler registry (M6).
//!
//! Plugins declare in their `plugin.toml`:
//!   * `[[events]]` — kinds they can publish, with payload schema +
//!     `expects_reply` flag (the validator gate)
//!   * `[[reply_handlers]]` — channels they can deliver replies to,
//!     with capability flags (streaming, attachments, max size, etc.)
//!   * `[[default_flows]]` — JSON flow defs shipped for the operator
//!     to use / fork (handled by `automations.rs`, not here)
//!
//! At plugin install + hydrate time the host imports the first two
//! into `state_registered_event_kinds` / `state_registered_reply_handlers`.
//! The Automations UI reads from these tables to populate the
//! trigger picker; the validator reads to gate `SendReply` nodes.
//!
//! Core also seeds these tables from a built-in list (web prompts,
//! routines, scheduled wakeups) at boot — see
//! `register_core_event_kinds()`.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One event kind a plugin (or core) can publish. Keyed by `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct RegisteredEventKind {
    /// Globally unique kind string. Convention: `<source>.<verb>`
    /// e.g. `whatsapp.message.received`, `calendar.event.starting_soon`,
    /// `web.prompt.submitted`. Lower-case dot-separated.
    pub kind: String,
    /// Owner — `"core"` for built-ins, `"plugin:<id>"` for plugins.
    pub source: String,
    pub description: String,
    /// Optional JSON Schema for the event's `payload` shape.
    /// Surfaced in the Automations UI as autocomplete hints when
    /// authors write Rhai filters / templates.
    #[schema(value_type = Object)]
    pub payload_schema: Option<serde_json::Value>,
    /// Validator gate: a flow whose trigger has this kind set may
    /// only include `SendReply` if this is `true`.
    pub expects_reply: bool,
    /// UI hint — which `OriginRef` variant this kind typically uses.
    /// Free-form string, not enforced.
    pub default_origin_kind: String,
}

/// One reply handler a plugin advertises. Keyed by `name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct RegisteredReplyHandler {
    /// Handler name. Must match `OriginRef::PluginChannel.plugin_id`.
    /// Convention: same as the plugin's id (`"whatsapp"`, `"signal"`).
    pub name: String,
    pub plugin_id: String,
    pub description: String,

    // ----- Capability flags consulted by the ReplyRouter degrade matrix.
    pub supports_streaming: bool,
    pub supports_attachments: bool,
    pub supports_inline_chart: bool,
    pub supports_table: bool,
    pub supports_card: bool,
    pub supports_markdown: bool,

    pub max_attachment_size_bytes: Option<u64>,
    pub max_attachments_per_message: Option<u32>,
    pub max_text_length: Option<u32>,
    /// Allowed MIME prefixes — e.g., `["image/", "video/"]`. `None`
    /// = any.
    pub allowed_mime_prefixes: Option<Vec<String>>,
}

impl Default for RegisteredReplyHandler {
    /// Conservative defaults: text-only, no attachments, no markdown.
    /// A manifest that forgets to declare a capability degrades
    /// gracefully — the router never crashes; it just sends text.
    fn default() -> Self {
        Self {
            name: String::new(),
            plugin_id: String::new(),
            description: String::new(),
            supports_streaming: false,
            supports_attachments: false,
            supports_inline_chart: false,
            supports_table: false,
            supports_card: false,
            supports_markdown: false,
            max_attachment_size_bytes: None,
            max_attachments_per_message: None,
            max_text_length: None,
            allowed_mime_prefixes: None,
        }
    }
}

pub struct EventRegistry<'db> {
    db: &'db Database,
}

impl<'db> EventRegistry<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Upsert by `kind`. Plugins call this on install / hydrate.
    pub fn upsert_event_kind(&self, k: &RegisteredEventKind) -> Result<(), DbError> {
        let schema_json = k
            .payload_schema
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_registered_event_kinds \
                 (kind, source, description, payload_schema_json, expects_reply, default_origin_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(kind) DO UPDATE SET \
                   source = excluded.source, \
                   description = excluded.description, \
                   payload_schema_json = excluded.payload_schema_json, \
                   expects_reply = excluded.expects_reply, \
                   default_origin_kind = excluded.default_origin_kind",
                params![
                    k.kind,
                    k.source,
                    k.description,
                    schema_json,
                    if k.expects_reply { 1 } else { 0 },
                    k.default_origin_kind,
                ],
            )?;
            Ok(())
        })
    }

    /// Remove a kind by name. Called on plugin uninstall.
    pub fn remove_event_kind(&self, kind: &str) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM state_registered_event_kinds WHERE kind = ?1",
                params![kind],
            )?;
            Ok(())
        })
    }

    /// Remove all kinds + handlers a plugin contributed. Idempotent.
    pub fn remove_by_plugin(&self, plugin_id: &str) -> Result<(), DbError> {
        let src = format!("plugin:{plugin_id}");
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM state_registered_event_kinds WHERE source = ?1",
                params![src],
            )?;
            c.execute(
                "DELETE FROM state_registered_reply_handlers WHERE plugin_id = ?1",
                params![plugin_id],
            )?;
            Ok(())
        })
    }

    pub fn list_event_kinds(&self) -> Result<Vec<RegisteredEventKind>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, source, description, payload_schema_json, \
                        expects_reply, default_origin_kind \
                 FROM state_registered_event_kinds \
                 ORDER BY kind ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                let schema_json: Option<String> = r.get(3)?;
                let payload_schema = schema_json.and_then(|s| serde_json::from_str(&s).ok());
                Ok(RegisteredEventKind {
                    kind: r.get(0)?,
                    source: r.get(1)?,
                    description: r.get(2)?,
                    payload_schema,
                    expects_reply: r.get::<_, i32>(4)? != 0,
                    default_origin_kind: r.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn get_event_kind(&self, kind: &str) -> Result<Option<RegisteredEventKind>, DbError> {
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT kind, source, description, payload_schema_json, \
                            expects_reply, default_origin_kind \
                     FROM state_registered_event_kinds WHERE kind = ?1",
                    params![kind],
                    |r| {
                        let schema_json: Option<String> = r.get(3)?;
                        Ok(RegisteredEventKind {
                            kind: r.get(0)?,
                            source: r.get(1)?,
                            description: r.get(2)?,
                            payload_schema: schema_json
                                .and_then(|s| serde_json::from_str(&s).ok()),
                            expects_reply: r.get::<_, i32>(4)? != 0,
                            default_origin_kind: r.get(5)?,
                        })
                    },
                )
                .ok();
            Ok(row)
        })
    }

    pub fn upsert_reply_handler(&self, h: &RegisteredReplyHandler) -> Result<(), DbError> {
        let mimes_json = h
            .allowed_mime_prefixes
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()));
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_registered_reply_handlers \
                 (name, plugin_id, description, supports_streaming, supports_attachments, \
                  supports_inline_chart, supports_table, supports_card, supports_markdown, \
                  max_attachment_size_bytes, max_attachments_per_message, max_text_length, \
                  allowed_mime_prefixes_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(name) DO UPDATE SET \
                   plugin_id = excluded.plugin_id, \
                   description = excluded.description, \
                   supports_streaming = excluded.supports_streaming, \
                   supports_attachments = excluded.supports_attachments, \
                   supports_inline_chart = excluded.supports_inline_chart, \
                   supports_table = excluded.supports_table, \
                   supports_card = excluded.supports_card, \
                   supports_markdown = excluded.supports_markdown, \
                   max_attachment_size_bytes = excluded.max_attachment_size_bytes, \
                   max_attachments_per_message = excluded.max_attachments_per_message, \
                   max_text_length = excluded.max_text_length, \
                   allowed_mime_prefixes_json = excluded.allowed_mime_prefixes_json",
                params![
                    h.name,
                    h.plugin_id,
                    h.description,
                    if h.supports_streaming { 1 } else { 0 },
                    if h.supports_attachments { 1 } else { 0 },
                    if h.supports_inline_chart { 1 } else { 0 },
                    if h.supports_table { 1 } else { 0 },
                    if h.supports_card { 1 } else { 0 },
                    if h.supports_markdown { 1 } else { 0 },
                    h.max_attachment_size_bytes.map(|v| v as i64),
                    h.max_attachments_per_message.map(|v| v as i64),
                    h.max_text_length.map(|v| v as i64),
                    mimes_json,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_reply_handler(&self, name: &str) -> Result<Option<RegisteredReplyHandler>, DbError> {
        self.db.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT name, plugin_id, description, supports_streaming, supports_attachments, \
                            supports_inline_chart, supports_table, supports_card, supports_markdown, \
                            max_attachment_size_bytes, max_attachments_per_message, max_text_length, \
                            allowed_mime_prefixes_json \
                     FROM state_registered_reply_handlers WHERE name = ?1",
                    params![name],
                    |r| {
                        let mimes_json: Option<String> = r.get(12)?;
                        Ok(RegisteredReplyHandler {
                            name: r.get(0)?,
                            plugin_id: r.get(1)?,
                            description: r.get(2)?,
                            supports_streaming: r.get::<_, i32>(3)? != 0,
                            supports_attachments: r.get::<_, i32>(4)? != 0,
                            supports_inline_chart: r.get::<_, i32>(5)? != 0,
                            supports_table: r.get::<_, i32>(6)? != 0,
                            supports_card: r.get::<_, i32>(7)? != 0,
                            supports_markdown: r.get::<_, i32>(8)? != 0,
                            max_attachment_size_bytes: r
                                .get::<_, Option<i64>>(9)?
                                .map(|v| v as u64),
                            max_attachments_per_message: r
                                .get::<_, Option<i64>>(10)?
                                .map(|v| v as u32),
                            max_text_length: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
                            allowed_mime_prefixes: mimes_json
                                .and_then(|s| serde_json::from_str(&s).ok()),
                        })
                    },
                )
                .ok();
            Ok(row)
        })
    }

    pub fn list_reply_handlers(&self) -> Result<Vec<RegisteredReplyHandler>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT name, plugin_id, description, supports_streaming, supports_attachments, \
                        supports_inline_chart, supports_table, supports_card, supports_markdown, \
                        max_attachment_size_bytes, max_attachments_per_message, max_text_length, \
                        allowed_mime_prefixes_json \
                 FROM state_registered_reply_handlers ORDER BY name ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                let mimes_json: Option<String> = r.get(12)?;
                Ok(RegisteredReplyHandler {
                    name: r.get(0)?,
                    plugin_id: r.get(1)?,
                    description: r.get(2)?,
                    supports_streaming: r.get::<_, i32>(3)? != 0,
                    supports_attachments: r.get::<_, i32>(4)? != 0,
                    supports_inline_chart: r.get::<_, i32>(5)? != 0,
                    supports_table: r.get::<_, i32>(6)? != 0,
                    supports_card: r.get::<_, i32>(7)? != 0,
                    supports_markdown: r.get::<_, i32>(8)? != 0,
                    max_attachment_size_bytes: r
                        .get::<_, Option<i64>>(9)?
                        .map(|v| v as u64),
                    max_attachments_per_message: r
                        .get::<_, Option<i64>>(10)?
                        .map(|v| v as u32),
                    max_text_length: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
                    allowed_mime_prefixes: mimes_json
                        .and_then(|s| serde_json::from_str(&s).ok()),
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

/// Built-in event kinds owned by core. Plugins register their own
/// via the plugin host's on-install hook.
pub fn core_event_kinds() -> Vec<RegisteredEventKind> {
    vec![
        RegisteredEventKind {
            kind: "web.prompt.submitted".into(),
            source: "core".into(),
            description: "An operator submitted a prompt via the web SPA chat input.".into(),
            payload_schema: Some(serde_json::json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "text": {"type": "string"},
                    "conversation_id": {"type": "string"},
                    "attachment_ids": {"type": "array", "items": {"type": "string"}},
                },
            })),
            expects_reply: true,
            default_origin_kind: "web_socket_session".into(),
        },
        RegisteredEventKind {
            kind: "routine.fired".into(),
            source: "core".into(),
            description: "A scheduled routine fired its trigger.".into(),
            payload_schema: None,
            expects_reply: false,
            default_origin_kind: "none".into(),
        },
        RegisteredEventKind {
            kind: "scheduled.wakeup".into(),
            source: "core".into(),
            description: "Internal wakeup — agent/runner pacing alarm.".into(),
            payload_schema: None,
            expects_reply: false,
            default_origin_kind: "none".into(),
        },
        RegisteredEventKind {
            kind: "webhook.received".into(),
            source: "core".into(),
            description: "Inbound webhook (plugin or generic). Carries method, path, body.".into(),
            payload_schema: None,
            expects_reply: false,
            default_origin_kind: "none".into(),
        },
    ]
}

/// Built-in reply handlers owned by core (web ws, chat_append,
/// alert, drop). The router special-cases these — they're not
/// routed via `plugin_host.call_tool`.
pub fn core_reply_handlers() -> Vec<RegisteredReplyHandler> {
    vec![
        RegisteredReplyHandler {
            name: "web_socket_session".into(),
            plugin_id: "core".into(),
            description: "Stream agent replies back to the originating SPA WebSocket session."
                .into(),
            supports_streaming: true,
            supports_attachments: true,
            supports_inline_chart: true,
            supports_table: true,
            supports_card: true,
            supports_markdown: true,
            max_attachment_size_bytes: None,
            max_attachments_per_message: None,
            max_text_length: None,
            allowed_mime_prefixes: None,
        },
        RegisteredReplyHandler {
            name: "chat_append".into(),
            plugin_id: "core".into(),
            description: "Append the reply to an existing chat thread (e.g., operator Inbox)."
                .into(),
            supports_streaming: true,
            supports_attachments: true,
            supports_inline_chart: true,
            supports_table: true,
            supports_card: true,
            supports_markdown: true,
            ..Default::default()
        },
        RegisteredReplyHandler {
            name: "alert".into(),
            plugin_id: "core".into(),
            description: "Surface the reply as an alert in the operator's alert dropdown.".into(),
            supports_streaming: false,
            supports_attachments: false,
            supports_inline_chart: false,
            supports_table: false,
            supports_card: false,
            supports_markdown: false,
            max_text_length: Some(2048),
            ..Default::default()
        },
        RegisteredReplyHandler {
            name: "drop".into(),
            plugin_id: "core".into(),
            description: "Silently discard the reply (Notify-only flows).".into(),
            ..Default::default()
        },
    ]
}

/// Seed core kinds + handlers. Call once at boot, after migrations.
pub fn register_core_event_kinds(db: &Database) -> Result<(), DbError> {
    let reg = EventRegistry::new(db);
    for k in core_event_kinds() {
        reg.upsert_event_kind(&k)?;
    }
    for h in core_reply_handlers() {
        reg.upsert_reply_handler(&h)?;
    }
    Ok(())
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

    #[test]
    fn core_event_kinds_round_trip_through_registry() {
        let db = fresh_db();
        register_core_event_kinds(&db).unwrap();
        let reg = EventRegistry::new(&db);
        let kinds = reg.list_event_kinds().unwrap();
        assert!(kinds.iter().any(|k| k.kind == "web.prompt.submitted"
            && k.expects_reply
            && k.source == "core"));
        assert!(kinds.iter().any(|k| k.kind == "routine.fired" && !k.expects_reply));
    }

    #[test]
    fn upsert_event_kind_is_idempotent() {
        let db = fresh_db();
        let reg = EventRegistry::new(&db);
        let k = RegisteredEventKind {
            kind: "test.foo".into(),
            source: "plugin:test".into(),
            description: "v1".into(),
            payload_schema: None,
            expects_reply: false,
            default_origin_kind: "none".into(),
        };
        reg.upsert_event_kind(&k).unwrap();
        let mut k2 = k.clone();
        k2.description = "v2".into();
        reg.upsert_event_kind(&k2).unwrap();
        let got = reg.get_event_kind("test.foo").unwrap().unwrap();
        assert_eq!(got.description, "v2");
    }

    #[test]
    fn remove_by_plugin_cleans_kinds_and_handlers() {
        let db = fresh_db();
        let reg = EventRegistry::new(&db);
        reg.upsert_event_kind(&RegisteredEventKind {
            kind: "foo.bar".into(),
            source: "plugin:foo".into(),
            description: "x".into(),
            payload_schema: None,
            expects_reply: false,
            default_origin_kind: "none".into(),
        })
        .unwrap();
        reg.upsert_reply_handler(&RegisteredReplyHandler {
            name: "foo".into(),
            plugin_id: "foo".into(),
            description: "y".into(),
            ..Default::default()
        })
        .unwrap();
        reg.remove_by_plugin("foo").unwrap();
        assert!(reg.get_event_kind("foo.bar").unwrap().is_none());
        assert!(reg.get_reply_handler("foo").unwrap().is_none());
    }

    #[test]
    fn reply_handler_capability_round_trip() {
        let db = fresh_db();
        let reg = EventRegistry::new(&db);
        let h = RegisteredReplyHandler {
            name: "whatsapp".into(),
            plugin_id: "whatsapp".into(),
            description: "WA reply handler".into(),
            supports_streaming: false,
            supports_attachments: true,
            supports_inline_chart: false,
            supports_table: false,
            supports_card: false,
            supports_markdown: true,
            max_attachment_size_bytes: Some(16_777_216),
            max_attachments_per_message: Some(1),
            max_text_length: Some(4096),
            allowed_mime_prefixes: Some(vec!["image/".into(), "application/pdf".into()]),
        };
        reg.upsert_reply_handler(&h).unwrap();
        let got = reg.get_reply_handler("whatsapp").unwrap().unwrap();
        assert_eq!(got, h);
    }

    #[test]
    fn core_reply_handlers_include_drop_and_alert() {
        let db = fresh_db();
        register_core_event_kinds(&db).unwrap();
        let reg = EventRegistry::new(&db);
        let names: Vec<_> = reg
            .list_reply_handlers()
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert!(names.contains(&"web_socket_session".into()));
        assert!(names.contains(&"chat_append".into()));
        assert!(names.contains(&"alert".into()));
        assert!(names.contains(&"drop".into()));
    }
}
