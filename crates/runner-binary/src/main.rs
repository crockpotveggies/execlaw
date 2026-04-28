//! `execlaw-runner` — long-lived per-principal-group container that
//! the control plane spawns to execute agent turns.
//!
//! Lifecycle:
//!   1. Read configuration from env vars (no CLI args — operators
//!      don't run this directly, the supervisor does).
//!   2. Open a WebSocket to the control plane carrying the
//!      one-time spawn secret in `Authorization: Bearer ...`.
//!   3. Receive `RegistrationAck`, verify protocol version.
//!   4. Loop reading `ServerToRunner` frames, dispatch each.
//!   5. On `Shutdown` (or supervisor disconnect), exit cleanly.
//!
//! v1 scope: streaming inference, no tools, no per-turn workspace
//! file IO. Tool dispatch over RPC + workspace tools land in a
//! follow-up. The shape is established now so the supervisor side
//! can be wired against a real binary instead of mocks.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

mod connect;
mod turn_loop;

use connect::{Connection, RunnerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg = RunnerConfig::from_env()
        .context("loading runner configuration from environment")?;

    tracing::info!(
        group_id = %cfg.group_id,
        rpc_url = %cfg.rpc_url,
        "execlaw-runner starting"
    );

    // Connect + authenticate. On failure we exit non-zero; the
    // supervisor's container watch sees a fast crash and surfaces
    // it as a spawn error instead of a silent bad runner.
    let mut conn = Connection::connect(&cfg)
        .await
        .context("connecting to control plane")?;

    tracing::info!(
        group_id = %cfg.group_id,
        protocol_version = conn.ack().protocol_version,
        server_time_ms = conn.ack().server_time_ms,
        "registered with control plane"
    );

    // Cancel-flag per `turn_id`. The main loop sets one when it
    // sees a `CancelTurn` frame; the running turn polls between
    // streaming chunks and aborts.
    let cancel_flags: Arc<Mutex<turn_loop::CancelFlags>> =
        Arc::new(Mutex::new(turn_loop::CancelFlags::default()));

    // Listen for ctrl-c / SIGTERM as a fallback (the supervisor's
    // happy-path is `Shutdown` over the WS). Either way we close
    // cleanly so the supervisor can wipe the workspace if needed.
    let shutdown_signal = tokio::signal::ctrl_c();

    tokio::pin!(shutdown_signal);

    let mut exit_code = 0;
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_signal => {
                tracing::info!("ctrl-c received; closing connection");
                break;
            }
            frame = conn.recv() => {
                match frame {
                    Ok(Some(frame)) => {
                        if !turn_loop::handle_frame(
                            &cfg,
                            &mut conn,
                            cancel_flags.clone(),
                            frame,
                        )
                        .await
                        {
                            // handle_frame returns false on Shutdown.
                            break;
                        }
                    }
                    Ok(None) => {
                        // Server closed the WS — supervisor went
                        // down or our reaper fired. Treat as
                        // graceful exit.
                        tracing::info!("control plane closed connection");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "websocket read error");
                        exit_code = 1;
                        break;
                    }
                }
            }
        }
    }

    let _ = conn.close().await;
    std::process::exit(exit_code);
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,execlaw_runner=debug"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();
}
