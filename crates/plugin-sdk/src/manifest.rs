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

    /// Plugin-served admin endpoints (§ channel-plugin surface).
    /// Each entry maps an HTTP method + path under
    /// `/api/admin/plugins/{plugin_id}/...` to a Rhai handler
    /// function the host invokes per request. Lets channel
    /// plugins (Signal pairing flows, future WhatsApp QR, etc.)
    /// expose admin UI without having to live in the host crate.
    #[serde(default)]
    pub admin_routes: Vec<AdminRouteDecl>,

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
    /// Minimum trust level required to invoke this tool. Mirrors
    /// selfhosted-claw's `controllerOnly: true` knob but generalised
    /// — operators can pin a tool at `Controller` (admin only),
    /// `KnownTrusted`, etc. Tools omitting this field have no trust
    /// floor (anyone whose conversation reaches the dispatcher may
    /// invoke them, subject to the existing `required_capabilities`
    /// gate).
    ///
    /// The string is parsed against `execlaw_policy::TrustLevel`;
    /// unknown strings cause manifest validation to fail loudly so a
    /// typo doesn't silently downgrade to "no floor". Concretely, a
    /// `signal.send_message` tool that pinned to `"Controller"`
    /// prevents a Signal contact (KnownLimited / KnownTrusted) from
    /// using the controller's outbound transport to spam other
    /// people — selfhosted-claw learned this the hard way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_floor: Option<String>,
    /// Phase 3 (signal sidecar): when `true`, the tool's
    /// implementation lives in the host crate as an
    /// `Arc<dyn ToolImpl>` builtin — the plugin manifest only
    /// declares its presence so install/upgrade can validate the
    /// sidecar dependency, set per-tool metadata (description,
    /// trust_floor), and surface the tool under the plugin's
    /// attribution in catalog UIs. The plugin's runtime
    /// (rhai/subprocess) is **not** consulted on dispatch.
    ///
    /// Practically, the registry's `enable_with_stage` skips
    /// inserting a host-implemented tool into `tools_by_name`,
    /// which means a builtin the host registers under the same
    /// name is no longer a conflict. Used today by
    /// `signal.send_message` and `signal.reply` — tools that need
    /// the host-side `TransportApi` capability that rhai scripts
    /// can't reach. Group ops stay rhai-implemented.
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_implemented: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Bootstrap-icons name (without the `bi-` prefix) the SPA renders
    /// next to thread titles for conversations bridged on this
    /// transport. Lets the operator visually distinguish "Web chat",
    /// "Signal group", "WhatsApp", etc. at a glance. Defaults to
    /// `"phone"` when omitted — picked because most external-channel
    /// transports are phone-based messengers; plugins SHOULD override
    /// with a more specific icon (e.g. `"chat-quote"` for Signal,
    /// `"envelope"` for email).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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
    /// Bind mounts the supervisor wires into the spawned container.
    /// Each entry resolves through [`MountDecl::source`] semantics —
    /// stage-relative for shipped scripts, plugin-state for
    /// persistent volumes, or absolute host paths for operator
    /// overrides. Empty by default; sidecars that need persistent
    /// state (e.g. signal-cli's account keystore) or shipped
    /// scripts (e.g. an entrypoint wrapper) declare them here.
    #[serde(default)]
    pub mounts: Vec<MountDecl>,
    /// Override the container image's `ENTRYPOINT`. When `None` (the
    /// default), the image's built-in entrypoint runs. Sidecars that
    /// need to wrap the upstream entrypoint (e.g. patching
    /// supervisord configs before exec'ing the original) set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// When present, this service is a **supervised sidecar** — a
    /// companion container the sidecar supervisor takes
    /// responsibility for: spawn, healthcheck, restart, alert on
    /// stuck. The sidecar's identity is the parent service's `name`
    /// (which the supervisor enforces is globally unique across
    /// installed plugins, so docker container names don't collide).
    ///
    /// The sidecar system is intentionally generic: signal-cli,
    /// WhatsApp Bridge, an ffmpeg pool, an OCR worker — all the
    /// supervisor needs is "a container to keep running on this
    /// port." Transport bridges happen to be the first set of
    /// sidecars we ship, but `SidecarMeta` carries no transport-
    /// specific knobs.
    ///
    /// Plain `[[services]]` entries that omit this table are
    /// **unsupervised** helper daemons — the manifest registers
    /// them but the supervisor leaves them alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<SidecarMeta>,
}

/// One bind mount declared by a sidecar. Source paths come in three
/// flavors so plugin authors don't need to hardcode absolute host
/// paths the operator's machine doesn't necessarily share:
///
///   * `stage://relative/path` — resolves against the plugin's
///     extracted stage directory. Used to mount shipped scripts,
///     config files, or other read-only resources baked into the
///     plugin zip. Defaults to read-only.
///   * `state://name` — resolves to a managed per-sidecar host
///     directory (`<execlaw>/sidecars/<plugin_id>/<sidecar_name>/<name>/`)
///     that the supervisor creates on first spawn and persists
///     across restarts. Used for stateful sidecar data (signal-cli
///     account keystore, etc.). Read-write.
///   * absolute `/path` — host-absolute. Operator-controlled,
///     e.g. mounting a shared dataset into an OCR worker. Direct
///     pass-through.
///
/// The `target` is always the absolute container-side path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MountDecl {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// Sidecar-specific metadata on a `ServiceDecl`. Combined with the
/// parent service's `name` + `image` + `ports`, this is everything
/// the sidecar supervisor needs to run + monitor a companion
/// container.
///
/// Deliberately tiny: the supervisor's job is generic
/// container-lifecycle, not a transport-specific abstraction.
/// Anything transport-specific lives in the plugin's tool layer or
/// in a future separate `[transport]` declaration; the sidecar meta
/// only carries fields the supervisor actually uses (RPC port to
/// publish + health path to probe).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SidecarMeta {
    /// Container port serving the sidecar's local RPC. The
    /// supervisor publishes this as `127.0.0.1:<host_port>` and
    /// probes `<rpc_health_path>` against it.
    pub rpc_port: u16,
    /// HTTP path on the RPC port that the supervisor probes for
    /// liveness. Defaults to `/healthz`. Sidecars that expose a
    /// different convention (signal-cli's `/v1/about`, an OCR
    /// worker's `/ready`, ...) override here.
    #[serde(default = "default_rpc_health_path")]
    pub rpc_health_path: String,
}

fn default_rpc_health_path() -> String {
    "/healthz".to_owned()
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

/// Plugin-served admin endpoint — declared in the plugin's
/// manifest, routed by the host under
/// `/api/admin/plugins/{plugin_id}{path}`, dispatched into the
/// plugin's Rhai script via the named handler function.
///
/// Lets channel plugins surface admin operations (Signal pairing,
/// future WhatsApp QR, future webhook secret rotation, etc.)
/// without their handler code having to live in the host crate.
///
/// Example:
/// ```toml
/// [[admin_routes]]
/// method = "POST"
/// path = "/pair"
/// handler = "on_pair_request"
/// ```
///
/// The Rhai handler signature is
/// `fn on_pair_request(args)` — `args` is a map containing
/// `method`, `path`, `query` (params parsed from the URL),
/// `body` (decoded JSON when the request was JSON; raw string
/// otherwise), and `headers` (subset the host whitelisted).
/// Return value is JSON the host serialises into the HTTP
/// response. Plugins can throw a Rhai error to surface a 500.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdminRouteDecl {
    /// HTTP method — `"GET"`, `"POST"`, `"DELETE"`, etc. The host
    /// uppercases before matching, so `"post"` is fine too.
    pub method: String,
    /// Path under `/api/admin/plugins/{plugin_id}`. Leading slash
    /// recommended for readability — the host normalises.
    pub path: String,
    /// Rhai top-level function name to invoke per request.
    pub handler: String,
    /// Optional human-readable description shown in plugin
    /// catalog / docs UIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    #[error(
        "tool '{tool}' has trust_floor = '{value}' which is not a known TrustLevel \
         (expected one of: Controller, Delegated, KnownTrusted, KnownLimited, \
         UnknownPending, Blocked)"
    )]
    UnknownTrustFloor { tool: String, value: String },
    #[error("duplicate service name '{0}' in the same plugin")]
    DuplicateServiceName(String),
    #[error("service has empty name string")]
    ServiceEmptyName,
}

/// Trust levels the manifest may pin a tool to. Kept as a small flat
/// set here so the SDK doesn't depend on `execlaw-policy`; the host
/// re-validates against `execlaw_policy::TrustLevel` at registration
/// time. Order matches `policy::trust::TrustLevel`'s declaration.
const KNOWN_TRUST_LEVELS: &[&str] = &[
    "Controller",
    "Delegated",
    "KnownTrusted",
    "KnownLimited",
    "UnknownPending",
    "Blocked",
];

/// True iff `s` is a valid trust level string accepted by the manifest.
/// Public so other crates can share the canonical list without re-typing
/// it.
pub fn is_known_trust_level(s: &str) -> bool {
    KNOWN_TRUST_LEVELS.iter().any(|k| *k == s)
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

        // Uniqueness + trust_floor validation, in one pass.
        let mut seen: std::collections::HashSet<&str> = Default::default();
        for t in &self.tools {
            if !seen.insert(&t.name) {
                return Err(ManifestError::DuplicateTool(t.name.clone()));
            }
            if let Some(tf) = &t.trust_floor {
                if !is_known_trust_level(tf) {
                    return Err(ManifestError::UnknownTrustFloor {
                        tool: t.name.clone(),
                        value: tf.clone(),
                    });
                }
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

        // Service-name uniqueness within one plugin. The supervisor
        // keys sidecars on `service.name`, so two `[[services]]`
        // entries in the same plugin can't share a name. (Cross-
        // plugin uniqueness is enforced at hook-registry enable
        // time — that needs context this validator doesn't have.)
        let mut service_names: std::collections::HashSet<&str> = Default::default();
        for s in &self.services {
            if s.name.is_empty() {
                return Err(ManifestError::ServiceEmptyName);
            }
            if !service_names.insert(&s.name) {
                return Err(ManifestError::DuplicateServiceName(s.name.clone()));
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
    fn trust_floor_parses_when_known() {
        let ok = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[tools]]
            name = "send"
            description = "Send a thing."
            trust_floor = "Controller"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        assert_eq!(m.tools[0].trust_floor.as_deref(), Some("Controller"));
    }

    #[test]
    fn trust_floor_rejected_when_unknown() {
        // A typo here used to silently downgrade to "no floor",
        // which is exactly the kind of bug that would let a Signal
        // contact invoke a Controller-only tool — fail loudly.
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[tools]]
            name = "send"
            trust_floor = "Admin"
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        match err {
            ManifestError::UnknownTrustFloor { tool, value } => {
                assert_eq!(tool, "send");
                assert_eq!(value, "Admin");
            }
            other => panic!("expected UnknownTrustFloor, got {other:?}"),
        }
    }

    #[test]
    fn trust_floor_optional_omission_leaves_none() {
        let ok = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[tools]]
            name = "free"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        assert!(m.tools[0].trust_floor.is_none());
    }

    #[test]
    fn sidecar_metadata_parses_with_explicit_health_path() {
        // The sidecar's identity comes from the parent service's
        // `name`; SidecarMeta only carries supervisor-specific
        // knobs (rpc_port + rpc_health_path).
        let ok = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[services]]
            name = "signal-cli"
            image = "asamuzak/signal-cli-rest-api:latest"
            ports = ["8080:8080"]

            [services.sidecar]
            rpc_port = 8080
            rpc_health_path = "/v1/about"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        assert_eq!(m.services.len(), 1);
        assert_eq!(m.services[0].name, "signal-cli");
        let b = m.services[0].sidecar.as_ref().unwrap();
        assert_eq!(b.rpc_port, 8080);
        assert_eq!(b.rpc_health_path, "/v1/about");
    }

    #[test]
    fn sidecar_metadata_defaults_health_path() {
        // Sidecars that omit rpc_health_path get the conventional
        // /healthz default — matches what most service meshes assume.
        let ok = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[services]]
            name = "ocr-worker"
            image = "execlaw/ocr-worker:0.1"

            [services.sidecar]
            rpc_port = 8081
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        assert_eq!(
            m.services[0].sidecar.as_ref().unwrap().rpc_health_path,
            "/healthz",
        );
    }

    #[test]
    fn non_sidecar_service_leaves_sidecar_field_none() {
        // An unsupervised helper service declares a [[services]]
        // entry but no [services.sidecar] table. The sidecar
        // supervisor must NOT pick this up.
        let ok = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[services]]
            name = "ocr-worker"
            image = "execlaw/ocr-worker:0.1"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        assert!(m.services[0].sidecar.is_none());
    }

    #[test]
    fn duplicate_service_names_in_one_plugin_are_rejected() {
        // The supervisor keys sidecars on `service.name`, so two
        // [[services]] entries in one plugin can't share a name.
        // (Cross-plugin dups are caught at hook-registry enable
        // time; this validator handles the within-plugin case.)
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[services]]
            name = "signal-cli"
            image = "x"
            [services.sidecar]
            rpc_port = 8080

            [[services]]
            name = "signal-cli"
            image = "y"
            [services.sidecar]
            rpc_port = 8081
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        match err {
            ManifestError::DuplicateServiceName(n) => assert_eq!(n, "signal-cli"),
            other => panic!("expected DuplicateServiceName, got {other:?}"),
        }
    }

    #[test]
    fn empty_service_name_rejected() {
        // A typo'd / empty service name would silently register the
        // sidecar under "" and then collide with any future plugin
        // doing the same. Reject loudly.
        let bad = r#"
            [plugin]
            id = "p"
            name = "P"
            version = "0.1.0"

            [[services]]
            name = ""
            image = "y"
            [services.sidecar]
            rpc_port = 8080
        "#;
        let err = PluginManifest::parse(bad).unwrap_err();
        match err {
            ManifestError::ServiceEmptyName => {}
            other => panic!("expected ServiceEmptyName, got {other:?}"),
        }
    }

    #[test]
    fn non_transport_sidecar_does_not_need_any_extra_fields() {
        // The whole point of dropping SidecarMeta.channel: an
        // ffmpeg pool / OCR worker / whisper helper is just a
        // companion container with an RPC port. No transport-
        // specific knob required.
        let ok = r#"
            [plugin]
            id = "ocr"
            name = "OCR"
            version = "0.1.0"

            [[services]]
            name = "ocr-worker"
            image = "execlaw/ocr-worker:0.1"

            [services.sidecar]
            rpc_port = 9090
            rpc_health_path = "/ready"
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        let s = &m.services[0];
        assert_eq!(s.name, "ocr-worker");
        assert!(s.sidecar.is_some());
    }

    #[test]
    fn shipped_signal_manifest_parses_cleanly() {
        // Pin the on-disk `plugins/signal/plugin.toml` against this
        // crate's parser so a manifest typo (or an SDK validator
        // change that breaks the existing description prose) is
        // caught at `cargo test` time, not at install time. This
        // mirrors the same "shipped manifest" smoke test we'd want
        // for every bundled plugin once we factor it out.
        const SIGNAL_MANIFEST: &str = include_str!("../../../plugins/signal/plugin.toml");
        let m = PluginManifest::parse(SIGNAL_MANIFEST)
            .expect("plugins/signal/plugin.toml must parse cleanly");
        assert_eq!(m.plugin.id, "signal");
        assert_eq!(m.plugin.version, "0.4.5");
        // The transport icon must propagate from manifest → SDK so
        // the SPA's sidebar can render a Signal-shaped marker on
        // bridged threads. Pin the literal so a typo in the manifest
        // (or a renamed Bootstrap-icons class) gets caught here.
        let transport = m.transport.as_ref().expect("[transport] must be present");
        assert_eq!(transport.icon.as_deref(), Some("chat-quote"));
        // 6 agent-callable tools mirror the selfhosted-claw integration,
        // plus 3 host-internal convention tools (v0.4.5+: set_typing,
        // send_with_attachments, fetch_attachment) registered so the
        // host's auto-bridge code can dial in via call_tool. If we
        // add or remove one, this assertion forces a deliberate
        // update — the audit doc should stay in sync.
        assert_eq!(m.tools.len(), 9);
        // signal.send_message and the group ops MUST carry a
        // Controller floor — that's the security invariant from the
        // audit. Letting a Signal contact use these tools would let
        // them message arbitrary other people via the controller's
        // outbound transport. signal.reply intentionally has no
        // floor (the inbound principal is by definition the one
        // being replied to).
        let send = m
            .tools
            .iter()
            .find(|t| t.name == "signal.send_message")
            .expect("signal.send_message must be declared");
        assert_eq!(send.trust_floor.as_deref(), Some("Controller"));
        let reply = m
            .tools
            .iter()
            .find(|t| t.name == "signal.reply")
            .expect("signal.reply must be declared");
        assert!(
            reply.trust_floor.is_none(),
            "signal.reply must NOT pin a trust floor"
        );
        // Phase B (v0.4.0): every Signal tool is now SCRIPT-tier.
        // host_implemented=false (the default) means tool dispatch
        // hits main.rhai's `tool_call`. The rhai script reaches the
        // sidecar via the `sidecar_http_*` host bindings.
        assert!(
            !send.host_implemented,
            "signal.send_message must be script-tier in v0.4.0+"
        );
        assert!(
            !reply.host_implemented,
            "signal.reply must be script-tier in v0.4.0+"
        );
        for name in [
            "signal.list_groups",
            "signal.create_group",
            "signal.add_group_members",
            "signal.leave_group",
            // Host-internal convention tools (v0.4.5+) — declared
            // so plugin_host.call_tool() can find them when the
            // host's auto-bridge code dials in.
            "signal.set_typing",
            "signal.send_with_attachments",
            "signal.fetch_attachment",
        ] {
            let t = m
                .tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} must be declared"));
            assert!(
                !t.host_implemented,
                "{name} must be script-tier in v0.4.0+"
            );
        }
        // Three admin routes — pairing flow + unregister are
        // plugin-served now.
        assert_eq!(m.admin_routes.len(), 3);
        // Sidecar declaration: signal-cli supervised on /v1/about.
        let signal_cli = m
            .services
            .iter()
            .find(|s| s.name == "signal-cli")
            .expect("[[services]] signal-cli must be declared");
        let sidecar = signal_cli
            .sidecar
            .as_ref()
            .expect("signal-cli must be a supervised sidecar");
        assert_eq!(sidecar.rpc_port, 8080);
        assert_eq!(sidecar.rpc_health_path, "/v1/about");
        // Outbound transport binding.
        let tr = m.transport.as_ref().expect("[transport] must be declared");
        assert_eq!(tr.transport_id, "signal");
        assert!(tr.supports_groups);
        assert!(tr.supports_attachments);
        // Phase 8: a [[ui_panels]] entry registers the Settings →
        // Plugin → Signal config page so the operator pairing UX
        // is reachable from the standard plugin shell.
        assert_eq!(
            m.ui_panels.len(),
            1,
            "exactly one [[ui_panels]] entry expected on the signal plugin",
        );
        assert_eq!(m.ui_panels[0].mount, "admin/plugins/signal");
    }

    #[test]
    fn is_known_trust_level_covers_full_ladder() {
        for s in [
            "Controller",
            "Delegated",
            "KnownTrusted",
            "KnownLimited",
            "UnknownPending",
            "Blocked",
        ] {
            assert!(is_known_trust_level(s), "{s} should be recognised");
        }
        assert!(!is_known_trust_level("admin"));
        assert!(!is_known_trust_level("CONTROLLER"));
        assert!(!is_known_trust_level(""));
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
