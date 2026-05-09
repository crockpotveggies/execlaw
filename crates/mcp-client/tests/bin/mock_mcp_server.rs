// This is a test fixture that prioritises readability over clippy
// hygiene — the `if x.is_some() { let v = x.unwrap(); }` pattern
// appears throughout because it mirrors the JSON-RPC spec's
// "method+id implies request" / "method-only implies notification"
// dispatch.
#![allow(clippy::unnecessary_unwrap)]

//! Mock stdio MCP server for the `mcp-client` integration tests.
//!
//! Reads JSON-RPC frames on stdin, replies on stdout, switches
//! behaviour based on the scenario passed as `argv[1]`. Avoids
//! pulling in a real Python or Node MCP runtime — keeps the test
//! suite hermetic.
//!
//! Scenarios:
//!   * `tools` — exposes `echo` + `add`; echo concatenates "echo:".
//!   * `refusal` — sends an unsolicited `sampling/createMessage`
//!     request right after the client's `notifications/initialized`,
//!     then exposes a `verify_refusal` tool that returns whether the
//!     client's refusal was seen.
//!   * `list_changed` — exposes `trigger`; calling it emits a
//!     `notifications/tools/list_changed` BEFORE the tool's response.
//!   * `resources` — exposes one resource at `file:///mock/readme.txt`
//!     whose body is "mock body".

use std::io::{BufRead, BufReader, Write};

const SCENARIO_TOOLS: &str = "tools";
const SCENARIO_REFUSAL: &str = "refusal";
const SCENARIO_TOOLS_LIST_CHANGED: &str = "list_changed";
const SCENARIO_RESOURCES: &str = "resources";

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let stdin = stdin.lock();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    let mut refusal_seen = false;

    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        let frame: serde_json::Value = match serde_json::from_str(line.trim_end()) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        line.clear();

        let id = frame.get("id").cloned();
        let method = frame
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if method == "initialize" && id.is_some() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.unwrap(),
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.1.0"},
                }
            });
            write_frame(&mut stdout, &resp);
            continue;
        }

        if method == "notifications/initialized" {
            if scenario == SCENARIO_REFUSAL {
                let req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 9001,
                    "method": "sampling/createMessage",
                    "params": {"messages": []}
                });
                write_frame(&mut stdout, &req);
            }
            continue;
        }

        // Capture the client's refusal of our sampling request.
        if id == Some(serde_json::json!(9001))
            && frame.get("error").is_some()
            && scenario == SCENARIO_REFUSAL
        {
            refusal_seen = true;
            continue;
        }

        if method == "tools/list" && id.is_some() {
            let tools = match scenario.as_str() {
                SCENARIO_TOOLS => vec![tool("echo"), tool("add")],
                SCENARIO_REFUSAL => vec![tool("verify_refusal")],
                SCENARIO_TOOLS_LIST_CHANGED => vec![tool("trigger")],
                SCENARIO_RESOURCES => vec![],
                _ => vec![],
            };
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.unwrap(),
                "result": { "tools": tools }
            });
            write_frame(&mut stdout, &resp);
            continue;
        }

        if method == "tools/call" && id.is_some() {
            let params = frame.get("params").cloned().unwrap_or_default();
            let tool_name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let result_payload = match (scenario.as_str(), tool_name.as_str()) {
                (SCENARIO_TOOLS, "echo") => {
                    let text = args
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    serde_json::json!({
                        "content": [{"type": "text", "text": format!("echo:{text}")}],
                        "isError": false
                    })
                }
                (SCENARIO_TOOLS, "add") => serde_json::json!({
                    "content": [{"type": "text", "text": "42"}],
                    "isError": false
                }),
                (SCENARIO_REFUSAL, "verify_refusal") => {
                    let txt = if refusal_seen { "1" } else { "0" };
                    serde_json::json!({
                        "content": [{"type": "text", "text": txt}],
                        "isError": !refusal_seen
                    })
                }
                (SCENARIO_TOOLS_LIST_CHANGED, "trigger") => {
                    let notif = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/tools/list_changed"
                    });
                    write_frame(&mut stdout, &notif);
                    serde_json::json!({"content": [], "isError": false})
                }
                _ => serde_json::json!({"content": [], "isError": true}),
            };
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.unwrap(),
                "result": result_payload
            });
            write_frame(&mut stdout, &resp);
            continue;
        }

        if method == "resources/list" && id.is_some() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.unwrap(),
                "result": {
                    "resources": [
                        {"uri": "file:///mock/readme.txt", "name": "readme", "mimeType": "text/plain"}
                    ]
                }
            });
            write_frame(&mut stdout, &resp);
            continue;
        }

        if method == "resources/read" && id.is_some() {
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.unwrap(),
                "result": {
                    "contents": [
                        {"uri": "file:///mock/readme.txt", "mimeType": "text/plain", "text": "mock body"}
                    ]
                }
            });
            write_frame(&mut stdout, &resp);
            continue;
        }

        if let Some(id) = id {
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown method: {method}")}
            });
            write_frame(&mut stdout, &resp);
        }
    }
}

fn tool(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": format!("mock tool {name}"),
        "inputSchema": {"type": "object"}
    })
}

fn write_frame<W: Write>(w: &mut W, value: &serde_json::Value) {
    let mut s = serde_json::to_string(value).expect("encode frame");
    s.push('\n');
    let _ = w.write_all(s.as_bytes());
    let _ = w.flush();
}
