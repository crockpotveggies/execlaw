//! `execlaw-runner` — long-lived per-principal-group container that
//! the control plane spawns to execute agent turns.
//!
//! Lifecycle:
//!   1. Read configuration from env vars (no CLI args — operators
//!      don't run this directly, the supervisor does).
//!   2. Open a WebSocket to the control plane carrying the
//!      one-time spawn secret in `Authorization: Bearer ...`.
//!   3. Receive `RegistrationAck`, verify protocol version.
//!   4. Demux loop: read `ServerToRunner` frames and route them.
//!      `Turn(req)` spawns a turn task; `ToolCallResult(...)` is
//!      forwarded into the matching turn's mailbox; `CancelTurn`,
//!      `Heartbeat`, and `Shutdown` are handled inline so an
//!      in-flight turn task doesn't block any of them.
//!   5. On `Shutdown` (or supervisor disconnect), break the demux
//!      loop, drop the in-flight turn tasks, exit cleanly.
//!
//! Tool dispatch (2026-04-28): when `req.tool_catalog` is non-empty,
//! the turn task forwards each `tool_call` the model emits as a
//! `ToolCallRequest` and parks on its per-turn mailbox until the
//! supervisor's `ToolCallResult` lands. The demux above keeps the
//! mailbox draining concurrently with any other in-flight runner
//! activity (multi-turn parallelism is bounded but not zero).

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use execlaw_runner_protocol::{RunnerToServer, ServerToRunner};
use std::sync::Arc;
use tokio::sync::Mutex;

mod connect;
mod turn_loop;

use connect::{ConnectionDriver, RunnerConfig};
use turn_loop::{CancelFlags, ToolResultRoutes};

// mimalloc beats both glibc-malloc and musl-malloc on the runner's
// concurrent JSON-parsing + buffer-churn workload. Load-bearing on
// alpine specifically — musl's default malloc is ~2-3× slower than
// glibc under multi-thread contention.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    // 2026-05-16 — install the panic hook BEFORE the tokio
    // runtime is built. A panic in any task otherwise unwinds
    // through tokio's worker thread, prints a default backtrace,
    // and exits 101 with no record in the runner's tracing
    // journal. With this hook the panic lands as a structured
    // ERROR log (full backtrace + location) AND we call
    // `process::abort()` to produce a core dump when the host's
    // `ulimit -c` allows it. That gives `rust-gdb core` something
    // to attach to for post-mortem analysis. Set
    // `RUST_BACKTRACE=full` in the runner container's env for the
    // longest captured backtrace; `1` is the OK default.
    install_panic_hook();

    // Manual runtime construction (instead of `#[tokio::main]`) so we
    // can pin a generous thread stack. musl's default is 128 KB —
    // dangerously small for deep async chains and serde_json
    // recursion. 2 MB matches glibc's effective working set.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(2 * 1024 * 1024)
        .build()
        .context("building tokio runtime")?;
    rt.block_on(async_main())
}

/// Replace the default `eprintln!`-based panic hook with one that
/// emits a structured tracing event AND aborts the process. The
/// abort matters because:
///   * `panic = "unwind"` (default in dev/release) keeps the
///     process alive after one task panics; subsequent task
///     panics race against an already-corrupted runtime.
///   * `process::abort` triggers `SIGABRT`, which the kernel can
///     core-dump (when `ulimit -c unlimited` and the host
///     allows). With debug symbols in the binary (`debug =
///     "limited"` already on the release profile via
///     workspace settings), `rust-gdb <binary> <core>` gives
///     the full panic-site backtrace.
///
/// The hook captures `std::backtrace::Backtrace::force_capture()`
/// unconditionally — distinct from `RUST_BACKTRACE`, which only
/// drives the default hook. Operators see the full stack in the
/// journal regardless of env-var state.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        // `panic_payload_string` mirrors the default hook's
        // extraction order: `String` payload, then `&str`, then
        // fall back to the Debug rendering.
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("<non-string panic payload>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        // ERROR not WARN — a panic is an invariant violation, not
        // a recoverable condition. The journal grep target
        // `runner::panic` mirrors `runner::turn_failure_snapshot`
        // so an operator can find both with the same query
        // prefix.
        tracing::error!(
            target: "runner::panic",
            message,
            location,
            backtrace = %backtrace,
            "RUNNER_PANIC — process aborting (core dump if ulimit -c allows)"
        );
        std::process::abort();
    }));
}

async fn async_main() -> Result<()> {
    init_tracing();

    let cfg = RunnerConfig::from_env().context("loading runner configuration from environment")?;

    tracing::info!(
        group_id = %cfg.group_id,
        rpc_url = %cfg.rpc_url,
        "execlaw-runner starting"
    );

    // Connect + authenticate. On failure we exit non-zero; the
    // supervisor's container watch sees a fast crash and surfaces
    // it as a spawn error instead of a silent bad runner.
    let mut driver = ConnectionDriver::connect(&cfg)
        .await
        .context("connecting to control plane")?;

    tracing::info!(
        group_id = %cfg.group_id,
        protocol_version = driver.ack().protocol_version,
        server_time_ms = driver.ack().server_time_ms,
        "registered with control plane"
    );

    // Per-turn cancel flags. The demux loop sets one when it sees
    // CancelTurn; the running turn polls it.
    let cancel_flags: Arc<Mutex<CancelFlags>> = Arc::new(Mutex::new(CancelFlags::default()));

    // Per-turn tool-result mailbox. Keyed by `turn_id`. When the
    // demux sees `ServerToRunner::ToolCallResult` it routes by
    // `result.turn_id`. The turn task's receiver is dropped when
    // the task returns; the next ToolCallResult that lands for a
    // dropped turn logs + drops, no panic.
    let tool_routes: ToolResultRoutes = Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Listen for ctrl-c / SIGTERM as a fallback (the supervisor's
    // happy-path is `Shutdown` over the WS). Either way we close
    // cleanly so the supervisor can wipe the workspace if needed.
    let shutdown_signal = tokio::signal::ctrl_c();

    tokio::pin!(shutdown_signal);

    let exit_code = 0;
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_signal => {
                tracing::info!("ctrl-c received; closing connection");
                break;
            }
            maybe_frame = driver.recv() => {
                let Some(frame) = maybe_frame else {
                    tracing::info!("control plane closed connection");
                    break;
                };
                if !handle_frame(
                    &cfg,
                    &driver,
                    cancel_flags.clone(),
                    tool_routes.clone(),
                    frame,
                ).await {
                    break;
                }
            }
        }
    }

    // Driver tasks abort on drop; explicit cleanup not needed.
    drop(driver);
    let _ = exit_code; // currently always 0; reserved for future error paths
    Ok(())
}

/// Returns false to exit the demux loop (Shutdown received).
async fn handle_frame(
    cfg: &RunnerConfig,
    driver: &ConnectionDriver,
    cancel_flags: Arc<Mutex<CancelFlags>>,
    tool_routes: ToolResultRoutes,
    frame: ServerToRunner,
) -> bool {
    let _ = cfg; // reserved for future use (e.g. scoping a workspace dir)
    match frame {
        ServerToRunner::Heartbeat { nonce } => {
            if let Err(e) = driver.tx().send(RunnerToServer::HeartbeatAck { nonce }) {
                tracing::warn!(error = %e, "failed to ack heartbeat");
            }
            true
        }
        ServerToRunner::Shutdown { reason } => {
            tracing::info!(?reason, "received shutdown");
            false
        }
        ServerToRunner::CancelTurn { turn_id } => {
            cancel_flags.lock().await.cancel(&turn_id);
            tracing::info!(turn_id = %turn_id, "cancel flag armed");
            true
        }
        ServerToRunner::ToolCallResult(result) => {
            let turn_id_for_lookup = result.turn_id.clone();
            let mut routes = tool_routes.lock().await;
            if let Some(route) = routes.get(&turn_id_for_lookup) {
                if route.send(result).is_err() {
                    // Turn's receiver dropped — race with the
                    // turn finishing right when the result was
                    // in flight. Clean up the entry.
                    routes.remove(&turn_id_for_lookup);
                }
            } else {
                tracing::warn!(
                    turn_id = %turn_id_for_lookup,
                    "tool result for unknown turn_id; dropping"
                );
            }
            true
        }
        ServerToRunner::Turn(req) => {
            let turn_id = req.turn_id.clone();
            let conversation_id = req.conversation_id.clone();
            let cancel = cancel_flags.lock().await.arm(&turn_id);
            // Allocate the per-turn tool-result mailbox before
            // spawning so the demux side's lookup is race-free.
            let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel();
            tool_routes.lock().await.insert(turn_id.clone(), tool_tx);

            let tx = driver.tx();
            let cancel_flags_for_drop = cancel_flags.clone();
            let tool_routes_for_drop = tool_routes.clone();
            tokio::spawn(async move {
                let result = turn_loop::run_turn(tx.clone(), cancel, tool_rx, *req).await;
                if let Err(e) = result {
                    // `{e:#}` walks the full anyhow context chain
                    // ("opening inference stream: 400 Bad Request: …")
                    // — `{e}` would only show the topmost label and
                    // hide the actual root cause.
                    tracing::error!(
                        turn_id = %turn_id,
                        error = %format!("{e:#}"),
                        "turn failed",
                    );
                    let _ = tx.send(RunnerToServer::Error {
                        turn_id: turn_id.clone(),
                        conversation_id,
                        message: format!("{e:#}"),
                        cancelled: false,
                    });
                }
                cancel_flags_for_drop.lock().await.drop_turn(&turn_id);
                tool_routes_for_drop.lock().await.remove(&turn_id);
            });
            true
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, Layer, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,execlaw_runner=debug"));
    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_filter(filter);
    let registry = tracing_subscriber::registry().with(fmt_layer);

    // 2026-05-16 — tokio-console wiring. Opt-in via the
    // `tokio-console` Cargo feature AND a runtime env var so a
    // production build can include the dep cost-free and operators
    // turn it on only when debugging. The env var doubles as the
    // bind address; absent → not wired, present → console layer
    // spawned on that address.
    //
    // Connect from the operator's laptop with:
    //   tokio-console http://<runner-host>:6669
    //
    // Build prerequisite (HARD requirement, not a recommendation):
    //   RUSTFLAGS="--cfg tokio_unstable" \
    //     cargo build -p execlaw-runner --features tokio-console
    // Without `tokio_unstable`, console-subscriber spawns but
    // can't see task spawn locations or per-task IDs — every task
    // shows as "anonymous" and the diagnostic value drops to zero.
    #[cfg(feature = "tokio-console")]
    {
        if let Ok(bind) = std::env::var("EXECLAW_TOKIO_CONSOLE_BIND") {
            match bind.parse::<std::net::SocketAddr>() {
                Ok(addr) => {
                    let console_layer = console_subscriber::ConsoleLayer::builder()
                        .with_default_env()
                        .server_addr(addr)
                        .spawn();
                    registry.with(console_layer).init();
                    tracing::info!(
                        target: "runner::tokio_console",
                        %addr,
                        "tokio-console layer engaged; connect with: \
                         tokio-console http://<host>:<port>"
                    );
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "EXECLAW_TOKIO_CONSOLE_BIND={bind:?} did not parse as a SocketAddr ({e}); \
                         tokio-console will be inactive this run"
                    );
                }
            }
        }
    }

    // Default path (no tokio-console, or feature disabled, or env
    // var unset) — fmt layer only.
    registry.init();
}
