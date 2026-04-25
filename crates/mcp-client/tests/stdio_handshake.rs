//! Integration tests for the stdio MCP client. The mock server lives
//! in `tests/bin/mock_mcp_server.rs` and is built as its own binary
//! by Cargo; we discover the path via `CARGO_BIN_EXE_mock_mcp_server`
//! at compile time.

use execlaw_mcp_client::{McpClient, McpNotification, McpResource, StdioSpec};
use std::collections::HashMap;
use std::sync::Arc;

const MOCK_SERVER: &str = env!("CARGO_BIN_EXE_mock_mcp_server");

fn spec_for(scenario: &str) -> StdioSpec {
    StdioSpec {
        command: MOCK_SERVER.into(),
        args: vec![scenario.into()],
        env: HashMap::new(),
        cwd: None,
    }
}

#[tokio::test]
async fn handshake_completes_and_lists_two_tools() {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let client = McpClient::stdio(&spec_for("tools"), shutdown.clone())
        .await
        .expect("MCP client connects");
    let tools = client.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().any(|t| t.name == "echo"));
    assert!(tools.iter().any(|t| t.name == "add"));

    let r = client
        .call_tool("echo", serde_json::json!({"text": "hello"}))
        .await
        .expect("call_tool");
    assert!(!r.is_error);
    assert_eq!(r.content.len(), 1);
    assert_eq!(r.content[0]["type"], "text");
    assert_eq!(r.content[0]["text"], "echo:hello");

    shutdown.notify_one();
}

#[tokio::test]
async fn server_initiated_sampling_is_refused() {
    // The mock server sends a `sampling/createMessage` request right
    // after `notifications/initialized`. The client must reply with a
    // -32601 error so the server records `refusal_seen = true`. The
    // `verify_refusal` tool returns is_error=false iff that happened.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let client = McpClient::stdio(&spec_for("refusal"), shutdown.clone())
        .await
        .expect("MCP client connects");
    // Brief sleep to let the server's race with `notifications/initialized`
    // settle deterministically — the client sends its initialized
    // notification before returning from McpClient::stdio, but the
    // server's reaction (sending sampling/createMessage) is async.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let r = client
        .call_tool("verify_refusal", serde_json::json!({}))
        .await
        .expect("call_tool");
    assert!(
        !r.is_error,
        "server expected to confirm refusal_seen=1, got {r:?}",
    );
    assert_eq!(r.content[0]["text"], "1");

    shutdown.notify_one();
}

#[tokio::test]
async fn tools_list_changed_notification_propagates() {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let client = McpClient::stdio(&spec_for("list_changed"), shutdown.clone())
        .await
        .expect("MCP client connects");
    let mut rx = client.subscribe_notifications();
    let _ = client
        .call_tool("trigger", serde_json::json!({}))
        .await
        .expect("call_tool");
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("notification arrives in time")
        .expect("channel still alive");
    assert!(matches!(n, McpNotification::ToolsListChanged));

    shutdown.notify_one();
}

#[tokio::test]
async fn list_resources_round_trips_a_resource_payload() {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let client = McpClient::stdio(&spec_for("resources"), shutdown.clone())
        .await
        .expect("MCP client connects");
    let resources: Vec<McpResource> = client.list_resources().await.expect("list_resources");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "file:///mock/readme.txt");
    let read = client
        .read_resource("file:///mock/readme.txt")
        .await
        .expect("read_resource");
    assert_eq!(read.contents.len(), 1);
    assert_eq!(read.contents[0]["text"], "mock body");

    shutdown.notify_one();
}
