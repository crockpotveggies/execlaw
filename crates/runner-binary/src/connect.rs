//! WebSocket client + registration handshake.
//!
//! On startup the runner reads its configuration from env vars
//! (passed in by the supervisor at `docker run` time), opens a WS
//! to `${rpc_url}/api/runner/register/${group_id}` carrying the
//! spawn secret as a Bearer token, and waits for the supervisor's
//! `RegistrationAck`. After the handshake the WS is symmetric —
//! either side can send `ServerToRunner` / `RunnerToServer` frames
//! at any time.

use anyhow::{Context, Result, anyhow, bail};
use execlaw_runner_protocol::{
    PROTOCOL_VERSION, RegistrationAck, RunnerToServer, ServerToRunner,
};
use futures_util::{SinkExt, StreamExt};
use std::env;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Configuration the supervisor passes via the container
/// environment. None of these are sensitive *to log* (the secret is
/// the only one we never print, and we keep it out of `Debug`).
#[derive(Clone)]
pub struct RunnerConfig {
    /// Control-plane WS base URL, e.g. `ws://control-plane:3030`.
    /// The runner appends `/api/runner/register/<group_id>`.
    pub rpc_url: String,
    pub group_id: String,
    /// Per-spawn one-time secret. Hex-encoded by the supervisor;
    /// we forward it verbatim as the bearer token.
    pub spawn_secret: String,
    /// Default vLLM URL. Per-turn requests can override via
    /// `TurnRequest.inference_url`, but having a sane default lets
    /// the runner short-circuit if a request lands without one
    /// (defensive — supervisor always sets it today).
    pub inference_url: Option<String>,
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("rpc_url", &self.rpc_url)
            .field("group_id", &self.group_id)
            .field("spawn_secret", &"<redacted>")
            .field("inference_url", &self.inference_url)
            .finish()
    }
}

impl RunnerConfig {
    pub fn from_env() -> Result<Self> {
        let rpc_url = env::var("EXECLAW_RPC_URL")
            .context("EXECLAW_RPC_URL must be set")?;
        let group_id = env::var("EXECLAW_GROUP_ID")
            .context("EXECLAW_GROUP_ID must be set")?;
        let spawn_secret = env::var("EXECLAW_SPAWN_SECRET")
            .context("EXECLAW_SPAWN_SECRET must be set")?;
        let inference_url = env::var("EXECLAW_INFERENCE_URL").ok();
        if rpc_url.is_empty() || group_id.is_empty() || spawn_secret.is_empty() {
            bail!("EXECLAW_RPC_URL / EXECLAW_GROUP_ID / EXECLAW_SPAWN_SECRET must all be non-empty");
        }
        Ok(Self {
            rpc_url,
            group_id,
            spawn_secret,
            inference_url,
        })
    }
}

/// One live WS connection. Owns the split sink + stream and the
/// `RegistrationAck` that the supervisor sent on handshake.
pub struct Connection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ack: RegistrationAck,
}

impl Connection {
    pub async fn connect(cfg: &RunnerConfig) -> Result<Self> {
        // Build the upgrade URL. Supervisor's route is
        // `/api/runner/register/{group_id}`.
        let url = format!(
            "{}/api/runner/register/{}",
            cfg.rpc_url.trim_end_matches('/'),
            urlencode(&cfg.group_id),
        );

        // Build the request with the auth header on the upgrade
        // itself. axum sees it before the WS upgrade completes and
        // can 401 cleanly without ever opening a half-built socket.
        let mut req = url
            .as_str()
            .into_client_request()
            .context("building WS upgrade request")?;
        let bearer = format!("Bearer {}", cfg.spawn_secret);
        req.headers_mut().insert(
            AUTHORIZATION,
            bearer
                .parse()
                .context("encoding spawn secret as Authorization header")?,
        );

        let (mut socket, response) = tokio_tungstenite::connect_async(req)
            .await
            .context("WS connect / upgrade failed")?;
        tracing::debug!(
            status = %response.status(),
            "WS upgrade accepted by control plane"
        );

        // First frame must be a RegistrationAck.
        let ack: RegistrationAck = match socket.next().await {
            Some(Ok(Message::Text(txt))) => serde_json::from_str(&txt)
                .with_context(|| {
                    format!("decoding registration ack: {}", trim(&txt, 200))
                })?,
            Some(Ok(Message::Binary(bytes))) => {
                serde_json::from_slice(&bytes).context("decoding binary ack")?
            }
            Some(Ok(other)) => {
                return Err(anyhow!(
                    "unexpected first frame from control plane: {other:?}"
                ));
            }
            Some(Err(e)) => return Err(e).context("reading registration ack"),
            None => return Err(anyhow!("control plane closed before sending ack")),
        };
        if ack.protocol_version != PROTOCOL_VERSION {
            bail!(
                "protocol version mismatch: server={} runner={}",
                ack.protocol_version,
                PROTOCOL_VERSION
            );
        }
        if ack.group_id != cfg.group_id {
            bail!(
                "group_id mismatch in registration ack: server={} runner={}",
                ack.group_id,
                cfg.group_id
            );
        }

        Ok(Self { socket, ack })
    }

    pub fn ack(&self) -> &RegistrationAck {
        &self.ack
    }

    /// Receive the next `ServerToRunner` frame.
    /// Returns Ok(None) when the supervisor closes the WS cleanly.
    pub async fn recv(&mut self) -> Result<Option<ServerToRunner>> {
        loop {
            let msg = match self.socket.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(None),
            };
            match msg {
                Message::Text(txt) => {
                    let frame: ServerToRunner = serde_json::from_str(&txt)
                        .with_context(|| {
                            format!("decoding ServerToRunner frame: {}", trim(&txt, 200))
                        })?;
                    return Ok(Some(frame));
                }
                Message::Binary(bytes) => {
                    let frame: ServerToRunner = serde_json::from_slice(&bytes)
                        .context("decoding binary ServerToRunner frame")?;
                    return Ok(Some(frame));
                }
                Message::Ping(payload) => {
                    let _ = self.socket.send(Message::Pong(payload)).await;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::Close(_) => return Ok(None),
                Message::Frame(_) => continue,
            }
        }
    }

    /// Send a frame back to the supervisor.
    pub async fn send(&mut self, frame: &RunnerToServer) -> Result<()> {
        let txt = serde_json::to_string(frame)
            .context("encoding RunnerToServer frame")?;
        self.socket
            .send(Message::Text(txt.into()))
            .await
            .context("WS send")?;
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        let _ = self.socket.close(None).await;
        Ok(())
    }
}

/// Minimal URL-encoder for the path segment. We control the inputs
/// (UUIDs from the supervisor), so no edge cases — but the helper
/// keeps the call site readable.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn trim(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}…", &s[..max])
    }
}
