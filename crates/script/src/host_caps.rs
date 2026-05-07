//! [`HostCapabilities`] — the surface a Rhai plugin reaches when
//! it needs the host to do something the script tier can't (yet)
//! do for itself.
//!
//! Why this trait exists: the script tier's `primitives.rs`
//! already exposes pure-data helpers (HTTP, JSON, base64 coming
//! soon, time, hashing). Channel plugins like Signal need more —
//! a long-lived WebSocket consumer, a way to push decoded
//! inbound messages through the host's standard routing pipeline
//! (trust admit → conversation resolve → group classifier →
//! dispatch), and a way to look up where a supervised sidecar is
//! actually listening. None of those make sense as plain Rhai
//! primitives because they need access to host-owned state
//! (sidecar supervisor, AppState, the routing pipeline).
//!
//! `HostCapabilities` is the narrow surface those bindings flow
//! through. The `execlaw-server` crate provides the concrete
//! `Arc<dyn HostCapabilities>` at boot; the script engine forwards
//! it into the per-plugin engine; primitives.rs registers
//! Rhai-callable closures that delegate to the trait methods.
//!
//! Keep this trait small. Every method is a coupling point
//! between the script tier and the host — easy to add, hard to
//! remove without breaking installed plugins.

use std::sync::Arc;

/// Async error returned by host-capability methods. Wraps a
/// human-readable string — Rhai surfaces it as
/// `EvalAltResult::ErrorRuntime` to the calling script. Keep the
/// payload string-shaped so plugin authors can `try { ... } catch
/// (e) { ... }` against it without our concrete error type
/// leaking into the plugin SDK.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HostCapError(pub String);

impl HostCapError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// One inbound message as a channel plugin's frame decoder built
/// it. Channel-agnostic — every transport that pushes through
/// `host_route_inbound` produces a record of this shape.
///
/// The host is the source of truth for what each field MEANS;
/// plugins just populate them from their wire format. New optional
/// fields can be added without breaking installed plugins (the
/// script-side conversion fills `None` for missing keys).
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Channel name (`"signal"`, future `"whatsapp"` / `"email"`
    /// / etc.). Lower-case, stable. The host uses this to key the
    /// transport binding row + drive routing.
    pub channel: String,
    /// Foreign id of the sender — E.164 phone for Signal, jid for
    /// WhatsApp, RFC-5322 address for email. The host's principal
    /// admit pipeline uses this as the routing key.
    pub native_id: String,
    /// Sender's display name when the underlying transport
    /// resolved one (Signal `sourceName`, future WhatsApp push
    /// name, future email "From" name). `None` is fine — the host
    /// falls back to the foreign id.
    pub display_name: Option<String>,
    /// Group id when the message landed in a multi-participant
    /// thread. `None` for 1:1 inbound.
    pub group_id: Option<String>,
    /// Group's user-facing name when the transport supplies one.
    /// Drives auto-rename via
    /// `crate::chats::apply_auto_display_name`.
    pub group_name: Option<String>,
    /// Body text. Empty string is fine for attachment-only
    /// messages.
    pub text: String,
    /// Sender's wall-clock timestamp in milliseconds when the
    /// transport supplies one. Used for read-receipt correlation.
    pub timestamp_ms: Option<i64>,
    /// Per-attachment metadata. Empty vec for text-only inbound.
    /// The host fetches the bytes through the originating
    /// transport (`fetch_attachment` on the plugin's `TransportApi`).
    pub attachments: Vec<InboundAttachmentMeta>,
}

#[derive(Debug, Clone)]
pub struct InboundAttachmentMeta {
    /// Bridge-side attachment id — the host passes this back to
    /// the transport to fetch the bytes.
    pub bridge_id: String,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
}

/// Outcome of [`HostCapabilities::route_inbound`]. Mirrors the
/// existing `RouteOutcome` shape from
/// `crates/server/src/signal_inbound.rs` so plugins can opt to
/// observe the verdict + log appropriately. Most plugins will
/// ignore it.
#[derive(Debug, Clone)]
pub enum RouteOutcome {
    /// Sender is `Blocked`; the inbound was dropped.
    Blocked,
    /// First contact — host minted a principal + binding +
    /// conversation + fired the approval alert. The plugin's
    /// inbound consumer keeps running; the agent doesn't run a
    /// turn until the controller approves.
    ColdContact,
    /// Sender is trusted and addressed the agent — host
    /// dispatched the turn through the normal pipeline.
    Dispatched,
    /// Group inbound where the LLM classifier decided the message
    /// wasn't directed at the agent. Persisted but no turn ran.
    GroupNotAddressed,
}

/// Long-lived background-task handle for a Rhai-driven WebSocket
/// consumer. The plugin gets one per `ws_subscribe` call; dropping
/// (or explicit close) cancels the task and releases the
/// connection. Cheap clone (Arc inside).
#[derive(Clone)]
pub struct WsSubscriptionHandle {
    cancel: Arc<tokio_util::sync::CancellationToken>,
}

impl WsSubscriptionHandle {
    pub fn new(cancel: Arc<tokio_util::sync::CancellationToken>) -> Self {
        Self { cancel }
    }

    /// Cooperative cancellation — the subscriber's reconnect loop
    /// checks the token between frames + on every reconnect tick.
    pub fn close(&self) {
        self.cancel.cancel();
    }

    /// True after `close()` has been called (or the plugin's
    /// engine was dropped).
    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Callback the host invokes per WebSocket text frame. The plugin
/// supplies this when subscribing — it's typically a thin shim
/// that hands the raw frame to a Rhai function:
///
/// ```ignore
/// |frame| {
///     plugin.invoke_async("on_frame", vec![Dynamic::from(frame)]).await;
/// }
/// ```
///
/// The host wraps invocation in `spawn_blocking` so a slow
/// per-frame Rhai handler doesn't pin a tokio worker.
pub type WsFrameHandler =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static>;

/// The trait. Implemented by `execlaw-server::host_caps_impl`;
/// the script tier owns no concrete impl (avoids upstream
/// dependency on AppState).
#[async_trait::async_trait]
pub trait HostCapabilities: Send + Sync {
    /// Resolve a supervised sidecar's host base URL by service
    /// name. Returns `None` when the supervisor hasn't published
    /// a port yet (sidecar still starting / crash-looping) — the
    /// caller falls back to retrying or surfacing the failure to
    /// the plugin's caller.
    ///
    /// Sidecar names match the manifest's `[[services]]` `name`
    /// field. The returned URL is the form `http://127.0.0.1:<port>`
    /// — no trailing slash, no path; plugin appends its own.
    async fn sidecar_url(&self, sidecar_name: &str) -> Option<String>;

    /// Like [`sidecar_url`] but waits up to `timeout_ms` for the
    /// supervisor to publish a port. Plugins call this from
    /// `on_enable` so the WS subscription survives the cold-boot
    /// race where the lifecycle hook fires before the sidecar
    /// supervisor's first reconcile pass has spawned the container.
    ///
    /// Returns the URL on success, `None` on timeout. The polling
    /// cadence is implementation-defined; the default impl polls
    /// every 500ms.
    async fn sidecar_url_blocking(
        &self,
        sidecar_name: &str,
        timeout_ms: u64,
    ) -> Option<String> {
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(url) = self.sidecar_url(sidecar_name).await {
                return Some(url);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// True iff `url` resolves to a registered supervised sidecar.
    /// The script tier's `sidecar_http_*` bindings consult this
    /// before bypassing the SSRF guard — only URLs whose
    /// host:port match a sidecar known to the supervisor get
    /// loopback access.
    async fn is_known_sidecar_url(&self, url: &str) -> bool {
        // Default impl: walk every supervised sidecar and check.
        // Concrete impls can override with a faster path if the
        // supervisor exposes a direct lookup.
        let _ = url;
        false
    }

    /// Subscribe to a long-lived WebSocket. The host owns the
    /// reconnect loop, exponential backoff, and cooperative
    /// shutdown. Per-frame the host invokes `on_frame` on a
    /// `spawn_blocking` thread so a slow Rhai handler can't pin
    /// a tokio worker.
    ///
    /// Returns a [`WsSubscriptionHandle`] for cooperative
    /// cancellation. Dropping the handle without `close()` does
    /// NOT cancel the loop — the host keeps the task alive until
    /// either the plugin is disabled (handle is held by the
    /// engine) or `close()` is called explicitly.
    async fn ws_subscribe(
        &self,
        url: String,
        on_frame: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError>;

    /// Push a decoded inbound message through the host's standard
    /// routing pipeline. The host owns trust admit, principal
    /// mint, conversation resolve, group-address classification,
    /// auto-rename, attachment ingest, and turn dispatch — the
    /// plugin's job ends at handing off the [`InboundMessage`].
    ///
    /// Errors here mean the host couldn't route at all (DB hiccup,
    /// principal mint failed). The classifier verdict (Dispatched
    /// vs GroupNotAddressed) flows back through the Ok variant of
    /// [`RouteOutcome`] for plugins that want to log per-message
    /// telemetry.
    async fn route_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<RouteOutcome, HostCapError>;

    /// Read an attachment's bytes from the host's attachment
    /// store, returned as a base64-encoded data URL the plugin
    /// can ship verbatim through whatever wire format its
    /// transport speaks (Signal's `base64_attachments` field, for
    /// instance). The plugin doesn't need to know the on-disk path
    /// — just the attachment_id the host minted on inbound.
    ///
    /// The host enforces trust scope: the attachment_id must be
    /// associated with the calling plugin's conversation context.
    /// Returns Err for missing rows, oversize payloads, etc.
    async fn get_attachment_bytes_b64(
        &self,
        attachment_id: &str,
    ) -> Result<AttachmentBytes, HostCapError>;

    /// Read a per-plugin secret from `vault_secrets`. Returns the
    /// value as a String when a row exists for `(self.plugin_id,
    /// name)`, or `None` when nothing's been written.
    ///
    /// Plugins use this to read operator-configured secrets they
    /// persist via [`vault_put`] — Pushover's user_key + app_token,
    /// future plugins' API tokens, anything an admin route writes
    /// through a config form. Stored bytes are interpreted as
    /// UTF-8; non-UTF-8 blobs surface an error rather than silently
    /// returning garbage.
    async fn vault_get(
        &self,
        plugin_id: &str,
        name: &str,
    ) -> Result<Option<String>, HostCapError>;

    /// Write a per-plugin secret to `vault_secrets`. Idempotent on
    /// `(plugin_id, name)` — the row's `value_blob` is replaced
    /// and `updated_at` is bumped. Plugins call this from admin-
    /// route handlers when the operator submits a config form.
    async fn vault_put(
        &self,
        plugin_id: &str,
        name: &str,
        value: &str,
    ) -> Result<(), HostCapError>;

    /// Delete a per-plugin secret. Returns `true` when a row was
    /// removed, `false` when no such row existed. Idempotent.
    async fn vault_delete(
        &self,
        plugin_id: &str,
        name: &str,
    ) -> Result<bool, HostCapError>;
}

/// Output of [`HostCapabilities::get_attachment_bytes_b64`].
#[derive(Debug, Clone)]
pub struct AttachmentBytes {
    /// `data:<mime>;base64,<payload>` — formatted to be dropped
    /// directly into a JSON request body field. signal-cli's
    /// bridge accepts this exact shape.
    pub data_url: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

/// Capability set the script engine carries when host-capabilities
/// bindings are available. `None` (the default) leaves the new
/// bindings registered as stubs that error cleanly at call time —
/// scripts without those calls keep working unchanged.
pub type HostCapabilitiesArc = Arc<dyn HostCapabilities>;
