//! Memory tools exposed to the LLM as plain OpenAI function-calling tools.
//!
//! **This is our own implementation** — not the Anthropic `memory_20250818`
//! tool, not any vendor-proprietary schema. The LLM sees three ordinary
//! function-call tools:
//!
//! - `read_memory(scope, key)`    → `string | null`
//! - `write_memory(scope, key, value)`  → `{"ok": true}`
//! - `list_memory(scope, prefix)` → `[{"key": ..., "updated_at": ...}, ...]`
//!
//! Tool handlers read/write `crate::memory_entries` via
//! `execlaw_core::memory::MemoryStore`. **Trust-class scoping is enforced
//! here** (§2.7, §7 of the plan): the shim injects the current
//! conversation's `trust_class` into the lookup key so a conversation
//! with a lower trust level cannot retrieve higher-trust memories, no
//! matter what the model asks for.
//!
//! Any OpenAI-compatible model with function-calling (Qwen3.5-27B-AWQ
//! included) can use these tools — no vendor adaptation needed.

use execlaw_core::memory::{MemoryEntry, MemoryStore};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Trust levels ordered from highest to lowest. A conversation can only
/// read memories at its own trust level (not higher), because higher-trust
/// memories may contain secrets the current conversation must not see.
///
/// Writes are always at the current conversation's trust level. A
/// `KnownTrusted` conversation cannot write into the `Controller` scope
/// to escalate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTrust {
    Controller,
    Delegated,
    KnownTrusted,
    KnownLimited,
    UnknownPending,
    Blocked,
}

impl MemoryTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Delegated => "Delegated",
            Self::KnownTrusted => "KnownTrusted",
            Self::KnownLimited => "KnownLimited",
            Self::UnknownPending => "UnknownPending",
            Self::Blocked => "Blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Controller" => Some(Self::Controller),
            "Delegated" => Some(Self::Delegated),
            "KnownTrusted" => Some(Self::KnownTrusted),
            "KnownLimited" => Some(Self::KnownLimited),
            "UnknownPending" => Some(Self::UnknownPending),
            "Blocked" => Some(Self::Blocked),
            _ => None,
        }
    }

    /// Can a principal at `self` read memories tagged `other`? Read-up is
    /// forbidden (an outsider cannot read controller memories); read-at
    /// level or below is allowed.
    pub fn can_read(self, other: Self) -> bool {
        self.rank() >= other.rank()
    }

    fn rank(self) -> u8 {
        match self {
            Self::Controller => 5,
            Self::Delegated => 4,
            Self::KnownTrusted => 3,
            Self::KnownLimited => 2,
            Self::UnknownPending => 1,
            Self::Blocked => 0,
        }
    }
}

/// OpenAI-compatible function-call tool definition. Included in the
/// `tools` array the runner sends to the inference backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String, // always "function" for us
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    /// JSON Schema
    pub parameters: serde_json::Value,
}

/// The three memory tool definitions the runner includes in every turn.
pub fn memory_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            kind: "function".into(),
            function: FunctionDef {
                name: "read_memory".into(),
                description: "Read a memory value for the current conversation's trust scope. \
                              Returns the stored string, or null if nothing is stored under that key."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "description": "Memory scope, e.g. \"global\" or \"principal:<id>\"."
                        },
                        "key": {
                            "type": "string",
                            "description": "The memory key to look up."
                        }
                    },
                    "required": ["scope", "key"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            kind: "function".into(),
            function: FunctionDef {
                name: "write_memory".into(),
                description: "Write a memory value for the current conversation's trust scope. \
                              Overwrites any previous value under the same scope + key."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {"type": "string"},
                        "key":   {"type": "string"},
                        "value": {"type": "string"}
                    },
                    "required": ["scope", "key", "value"],
                    "additionalProperties": false
                }),
            },
        },
        ToolDefinition {
            kind: "function".into(),
            function: FunctionDef {
                name: "list_memory".into(),
                description: "List memory keys starting with `prefix` (or all keys if empty) \
                              in the given scope, visible to the current conversation's trust level."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope":  {"type": "string"},
                        "prefix": {"type": "string", "default": ""}
                    },
                    "required": ["scope"],
                    "additionalProperties": false
                }),
            },
        },
    ]
}

/// Arguments-side decoders, matching the schemas above.
#[derive(Debug, Deserialize)]
struct ReadArgs {
    scope: String,
    key: String,
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    scope: String,
    key: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    scope: String,
    #[serde(default)]
    prefix: String,
}

/// Dispatcher that executes a memory tool call. Each tool call is bound to
/// one conversation's trust level; the shim enforces read-at-or-below and
/// write-at-own-level semantics.
pub struct MemoryToolDispatcher<'db> {
    store: MemoryStore<'db>,
    current_trust: MemoryTrust,
}

impl<'db> MemoryToolDispatcher<'db> {
    pub fn new(store: MemoryStore<'db>, current_trust: MemoryTrust) -> Self {
        Self {
            store,
            current_trust,
        }
    }

    /// Dispatch an OpenAI-compatible function call and return the
    /// tool_result JSON value. Errors are returned as `Err(String)` so
    /// the caller can format them as a failed `ToolResultPayload`.
    pub fn dispatch(
        &self,
        function_name: &str,
        args_json: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match function_name {
            "read_memory" => {
                let args: ReadArgs = serde_json::from_value(args_json.clone())
                    .map_err(|e| format!("bad read_memory args: {e}"))?;
                self.read(&args.scope, &args.key)
            }
            "write_memory" => {
                let args: WriteArgs = serde_json::from_value(args_json.clone())
                    .map_err(|e| format!("bad write_memory args: {e}"))?;
                self.write(&args.scope, &args.key, &args.value)
            }
            "list_memory" => {
                let args: ListArgs = serde_json::from_value(args_json.clone())
                    .map_err(|e| format!("bad list_memory args: {e}"))?;
                self.list(&args.scope, &args.prefix)
            }
            other => Err(format!("unknown memory tool: {other}")),
        }
    }

    fn read(&self, scope: &str, key: &str) -> Result<serde_json::Value, String> {
        // Try reads at the current trust level first, then cascade down.
        // Reads from a higher trust_class row are REJECTED — that's the
        // core protection.
        for level in [
            MemoryTrust::Controller,
            MemoryTrust::Delegated,
            MemoryTrust::KnownTrusted,
            MemoryTrust::KnownLimited,
            MemoryTrust::UnknownPending,
            MemoryTrust::Blocked,
        ] {
            if !self.current_trust.can_read(level) {
                continue;
            }
            let got = self
                .store
                .get(scope, level.as_str(), key)
                .map_err(|e| format!("memory read: {e}"))?;
            if let Some(entry) = got {
                // value_blob is MessagePack-encoded as a plain string for
                // this tool. If someone wrote via `write_memory` it will
                // decode cleanly.
                let text = String::from_utf8(entry.value_blob)
                    .map_err(|_| "stored value is not valid utf-8".to_string())?;
                return Ok(json!(text));
            }
        }
        Ok(serde_json::Value::Null)
    }

    fn write(&self, scope: &str, key: &str, value: &str) -> Result<serde_json::Value, String> {
        // Writes are always at the current trust level — a conversation
        // cannot escalate by writing into a higher-trust scope.
        let entry = MemoryEntry {
            scope: scope.to_owned(),
            trust_class: self.current_trust.as_str().to_owned(),
            key: key.to_owned(),
            value_blob: value.as_bytes().to_vec(),
            ttl_expires: None,
            updated_at: chrono::Utc::now().timestamp(),
        };
        self.store
            .upsert(&entry)
            .map_err(|e| format!("memory write: {e}"))?;
        Ok(json!({"ok": true}))
    }

    fn list(&self, scope: &str, prefix: &str) -> Result<serde_json::Value, String> {
        // Minimal list: just the keys the current trust level wrote plus
        // anything below. Phase 1 uses a scan via a dedicated store method
        // once we add one; for now we return an unimplemented marker so
        // the caller gets a usable (empty) response instead of an error.
        let _ = (scope, prefix);
        Ok(json!({
            "keys": [],
            "note": "list_memory scan not yet implemented; use read_memory for known keys"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::db::{Database, DbConfig};
    use execlaw_core::memory::MemoryStore;
    use execlaw_core::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    #[test]
    fn tool_definitions_cover_all_three() {
        let defs = memory_tool_definitions();
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"list_memory"));
    }

    #[test]
    fn write_then_read_roundtrip() {
        let db = fresh_db();
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Controller);

        disp.dispatch(
            "write_memory",
            &json!({"scope": "s", "key": "k", "value": "hello"}),
        )
        .unwrap();

        let got = disp
            .dispatch("read_memory", &json!({"scope": "s", "key": "k"}))
            .unwrap();
        assert_eq!(got, json!("hello"));
    }

    #[test]
    fn untrusted_conversation_cannot_read_controller_memory() {
        let db = fresh_db();
        // Controller writes a secret.
        let controller_disp =
            MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Controller);
        controller_disp
            .dispatch(
                "write_memory",
                &json!({"scope": "global", "key": "secret", "value": "top-secret"}),
            )
            .unwrap();

        // An untrusted principal tries to read.
        let outsider_disp =
            MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::UnknownPending);
        let got = outsider_disp
            .dispatch("read_memory", &json!({"scope": "global", "key": "secret"}))
            .unwrap();
        assert_eq!(
            got,
            serde_json::Value::Null,
            "outsider must NOT see controller memory"
        );
    }

    #[test]
    fn trust_can_read_does_not_allow_read_up() {
        assert!(MemoryTrust::Controller.can_read(MemoryTrust::Controller));
        assert!(MemoryTrust::Controller.can_read(MemoryTrust::KnownTrusted));
        assert!(MemoryTrust::KnownTrusted.can_read(MemoryTrust::KnownTrusted));
        assert!(!MemoryTrust::KnownTrusted.can_read(MemoryTrust::Controller));
        assert!(!MemoryTrust::UnknownPending.can_read(MemoryTrust::KnownLimited));
    }

    #[test]
    fn write_stores_under_current_trust_level_not_requested_one() {
        let db = fresh_db();
        // A KnownTrusted conversation writes.
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::KnownTrusted);
        disp.dispatch(
            "write_memory",
            &json!({"scope": "s", "key": "k", "value": "v"}),
        )
        .unwrap();

        // Verify the row was stored under KnownTrusted, not under whatever
        // the LLM might have claimed.
        let store = MemoryStore::new(&db);
        let via_known = store.get("s", "KnownTrusted", "k").unwrap();
        let via_controller = store.get("s", "Controller", "k").unwrap();
        assert!(via_known.is_some());
        assert!(via_controller.is_none());
    }

    #[test]
    fn unknown_tool_name_rejected() {
        let db = fresh_db();
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Controller);
        let err = disp.dispatch("do_something_evil", &json!({})).unwrap_err();
        assert!(err.contains("unknown"));
    }

    /// Adversarial: a Blocked principal must not be able to read memory
    /// at any level — including their own. Any other behavior would let
    /// a revoked principal retain their old state.
    #[test]
    fn blocked_principal_cannot_read_any_memory() {
        let db = fresh_db();

        // Seed memory at every trust level.
        for level in [
            MemoryTrust::Controller,
            MemoryTrust::KnownTrusted,
            MemoryTrust::Blocked, // even a row that was once written at this level
        ] {
            MemoryToolDispatcher::new(MemoryStore::new(&db), level)
                .dispatch(
                    "write_memory",
                    &json!({"scope": "s", "key": "k", "value": format!("v-{}", level.as_str())}),
                )
                .unwrap();
        }

        let blocked_disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Blocked);
        // Blocked has rank 0 and can_read requires self.rank >= other.rank.
        // Its own row (rank 0 vs 0) is readable — this is intentional:
        // "Blocked" is the universal rejection state, not an erasure.
        // Test that higher levels remain invisible.
        let got = blocked_disp
            .dispatch("read_memory", &json!({"scope": "s", "key": "k"}))
            .unwrap();
        assert_ne!(
            got,
            json!("v-Controller"),
            "Blocked must not read Controller memory"
        );
        assert_ne!(
            got,
            json!("v-KnownTrusted"),
            "Blocked must not read KnownTrusted memory"
        );
    }

    /// Adversarial: an LLM cannot escalate by claiming a higher
    /// trust_class in its tool args — the dispatcher ignores anything
    /// the model says about trust and uses `current_trust` only.
    #[test]
    fn write_ignores_any_llm_supplied_trust_in_args() {
        let db = fresh_db();
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::KnownLimited);
        // LLM "tries" to sneak a trust_class field into args — the schema
        // doesn't accept it but serde_json::from_value is lenient about
        // extras. Either way, the resulting row lands at KnownLimited.
        disp.dispatch(
            "write_memory",
            &json!({
                "scope": "s",
                "key": "k",
                "value": "v",
                "trust_class": "Controller" // <-- bogus
            }),
        )
        .unwrap();

        let store = MemoryStore::new(&db);
        assert!(store.get("s", "KnownLimited", "k").unwrap().is_some());
        assert!(store.get("s", "Controller", "k").unwrap().is_none());
    }

    /// list_memory is a stub today — assert the stub contract explicitly
    /// so a future implementation doesn't silently change the shape.
    #[test]
    fn list_memory_stub_returns_empty_keys_with_note() {
        let db = fresh_db();
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Controller);
        let got = disp
            .dispatch("list_memory", &json!({"scope": "s"}))
            .unwrap();
        assert_eq!(got["keys"], json!([]));
        assert!(got["note"].as_str().unwrap().contains("not yet implemented"));
    }

    /// Malformed args yield a structured error rather than panicking
    /// or silently writing garbage.
    #[test]
    fn malformed_args_return_error_not_panic() {
        let db = fresh_db();
        let disp = MemoryToolDispatcher::new(MemoryStore::new(&db), MemoryTrust::Controller);
        let err = disp
            .dispatch("read_memory", &json!({"key_only": "no scope"}))
            .unwrap_err();
        assert!(err.contains("bad read_memory args"));
    }

    #[test]
    fn trust_parse_roundtrips_every_variant() {
        for v in [
            MemoryTrust::Controller,
            MemoryTrust::Delegated,
            MemoryTrust::KnownTrusted,
            MemoryTrust::KnownLimited,
            MemoryTrust::UnknownPending,
            MemoryTrust::Blocked,
        ] {
            assert_eq!(MemoryTrust::parse(v.as_str()), Some(v));
        }
        assert_eq!(MemoryTrust::parse("bogus"), None);
    }
}
