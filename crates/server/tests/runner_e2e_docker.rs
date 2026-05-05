//! End-to-end Docker smoke test for the runner supervisor.
//!
//! Skipped unless `EXECLAW_E2E_DOCKER=1` and the runner image
//! `execlaw/runner:dev` is already built. Verifies that:
//!
//!   1. `BollardRunnerLauncher::spawn` actually launches a real
//!      Docker container with the right env vars + volume mount.
//!   2. The runner binary inside the container connects back to
//!      our test WS endpoint and authenticates with the spawn
//!      secret.
//!   3. `RunnerSupervisor::ensure_runner` returns a `RunnerHandle`
//!      after the registration handshake completes.
//!   4. `reap_runner(IdleReap)` kills the container AND removes
//!      the workspace volume.
//!
//! This test costs ~3-5s of Docker spawn + image pull + connect
//! latency. CI doesn't run it by default — it's the test we run
//! by hand to confirm the wiring works against a real daemon.

use axum::{Router, extract::State, routing::get};
use execlaw_core::Database;
use execlaw_core::db::DbConfig;
use execlaw_core::migrations::MigrationRunner;
use execlaw_runner_protocol::{PROTOCOL_VERSION, RegistrationAck, RunnerToServer};
use execlaw_server::events::EventBus;
use execlaw_server::runner_spawn::{BollardRunnerLauncher, RunnerLauncher, RunnerSpec};
use execlaw_server::runner_supervisor::RunnerSupervisor;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Clone)]
struct TestState {
    supervisor: RunnerSupervisor,
}

async fn spawn_test_ws_server() -> (RunnerSupervisor, String) {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let supervisor = RunnerSupervisor::new(db, EventBus::new());
    let state = TestState {
        supervisor: supervisor.clone(),
    };
    let app: Router = Router::new()
        .route("/api/runner/register/{group_id}", get(register_handler))
        .with_state(state);
    // Bind to all interfaces so the runner container can reach us
    // via host.docker.internal.
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let port = addr.port();
    (supervisor, format!("ws://host.docker.internal:{port}"))
}

async fn register_handler(
    State(state): State<TestState>,
    axum::extract::Path(group_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::http::{StatusCode, header::AUTHORIZATION};
    use axum::response::IntoResponse;
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let secret = match hex::decode(bearer) {
        Ok(b) if b.len() == 32 => b,
        _ => return (StatusCode::UNAUTHORIZED, "bad secret").into_response(),
    };
    if state
        .supervisor
        .accept_registration(&group_id, &secret, false)
        .is_err()
    {
        return (StatusCode::UNAUTHORIZED, "auth failed").into_response();
    }
    let supervisor = state.supervisor.clone();
    let group_id_clone = group_id.clone();
    ws.on_upgrade(move |socket| async move {
        let (mut tx, mut rx) = socket.split();
        let ack = RegistrationAck {
            protocol_version: PROTOCOL_VERSION,
            group_id: group_id_clone.clone(),
            server_time_ms: 0,
        };
        let _ = tx
            .send(axum::extract::ws::Message::Text(
                serde_json::to_string(&ack).unwrap().into(),
            ))
            .await;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        supervisor.attach_tx(&group_id_clone, out_tx).await;
        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let _ = tx
                    .send(axum::extract::ws::Message::Text(
                        serde_json::to_string(&frame).unwrap().into(),
                    ))
                    .await;
            }
        });
        while let Some(msg) = rx.next().await {
            if let Ok(axum::extract::ws::Message::Text(t)) = msg {
                if let Ok(frame) = serde_json::from_str::<RunnerToServer>(&t) {
                    supervisor.handle_inbound(&group_id_clone, frame).await;
                }
            }
        }
        writer.abort();
        supervisor.drop_registration(&group_id_clone).await;
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires EXECLAW_E2E_DOCKER=1 + execlaw/runner:dev image built"]
async fn ensure_runner_spawns_real_docker_container_and_handshakes() {
    if std::env::var("EXECLAW_E2E_DOCKER").ok().as_deref() != Some("1") {
        eprintln!("set EXECLAW_E2E_DOCKER=1 to run this test");
        return;
    }
    let (supervisor, rpc_url) = spawn_test_ws_server().await;
    eprintln!("[e2e] WS server listening at {rpc_url}");
    // Give axum's serve task a moment to bind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let launcher = Arc::new(BollardRunnerLauncher::new().expect("docker reachable"));

    let group_id = format!("e2e-{}", uuid::Uuid::new_v4());
    eprintln!("[e2e] spawning runner for group {group_id}");
    let spec = RunnerSpec {
        group_id: group_id.clone(),
        image: "execlaw/runner:dev".into(),
        spawn_secret_hex: String::new(), // ensure_runner fills this
        rpc_url,
        // We're not actually going to drive a turn (which would
        // need a real vLLM); the runner just registers + idles.
        inference_url: "http://host.docker.internal:8101/v1".into(),
        memory_bytes: Some(1024 * 1024 * 1024),
        network: None,
        env: vec![("RUST_LOG".into(), "debug".into())],
    };

    // Spawn + wait for WS register.
    let handle = supervisor
        .ensure_runner(launcher.as_ref(), &group_id, spec, Duration::from_secs(30))
        .await
        .expect("ensure_runner ok");
    assert_eq!(handle.group_id, group_id);
    let cid = handle
        .state
        .read()
        .await
        .container_id
        .clone()
        .expect("container_id stamped");
    assert!(!cid.is_empty());

    // Confirm Docker has it.
    let containers = launcher.list_runner_volumes().await.unwrap();
    assert!(
        containers
            .iter()
            .any(|n| n == &format!("execlaw-runner-{group_id}")),
        "workspace volume should exist; got {containers:?}"
    );

    // Reap it.
    let report = supervisor
        .reap_runner(
            launcher.as_ref(),
            &group_id,
            execlaw_runner_protocol::ShutdownReason::IdleReap,
        )
        .await
        .expect("reap_runner ok");
    assert!(report.wiped_volume, "idle reap must wipe volume");

    // Volume gone after reap.
    let after = launcher.list_runner_volumes().await.unwrap();
    assert!(
        !after
            .iter()
            .any(|n| n == &format!("execlaw-runner-{group_id}")),
        "volume should have been removed; still present: {after:?}"
    );
}
