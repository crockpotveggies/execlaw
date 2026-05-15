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
    /// Did the underlying transport's wire format flag this message
    /// as mentioning the agent? Slack populates `Some(true)` when
    /// `<@bot-user-id>` appears in the channel-message text;
    /// transports without a structured mention concept (Signal,
    /// WhatsApp, SMS, email) leave this `None` and the host falls
    /// back to a substring check on the agent's `display_name`.
    ///
    /// Agent-routing (`should_dispatch_to_agent`) treats `Some(true)`
    /// as a hard "directed" verdict and short-circuits the LLM
    /// classifier — saving inference cost AND ensuring an explicit
    /// `@agent` mention is never misclassified as group banter.
    pub mention_of_self: Option<bool>,
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
/// connection.
///
/// Carries an outbound sender slot so plugins can write back over
/// the same socket — required for protocols with bidirectional
/// envelopes (Slack Socket Mode `envelope_id` ACKs, Discord
/// gateway heartbeats, MCP-over-WS, etc.). The slot is `None` while
/// disconnected; the consumer loop refreshes it on each successful
/// connect. `send()` while disconnected returns Err — the protocol
/// usually assumes the server will re-deliver, and silently
/// queueing across reconnect would replay stale ACKs.
///
/// Also carries a periodic-keepalive cancellation slot so
/// `ws_set_keepalive` can replace a previously-installed timer
/// without leaking the prior task (Discord's gateway raises the
/// heartbeat interval mid-session via a new Hello on Resume; the
/// plugin re-registers and the prior task must die). Closing the
/// handle cancels both the consumer loop and any active keepalive.
///
/// Cheap clone (Arc inside everything).
#[derive(Clone)]
pub struct WsSubscriptionHandle {
    cancel: Arc<tokio_util::sync::CancellationToken>,
    outbox: Arc<std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
    keepalive: Arc<std::sync::RwLock<Option<tokio_util::sync::CancellationToken>>>,
}

impl WsSubscriptionHandle {
    pub fn new(cancel: Arc<tokio_util::sync::CancellationToken>) -> Self {
        Self {
            cancel,
            outbox: Arc::new(std::sync::RwLock::new(None)),
            keepalive: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Cooperative cancellation — the subscriber's reconnect loop
    /// checks the token between frames + on every reconnect tick.
    /// Also cancels any periodic keepalive timer installed via
    /// `ws_set_keepalive`.
    pub fn close(&self) {
        self.cancel.cancel();
        if let Ok(mut slot) = self.keepalive.write() {
            if let Some(tok) = slot.take() {
                tok.cancel();
            }
        }
    }

    /// True after `close()` has been called (or the plugin's
    /// engine was dropped).
    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Set or clear the active sender. The consumer loop swaps
    /// the slot on every successful connect (Some) and on
    /// disconnect (None).
    pub fn set_outbox(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<String>>) {
        if let Ok(mut slot) = self.outbox.write() {
            *slot = tx;
        }
    }

    /// Install a new keepalive cancellation token, cancelling any
    /// previously-installed one. The keepalive task watches this
    /// token + the handle's main cancel; replacing the token kills
    /// the old timer cleanly so a re-`ws_set_keepalive` (e.g.
    /// Discord Resume issues a fresh Hello with a possibly-changed
    /// interval) doesn't run two timers in parallel.
    pub fn install_keepalive(&self, token: tokio_util::sync::CancellationToken) {
        if let Ok(mut slot) = self.keepalive.write() {
            if let Some(prev) = slot.replace(token) {
                prev.cancel();
            }
        }
    }

    /// True when a keepalive timer is currently installed. Used by
    /// tests + diagnostics; plugins don't need to check this.
    pub fn has_keepalive(&self) -> bool {
        self.keepalive
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|_| ()))
            .is_some()
    }

    /// Submit a text frame to be written on the active socket.
    /// Returns Err when no active connection (plugin should let
    /// the protocol's redelivery semantics handle the gap rather
    /// than blind-retry).
    pub fn send(&self, msg: String) -> Result<(), HostCapError> {
        let slot = self
            .outbox
            .read()
            .map_err(|_| HostCapError::new("ws send: outbox lock poisoned"))?;
        match slot.as_ref() {
            Some(tx) => tx.send(msg).map_err(|_| {
                HostCapError::new("ws send: writer task gone (likely just disconnected)")
            }),
            None => Err(HostCapError::new(
                "ws send: not connected (plugin should rely on protocol-level redelivery)",
            )),
        }
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

/// Callback the host invokes periodically to produce the next
/// keepalive frame body. Plugins use this for application-layer
/// heartbeats that carry mutable state — Discord's gateway sends
/// `{"op":1,"d":<last_sequence>}` every Hello-supplied interval,
/// where `last_sequence` is the highest sequence the bot has seen
/// from the server. The callback runs each tick, so it always
/// captures the latest state.
///
/// Returning an empty string skips the send for that tick. Useful
/// for "we're mid-handshake and don't yet have a valid frame to
/// send" — the timer keeps firing without spamming the wire.
pub type WsKeepaliveCallback =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, String> + Send + Sync + 'static>;

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
    async fn sidecar_url_blocking(&self, sidecar_name: &str, timeout_ms: u64) -> Option<String> {
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
    ) -> Result<WsSubscriptionHandle, HostCapError> {
        // Default delegates with no headers and no init frames so
        // existing impls only need to implement the full method.
        self.ws_subscribe_with_init(url, vec![], vec![], on_frame)
            .await
    }

    /// Same as [`ws_subscribe`] but adds custom HTTP headers on the
    /// upgrade request. Required for protocols that authenticate
    /// via `Authorization: Bearer …` on connect (sms-socket
    /// gateway, some MCP-over-WS implementations) — alternatives
    /// to URL-query-param tokens (Slack Socket Mode) where the
    /// server only honors headers.
    ///
    /// `headers` is a list of `(name, value)` pairs. Empty vec is
    /// equivalent to calling `ws_subscribe`.
    async fn ws_subscribe_with_headers(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        on_frame: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError> {
        // Default delegates to the full ws_subscribe_with_init.
        self.ws_subscribe_with_init(url, headers, vec![], on_frame)
            .await
    }

    /// Full subscribe surface: connect-time HTTP headers PLUS init
    /// frames the host writes on every successful (re)connect
    /// before the per-frame callback starts firing.
    ///
    /// Required for protocols where the server expects a
    /// client-initiated handshake to register the connection as an
    /// active subscriber. The sms-socket-app gateway is one such:
    /// without an initial `getGatewayState` envelope (and a
    /// `rehydrate` to catch missed events) the gateway never
    /// delivers live `sms.received` frames to this connection. The
    /// resulting "connected but no traffic" state crashes the
    /// gateway when an SMS arrives, because nothing has registered
    /// to consume the inbound event.
    ///
    /// `init_frames` is an ordered list of UTF-8 text frames. The
    /// host writes them in order, before reading any inbound, on
    /// every successful connect — including reconnects after a
    /// disconnect. An empty vec is equivalent to
    /// [`ws_subscribe_with_headers`].
    async fn ws_subscribe_with_init(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        init_frames: Vec<String>,
        on_frame: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError>;

    /// Install a periodic application-layer keepalive on an existing
    /// bidi WS handle. Every `interval_ms` the host invokes
    /// `callback`, takes the returned String, and writes it to the
    /// handle's outbox as a text frame. Returning an empty string
    /// from the callback skips that tick — useful when the plugin
    /// is mid-handshake and doesn't yet have a valid frame body.
    ///
    /// This is the *application*-layer keepalive — Discord gateway's
    /// `{"op":1,"d":<sequence>}`, MCP-over-WS's ping envelope, etc.
    /// RFC-6455 protocol-level Ping/Pong is auto-handled by the
    /// consumer loop and does NOT need plugin participation.
    ///
    /// When `jitter_first` is true, the first tick fires after a
    /// random fraction of `interval_ms` (0..1 × interval). Discord's
    /// gateway docs require this so a fleet of reconnecting bots
    /// doesn't synchronize their first heartbeat into a thundering
    /// herd. Subsequent ticks fire exactly on the interval.
    ///
    /// Replaces any previously-installed keepalive on the same
    /// handle (the prior timer is cancelled before this one starts).
    /// Closing the handle cancels the keepalive. Send failures
    /// during a disconnect window are silently dropped — the
    /// consumer loop's reconnect will fire a fresh Hello and the
    /// plugin re-registers from there.
    async fn ws_set_keepalive(
        &self,
        handle: &WsSubscriptionHandle,
        interval_ms: u64,
        jitter_first: bool,
        callback: WsKeepaliveCallback,
    ) -> Result<(), HostCapError> {
        if interval_ms == 0 {
            return Err(HostCapError::new(
                "ws_set_keepalive: interval_ms must be > 0",
            ));
        }
        let token = tokio_util::sync::CancellationToken::new();
        handle.install_keepalive(token.clone());

        let handle = handle.clone();
        let interval = std::time::Duration::from_millis(interval_ms);
        tokio::spawn(async move {
            // Jittered first delay — Discord requires this, others
            // benefit from it. When jitter_first is false the first
            // tick fires after a full interval (NOT immediately;
            // immediately-on-Hello can race with the server's
            // expected handshake order on some protocols).
            let first_delay = if jitter_first {
                let frac: f64 = rand::Rng::gen_range(&mut rand::thread_rng(), 0.0_f64..1.0_f64);
                std::time::Duration::from_millis((interval_ms as f64 * frac) as u64)
            } else {
                interval
            };
            tokio::select! {
                _ = tokio::time::sleep(first_delay) => {}
                _ = token.cancelled() => return,
            }
            // First tick
            if !ws_keepalive_send_one(&handle, &callback).await {
                return;
            }
            // Subsequent ticks on the steady interval.
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick of `interval` fires immediately by design;
            // consume it so the cadence is `interval` from now.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = ticker.tick() => {
                        if !ws_keepalive_send_one(&handle, &callback).await {
                            return;
                        }
                    }
                }
            }
        });

        Ok(())
    }

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
    async fn route_inbound(&self, msg: InboundMessage) -> Result<RouteOutcome, HostCapError>;

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
    async fn vault_get(&self, plugin_id: &str, name: &str) -> Result<Option<String>, HostCapError>;

    /// Write a per-plugin secret to `vault_secrets`. Idempotent on
    /// `(plugin_id, name)` — the row's `value_blob` is replaced
    /// and `updated_at` is bumped. Plugins call this from admin-
    /// route handlers when the operator submits a config form.
    async fn vault_put(&self, plugin_id: &str, name: &str, value: &str)
    -> Result<(), HostCapError>;

    /// Delete a per-plugin secret. Returns `true` when a row was
    /// removed, `false` when no such row existed. Idempotent.
    async fn vault_delete(&self, plugin_id: &str, name: &str) -> Result<bool, HostCapError>;

    /// Persist plugin-rendered bytes as an attachment the agent can
    /// hand to a transport plugin (`{transport}.send_with_attachments`),
    /// or that the SPA can fetch via `/api/attachments/<id>`.
    ///
    /// The returned `attachment_id` shares a namespace with the inbound
    /// attachment ids minted by transport plugins — both flow through
    /// [`get_attachment_bytes_b64`] for read. Plugin artifacts have an
    /// optional TTL (default policy chosen by the plugin) so chart
    /// renders don't accumulate forever; the ephemeral sweeper culls
    /// expired rows + their on-disk bytes.
    ///
    /// `mime_type` is what transports announce to the recipient and
    /// what the SPA's download endpoint puts in `Content-Type`. Common
    /// values: `image/png`, `image/svg+xml`, `application/pdf`.
    ///
    /// `filename` is what the operator sees in the save-as dialog +
    /// what transports use as the attachment name; it does NOT
    /// determine the on-disk path (which is sha256-named for content
    /// addressing).
    async fn create_artifact_attachment(
        &self,
        plugin_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: Vec<u8>,
        ttl_seconds: Option<i64>,
    ) -> Result<CreatedArtifact, HostCapError>;
}

/// Output of [`HostCapabilities::create_artifact_attachment`]. Returned
/// to the calling Rhai script verbatim — the agent gets the
/// attachment_id, and downstream tools (`discord.send_with_attachments`,
/// etc.) accept the same string.
#[derive(Debug, Clone)]
pub struct CreatedArtifact {
    pub attachment_id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Helper for the default `ws_set_keepalive` impl. Invokes the
/// callback, sends the result if non-empty, returns false when the
/// handle has been closed (signalling the spawned task to exit).
/// A `send()` failure during a disconnect window is NOT fatal —
/// the task should keep running so the plugin's re-register-after-
/// reconnect path can fire on the next Hello.
async fn ws_keepalive_send_one(
    handle: &WsSubscriptionHandle,
    callback: &WsKeepaliveCallback,
) -> bool {
    if handle.is_closed() {
        return false;
    }
    let frame = callback().await;
    if frame.is_empty() {
        return true;
    }
    let _ = handle.send(frame);
    true
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

#[cfg(test)]
mod ws_set_keepalive_default_impl_tests {
    //! Coverage for the default-trait-method
    //! `HostCapabilities::ws_set_keepalive`. The default impl
    //! spawns a tokio task that watches a per-handle cancel
    //! token and writes to the handle's outbox on each tick.
    //!
    //! These tests bypass the real WS consumer by attaching the
    //! outbox to a plain `mpsc` we can read directly — no socket
    //! is opened. Lets us assert timing + cancellation behaviour
    //! deterministically without standing up a fake server.
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Minimal `HostCapabilities` impl that only exists so we can
    /// call the default `ws_set_keepalive`. All other methods are
    /// `unreachable!()` — the default impl doesn't call back into
    /// `self` for anything.
    struct StubCaps;

    #[async_trait::async_trait]
    impl HostCapabilities for StubCaps {
        async fn sidecar_url(&self, _: &str) -> Option<String> {
            None
        }
        async fn ws_subscribe_with_init(
            &self,
            _: String,
            _: Vec<(String, String)>,
            _: Vec<String>,
            _: WsFrameHandler,
        ) -> Result<WsSubscriptionHandle, HostCapError> {
            unreachable!("test stub")
        }
        async fn route_inbound(&self, _: InboundMessage) -> Result<RouteOutcome, HostCapError> {
            unreachable!("test stub")
        }
        async fn get_attachment_bytes_b64(&self, _: &str) -> Result<AttachmentBytes, HostCapError> {
            unreachable!("test stub")
        }
        async fn vault_get(&self, _: &str, _: &str) -> Result<Option<String>, HostCapError> {
            Ok(None)
        }
        async fn vault_put(&self, _: &str, _: &str, _: &str) -> Result<(), HostCapError> {
            Ok(())
        }
        async fn vault_delete(&self, _: &str, _: &str) -> Result<bool, HostCapError> {
            Ok(false)
        }
        async fn create_artifact_attachment(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Vec<u8>,
            _: Option<i64>,
        ) -> Result<CreatedArtifact, HostCapError> {
            unreachable!("test stub")
        }
    }

    fn handle_with_outbox() -> (WsSubscriptionHandle, mpsc::UnboundedReceiver<String>) {
        let cancel = Arc::new(CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel);
        let (tx, rx) = mpsc::unbounded_channel();
        handle.set_outbox(Some(tx));
        (handle, rx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fires_periodically_on_active_handle() {
        // No jitter so first tick lands at +interval and timing
        // is predictable. interval=50ms gives a generous margin
        // against test-runner scheduling jitter while keeping the
        // whole test under a second.
        let (handle, mut rx) = handle_with_outbox();
        let caps = StubCaps;
        let cb: WsKeepaliveCallback = Arc::new(|| Box::pin(async { "tick".to_string() }));
        caps.ws_set_keepalive(&handle, 50, false, cb).await.unwrap();
        let f1 = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("first tick timed out — keepalive task didn't fire")
            .expect("outbox closed");
        let f2 = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("second tick timed out — keepalive task fired once then stopped")
            .expect("outbox closed");
        assert_eq!(f1, "tick");
        assert_eq!(f2, "tick");
        assert!(
            handle.has_keepalive(),
            "handle should report keepalive installed"
        );
        handle.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_callback_return_skips_the_tick() {
        // Plugins use empty-string return to mean "mid-handshake;
        // don't fire". The timer must keep running (so the next
        // tick re-checks) but no frame should hit the wire.
        let (handle, mut rx) = handle_with_outbox();
        let caps = StubCaps;
        let cb: WsKeepaliveCallback = Arc::new(|| Box::pin(async { String::new() }));
        caps.ws_set_keepalive(&handle, 50, false, cb).await.unwrap();
        let r = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            r.is_err(),
            "expected no frame within 200ms when callback returns empty"
        );
        // And the timer is still alive (didn't return-early).
        assert!(handle.has_keepalive());
        handle.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replace_cancels_prior_timer() {
        // Install A with a long interval so its first tick won't
        // fire before we replace it. Then install B with a short
        // interval and observe only B frames — proving A was
        // cancelled cleanly before it ever wrote anything.
        let (handle, mut rx) = handle_with_outbox();
        let caps = StubCaps;
        let cb_a: WsKeepaliveCallback = Arc::new(|| Box::pin(async { "A".to_string() }));
        caps.ws_set_keepalive(&handle, 10_000, false, cb_a)
            .await
            .unwrap();
        // No way for A to have fired yet (interval is 10s).
        let cb_b: WsKeepaliveCallback = Arc::new(|| Box::pin(async { "B".to_string() }));
        caps.ws_set_keepalive(&handle, 50, false, cb_b)
            .await
            .unwrap();
        for i in 0..3 {
            let f = tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("tick {i} timed out after replace"))
                .expect("outbox closed");
            assert_eq!(
                f, "B",
                "tick {i}: only B frames should arrive after replace"
            );
        }
        handle.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_stops_timer() {
        let (handle, mut rx) = handle_with_outbox();
        let caps = StubCaps;
        let cb: WsKeepaliveCallback = Arc::new(|| Box::pin(async { "tick".to_string() }));
        caps.ws_set_keepalive(&handle, 50, false, cb).await.unwrap();
        // Wait for at least one frame so we know the timer started.
        let _ = tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .expect("first tick timed out")
            .expect("outbox closed");
        handle.close();
        // Drain any frame already in-flight.
        tokio::time::sleep(Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}
        // Past close + drain, no further frames should arrive.
        let r = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(r.is_err(), "no frame should arrive after close()");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_interval_errors() {
        let cancel = Arc::new(CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel);
        let caps = StubCaps;
        let cb: WsKeepaliveCallback = Arc::new(|| Box::pin(async { String::new() }));
        let r = caps.ws_set_keepalive(&handle, 0, false, cb).await;
        assert!(r.is_err(), "interval_ms=0 must error");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jitter_first_tick_under_full_interval() {
        // With a 1000ms interval and jitter, the first tick must
        // land somewhere in 0..1000ms. We assert it arrives well
        // before a full interval (< 950ms) — the probability of
        // jitter landing in the top 5% is 5% even at the
        // worst-case end of the range, but we repeat with retry
        // tolerance to keep this from being flaky. If this
        // empirically flakes we can lower the threshold or use a
        // seedable RNG; for now 950ms is comfortably non-tight.
        for attempt in 0..3 {
            let (handle, mut rx) = handle_with_outbox();
            let caps = StubCaps;
            let cb: WsKeepaliveCallback = Arc::new(|| Box::pin(async { "tick".to_string() }));
            caps.ws_set_keepalive(&handle, 1000, true, cb)
                .await
                .unwrap();
            let start = std::time::Instant::now();
            let _ = tokio::time::timeout(Duration::from_millis(1200), rx.recv())
                .await
                .expect("jittered first tick timed out");
            let elapsed = start.elapsed();
            handle.close();
            if elapsed < Duration::from_millis(950) {
                return;
            }
            tracing::warn!(
                attempt,
                elapsed_ms = elapsed.as_millis() as u64,
                "jittered first tick landed in the top 5% of the interval; retrying"
            );
        }
        panic!("jittered first tick was >=950ms three times in a row — jitter likely broken");
    }
}
