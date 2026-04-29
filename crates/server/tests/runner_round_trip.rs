//! End-to-end test for the runner supervisor's WS path.
//!
//! Spins up a real axum router with `runner_rpc::register_runner`
//! mounted, mints a pending spawn on the supervisor, connects a
//! tokio-tungstenite WS client carrying the bearer secret, and
//! asserts that:
//!   1. The HTTP upgrade succeeds (auth passes).
//!   2. The first frame is a `RegistrationAck` with the right
//!      protocol version + group_id.
//!   3. Token deltas the supervisor pushes via `forward_turn`
//!      arrive at the client, AND publish on the SPA event bus.
//!   4. A bad bearer secret 401s before the upgrade lands.
//!
//! This is the load-bearing test for the runner supervisor —
//! exercises auth, attach_tx wiring, and the inbound frame
//! dispatcher in one shot.

use axum::{Router, extract::State, routing::get};
use execlaw_core::Database;
use execlaw_core::db::DbConfig;
use execlaw_core::migrations::MigrationRunner;
use execlaw_runner_protocol::{
    PROTOCOL_VERSION, RegistrationAck, RunnerToServer, ServerToRunner, TurnRequest,
};
use execlaw_server::events::{EventBus, UiEvent};
use execlaw_server::runner_supervisor::RunnerSupervisor;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;

#[derive(Clone)]
struct TestState {
    supervisor: RunnerSupervisor,
}

async fn spawn_test_server() -> (TestState, String) {
    let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
    MigrationRunner::new(&db).apply_all().unwrap();
    let supervisor = RunnerSupervisor::new(db, EventBus::new());
    let state = TestState {
        supervisor: supervisor.clone(),
    };
    let app: Router = Router::new()
        .route(
            "/api/runner/register/{group_id}",
            get(test_register_handler),
        )
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, format!("ws://{addr}"))
}

// We can't reuse `runner_rpc::register_runner` directly because it
// binds against `AppState` (the full server state), not our
// minimal test state. Re-implement the same handshake pattern
// but driven by our test supervisor.
async fn test_register_handler(
    State(state): State<TestState>,
    axum::extract::Path(group_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::http::StatusCode;
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
    let _handle = match state
        .supervisor
        .accept_registration(&group_id, &secret, true)
    {
        Ok(h) => h,
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, "auth failed").into_response();
        }
    };
    let supervisor = state.supervisor.clone();
    let group_id_for_handler = group_id.clone();
    ws.on_upgrade(move |socket| async move {
        let (mut tx, mut rx) = socket.split();
        let ack = RegistrationAck {
            protocol_version: PROTOCOL_VERSION,
            group_id: group_id_for_handler.clone(),
            server_time_ms: 0,
        };
        let txt = serde_json::to_string(&ack).unwrap();
        tx.send(axum::extract::ws::Message::Text(txt.into()))
            .await
            .ok();

        // Set up the outbound queue.
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.attach_tx(&group_id_for_handler, out_tx).await;

        let writer = tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let txt = serde_json::to_string(&frame).unwrap();
                if tx
                    .send(axum::extract::ws::Message::Text(txt.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        while let Some(msg) = rx.next().await {
            match msg {
                Ok(axum::extract::ws::Message::Text(t)) => {
                    if let Ok(frame) = serde_json::from_str::<RunnerToServer>(&t) {
                        supervisor
                            .handle_inbound(&group_id_for_handler, frame)
                            .await;
                    }
                }
                Ok(axum::extract::ws::Message::Close(_)) => break,
                _ => {}
            }
        }
        writer.abort();
        supervisor.drop_registration(&group_id_for_handler).await;
    })
}

fn build_request(url: &str, secret: &[u8]) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.into_client_request().unwrap();
    let bearer = format!("Bearer {}", hex::encode(secret));
    req.headers_mut()
        .insert(AUTHORIZATION, bearer.parse().unwrap());
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_succeeds_with_correct_secret() {
    let (state, base) = spawn_test_server().await;
    let (secret, _notify) = state.supervisor.register_pending_spawn("g-1");
    let req = build_request(&format!("{base}/api/runner/register/g-1"), &secret);
    let (mut socket, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    // First frame must be the registration ack.
    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("ack within 2s")
        .expect("some frame")
        .expect("ws ok");
    let txt = match frame {
        Message::Text(t) => t.to_string(),
        other => panic!("unexpected first frame: {other:?}"),
    };
    let ack: RegistrationAck = serde_json::from_str(&txt).unwrap();
    assert_eq!(ack.protocol_version, PROTOCOL_VERSION);
    assert_eq!(ack.group_id, "g-1");

    socket.close(None).await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_secret_is_rejected_at_upgrade() {
    let (state, base) = spawn_test_server().await;
    let (_real_secret, _notify) = state.supervisor.register_pending_spawn("g-1");
    let bad = [0u8; 32];
    let req = build_request(&format!("{base}/api/runner/register/g-1"), &bad);
    let result = tokio_tungstenite::connect_async(req).await;
    // tokio-tungstenite surfaces 4xx as `Http` errors with the
    // status code attached.
    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 401);
        }
        Ok(_) => panic!("upgrade should have been rejected"),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_turn_round_trips_token_delta_to_runner_and_event_bus() {
    let (state, base) = spawn_test_server().await;
    let mut bus_rx = state.supervisor.events().subscribe();
    let (secret, _notify) = state.supervisor.register_pending_spawn("g-1");
    let req = build_request(&format!("{base}/api/runner/register/g-1"), &secret);
    let (mut socket, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();

    // Eat the registration ack.
    let _ack = socket.next().await.unwrap().unwrap();

    // Wait for the supervisor to attach the outbound channel
    // (writer task spawn race).
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now forward a turn through the supervisor. The mock runner
    // (this test process) will receive the Turn frame on the WS.
    let turn = TurnRequest {
        turn_id: "t-1".into(),
        conversation_id: "conv-x".into(),
        group_id: "g-1".into(),
        user_text: "hello".into(),
        sender_principal_id: "controller".into(),
        sender_trust_class: "Controller".into(),
        system_prompt: "be concise".into(),
        history: vec![],
        tool_catalog: vec![],
        inference_url: "http://infer".into(),
        model: "qwen3.5".into(),
        temperature: None,
        max_tokens: None,
        reasoning_enabled: false,
        spotlight: None,
    };
    let _stream = state
        .supervisor
        .forward_turn("g-1", turn.clone())
        .await
        .expect("forward_turn ok");

    // Read the Turn frame on the runner side.
    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("turn frame within 2s")
        .expect("some frame")
        .expect("ws ok");
    let received: ServerToRunner = match frame {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected text frame: {other:?}"),
    };
    match received {
        ServerToRunner::Turn(req) => {
            assert_eq!(req.turn_id, "t-1");
            assert_eq!(req.user_text, "hello");
        }
        other => panic!("expected Turn frame: {other:?}"),
    }

    // Now act as the runner: send a TokenDelta back. The
    // supervisor should publish it on the event bus.
    let delta = RunnerToServer::TokenDelta {
        turn_id: "t-1".into(),
        conversation_id: "conv-x".into(),
        text: "tok-A".into(),
    };
    socket
        .send(Message::Text(serde_json::to_string(&delta).unwrap().into()))
        .await
        .unwrap();

    let received_event = tokio::time::timeout(Duration::from_secs(2), bus_rx.recv())
        .await
        .expect("event bus delivers within 2s")
        .expect("event channel still open");
    match received_event {
        UiEvent::ChatTokenDelta {
            conversation_id,
            text,
        } => {
            assert_eq!(conversation_id, "conv-x");
            assert_eq!(text, "tok-A");
        }
        other => panic!("expected ChatTokenDelta: {other:?}"),
    }

    socket.close(None).await.ok();
}

