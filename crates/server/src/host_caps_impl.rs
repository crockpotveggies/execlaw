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

    async fn ws_subscribe(
        &self,
        url: String,
        on_frame: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError> {
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());

        // Spawn the long-lived consumer. Reconnect with capped
        // exponential backoff. Cancellation token wakes the loop
        // out of any awaited future.
        tokio::spawn(consumer_loop(url, on_frame, cancel));

        Ok(handle)
    }

    async fn route_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<RouteOutcome, HostCapError> {
        crate::generic_inbound::route_inbound(&self.state, msg).await
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
    on_frame: WsFrameHandler,
    cancel: Arc<tokio_util::sync::CancellationToken>,
) {
    use futures::stream::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut backoff = WS_MIN_BACKOFF;
    while !cancel.is_cancelled() {
        // Connect with a hard timeout so a hung server doesn't
        // wedge the consumer on a single attempt.
        let connect = tokio::time::timeout(
            WS_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(&url),
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

        let (_write, mut read) = stream.split();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!(target: "host_caps::ws", %url, "cancellation requested; closing");
                    return;
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            // Spawn handler so the consumer keeps
                            // pulling the next frame. A slow
                            // Rhai handler can fall behind without
                            // blocking the socket — at worst we
                            // accumulate handler tasks; at best
                            // they run concurrently.
                            let on_frame = on_frame.clone();
                            tokio::spawn(async move {
                                on_frame(text.to_string()).await;
                            });
                        }
                        Some(Ok(Message::Binary(_))) => {
                            // bbernhard / signal-cli emits text
                            // only; future binary-framed protocols
                            // can extend WsFrameHandler later.
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
            }
        }
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
