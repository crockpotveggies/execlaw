//! Concrete [`HostCapabilities`] implementation.
//!
//! Lives in the host crate because the trait methods need
//! [`AppState`] (db, sidecar supervisor, plugin host, event bus).
//! The script tier carries this as `Arc<dyn HostCapabilities>` —
//! plugins reach it through the four Rhai bindings (`sidecar_url`,
//! `ws_subscribe`, `host_route_inbound`, plus the helper plumbing).
//!
//! ### Non-goals
//!
//! - **No SSRF guard for `ws_subscribe` URLs** — channel plugins
//!   legitimately connect to loopback (the supervised sidecar's
//!   published port). The pre-existing `validate_url` SSRF guard
//!   on `http_*` bindings stays unchanged; ws_subscribe trades
//!   that guard for a strict requirement that the URL came from
//!   `sidecar_url(name)` at the script's request — i.e. the
//!   plugin author is reaching their OWN sidecar, never an
//!   arbitrary internal host. (Future: tighten by validating the
//!   URL's host:port matches a sidecar known to the supervisor.)
//!
//! - **No retry / backoff knobs in the trait** — the host's
//!   `WsConsumer` has hardcoded reconnect cadence (capped
//!   exponential, max 60s). Plugins don't need to tune this; if a
//!   plugin author wants different timing they can stop the WS
//!   handle and re-subscribe.

use crate::state::AppState;
use execlaw_script::{
    HostCapError, HostCapabilities, InboundMessage, RouteOutcome, WsFrameHandler,
    WsSubscriptionHandle,
};
use std::sync::Arc;
use std::time::Duration;

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const WS_MIN_BACKOFF: Duration = Duration::from_millis(500);
const WS_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Host-side capability surface backed by an [`AppState`].
/// Cheap to clone (Arc inside) — the script engine carries one
/// `Arc<dyn HostCapabilities>` for every per-plugin engine it
/// builds.
pub struct AppStateHostCapabilities {
    state: AppState,
}

impl AppStateHostCapabilities {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub fn into_arc(self) -> Arc<dyn HostCapabilities> {
        Arc::new(self)
    }
}

#[async_trait::async_trait]
impl HostCapabilities for AppStateHostCapabilities {
    async fn sidecar_url(&self, sidecar_name: &str) -> Option<String> {
        // Look up the supervised sidecar's published host port.
        // Returns None when the sidecar is still spawning or
        // crash-looping — plugin's responsibility to handle.
        let supervisor = self.state.sidecar_supervisor.as_ref()?;
        let port = supervisor.host_port_for(sidecar_name).await?;
        Some(format!("http://127.0.0.1:{port}"))
    }

    async fn is_known_sidecar_url(&self, url: &str) -> bool {
        // Parse host:port out of the URL and compare against every
        // supervised sidecar's published port. Only `http://127.0.0.1:*`
        // qualifies — defends against a plugin smuggling a
        // non-loopback URL through the sidecar_http_* path.
        let parsed = match url::Url::parse(url) {
            Ok(u) => u,
            Err(_) => return false,
        };
        if parsed.scheme() != "http" && parsed.scheme() != "ws" {
            return false;
        }
        if parsed.host_str() != Some("127.0.0.1") {
            return false;
        }
        let port = match parsed.port() {
            Some(p) => p,
            None => return false,
        };
        let supervisor = match self.state.sidecar_supervisor.as_ref() {
            Some(s) => s,
            None => return false,
        };
        // Walk every running sidecar; match on port.
        supervisor.has_published_port(port).await
    }

    async fn ws_subscribe_with_headers(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        on_frame: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError> {
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());

        // Spawn the long-lived consumer. Reconnect with capped
        // exponential backoff. Cancellation token wakes the loop
        // out of any awaited future. The handle's outbox slot is
        // refreshed by the consumer on every successful connect so
        // plugins can `ws_send` text frames back through the same
        // socket (Slack Socket Mode envelope_id ACKs, etc.).
        tokio::spawn(consumer_loop(url, headers, on_frame, cancel, handle.clone()));

        Ok(handle)
    }

    async fn route_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<RouteOutcome, HostCapError> {
        crate::generic_inbound::route_inbound(&self.state, msg).await
    }

    async fn get_attachment_bytes_b64(
        &self,
        attachment_id: &str,
    ) -> Result<execlaw_script::AttachmentBytes, HostCapError> {
        use base64::Engine as _;
        use execlaw_core::attachments::AttachmentStore;
        use execlaw_core::ids::AttachmentId;
        let store = AttachmentStore::new(&self.state.db);
        let aid = AttachmentId::from(attachment_id);
        let row = store
            .get(&aid)
            .map_err(|e| HostCapError::new(format!("attachment lookup: {e}")))?
            .ok_or_else(|| HostCapError::new(format!("no attachment '{attachment_id}'")))?;
        // 25 MiB cap mirrors the inbound + outbound caps in the
        // retired signal_transport.rs.
        const MAX_BYTES: u64 = 25 * 1024 * 1024;
        let on_disk = std::fs::metadata(&row.path)
            .map_err(|e| HostCapError::new(format!("attachment stat: {e}")))?
            .len();
        if on_disk > MAX_BYTES {
            return Err(HostCapError::new(format!(
                "attachment '{attachment_id}' is {on_disk} bytes; max is {MAX_BYTES}"
            )));
        }
        let path = row.path.clone();
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .map_err(|e| HostCapError::new(format!("attachment read join: {e}")))?
            .map_err(|e| HostCapError::new(format!("attachment read: {e}")))?;
        let mime = if row.mime_type.is_empty() {
            "application/octet-stream"
        } else {
            row.mime_type.as_str()
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(execlaw_script::AttachmentBytes {
            data_url: format!("data:{mime};base64,{encoded}"),
            mime_type: mime.to_owned(),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn vault_get(
        &self,
        plugin_id: &str,
        name: &str,
    ) -> Result<Option<String>, HostCapError> {
        use execlaw_core::vault_row::VaultRowStore;
        let store = VaultRowStore::new(&self.state.db);
        let raw = store
            .get(Some(plugin_id), name)
            .map_err(|e| HostCapError::new(format!("vault_get: {e}")))?;
        match raw {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Ok(Some(s)),
                Err(_) => Err(HostCapError::new(format!(
                    "vault row '{name}' for plugin '{plugin_id}' is not valid UTF-8"
                ))),
            },
            None => Ok(None),
        }
    }

    async fn vault_put(
        &self,
        plugin_id: &str,
        name: &str,
        value: &str,
    ) -> Result<(), HostCapError> {
        use execlaw_core::vault_row::VaultRowStore;
        let store = VaultRowStore::new(&self.state.db);
        let now = chrono::Utc::now().timestamp();
        store
            .put(Some(plugin_id), name, value.as_bytes(), now)
            .map_err(|e| HostCapError::new(format!("vault_put: {e}")))?;
        Ok(())
    }

    async fn vault_delete(
        &self,
        plugin_id: &str,
        name: &str,
    ) -> Result<bool, HostCapError> {
        use execlaw_core::vault_row::VaultRowStore;
        let store = VaultRowStore::new(&self.state.db);
        store
            .delete(Some(plugin_id), name)
            .map_err(|e| HostCapError::new(format!("vault_delete: {e}")))
    }
}

/// Long-lived WebSocket consumer task. Reconnects on disconnect
/// with capped exponential backoff. Per-frame the operator-supplied
/// `on_frame` future is awaited; the consumer keeps reading frames
/// even while a frame handler is in flight (handlers run via
/// `tokio::spawn` so a slow Rhai callback doesn't block frame
/// reads).
async fn consumer_loop(
    url: String,
    headers: Vec<(String, String)>,
    on_frame: WsFrameHandler,
    cancel: Arc<tokio_util::sync::CancellationToken>,
    handle: WsSubscriptionHandle,
) {
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut backoff = WS_MIN_BACKOFF;
    while !cancel.is_cancelled() {
        // Build a client request so we can stamp custom headers
        // on the WS upgrade. Empty `headers` is equivalent to a
        // bare `connect_async(url)`.
        let request = match url.as_str().into_client_request() {
            Ok(mut r) => {
                for (name, value) in &headers {
                    if let Ok(v) = HeaderValue::from_str(value) {
                        // Best-effort header insert. Bad header
                        // names (with colons / control chars) get
                        // dropped; we don't fail the whole
                        // subscription over one malformed header.
                        if let Ok(name) = name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>() {
                            r.headers_mut().insert(name, v);
                        }
                    }
                }
                r
            }
            Err(e) => {
                tracing::warn!(target: "host_caps::ws", %url, error = %e, "invalid ws url; aborting consumer");
                return;
            }
        };
        // Connect with a hard timeout so a hung server doesn't
        // wedge the consumer on a single attempt.
        let connect = tokio::time::timeout(
            WS_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await;
        let stream = match connect {
            Ok(Ok((stream, _resp))) => {
                // Reset backoff on a successful handshake.
                backoff = WS_MIN_BACKOFF;
                tracing::info!(target: "host_caps::ws", %url, "connected");
                stream
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "host_caps::ws", %url, error = %e, "connect failed; backing off");
                if !sleep_or_cancel(backoff, &cancel).await {
                    return;
                }
                backoff = (backoff * 2).min(WS_MAX_BACKOFF);
                continue;
            }
            Err(_) => {
                tracing::warn!(target: "host_caps::ws", %url, "connect timed out; backing off");
                if !sleep_or_cancel(backoff, &cancel).await {
                    return;
                }
                backoff = (backoff * 2).min(WS_MAX_BACKOFF);
                continue;
            }
        };

        let (mut write, mut read) = stream.split();

        // Outbox: per-connection mpsc the handle's send() drops
        // text frames into. Refreshed on every reconnect. Plugins
        // calling send() while disconnected get an Err — protocol
        // redelivery handles the gap (e.g. Slack re-sends events
        // whose envelope_id wasn't ACKed in time).
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        handle.set_outbox(Some(out_tx));

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!(target: "host_caps::ws", %url, "cancellation requested; closing");
                    handle.set_outbox(None);
                    return;
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            let on_frame = on_frame.clone();
                            tokio::spawn(async move {
                                on_frame(text.to_string()).await;
                            });
                        }
                        Some(Ok(Message::Binary(_))) => {
                            tracing::debug!(target: "host_caps::ws", %url, "ignoring binary frame");
                        }
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) | None => {
                            tracing::info!(target: "host_caps::ws", %url, "stream ended; reconnecting");
                            break;
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(e)) => {
                            tracing::warn!(target: "host_caps::ws", %url, error = %e, "stream error; reconnecting");
                            break;
                        }
                    }
                }
                outbound = out_rx.recv() => {
                    match outbound {
                        Some(msg) => {
                            if let Err(e) = write.send(Message::Text(msg.into())).await {
                                tracing::warn!(target: "host_caps::ws", %url, error = %e, "ws send failed; closing connection");
                                break;
                            }
                        }
                        None => {
                            // Sender dropped — should only happen if
                            // handle is dropped (engine teardown).
                            tracing::debug!(target: "host_caps::ws", %url, "outbox closed; closing connection");
                            break;
                        }
                    }
                }
            }
        }
        // Disconnect: clear the outbox slot so plugin send() returns
        // a clean error rather than queueing into a dead mpsc.
        handle.set_outbox(None);
        if cancel.is_cancelled() {
            return;
        }
        if !sleep_or_cancel(backoff, &cancel).await {
            return;
        }
        backoff = (backoff * 2).min(WS_MAX_BACKOFF);
    }
}

/// Sleep that wakes early on cancellation. Returns `false` when
/// the cancel token fired (so the caller should exit), `true`
/// when the sleep finished naturally.
async fn sleep_or_cancel(
    duration: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => true,
        _ = cancel.cancelled() => false,
    }
}
