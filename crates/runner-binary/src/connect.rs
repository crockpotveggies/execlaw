//! WebSocket client + registration handshake.
//!
//! On startup the runner reads its configuration from env vars
//! (passed in by the supervisor at `docker run` time), opens a WS
//! to `${rpc_url}/api/runner/register/${group_id}` carrying the
//! spawn secret as a Bearer token, and waits for the supervisor's
//! `RegistrationAck`. After the handshake the WS is symmetric —
//! either side can send `ServerToRunner` / `RunnerToServer` frames
//! at any time.
//!
//! 2026-04-28 — split into a `ConnectionDriver` that owns the socket
//! and exposes:
//!   * `tx()` — a cloneable `ConnectionTx` that any task can use to
//!     push `RunnerToServer` frames. A background writer task drains
//!     the mpsc and serialises sends so multiple in-flight turn
//!     tasks never race on the WS sink.
//!   * `recv()` — the main loop's pull side, demuxing inbound frames
//!     to the per-turn handlers. A background reader task feeds the
//!     internal channel; on a closed socket the channel returns
//!     `None`, signalling the main loop to exit.
//!
//! This shape is what unlocks tool-call dispatch: a turn task can
//! send a `ToolCallRequest` and park on a per-turn `ToolCallResult`
//! mailbox WITHOUT blocking the WS read loop, so a `CancelTurn` or
//! a `Heartbeat` arriving mid-tool-call doesn't get dropped.

use anyhow::{Context, Result, anyhow, bail};
use execlaw_runner_protocol::{PROTOCOL_VERSION, RegistrationAck, RunnerToServer, ServerToRunner};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use std::env;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Configuration the supervisor passes via the container
/// environment. None of these are sensitive *to log* (the secret is
/// the only one we never print, and we keep it out of `Debug`).
#[derive(Clone)]
pub struct RunnerConfig {
    /// Control-plane WS base URL, e.g. `ws://host.docker.internal:3031`
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
        let rpc_url = env::var("EXECLAW_RPC_URL").context("EXECLAW_RPC_URL must be set")?;
        let group_id = env::var("EXECLAW_GROUP_ID").context("EXECLAW_GROUP_ID must be set")?;
        let spawn_secret =
            env::var("EXECLAW_SPAWN_SECRET").context("EXECLAW_SPAWN_SECRET must be set")?;
        let inference_url = env::var("EXECLAW_INFERENCE_URL").ok();
        if rpc_url.is_empty() || group_id.is_empty() || spawn_secret.is_empty() {
            bail!(
                "EXECLAW_RPC_URL / EXECLAW_GROUP_ID / EXECLAW_SPAWN_SECRET must all be non-empty"
            );
        }
        Ok(Self {
            rpc_url,
            group_id,
            spawn_secret,
            inference_url,
        })
    }
}

/// Cloneable handle for sending `RunnerToServer` frames. Backed by a
/// single-writer mpsc to the dedicated writer task that owns the WS
/// sink — concurrent sends from multiple turn tasks are serialised
/// safely without explicit locks.
#[derive(Clone)]
pub struct ConnectionTx {
    inner: mpsc::UnboundedSender<RunnerToServer>,
}

impl ConnectionTx {
    pub fn send(&self, frame: RunnerToServer) -> Result<()> {
        self.inner
            .send(frame)
            .map_err(|_| anyhow!("connection closed; outbound channel disconnected"))
    }
}

/// Live WS connection to the control plane. Construct via
/// `ConnectionDriver::connect`; pull inbound frames via `recv()`,
/// push outbound frames via `tx()`.
pub struct ConnectionDriver {
    ack: RegistrationAck,
    out_tx: ConnectionTx,
    in_rx: mpsc::UnboundedReceiver<ServerToRunner>,
    /// JoinHandles kept so the tasks abort if the driver is dropped
    /// before a clean close. Tagged with `_` because we never poll
    /// them directly — abort-on-drop is the contract.
    _writer: JoinHandle<()>,
    _reader: JoinHandle<()>,
}

impl ConnectionDriver {
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
                .with_context(|| format!("decoding registration ack: {}", trim(&txt, 200)))?,
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

        // Split the socket. The writer task owns the sink
        // exclusively (no Mutex needed) and drains an mpsc. The
        // reader task owns the stream and pushes decoded frames to
        // the inbound mpsc. Either task ending closes its half;
        // when both halves are closed the driver's `recv` returns
        // `None` and the main loop exits.
        let (sink, mut stream) = socket.split();
        let (out_tx_inner, mut out_rx) = mpsc::unbounded_channel::<RunnerToServer>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerToRunner>();

        // Writer task: serialise frames + drain the outbound mpsc.
        // Also forwards Pong control frames received from the reader
        // via the same channel, since SinkSplit'd halves can't share
        // a Sink. To keep the surface small we don't model Pong as a
        // RunnerToServer variant — instead the reader auto-pongs by
        // ignoring Ping (relying on tungstenite's frame-level
        // semantics; the supervisor's heartbeat is application-
        // level, not WS Ping/Pong).
        let writer = tokio::spawn(async move {
            let mut sink: SplitSink<_, Message> = sink;
            while let Some(frame) = out_rx.recv().await {
                let txt = match serde_json::to_string(&frame) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "encode RunnerToServer frame failed");
                        continue;
                    }
                };
                if let Err(e) = sink.send(Message::Text(txt)).await {
                    tracing::error!(error = %e, "WS send failed; closing writer task");
                    break;
                }
            }
            // Best-effort close.
            let _ = sink.close().await;
        });

        // Reader task: decode + forward.
        let reader = tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(error = %e, "WS read failed; closing reader task");
                        break;
                    }
                };
                match msg {
                    Message::Text(txt) => match serde_json::from_str::<ServerToRunner>(&txt) {
                        Ok(frame) => {
                            if in_tx.send(frame).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                payload = %trim(&txt, 200),
                                "dropping undecodable ServerToRunner frame"
                            );
                        }
                    },
                    Message::Binary(bytes) => {
                        match serde_json::from_slice::<ServerToRunner>(&bytes) {
                            Ok(frame) => {
                                if in_tx.send(frame).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "dropping undecodable binary frame");
                            }
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                        // tungstenite handles control-frame
                        // round-trips; the supervisor uses
                        // application-level Heartbeat frames.
                    }
                    Message::Close(_) => {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            ack,
            out_tx: ConnectionTx {
                inner: out_tx_inner,
            },
            in_rx,
            _writer: writer,
            _reader: reader,
        })
    }

    pub fn ack(&self) -> &RegistrationAck {
        &self.ack
    }

    /// A cloneable sender. Use this from any task that needs to
    /// push frames back to the supervisor.
    pub fn tx(&self) -> ConnectionTx {
        self.out_tx.clone()
    }

    /// Pull the next inbound frame. Returns `None` when the WS has
    /// closed (clean close or error — the reader task already logged
    /// the cause).
    pub async fn recv(&mut self) -> Option<ServerToRunner> {
        self.in_rx.recv().await
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

/// Compile-time assertions: `WebSocketStream<MaybeTlsStream<TcpStream>>`
/// must split into `SplitSink<_, Message>` for the writer task. If
/// tungstenite changes the surface, we want the runner to fail to
/// build rather than fail at runtime.
#[allow(dead_code)]
fn _typecheck_socket_split(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message> {
    let (sink, _stream) = socket.split();
    sink
}
