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
    CreatedArtifact, HostCapError, HostCapabilities, InboundMessage, RouteOutcome, WsFrameHandler,
    WsSubscriptionHandle,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const WS_MIN_BACKOFF: Duration = Duration::from_millis(500);
const WS_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Special-case backoff for connections the peer closed gracefully
/// (RFC 6455 close frame). The sms-socket-app gateway restarts its
/// WebSocket server on every inbound SMS broadcast — see
/// SmsDeliverReceiver -> ensureStarted -> startServer's
/// stop-and-restart pattern. The new server typically binds within
/// 1-2 seconds; reconnecting at the default 500ms hits the
/// rebind-in-progress window and gets TCP RST, escalating backoff
/// unnecessarily. selfhosted-claw used a fixed 3s delay for the
/// same reason; 2s gives us a comfortable margin.
const WS_GRACEFUL_RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Backoff cap used after we've seen a clean Close frame from the
/// peer. Lower than `WS_MAX_BACKOFF` because a graceful close
/// usually means the peer is intentionally cycling (Android
/// foreground-service restart, gateway restart-on-sms, k8s pod
/// rotation) and will be back within tens of seconds; the default
/// 60s cap leaves the operator staring at "actively refused" much
/// longer than necessary. Resets to MAX as soon as we successfully
/// connect again.
const WS_GRACEFUL_RECONNECT_CAP: Duration = Duration::from_secs(10);

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

    async fn ws_subscribe_with_init(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        init_frames: Vec<String>,
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
        //
        // `init_frames` is replayed on every successful (re)connect
        // before any inbound is read — required by handshake-driven
        // protocols like the sms-socket-app gateway, which only
        // delivers events to subscribers that have introduced
        // themselves.
        tokio::spawn(consumer_loop(
            url,
            headers,
            init_frames,
            on_frame,
            cancel,
            handle.clone(),
        ));

        Ok(handle)
    }

    async fn route_inbound(&self, msg: InboundMessage) -> Result<RouteOutcome, HostCapError> {
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

        // Two stores share the read path: inbound `state_attachments`
        // (transport-minted) AND plugin-rendered `state_artifacts`. Try
        // attachments first since that's the historical hot path; fall
        // back to artifacts so `discord.send_with_attachments` (and the
        // SPA's `/api/attachments/<id>` route) accept chart ids minted
        // by `host_create_attachment`.
        let (path, mime_type) = match store
            .get(&aid)
            .map_err(|e| HostCapError::new(format!("attachment lookup: {e}")))?
        {
            Some(row) => (row.path, row.mime_type),
            None => {
                let art = store
                    .get_artifact(attachment_id)
                    .map_err(|e| HostCapError::new(format!("artifact lookup: {e}")))?
                    .ok_or_else(|| HostCapError::new(format!("no attachment '{attachment_id}'")))?;
                (art.path, art.mime_type)
            }
        };
        // 25 MiB cap mirrors the inbound + outbound caps in the
        // retired signal_transport.rs.
        const MAX_BYTES: u64 = 25 * 1024 * 1024;
        let on_disk = std::fs::metadata(&path)
            .map_err(|e| HostCapError::new(format!("attachment stat: {e}")))?
            .len();
        if on_disk > MAX_BYTES {
            return Err(HostCapError::new(format!(
                "attachment '{attachment_id}' is {on_disk} bytes; max is {MAX_BYTES}"
            )));
        }
        let path_for_read = path.clone();
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(path_for_read))
            .await
            .map_err(|e| HostCapError::new(format!("attachment read join: {e}")))?
            .map_err(|e| HostCapError::new(format!("attachment read: {e}")))?;
        let mime = if mime_type.is_empty() {
            "application/octet-stream"
        } else {
            mime_type.as_str()
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(execlaw_script::AttachmentBytes {
            data_url: format!("data:{mime};base64,{encoded}"),
            mime_type: mime.to_owned(),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn create_artifact_attachment(
        &self,
        plugin_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: Vec<u8>,
        ttl_seconds: Option<i64>,
    ) -> Result<CreatedArtifact, HostCapError> {
        use execlaw_core::attachments::AttachmentStore;
        // Clone the db handle out of &self so the spawn_blocking
        // closure owns a 'static set of inputs — the store itself
        // borrows from &self.state.db and can't escape the method.
        let db = self.state.db.clone();
        let root = plugin_artifacts_root(&self.state);
        let plugin_id_owned = plugin_id.to_owned();
        let filename_owned = filename.to_owned();
        let mime_owned = mime_type.to_owned();
        let now = chrono::Utc::now().timestamp();
        let created = tokio::task::spawn_blocking(move || {
            AttachmentStore::new(&db).insert_plugin_artifact(
                &root,
                &plugin_id_owned,
                &filename_owned,
                &mime_owned,
                &bytes,
                ttl_seconds,
                now,
            )
        })
        .await
        .map_err(|e| HostCapError::new(format!("artifact write join: {e}")))?
        .map_err(|e| HostCapError::new(format!("artifact write: {e}")))?;
        Ok(CreatedArtifact {
            attachment_id: created.attachment_id,
            sha256: created.sha256,
            size_bytes: created.size_bytes,
        })
    }

    async fn vault_get(&self, plugin_id: &str, name: &str) -> Result<Option<String>, HostCapError> {
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

    async fn vault_delete(&self, plugin_id: &str, name: &str) -> Result<bool, HostCapError> {
        use execlaw_core::vault_row::VaultRowStore;
        let store = VaultRowStore::new(&self.state.db);
        store
            .delete(Some(plugin_id), name)
            .map_err(|e| HostCapError::new(format!("vault_delete: {e}")))
    }
}

/// Where plugin-rendered artifacts live on disk. Resolution order:
///   1. `EXECLAW_PLUGIN_ARTIFACTS_DIR` env var — wins outright. Tests
///      set this to a tempdir so a misconfigured run can't escape its
///      sandbox.
///   2. `<home>/.execlaw/plugin_artifacts/` — the production default,
///      matches the rest of execlaw's `~/.execlaw/` data layout.
///   3. `./.execlaw/plugin_artifacts/` (cwd-relative) — last-ditch
///      fallback for environments without a resolvable home dir.
///
/// The directory is created lazily by `insert_plugin_artifact`.
fn plugin_artifacts_root(_state: &AppState) -> PathBuf {
    builtin_artifacts_root_path()
}

/// 2026-05-15 — pulled out as a free function so the built-in
/// `chart.render` tool's dispatch wiring (in `tool_dispatch.rs`)
/// can reach the same path without an `&AppState` borrow. Same
/// resolution chain as `plugin_artifacts_root`: env override →
/// `~/.execlaw/plugin_artifacts/` → cwd-relative fallback.
pub(crate) fn builtin_artifacts_root_path() -> PathBuf {
    if let Ok(p) = std::env::var("EXECLAW_PLUGIN_ARTIFACTS_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match directories::UserDirs::new() {
        Some(d) => d.home_dir().join(".execlaw").join("plugin_artifacts"),
        None => PathBuf::from(".execlaw").join("plugin_artifacts"),
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
    init_frames: Vec<String>,
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
    // Backoff cap. Defaults to WS_MAX_BACKOFF (60s) but gets
    // tightened to WS_GRACEFUL_RECONNECT_CAP (10s) after a clean
    // close so we land on a cycling peer faster. Reset to default
    // on each successful connect.
    let mut cap = WS_MAX_BACKOFF;
    while !cancel.is_cancelled() {
        // Build a client request so we can stamp custom headers
        // on the WS upgrade. Empty `headers` is equivalent to a
        // bare `connect_async(url)`.
        let request = match url.as_str().into_client_request() {
            Ok(mut r) => {
                for (name, value) in &headers {
                    // Validate value first — control chars / non-
                    // visible ASCII / etc. land here. Failing this
                    // is almost always an upstream config bug
                    // (e.g. an api_key with an embedded newline) so
                    // emit a warn loudly enough that operators see
                    // it; otherwise the WS would connect anonymously
                    // and the failure mode is "auth silently doesn't
                    // work" — far worse than a noisy log.
                    let v = match HeaderValue::from_str(value) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                target: "host_caps::ws",
                                %url,
                                header_name = %name,
                                error = %e,
                                "dropping ws header with invalid value (control chars / non-ascii?) — \
                                 the WS connect will proceed without it; check the header source"
                            );
                            continue;
                        }
                    };
                    let parsed_name = match name
                        .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
                    {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::warn!(
                                target: "host_caps::ws",
                                %url,
                                header_name = %name,
                                error = %e,
                                "dropping ws header with invalid name — \
                                 must be a valid HTTP token (no colons, control chars, or whitespace)"
                            );
                            continue;
                        }
                    };
                    r.headers_mut().insert(parsed_name, v);
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
                // Reset backoff + cap on a successful handshake. A
                // peer that just accepted our connection might
                // misbehave next time, so we deliberately re-open
                // the cap to its full default — the post-close
                // tightening only applies to ONE close→reconnect
                // cycle.
                backoff = WS_MIN_BACKOFF;
                cap = WS_MAX_BACKOFF;
                tracing::info!(target: "host_caps::ws", %url, "connected");
                stream
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "host_caps::ws", %url, error = %e, "connect failed; backing off");
                if !sleep_or_cancel(backoff, &cancel).await {
                    return;
                }
                backoff = (backoff * 2).min(cap);
                continue;
            }
            Err(_) => {
                tracing::warn!(target: "host_caps::ws", %url, "connect timed out; backing off");
                if !sleep_or_cancel(backoff, &cancel).await {
                    return;
                }
                backoff = (backoff * 2).min(cap);
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
        handle.set_outbox(Some(out_tx.clone()));

        // Replay any handshake / init frames declared at subscribe
        // time. Goes through the same outbox so the writer half of
        // the select! below picks it up — keeps the write contract
        // single-source. Failure to enqueue means the receiver was
        // already dropped (impossible at this point), so we ignore
        // the SendError.
        if !init_frames.is_empty() {
            tracing::debug!(
                target: "host_caps::ws",
                %url,
                count = init_frames.len(),
                "replaying init frames"
            );
            for frame in &init_frames {
                let _ = out_tx.send(frame.clone());
            }
        }

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
                            // INFO-level frame trace. Truncated to
                            // 400 chars so a chatty stream doesn't
                            // overwhelm the log. Plenty for
                            // diagnosing handshake / event-shape
                            // bugs against an unfamiliar gateway.
                            let preview = if text.len() > 400 {
                                format!("{}…[+{} bytes]", &text[..400], text.len() - 400)
                            } else {
                                text.to_string()
                            };
                            tracing::info!(
                                target: "host_caps::ws",
                                %url,
                                len = text.len(),
                                preview = %preview,
                                "<<< text frame"
                            );
                            let on_frame = on_frame.clone();
                            tokio::spawn(async move {
                                on_frame(text.to_string()).await;
                            });
                        }
                        Some(Ok(Message::Binary(b))) => {
                            tracing::debug!(
                                target: "host_caps::ws",
                                %url,
                                len = b.len(),
                                "<<< binary frame (ignored)"
                            );
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            // CRITICAL: once a WebSocketStream is
                            // split() the library STOPS auto-ponging.
                            // The application is responsible for
                            // mirroring every Ping back as a Pong
                            // carrying the same payload, or the
                            // peer's keepalive timer fires and
                            // closes the connection (sms-socket-app
                            // gateway: ~30s ping, drops after two
                            // missed pongs ≈ 60-90s of silence on
                            // the wire).
                            //
                            // Writing directly from the read arm
                            // is safe — `write` is held exclusively
                            // by the consumer task and only ONE arm
                            // of this `select!` runs per iteration,
                            // so we never race the outbox writer.
                            // Keepalive — fires every ~30s per live WS
                            // (sms-socket gateway, Slack RTM, etc.).
                            // DEBUG so it stays out of the operator
                            // log while still being visible when an
                            // operator is debugging a stalled socket.
                            tracing::debug!(
                                target: "host_caps::ws",
                                %url,
                                len = payload.len(),
                                "<<< ping; >>> pong"
                            );
                            if let Err(e) = write.send(Message::Pong(payload)).await {
                                tracing::warn!(target: "host_caps::ws", %url, error = %e, "pong send failed; closing connection");
                                break;
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            tracing::debug!(target: "host_caps::ws", %url, "<<< pong (ignored)");
                        }
                        Some(Ok(Message::Close(frame))) => {
                            // Surface the close code + reason from
                            // the peer. RFC 6455 codes:
                            //   1000 normal, 1001 going away,
                            //   1002 protocol error, 1003 unsupported
                            //   data, 1006 abnormal close (no frame),
                            //   1008 policy violation, 1009 too big,
                            //   1011 internal error.
                            let (code, reason) = match frame {
                                Some(ref f) => (
                                    u16::from(f.code),
                                    f.reason.to_string(),
                                ),
                                None => (0, String::new()),
                            };
                            tracing::warn!(
                                target: "host_caps::ws",
                                %url,
                                close_code = code,
                                close_reason = %reason,
                                "<<< close frame; reconnecting"
                            );
                            // RFC 6455 §5.5.1: when receiving a Close
                            // frame, an endpoint MUST send a Close
                            // frame in response. Without this, the
                            // peer's close handshake waits the full
                            // timeout — sms-socket-app's
                            // `server.stop(1000)` blocks for a
                            // gratuitous second per client before
                            // unbinding the listening port. Echoing
                            // the same close frame lets the gateway
                            // rebind faster on its post-SMS restart
                            // cycle.
                            let _ = write
                                .send(Message::Close(frame.clone()))
                                .await;
                            // Force the post-loop sleep to use the
                            // graceful-reconnect delay so we land
                            // cleanly on a peer that's mid-restart
                            // (sms-socket-app gateway pattern). For
                            // any close code we treat the close as
                            // graceful — if the peer sent a frame at
                            // all, it intended a clean shutdown,
                            // even if the code is 1011 etc.
                            backoff = WS_GRACEFUL_RECONNECT_DELAY;
                            // Cap subsequent retries at 10s instead
                            // of the default 60s so we discover a
                            // rebound peer faster. After a clean
                            // close the peer is usually
                            // intentionally cycling (Android service
                            // restart, gateway restart-on-sms
                            // pattern) and 60s is way too sparse.
                            cap = WS_GRACEFUL_RECONNECT_CAP;
                            break;
                        }
                        None => {
                            tracing::warn!(
                                target: "host_caps::ws",
                                %url,
                                "stream ended without close frame (peer dropped TCP); reconnecting"
                            );
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
                            let preview = if msg.len() > 400 {
                                format!("{}…[+{} bytes]", &msg[..400], msg.len() - 400)
                            } else {
                                msg.clone()
                            };
                            tracing::info!(
                                target: "host_caps::ws",
                                %url,
                                len = msg.len(),
                                preview = %preview,
                                ">>> text frame"
                            );
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
        backoff = (backoff * 2).min(cap);
    }
}

/// Sleep that wakes early on cancellation. Returns `false` when
/// the cancel token fired (so the caller should exit), `true`
/// when the sleep finished naturally.
async fn sleep_or_cancel(duration: Duration, cancel: &tokio_util::sync::CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => true,
        _ = cancel.cancelled() => false,
    }
}

#[cfg(test)]
mod ws_headers_tests {
    //! Adversarial coverage for the `consumer_loop` header
    //! injection path. The sms-socket gateway authenticates via
    //! `Authorization: Bearer <api_key>` on the WS upgrade — if a
    //! refactor accidentally drops the header, the connect would
    //! still succeed (gateway accepts anonymous → silently
    //! authenticated as the wrong principal) and the failure mode
    //! would be invisible. These tests pin the wire-level
    //! behavior so that drop never happens silently.
    //!
    //! The fixture spins up a real WS server via
    //! `tokio_tungstenite::accept_hdr_async`, captures the upgrade
    //! request's headers in the callback, and asserts what
    //! `consumer_loop` actually sent.
    use super::*;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    /// Spin up a one-shot WS server, return (url, captured_headers_rx).
    /// The server accepts ONE connection, captures its upgrade headers
    /// into the oneshot, then closes the socket.
    async fn one_shot_capture_server() -> (String, oneshot::Receiver<Vec<(String, String)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let captured: Arc<std::sync::Mutex<Vec<(String, String)>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured_for_cb = captured.clone();
            let callback = move |req: &Request, resp: Response| {
                let mut slot = captured_for_cb.lock().expect("mutex");
                for (name, val) in req.headers().iter() {
                    let v = val.to_str().unwrap_or("<binary>").to_owned();
                    slot.push((name.as_str().to_owned(), v));
                }
                Ok(resp)
            };
            // accept_hdr_async drives the handshake AND fires the
            // callback synchronously inside it — by the time it
            // returns Ok, headers are captured.
            let ws = tokio_tungstenite::accept_hdr_async(stream, callback).await;
            let captured_now = captured.lock().expect("mutex").clone();
            let _ = tx.send(captured_now);
            // Hold the socket open just long enough for the client
            // to consider the connect successful — otherwise the
            // client may see an immediate close before our send()
            // pumps. We'll let the consumer's reconnect loop kick
            // in after we drop the socket.
            drop(ws);
        });
        (format!("ws://127.0.0.1:{port}/"), rx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_subscribe_with_headers_sends_authorization_on_upgrade() {
        let (url, captured_rx) = one_shot_capture_server().await;
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());
        let on_frame: WsFrameHandler = Arc::new(|_| Box::pin(async move { /* drop frames */ }));
        let headers = vec![(
            "Authorization".to_owned(),
            "Bearer test-api-key-12345".to_owned(),
        )];
        // Run consumer_loop briefly; cancel as soon as we've
        // captured the upgrade headers.
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            consumer_loop(url, headers, vec![], on_frame, cancel_for_task, handle).await;
        });
        let captured = tokio::time::timeout(Duration::from_secs(3), captured_rx)
            .await
            .expect("server captured headers within 3s")
            .expect("oneshot received");
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let auth = captured
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("authorization"));
        assert!(
            auth.is_some(),
            "expected Authorization header on WS upgrade; got headers={captured:?}"
        );
        assert_eq!(
            auth.unwrap().1,
            "Bearer test-api-key-12345",
            "Authorization header value should arrive verbatim"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_subscribe_with_headers_drops_invalid_header_value_but_keeps_connecting() {
        // A header value with an embedded newline is invalid per
        // HeaderValue::from_str — the loop should drop it,
        // tracing::warn, and proceed to connect anyway. Other
        // (valid) headers must still arrive.
        let (url, captured_rx) = one_shot_capture_server().await;
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());
        let on_frame: WsFrameHandler = Arc::new(|_| Box::pin(async move {}));
        let headers = vec![
            ("X-Bad".to_owned(), "value\nwith-newline".to_owned()),
            ("X-Good".to_owned(), "ok".to_owned()),
        ];
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            consumer_loop(url, headers, vec![], on_frame, cancel_for_task, handle).await;
        });
        let captured = tokio::time::timeout(Duration::from_secs(3), captured_rx)
            .await
            .expect("server captured headers within 3s")
            .expect("oneshot received");
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let bad = captured
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-bad"));
        let good = captured
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-good"));
        assert!(
            bad.is_none(),
            "X-Bad header had a control char in the value and should have been dropped; \
             got headers={captured:?}"
        );
        assert!(
            good.is_some(),
            "X-Good is well-formed and must still arrive on the upgrade; \
             got headers={captured:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_subscribe_with_no_headers_still_connects_and_omits_extras() {
        // Empty headers vec is the default-ws_subscribe path. The
        // upgrade should succeed and carry only the standard
        // tungstenite headers (Host, Upgrade, Connection,
        // Sec-WebSocket-Key, Sec-WebSocket-Version).
        let (url, captured_rx) = one_shot_capture_server().await;
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());
        let on_frame: WsFrameHandler = Arc::new(|_| Box::pin(async move {}));
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            consumer_loop(url, vec![], vec![], on_frame, cancel_for_task, handle).await;
        });
        let captured = tokio::time::timeout(Duration::from_secs(3), captured_rx)
            .await
            .expect("server captured headers within 3s")
            .expect("oneshot received");
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        // Sanity: the standard upgrade headers are present (proves
        // we actually connected).
        let upgrade = captured
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("upgrade"));
        assert!(
            upgrade.is_some(),
            "expected Upgrade header on a successful WS handshake; got={captured:?}"
        );
        // And no Authorization slipped in from somewhere else.
        let auth = captured
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("authorization"));
        assert!(
            auth.is_none(),
            "no headers requested but Authorization arrived: {captured:?}"
        );
    }
}

#[cfg(test)]
mod ws_keepalive_tests {
    //! Regression coverage for WebSocket-level keepalive (Ping →
    //! Pong). The sms-socket-app gateway sends a Ping every ~30s
    //! and drops the connection after two missed pongs (~60-90s
    //! of silence). Tokio-tungstenite STOPS auto-ponging once the
    //! stream is split — the application is responsible for
    //! mirroring every Ping back as a Pong with the same payload.
    //!
    //! Symptom of regression: a connection that's idle for ~90s
    //! gets cleanly closed by the server (`stream ended` log),
    //! and on a busy server, the close happens around the moment
    //! of the next inbound event because that's when the server
    //! tries to deliver to a connection it's already marked dead.
    //!
    //! These tests spin up a real WS server, send a Ping with a
    //! known payload, and assert the consumer_loop returns a
    //! Pong with the same payload within a generous timeout.
    use super::*;
    use futures::sink::SinkExt;
    use futures::stream::StreamExt;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Ping-pong fixture: spawn a server that accepts a single
    /// connection, sends a Ping with a known payload after a
    /// brief settle, then waits for the matching Pong (or any
    /// frame), forwarding what it received via the oneshot.
    async fn ping_then_capture_pong(
        ping_payload: Vec<u8>,
    ) -> (String, oneshot::Receiver<Option<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let ws = match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => ws,
                Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            };
            let (mut server_write, mut server_read) = ws.split();

            // Brief settle — the client (consumer_loop) needs to
            // get into its select! loop. Without this, the Ping
            // can race the read.next() registration.
            tokio::time::sleep(Duration::from_millis(50)).await;

            if server_write
                .send(Message::Ping(ping_payload.into()))
                .await
                .is_err()
            {
                let _ = tx.send(None);
                return;
            }

            // Wait for the response. We expect Message::Pong with
            // the same payload. Bounded — if the consumer never
            // pongs, the test fails on the outer timeout.
            let frame = tokio::time::timeout(Duration::from_secs(5), server_read.next())
                .await
                .ok()
                .flatten();
            let payload = match frame {
                Some(Ok(Message::Pong(p))) => Some(p.to_vec()),
                _ => None,
            };
            let _ = tx.send(payload);
        });
        (format!("ws://127.0.0.1:{port}/"), rx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consumer_responds_to_ping_with_matching_pong() {
        let ping_payload = b"keepalive-12345".to_vec();
        let expected = ping_payload.clone();
        let (url, captured_rx) = ping_then_capture_pong(ping_payload).await;

        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let handle = WsSubscriptionHandle::new(cancel.clone());
        let on_frame: WsFrameHandler = Arc::new(|_| Box::pin(async move { /* drop */ }));
        let cancel_for_task = cancel.clone();
        let task = tokio::spawn(async move {
            consumer_loop(url, vec![], vec![], on_frame, cancel_for_task, handle).await;
        });

        let captured = tokio::time::timeout(Duration::from_secs(8), captured_rx)
            .await
            .expect("test timed out waiting for pong")
            .expect("oneshot received");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let payload = captured.expect(
            "consumer_loop did not respond to Ping with a Pong — \
             tokio-tungstenite stops auto-ponging after split(); \
             see the Message::Ping arm in consumer_loop",
        );
        assert_eq!(
            payload, expected,
            "Pong payload must mirror the Ping payload bit-for-bit \
             per RFC 6455 §5.5.3"
        );
    }
}
