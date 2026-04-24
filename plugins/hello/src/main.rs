//! Reference subprocess plugin for execlaw.
//!
//! Implements the minimal contract from `plugin-host::subprocess`:
//!
//! - One JSON-RPC request per line arriving on stdin.
//! - One JSON-RPC response per line written to stdout.
//! - Shape:
//!   → `{ "id": N, "method": "tool.call", "params": { "tool": ..., "args": {...} } }`
//!   ← `{ "id": N, "result": {...} }` or `{ "id": N, "error": { "code": ..., "message": "..." } }`
//! - The `shutdown` method exits the process cleanly.
//!
//! This binary is compiled with `cargo build -p plugin-hello` and
//! expected to sit at `./hello-plugin` next to `plugin.toml` inside
//! the staged plugin directory.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: u64,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcOk {
    id: u64,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RpcErr {
    id: u64,
    error: RpcErrBody,
}

#[derive(Debug, Serialize)]
struct RpcErrBody {
    code: i32,
    message: String,
}

fn handle(req: &RpcRequest) -> Result<serde_json::Value, (i32, String)> {
    match req.method.as_str() {
        "tool.call" => {
            // Phase 2 wire shape: params = { "tool": "...", "args": {...} }
            let tool = req
                .params
                .get("tool")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing 'tool' param".to_owned()))?;
            let args = req
                .params
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match tool {
                "hello.echo" => {
                    let message = args
                        .get("message")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| (-32602, "missing 'message' field in args".to_owned()))?;
                    Ok(serde_json::json!({
                        "greeting": "Hello from execlaw's reference plugin!",
                        "echoed": message,
                    }))
                }
                other => Err((-32601, format!("unknown tool '{other}'"))),
            }
        }
        "shutdown" => std::process::exit(0),
        other => Err((-32601, format!("unknown method '{other}'"))),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                // Protocol violation — write to stderr, keep going.
                eprintln!("plugin-hello: unparseable request: {e}: {trimmed}");
                continue;
            }
        };
        let resp_json = match handle(&req) {
            Ok(result) => serde_json::to_string(&RpcOk {
                id: req.id,
                result,
            })
            .unwrap_or_else(|_| "{}".into()),
            Err((code, message)) => serde_json::to_string(&RpcErr {
                id: req.id,
                error: RpcErrBody { code, message },
            })
            .unwrap_or_else(|_| "{}".into()),
        };
        if writeln!(stdout, "{resp_json}").is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
    }
}
