//! `plugin.toml` manifest shape.
//!
//! Hook declarations per §4.2. All hook tables are optional — a plugin that
//! only adds tools omits `[[oauth_accounts]]` etc. The set is additive; new
//! hook points land without breaking existing manifests.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// Top-level manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginManifest {
    pub plugin: PluginHeader,

    #[serde(default)]
    pub tools: Vec<ToolDecl>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportDecl>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_provider: Option<IdentityProviderDecl>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_backend: Option<InferenceBackendDecl>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_probe: Option<HardwareProbeDecl>,

    #[serde(default)]
    pub services: Vec<ServiceDecl>,

    #[serde(default)]
    pub oauth_accounts: Vec<OauthAccountDecl>,

    #[serde(default)]
    pub ui_panels: Vec<UiPanelDecl>,

    #[serde(default)]
    pub chat_components: Vec<ChatComponentDecl>,

    #[serde(default)]
    pub event_subscriptions: Vec<EventSubscriptionDecl>,

    #[serde(default)]
    pub alert_sources: Vec<AlertSourceDecl>,

    #[serde(default)]
    pub health_checks: Vec<HealthCheckDecl>,

    #[serde(default)]
    pub skills: Vec<SkillDecl>,

    /// Runtime declaration — which isolation tier (§4.4) the plugin
    /// runs in and how to spawn it. Optional because tool-only
    /// plugins can sometimes be resolved in-process (Phase 3+
    /// feature). For Phase 2 every plugin that declares tools or a
    /// transport MUST set `[runtime]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginHeader {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDecl {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Path (relative to the plugin root) to a JSON Schema describing
    /// the tool's arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// `low` | `medium` | `high` — voice runner only exposes tools with
    /// `low`.
    #[serde(default)]
    pub latency: ToolLatency,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolLatency {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransportDecl {
    pub transport_id: String,
    #[serde(default)]
    pub supports_attachments: bool,
    #[serde(default)]
    pub supports_groups: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdentityProviderDecl {
    /// Identifier kinds this provider can resolve. E.g.
    /// `["phone", "email", "signal_uuid"]`.
    #[serde(default)]
    pub resolves: Vec<String>,
    /// Default trust hint the provider will publish when it matches with
    /// reasonable confidence.
    #[serde(default = "default_trust_hint")]
    pub trust_hint_default: String,
    /// Plugin-self-imposed confidence ceiling (0..1).
    #[serde(default = "default_confidence_ceiling")]
    pub confidence_ceiling: f32,
}

fn default_trust_hint() -> String {
    "Contact".to_owned()
}
fn default_confidence_ceiling() -> f32 {
    0.95
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InferenceBackendDecl {
    pub openai_compatible_endpoint: String,
    pub supports_streaming: bool,
    pub supports_tools: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HardwareProbeDecl {
    /// Which GPU vendors this probe handles.
    #[serde(default)]
    pub vendors: Vec<String>,
    /// Docker image tag the control plane runs.
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDecl {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OauthAccountDecl {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive_refresh_window: Option<String>,
    #[serde(default)]
    pub warn_before_expiry: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_store: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiPanelDecl {
    pub mount: String,
    pub entry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatComponentDecl {
    pub kind: String,
    pub entry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventSubscriptionDecl {
    pub on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertSourceDecl {
    pub fingerprint_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheckDecl {
    pub name: String,
    pub interval: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default)]
    pub failure_threshold: Option<u32>,
    #[serde(default)]
    pub recovery_threshold: Option<u32>,
    pub probe: HealthCheckProbe,
    #[serde(default = "default_severity")]
    pub on_fail_severity: String,
}

fn default_severity() -> String {
    "Error".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HealthCheckProbe {
    Http {
        url: String,
        #[serde(default)]
        expect_status: Option<u16>,
    },
    Tcp {
        host: String,
        port: u16,
    },
    Exec {
        command: Vec<String>,
    },
}

/// One skill shipped by a plugin (Phase B, 2026-05-03).
///
/// At install time the host:
///   1. Reads `entry` (relative to the staged plugin root) as a UTF-8
///      markdown file — the skill's body.
///   2. Sanitizes `name` (lowercase, alphanumeric + hyphen, slashes
///      stripped) and prepends `<plugin_id>/` so the stored skill
///      name is always `<plugin_id>/<sanitized_name>`. The plugin
///      author cannot author a skill outside their own namespace.
///   3. Builds a structured frontmatter from `description` + `tags`
///      and inserts the row via `execlaw_skills::SkillStore` with
///      `registration_kind = "shipped"` and `owning_plugin_id` set.
///
/// On plugin uninstall, every skill with this `owning_plugin_id` is
/// archived. Admins can re-author a clean copy via `skills.create` if
/// they want to keep the procedure after removing the plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillDecl {
    /// Local skill name (without the `<plugin_id>/` prefix). Lowercase
    /// alphanumeric + hyphen. Slashes are sanitized to hyphens.
    pub name: String,
    /// One- or two-sentence description shown to the LLM in
    /// `skills.list`. Required so the agent can decide whether to
    /// activate the skill without reading the body first.
    pub description: String,
    /// Path to the skill's body markdown file, relative to the plugin's
    /// staged root. Example: `"skills/query-builder.md"`.
    pub entry: String,
    /// Optional tags surfaced in the structured frontmatter. Useful for
    /// admin-UI filtering; the LLM doesn't see them directly.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// How the plugin's code actually runs.
///
/// Two tiers are supported:
///
/// * **`subprocess`** — the original isolation tier. Plugin is a
///   binary the host spawns; communication is JSON-RPC over stdio.
///   Use for plugins that need native code (audio pipelines, ffmpeg,
///   signal-cli, ONNX vision models, etc.). Requires `executable`.
///
/// * **`script`** — embedded Rhai interpreter. Plugin is a `.rhai`
///   file the host loads + runs in-process under a sandbox.
///   Use for HTTP-API wrappers, identity providers, simple
///   transforms — anything that doesn't need native deps.
///   Requires `source`. **No compilation cost; install ZIP is
///   the script + the manifest.**
///
/// The validator enforces the right field is set per tier so a
/// script plugin can't accidentally smuggle in a binary executable
/// path and vice versa.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeDecl {
    /// `"subprocess"` or `"script"`. WASM lands later.
    pub tier: String,
    /// Path to the executable, relative to the plugin's staged root.
    /// Examples: `"node"` (resolved via PATH), `"./dist/plugin"`.
    /// Required for `tier = "subprocess"`; ignored for `"script"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Path to the Rhai source file, relative to the plugin's
    /// staged root. Example: `"main.rhai"`. Required for
    /// `tier = "script"`; ignored for `"subprocess"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Arguments passed to the executable, in order.
    /// (Subprocess tier only; ignored for script.)
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the child. Secret references
    /// (`secret://<plugin_id>/<name>`) are resolved by the host
    /// against the vault before spawn.
    /// (Subprocess tier only; ignored for script.)
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Symbolic tier choice. Parsed from the manifest's
/// `runtime.tier` string field; surfaced to the host so it can
/// dispatch to the right loader without re-stringly-comparing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTier {
    Subprocess,
    Script,
}

impl RuntimeTier {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "subprocess" => Some(Self::Subprocess),
            "script" => Some(Self::Script),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subprocess => "subprocess",
            Self::Script => "script",
        }
    }
}

impl RuntimeDecl {
    /// Parsed tier or `None` if `tier` is not a known string.
    pub fn parsed_tier(&self) -> Option<RuntimeTier> {
        RuntimeTier::parse(&self.tier)
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("could not parse TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("plugin id is empty or contains invalid characters: '{0}'")]
    BadId(String),
    #[error("plugin version is empty")]
    EmptyVersion,
    #[error("duplicate tool name: '{0}'")]
    DuplicateTool(String),
    #[error("duplicate oauth account name: '{0}'")]
    DuplicateOauth(String),
    #[error("duplicate ui panel mount: '{0}'")]
    DuplicatePanel(String),
    #[error("unknown runtime.tier '{0}' (must be 'subprocess' or 'script')")]
    UnknownRuntimeTier(String),
    #[error("runtime.tier = 'subprocess' requires 'executable'")]
    SubprocessMissingExecutable,
    #[error("runtime.tier = 'script' requires 'source' (path to .rhai file)")]
    ScriptMissingSource,
}

impl PluginManifest {
    /// Parse and validate a manifest from a TOML string.
    pub fn parse(s: &str) -> Result<Self, ManifestError> {
        let m: PluginManifest = toml::from_str(s)?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.plugin.id.is_empty()
            || !self
                .plugin
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ManifestError::BadId(self.plugin.id.clone()));
        }
        if self.plugin.version.is_empty() {
            return Err(ManifestError::EmptyVersion);
        }

        // Uniqueness checks.
        let mut seen: std::collections::HashSet<&str> = Default::default();
        for t in &self.tools {
            if !seen.insert(&t.name) {
                return Err(ManifestError::DuplicateTool(t.name.clone()));
            }
        }
        let mut seen: std::collections::HashSet<&str> = Default::default();
        for o in &self.oauth_accounts {
            if !seen.insert(&o.name) {
                return Err(ManifestError::DuplicateOauth(o.name.clone()));
            }
        }
        let mut seen: std::collections::HashSet<&str> = Default::default();
        for p in &self.ui_panels {
            if !seen.insert(&p.mount) {
                return Err(ManifestError::DuplicatePanel(p.mount.clone()));
            }
        }

        // Runtime tier validation. Only enforce when [runtime] is
        // present; some plugins (transports declared by external
        // controllers, future tiers) may legitimately omit it.
        if let Some(rt) = &self.runtime {
            let tier = rt
                .parsed_tier()
                .ok_or_else(|| ManifestError::UnknownRuntimeTier(rt.tier.clone()))?;
            match tier {
                RuntimeTier::Subprocess => {
                    let has_exe = rt
                        .executable
                        .as_ref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    if !has_exe {
                        return Err(ManifestError::SubprocessMissingExecutable);
                    }
                }
                RuntimeTier::Script => {
                    let has_src = rt
                        .source
                        .as_ref()
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    if !has_src {
                        return Err(ManifestError::ScriptMissingSource);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        [plugin]
        id = "google-calendar"
        name = "Google Calendar"
        version = "1.0.0"
        description = "List and create calendar events."

        [[tools]]
        name = "calendar_list_events"
        schema = "schemas/list_events.json"
        latency = "medium"
        required_capabilities = ["plugin.google-calendar.calendar.read"]

        [[tools]]
        name = "calendar_create_event"
        schema = "schemas/create_event.json"
        latency = "medium"
        required_capabilities = ["plugin.google-calendar.calendar.write"]

        [[oauth_accounts]]
        name = "controller"
        provider = "google"
        scopes = ["calendar.readonly", "calendar.events"]
        proactive_refresh_window = "10m"
        warn_before_expiry = ["7d", "3d", "1d"]

        [[ui_panels]]
        mount = "admin/plugins/google-calendar"
        entry = "ui/panel.js"

        [[alert_sources]]
        fingerprint_prefix = "plugin.google-calendar"

        [[health_checks]]
        name = "calendar_api_reachable"
        interval = "5m"
        probe = { kind = "http", url = "https://www.googleapis.com/discovery/v1/apis/calendar/v3/rest", expect_status = 200 }
        on_fail_severity = "Error"
    "#;

    #[test]
    fn parse_example_manifest() {
        let m = PluginManifest::parse(EXAMPLE).unwrap();
        assert_eq!(m.plugin.id, "google-calendar");
        assert_eq!(m.tools.len(), 2);
        assert_eq!(m.oauth_accounts.len(), 1);
        assert_eq!(m.ui_panels.len(), 1);
        assert_eq!(m.health_checks.len(), 1);
        let HealthCheckProbe::Http { url, expect_status } = &m.health_checks[0].probe else {
            panic!("expected http probe");
        };
        assert!(url.contains("googleapis.com"));
        assert_eq!(*expect_status, Some(200));
    }

    #[test]
    fn duplicate_tool_names_rejected() {
        let bad = r#"
            [plugin]
            id = "dup"
            name = "D"
            version = "1.0"

            [[tools]]
            name = "x"
            [[tools]]
            name = "x"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateTool(_)));
    }

    #[test]
    fn empty_id_rejected() {
        let bad = r#"
            [plugin]
            id = ""
            name = ""
            version = "1.0"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::BadId(_)));
    }

    #[test]
    fn bad_id_chars_rejected() {
        let bad = r#"
            [plugin]
            id = "oops/slash"
            name = "n"
            version = "1"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::BadId(_)));
    }

    #[test]
    fn minimal_manifest_parses() {
        let tiny = r#"
            [plugin]
            id = "x"
            name = "X"
            version = "0.1.0"
        "#;
        let m = PluginManifest::parse(tiny).unwrap();
        assert!(m.tools.is_empty());
        assert!(m.oauth_accounts.is_empty());
    }

    #[test]
    fn subprocess_tier_requires_executable() {
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [runtime]
            tier = "subprocess"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::SubprocessMissingExecutable));
    }

    #[test]
    fn script_tier_requires_source() {
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [runtime]
            tier = "script"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::ScriptMissingSource));
    }

    #[test]
    fn script_tier_with_source_parses_cleanly() {
        let ok = r#"
            [plugin]
            id = "google-contacts"
            name = "Google Contacts"
            version = "0.1.0"

            [identity_provider]
            resolves = ["email", "phone"]
            trust_hint_default = "Contact"
            confidence_ceiling = 0.95

            [runtime]
            tier = "script"
            source = "main.rhai"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        let rt = m.runtime.unwrap();
        assert_eq!(rt.parsed_tier(), Some(RuntimeTier::Script));
        assert_eq!(rt.source.as_deref(), Some("main.rhai"));
        assert!(rt.executable.is_none());
    }

    #[test]
    fn unknown_tier_rejected() {
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [runtime]
            tier = "wasm"
            source = "main.wasm"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        assert!(matches!(err, ManifestError::UnknownRuntimeTier(_)));
    }

    #[test]
    fn subprocess_tier_with_executable_still_parses() {
        // Backwards-compat: existing hello + identity-local-address-book
        // manifests must still parse cleanly.
        let ok = r#"
            [plugin]
            id = "hello"
            name = "Hello"
            version = "0.1.0"

            [runtime]
            tier = "subprocess"
            executable = "./hello"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        let rt = m.runtime.unwrap();
        assert_eq!(rt.parsed_tier(), Some(RuntimeTier::Subprocess));
        assert_eq!(rt.executable.as_deref(), Some("./hello"));
        assert!(rt.source.is_none());
    }
}
