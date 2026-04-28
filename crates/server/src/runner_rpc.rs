//! Axum WebSocket endpoint for runner registration + bidirectional
//! frame plumbing.
//!
//! Route: `GET /api/runner/register/{group_id}` upgrades to WS.
//! Auth: `Authorization: Bearer <hex-secret>` header. The secret
//! must match the one minted by `RunnerSupervisor::register_pending_spawn`
//! for the same `group_id`.
//!
//! Once authenticated:
//!   * The handler spawns a writer task that drains a tokio mpsc
//!     channel and serialises frames to the socket.
//!   * The reader task reads frames from the socket and dispatches
//!     to `RunnerSupervisor::handle_inbound`.
//!   * Either task ending tears the other down and removes the
//!     registry entry.

use crate::runner_supervisor::{RegistrationError, RunnerSupervisor, ServerToRunnerTx};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use execlaw_runner_protocol::{
    PROTOCOL_VERSION, RegistrationAck, RunnerToServer, ServerToRunner,
};
use futures::{SinkExt, StreamExt};
use std::time::SystemTime;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// `GET /api/runner/register/{group_id}` — runner WS upgrade
/// endpoint. Validates the bearer secret BEFORE the upgrade
/// completes; returns 401 cleanly when auth fails.
pub async fn register_runner(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let supervisor = match state.runner_supervisor.as_ref() {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "runner supervisor not configured",
            )
                .into_response();
        }
    };

    let bearer = match headers.get(AUTHORIZATION) {
        Some(v) => v.to_str().unwrap_or_default().to_owned(),
        None => {
            return (StatusCode::UNAUTHORIZED, "missing authorization header")
                .into_response();
        }
    };
    let hex_secret = match bearer.strip_prefix("Bearer ") {
        Some(s) => s.trim().to_owned(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                "Authorization must be `Bearer <hex>`",
            )
                .into_response();
        }
    };
    let secret_bytes = match hex::decode(&hex_secret) {
        Ok(b) if b.len() == 32 => b,
        Ok(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                "secret must decode to 32 bytes of hex",
            )
                .into_response();
        }
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, "secret is not valid hex").into_response();
        }
    };

    // We don't yet know controller-runner-ness here without a DB
    // round-trip; the chat handler that prewarms / spawns the
    // runner already knows it (from `principal_groups.includes_controller`)
    // and will set the right flag via `runner_supervisor.set_controller_flag`
    // after the spawn. v1: pass false here; the supervisor's
    // controller-pin policy is enforced when reaping (which
    // re-reads the principal group).
    //
    // TODO(phase 6): inline a `principal_groups.get(&group_id)` so
    // the controller flag is correct from the moment of
    // registration. v1 just defers it to first reap-pass lookup.
    let controller_runner = false;

    let handle = match supervisor.accept_registration(
        &group_id,
        &secret_bytes,
        controller_runner,
    ) {
        Ok(h) => h,
        Err(RegistrationError::NoPendingSpawn) => {
            return (
                StatusCode::UNAUTHORIZED,
                "no pending spawn for this group_id",
            )
                .into_response();
        }
        Err(RegistrationError::SecretMismatch) => {
            return (StatusCode::UNAUTHORIZED, "registration secret mismatch")
                .into_response();
        }
    };

    // Auth succeeded — upgrade. The supervisor has already inserted
    // the runner into the registry; the writer-task setup below
    // attaches the outbound channel.
    let group_id_for_handler = group_id.clone();
    ws.on_upgrade(move |socket| async move {
        handle_runner_ws(socket, supervisor, group_id_for_handler).await;
    })
}

async fn handle_runner_ws(
    socket: WebSocket,
    supervisor: RunnerSupervisor,
    group_id: String,
) {
    info!(group_id = %group_id, "runner WS upgraded");
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send the registration ack first.
    let ack = RegistrationAck {
        protocol_version: PROTOCOL_VERSION,
        group_id: group_id.clone(),
        server_time_ms: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default(),
    };
    let ack_text = match serde_json::to_string(&ack) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "encode registration ack");
            return;
        }
    };
    if ws_tx.send(Message::Text(ack_text.into())).await.is_err() {
        warn!(group_id = %group_id, "runner WS closed before ack landed");
        supervisor.drop_registration(&group_id).await;
        return;
    }

    // Outbound channel: supervisor → runner. Writer task drains it
    // and writes to the WS.
    let (out_tx, mut out_rx): (
        ServerToRunnerTx,
        mpsc::UnboundedReceiver<ServerToRunner>,
    ) = mpsc::unbounded_channel();
    supervisor.attach_tx(&group_id, out_tx).await;

    let group_id_writer = group_id.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let txt = match serde_json::to_string(&frame) {
                Ok(s) => s,
                Err(e) => {
                    warn!(group_id = %group_id_writer, error = %e, "encode outbound frame");
                    continue;
                }
            };
            if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                debug!(group_id = %group_id_writer, "runner WS closed; writer task ending");
                break;
            }
        }
    });

    // Reader task: WS → supervisor.
    let group_id_reader = group_id.clone();
    let supervisor_reader = supervisor.clone();
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(txt)) => {
                let frame: Result<RunnerToServer, _> = serde_json::from_str(&txt);
                match frame {
                    Ok(f) => supervisor_reader.handle_inbound(&group_id_reader, f).await,
                    Err(e) => {
                        warn!(
                            group_id = %group_id_reader,
                            error = %e,
                            "decode inbound runner frame failed"
                        );
                    }
                }
            }
            Ok(Message::Binary(bytes)) => {
                let frame: Result<RunnerToServer, _> = serde_json::from_slice(&bytes);
                match frame {
                    Ok(f) => supervisor_reader.handle_inbound(&group_id_reader, f).await,
                    Err(e) => {
                        warn!(
                            group_id = %group_id_reader,
                            error = %e,
                            "decode binary runner frame failed"
                        );
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Err(e) => {
                warn!(group_id = %group_id_reader, error = %e, "runner WS read error");
                break;
            }
        }
    }

    debug!(group_id = %group_id, "runner WS read loop exited");
    writer.abort();
    supervisor.drop_registration(&group_id).await;
}
