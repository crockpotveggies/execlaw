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

    /// Public webhook routes the plugin exposes for receiving
    /// callbacks from third-party services. Mounted UNAUTHENTICATED
    /// at `/api/webhooks/{plugin_id}{path}` — see `WebhookRouteDecl`
    /// for the security contract (handlers MUST validate a shared
    /// secret in the URL or body). First user: WhatsApp / wuzapi
    /// posting message events here.
    #[serde(default)]
    pub webhook_routes: Vec<WebhookRouteDecl>,

    /// Runtime declaration — which isolation tier (§4.4) the plugin
    /// runs in and how to spawn it. Optional because tool-only
    /// plugins can sometimes be resolved in-process (Phase 3+
    /// feature). For Phase 2 every plugin that declares tools or a
    /// transport MUST set `[runtime]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeDecl>,

    // ---------------- M6 event-driven architecture ------------------

    /// Bus event kinds the plugin PUBLISHES (producer side — distinct
    /// from `event_subscriptions` which is the consumer side). One
    /// row per `(kind, schema, expects_reply)` triple — the host
    /// registers each into `state_registered_event_kinds` at install
    /// / hydrate time so the Automations UI trigger picker lists
    /// them.
    #[serde(default)]
    pub bus_events: Vec<BusEventDecl>,

    /// Reply handlers the plugin advertises — i.e., "I know how to
    /// deliver an agent-authored reply back to channel X." Each row
    /// declares the capability matrix the `ReplyRouter` consults to
    /// decide how to pack rich payloads. The router invokes
    /// `<plugin_id>.send_reply` (the canonical reply-tool name) when
    /// dispatching.
    #[serde(default)]
    pub reply_handlers: Vec<ReplyHandlerDecl>,

    /// Automation flow definitions shipped by the plugin as
    /// "defaults" the operator can use / fork. Each row references a
    /// JSON file relative to the plugin root (e.g.,
    /// `flows/whatsapp_default.json`) containing the full
    /// `AutomationDef` shape. The host imports these at install time
    /// with `source = "plugin:<id>"` + `source_version = <plugin
    /// version>` so the Automations UI can diff them on plugin
    /// upgrade.
    #[serde(default)]
    pub default_automations: Vec<DefaultAutomationDecl>,
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
    /// When true, the tool registers with the host's `call_tool`
    /// dispatch table (so auto-bridge code can dial in) but does
    /// NOT surface in the agent's tool catalog. Used for "host
    /// calls these on behalf of the agent" convention tools like
    /// `signal.set_typing` and `signal.send_with_attachments`
    /// where letting the planner see them causes tool-call loops
    /// (the model decides "let me set typing!" repeatedly until
    /// max_tool_rounds trips).
    #[serde(default, skip_serializing_if = "is_false")]
    pub host_internal: bool,
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
    /// Deployment runtimes this backend can run under. Each entry is
    /// a lowercase identifier matching the supervisor's runtime
    /// dispatch (`"docker"`, `"native"`). When omitted, the supervisor
    /// assumes `["docker"]` — every existing managed-mode preset
    /// (vLLM, Whisper, Kokoro, Piper) ships as a Docker container.
    ///
    /// The first non-Docker entry is `service-ollama`, which on Apple
    /// Silicon must run as a native macOS subprocess because Docker
    /// Desktop on macOS has no Metal passthrough — that plugin
    /// declares `runtimes = ["native"]` so the wizard's plugin store
    /// can warn ahead of time when the operator is on a host class
    /// the backend can't deploy to (e.g. an Ollama-native plugin
    /// shouldn't appear "Ready" on a Linux/NVIDIA host where the
    /// vLLM Docker preset is the right choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes: Option<Vec<String>>,
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

/// Public webhook route a plugin exposes for receiving callbacks
/// from third-party services (e.g. wuzapi → execlaw event POSTs).
/// Mounted by the host under `/api/webhooks/{plugin_id}{path}` —
/// no execlaw JWT is checked (external callers can't hold one).
/// Authentication is described by the optional `auth` field below
/// and enforced by the host BEFORE the request is published to the
/// automation bus or dispatched to the plugin's handler.
///
/// ```toml
/// [[webhook_routes]]
/// method = "POST"
/// path   = "/event"
/// handler = "on_webhook_event"
/// description = "wuzapi posts WhatsApp message events here."
/// auth = { kind = "query_token", query = "token", vault_key = "webhook_secret" }
/// ```
///
/// The Rhai handler signature mirrors `[[admin_routes]]` —
/// `fn on_webhook_event(args)` — `args` carries `method`, `path`,
/// `query`, `body`. Return value becomes the HTTP response body
/// (JSON-serialised). Throw to surface a 500.
///
/// **Security**: this surface is reachable from anyone who can
/// hit the host's bind address. Declare `auth` so the host (not
/// the handler) enforces the check before any side effect. Omitting
/// `auth` is permitted for backward compatibility but logs a
/// `webhook_route_auth_unset` warning at plugin enable and leaves
/// the handler solely responsible for validating the caller —
/// migrate to a declared `auth` mode as soon as practical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookRouteDecl {
    pub method: String,
    pub path: String,
    pub handler: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Host-enforced authentication declaration. When set, the
    /// webhook dispatcher validates the request BEFORE publishing
    /// to the automation bus or invoking the handler. When unset,
    /// behavior falls back to the legacy "handler validates" model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<WebhookAuthDecl>,
}

/// How the host should authenticate an inbound webhook before
/// publishing it to the automation bus or dispatching the handler.
///
/// All variants resolve their secret via the plugin's vault
/// (`vault_secrets` table, scoped to the plugin id) so the operator
/// can rotate the secret without code changes. Comparisons are
/// constant-time. A missing or empty vault value is treated as an
/// authentication failure (NOT as "no auth required").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebhookAuthDecl {
    /// Match `?<query>=<value>` constant-time against the plugin's
    /// vault entry `vault_key`. The query field is redacted from
    /// the automation-bus payload before persistence.
    QueryToken {
        /// Query-string key the secret travels in. Typically `token`.
        query: String,
        /// Name of the per-plugin vault row holding the expected secret.
        vault_key: String,
    },
    /// Compute `HMAC-SHA256(vault[vault_key], body)` and compare
    /// against the request header `header`. Accepts both raw hex
    /// and `sha256=<hex>` (GitHub's `X-Hub-Signature-256` style).
    HmacSha256Header {
        /// HTTP header carrying the signature, e.g. `X-Hub-Signature-256`.
        header: String,
        /// Name of the per-plugin vault row holding the shared secret.
        vault_key: String,
    },
    /// Explicit opt-out: declare that this route is not host-authenticated
    /// and the handler is solely responsible for validating the caller.
    /// Distinct from omitting `auth` only in that no deprecation warning
    /// is logged — operators have acknowledged the model.
    None,
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

// ------------------- M6 event-driven architecture ---------------------

/// One bus event kind the plugin publishes. Imported into
/// `state_registered_event_kinds` at install time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BusEventDecl {
    /// Globally unique, dot-separated, lowercase
    /// (e.g., `whatsapp.message.received`).
    pub kind: String,
    /// One-line human description shown in the Automations UI
    /// trigger picker.
    #[serde(default)]
    pub description: String,
    /// Optional JSON Schema string (`json` field) OR path to a JSON
    /// file (`schema_path` field) describing the `payload` shape.
    /// Surfaced as autocomplete hints when authoring Rhai filters /
    /// templates. Both fields are optional; provide at most one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_schema_path: Option<String>,
    /// Inline JSON Schema (alternative to `payload_schema_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_schema: Option<serde_json::Value>,
    /// Validator gate. When `false`, the automations validator
    /// rejects flows that attach a `SendReply` node to a trigger
    /// with this kind. Default: `false` (safer — explicit reply
    /// declaration required for two-way channels).
    #[serde(default)]
    pub expects_reply: bool,
    /// UI hint for which `OriginRef` variant this kind typically
    /// uses. Free-form string; common values:
    /// `"plugin_channel"`, `"none"`, `"web_socket_session"`,
    /// `"chat_append"`.
    #[serde(default = "default_origin_kind_none")]
    pub default_origin_kind: String,
}

fn default_origin_kind_none() -> String {
    "none".to_string()
}

/// One reply handler the plugin advertises. The router invokes the
/// plugin's `send_reply` tool when an `OriginRef::PluginChannel`
/// resolves to this handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplyHandlerDecl {
    /// Handler name. Convention: same as the plugin id
    /// (`"whatsapp"`, `"signal"`). Used as the `plugin_id` in
    /// `OriginRef::PluginChannel`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    // Capability matrix — conservative defaults (text-only, no
    // attachments). A manifest that forgets to declare a capability
    // degrades gracefully — the router never panics; it just
    // delivers a text-only fallback.
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_attachments: bool,
    #[serde(default)]
    pub supports_inline_chart: bool,
    #[serde(default)]
    pub supports_table: bool,
    #[serde(default)]
    pub supports_card: bool,
    #[serde(default)]
    pub supports_markdown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attachment_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attachments_per_message: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_text_length: Option<u32>,
    /// Allowed MIME prefixes (e.g., `["image/", "application/pdf"]`).
    /// `None` = any mime; empty list = no attachments allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_mime_prefixes: Option<Vec<String>>,
}

/// One default flow the plugin ships. Imported into
/// `state_automations` with `source = "plugin:<id>"` at install.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultAutomationDecl {
    /// Operator-facing flow name. Must be unique across the plugin.
    pub name: String,
    /// Path to the JSON flow file, relative to the plugin's staged
    /// root. The file's content is the full `AutomationDef` shape
    /// (trigger + nodes + edges).
    pub flow_path: String,
    /// `true` if this flow should start enabled. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Short description displayed in the Automations UI when the
    /// operator browses default flows.
    #[serde(default)]
    pub description: String,
}

fn default_true() -> bool {
    true
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
        assert_eq!(m.plugin.version, "0.5.0");
        // The transport icon must propagate from manifest → SDK so
        // the SPA's sidebar can render a Signal-shaped marker on
        // bridged threads. The SPA's ChannelIcon has a brand-SVG
        // override path for "signal" (Bootstrap's bi-signal is the
        // cellular-meter glyph, not the messenger app), so the
        // manifest's `icon = "signal"` is intentional: it indicates
        // the channel + plays nicely with the host-side fallback
        // chain. A future plugin author who copy-pastes the Signal
        // manifest as a starting point for a new transport gets a
        // brand-named icon string they can adjust in one place.
        let transport = m.transport.as_ref().expect("[transport] must be present");
        assert_eq!(transport.icon.as_deref(), Some("signal"));
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
            assert!(!t.host_implemented, "{name} must be script-tier in v0.4.0+");
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
    fn shipped_discord_manifest_parses_cleanly() {
        // Same "shipped manifest parses against the SDK validator"
        // smoke test we have for Signal — catches typos /
        // validator-tightening regressions at `cargo test` time.
        const DISCORD_MANIFEST: &str = include_str!("../../../plugins/discord/plugin.toml");
        let m = PluginManifest::parse(DISCORD_MANIFEST)
            .expect("plugins/discord/plugin.toml must parse cleanly");
        assert_eq!(m.plugin.id, "discord");
        assert_eq!(m.plugin.version, "0.2.0");

        let transport = m.transport.as_ref().expect("[transport] must be present");
        assert_eq!(transport.transport_id, "discord");
        assert_eq!(transport.icon.as_deref(), Some("discord"));
        assert!(transport.supports_attachments);
        assert!(transport.supports_groups);

        // Three agent-callable tools + three host-internal convention
        // tools (set_typing, send_with_attachments, fetch_attachment).
        assert_eq!(m.tools.len(), 6);
        let send = m
            .tools
            .iter()
            .find(|t| t.name == "discord.send_message")
            .expect("discord.send_message must be declared");
        assert_eq!(send.trust_floor.as_deref(), Some("Controller"));
        let reply = m
            .tools
            .iter()
            .find(|t| t.name == "discord.reply")
            .expect("discord.reply must be declared");
        assert!(
            reply.trust_floor.is_none(),
            "discord.reply must NOT pin a trust floor — host fills `to` from the inbound binding"
        );

        // Script-tier — no sidecar, no host-implemented bypasses.
        for t in &m.tools {
            assert!(
                !t.host_implemented,
                "{} must be script-tier in v0.1",
                t.name
            );
        }

        // Admin routes: status, GET config, POST config, POST test.
        assert_eq!(m.admin_routes.len(), 4);

        // No sidecar — gateway is public WSS.
        assert!(
            m.services.is_empty(),
            "discord plugin must not declare any sidecar in v0.1"
        );

        // UI panel mount.
        assert_eq!(m.ui_panels.len(), 1);
        assert_eq!(m.ui_panels[0].mount, "admin/plugins/discord");

        // Alert-source surface declared for v0.2 use.
        assert_eq!(m.alert_sources.len(), 1);
        assert_eq!(m.alert_sources[0].fingerprint_prefix, "plugin.discord");
    }

    #[test]
    fn shipped_google_apps_manifest_parses_cleanly() {
        // Pin the on-disk `plugins/google-apps/plugin.toml` against
        // this crate's parser so a manifest typo (or an SDK validator
        // change that breaks the existing description prose) is caught
        // at `cargo test` time, not at install time.
        //
        // Asserts:
        //   * tool count covers all five modules (Gmail, Calendar,
        //     Contacts, Tasks, Drive).
        //   * destructive tools carry a Controller trust floor.
        //   * read-only tools have NO trust floor.
        //   * the [identity_provider] section is present (contacts
        //     module still serves as an identity provider).
        //   * the oauth scope set includes the union across all five
        //     modules.
        const GOOGLE_APPS_MANIFEST: &str = include_str!("../../../plugins/google-apps/plugin.toml");
        let m = PluginManifest::parse(GOOGLE_APPS_MANIFEST)
            .expect("plugins/google-apps/plugin.toml must parse cleanly");
        assert_eq!(m.plugin.id, "google-apps");
        assert_eq!(m.plugin.version, "0.3.0");

        // Identity provider survives the consolidation — same shape
        // as google-contacts had.
        let idp = m
            .identity_provider
            .as_ref()
            .expect("[identity_provider] must be declared");
        assert!(idp.resolves.iter().any(|r| r == "email"));
        assert!(idp.resolves.iter().any(|r| r == "phone"));

        // Single OAuth account, with union scopes across all 5 modules.
        assert_eq!(m.oauth_accounts.len(), 1);
        let acc = &m.oauth_accounts[0];
        assert_eq!(acc.name, "controller");
        assert_eq!(acc.provider, "google");
        for required in [
            "https://www.googleapis.com/auth/gmail.readonly",
            "https://www.googleapis.com/auth/gmail.send",
            "https://www.googleapis.com/auth/gmail.modify",
            "https://www.googleapis.com/auth/calendar.readonly",
            "https://www.googleapis.com/auth/calendar.events",
            "https://www.googleapis.com/auth/contacts.readonly",
            "https://www.googleapis.com/auth/tasks",
            "https://www.googleapis.com/auth/drive.readonly",
            "https://www.googleapis.com/auth/drive.file",
        ] {
            assert!(
                acc.scopes.iter().any(|s| s == required),
                "manifest must declare scope '{required}'",
            );
        }

        // Tool count: Gmail (10) + Calendar (7) + Contacts (2) + Tasks (6)
        // + Drive (6) = 31. If you add or remove a tool, update both
        // here and the dispatch table in main.rhai.
        assert_eq!(m.tools.len(), 31);

        // EVERY tool pins Controller (v0.3.0). The earlier carve-out
        // for `calendar.check_availability` reasoned that freeBusy
        // was opaque busy/free intervals and thus safe for outside
        // contacts to query — but those intervals are themselves a
        // surveillance + social-engineering primitive. Tightening:
        // every google-apps tool requires Controller, no exceptions.
        // If a "let outside contacts schedule with me" workflow is
        // needed later, it lives in a dedicated booking tool with
        // explicit scope, not as a side effect of leaving freeBusy
        // unguarded.
        const CONTROLLER_FLOOR_TOOLS: &[&str] = &[
            // Gmail — all tools (read AND write touch the mailbox).
            "gmail.list_messages",
            "gmail.search",
            "gmail.get_message",
            "gmail.list_labels",
            "gmail.send_message",
            "gmail.create_draft",
            "gmail.reply",
            "gmail.add_label",
            "gmail.remove_label",
            "gmail.trash",
            // Calendar — every tool, including check_availability
            // (the v0.2.0 carve-out was removed in v0.3.0).
            "calendar.list_calendars",
            "calendar.list_events",
            "calendar.check_availability",
            "calendar.get_event",
            "calendar.create_event",
            "calendar.update_event",
            "calendar.delete_event",
            // Contacts.
            "contacts.search",
            "contacts.list",
            // Tasks — all tools.
            "tasks.list_lists",
            "tasks.list_tasks",
            "tasks.create_task",
            "tasks.update_task",
            "tasks.complete_task",
            "tasks.delete_task",
            // Drive — all tools.
            "drive.search",
            "drive.get_file_metadata",
            "drive.list_folder",
            "drive.read_file",
            "drive.create_file",
            "drive.share",
        ];
        for name in CONTROLLER_FLOOR_TOOLS {
            let t = m
                .tools
                .iter()
                .find(|t| t.name == *name)
                .unwrap_or_else(|| panic!("{name} must be declared"));
            assert_eq!(
                t.trust_floor.as_deref(),
                Some("Controller"),
                "{name} must pin Controller floor",
            );
        }

        // No carve-outs in v0.3.0 — every declared tool must pin
        // Controller. If you're adding a new tool that can SAFELY be
        // called from outside principals (rare; needs explicit
        // justification), add it to a new NO_FLOOR_TOOLS list here
        // AND document why in the manifest comment.
        assert_eq!(
            CONTROLLER_FLOOR_TOOLS.len(),
            m.tools.len(),
            "every declared tool must pin a Controller trust floor in v0.3.0+",
        );
        for t in &m.tools {
            assert_eq!(
                t.trust_floor.as_deref(),
                Some("Controller"),
                "{} must pin Controller floor (no carve-outs in v0.3.0+)",
                t.name,
            );
        }

        // Runtime tier — script, source = main.rhai.
        let rt = m.runtime.as_ref().expect("[runtime] must be declared");
        assert_eq!(rt.parsed_tier(), Some(RuntimeTier::Script));
        assert_eq!(rt.source.as_deref(), Some("main.rhai"));
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
    fn inference_backend_decl_runtimes_omitted_is_none() {
        // Backwards-compat: every shipped manifest pre-Apple-Silicon
        // omits `runtimes`. The default must be `None` (which the
        // supervisor reads as "docker only"), not an empty Vec — an
        // empty Vec would imply "no runtimes supported," which is
        // exactly the opposite intent.
        let ok = r#"
            [plugin]
            id = "x"
            name = "X"
            version = "0.1.0"

            [inference_backend]
            openai_compatible_endpoint = "/v1"
            supports_streaming = true
            supports_tools = true
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        let ib = m.inference_backend.as_ref().unwrap();
        assert!(ib.runtimes.is_none());
    }

    #[test]
    fn inference_backend_decl_runtimes_native_parses() {
        // The service-ollama plugin (Apple Silicon) declares native
        // because Docker Desktop on macOS has no Metal passthrough.
        let ok = r#"
            [plugin]
            id = "service-ollama"
            name = "Ollama"
            version = "0.1.0"

            [inference_backend]
            openai_compatible_endpoint = "/v1"
            supports_streaming = true
            supports_tools = true
            runtimes = ["native"]
        "#;
        let m = PluginManifest::parse(ok).unwrap();
        let ib = m.inference_backend.as_ref().unwrap();
        assert_eq!(ib.runtimes.as_deref(), Some(&["native".to_owned()][..]));
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

    // -------------- M6 manifest sections ----------------------------

    #[test]
    fn parses_bus_events_and_reply_handlers() {
        let s = r#"
            [plugin]
            id = "whatsapp"
            name = "WhatsApp"
            version = "0.1.0"

            [[bus_events]]
            kind = "whatsapp.message.received"
            description = "Inbound WA message (DM or group)"
            expects_reply = true
            default_origin_kind = "plugin_channel"

            [[bus_events]]
            kind = "whatsapp.delivery.failed"
            description = "WA reported a delivery failure"
            expects_reply = false

            [[reply_handlers]]
            name = "whatsapp"
            description = "Send agent text back to the originating WA chat"
            supports_streaming = false
            supports_attachments = true
            supports_markdown = true
            max_attachment_size_bytes = 16777216
            max_attachments_per_message = 1
            max_text_length = 4096
            allowed_mime_prefixes = ["image/", "video/", "audio/", "application/pdf"]
        "#;
        let m = PluginManifest::parse(s).unwrap();
        assert_eq!(m.bus_events.len(), 2);
        assert_eq!(m.bus_events[0].kind, "whatsapp.message.received");
        assert!(m.bus_events[0].expects_reply);
        assert_eq!(m.bus_events[0].default_origin_kind, "plugin_channel");
        assert!(!m.bus_events[1].expects_reply);

        assert_eq!(m.reply_handlers.len(), 1);
        let h = &m.reply_handlers[0];
        assert!(h.supports_attachments);
        assert!(h.supports_markdown);
        assert!(!h.supports_streaming);
        assert_eq!(h.max_attachment_size_bytes, Some(16_777_216));
        assert_eq!(h.max_attachments_per_message, Some(1));
        assert_eq!(h.max_text_length, Some(4096));
        assert_eq!(
            h.allowed_mime_prefixes.as_ref().unwrap(),
            &vec![
                "image/".to_string(),
                "video/".to_string(),
                "audio/".to_string(),
                "application/pdf".to_string(),
            ]
        );
    }

    #[test]
    fn reply_handler_conservative_defaults_when_unset() {
        let s = r#"
            [plugin]
            id = "minimal"
            name = "Minimal"
            version = "0.1.0"

            [[reply_handlers]]
            name = "minimal"
        "#;
        let m = PluginManifest::parse(s).unwrap();
        let h = &m.reply_handlers[0];
        assert!(!h.supports_streaming);
        assert!(!h.supports_attachments);
        assert!(!h.supports_markdown);
        assert!(h.max_attachment_size_bytes.is_none());
    }

    #[test]
    fn parses_default_automations() {
        let s = r#"
            [plugin]
            id = "whatsapp"
            name = "WhatsApp"
            version = "0.1.0"

            [[default_automations]]
            name = "WhatsApp default"
            flow_path = "flows/whatsapp_default.json"
            description = "Default flow: route incoming WA messages to the agent"

            [[default_automations]]
            name = "WhatsApp delivery alerts"
            flow_path = "flows/delivery_alerts.json"
            enabled = false
            description = "Fire alerts on WA delivery failures"
        "#;
        let m = PluginManifest::parse(s).unwrap();
        assert_eq!(m.default_automations.len(), 2);
        assert_eq!(m.default_automations[0].flow_path, "flows/whatsapp_default.json");
        assert!(m.default_automations[0].enabled);
        assert!(!m.default_automations[1].enabled);
    }

    #[test]
    fn bus_event_with_inline_schema() {
        let s = r#"
            [plugin]
            id = "calendar"
            name = "Calendar"
            version = "0.1.0"

            [[bus_events]]
            kind = "calendar.event.starting_soon"
            description = "A calendar event is starting within 15 minutes"
            expects_reply = false
            [bus_events.payload_schema]
            type = "object"
            required = ["attendee_name", "start_at"]
        "#;
        let m = PluginManifest::parse(s).unwrap();
        assert_eq!(m.bus_events.len(), 1);
        assert!(m.bus_events[0].payload_schema.is_some());
        assert_eq!(
            m.bus_events[0]
                .payload_schema
                .as_ref()
                .unwrap()
                .get("type")
                .and_then(|v| v.as_str()),
            Some("object")
        );
    }
}
