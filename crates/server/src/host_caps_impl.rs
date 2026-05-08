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
        let on_frame: WsFrameHandler =
            Arc::new(|_| Box::pin(async move { /* drop frames */ }));
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
