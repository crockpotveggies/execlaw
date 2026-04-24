//! Subprocess plugin tier (§4.4 tier 2).
//!
//! A plugin runs as a child process; the control plane exchanges
//! JSON-RPC messages over its stdin/stdout. This is the cheapest
//! isolation tier — no container, no runtime boundary — suitable for
//! porting existing Node / Python integrations without rewriting in
//! Rust.
//!
//! Wire format (one message per line):
//!
//! ```text
//! → {"id": 1, "method": "tool.call", "params": {...}}
//! ← {"id": 1, "result": {...}}
//! ← {"id": 1, "error": {"code": ..., "message": "..."}}
//! ```
//!
//! No cloud deps; just `tokio::process` + `serde_json`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, warn};

/// Config for launching a subprocess plugin.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    pub plugin_id: String,
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
}

/// A JSON-RPC request the control plane sends to a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

/// One JSON-RPC response from a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Live handle to a running subprocess plugin.
#[derive(Debug)]
pub struct SubprocessPlugin {
    spec: SubprocessSpec,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
}

impl SubprocessPlugin {
    /// Spawn the plugin process and start its stdout reader task.
    pub async fn spawn(spec: SubprocessSpec) -> Result<Self, String> {
        let mut cmd = Command::new(&spec.executable);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(d) = &spec.cwd {
            cmd.current_dir(d);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn '{}': {e}", spec.executable))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("missing stdin on spawned plugin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("missing stdout on spawned plugin")?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_for_reader = pending.clone();
        let plugin_id = spec.plugin_id.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<RpcResponse>(trimmed) {
                            Ok(resp) => {
                                let id = resp.id;
                                let mut p = pending_for_reader.lock().await;
                                if let Some(tx) = p.remove(&id) {
                                    let _ = tx.send(resp);
                                } else {
                                    warn!(
                                        plugin_id = %plugin_id,
                                        id,
                                        "rpc response with no matching pending request"
                                    );
                                }
                            }
                            Err(e) => warn!(
                                plugin_id = %plugin_id,
                                error = %e,
                                line,
                                "unparseable rpc line from plugin"
                            ),
                        }
                    }
                    Ok(None) => {
                        debug!(plugin_id = %plugin_id, "plugin stdout closed");
                        break;
                    }
                    Err(e) => {
                        warn!(plugin_id = %plugin_id, error = %e, "plugin stdout read error");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            spec,
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            next_id: AtomicU64::new(1),
            pending,
        })
    }

    pub fn plugin_id(&self) -> &str {
        &self.spec.plugin_id
    }

    /// Send a JSON-RPC call, await response.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = RpcRequest {
            id,
            method: method.to_owned(),
            params,
        };
        let mut line = serde_json::to_vec(&req).map_err(|e| format!("encode rpc: {e}"))?;
        line.push(b'\n');

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(&line)
                .await
                .map_err(|e| format!("write rpc: {e}"))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("flush rpc: {e}"))?;
        }

        let resp = rx
            .await
            .map_err(|_| "plugin dropped before responding".to_string())?;
        if let Some(err) = resp.error {
            return Err(format!("rpc error {}: {}", err.code, err.message));
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    /// Ask the child to exit. Best-effort; the `kill_on_drop` guarantee
    /// still holds if this fails.
    pub async fn shutdown(&self) {
        // Best-effort graceful shutdown; any error just falls through to kill.
        let _ = self.call("shutdown", serde_json::Value::Null).await;
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_request_serializes_as_expected() {
        let req = RpcRequest {
            id: 7,
            method: "tool.call".into(),
            params: serde_json::json!({"name": "ping"}),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"method\":\"tool.call\""));
        assert!(s.contains("\"name\":\"ping\""));
    }

    #[test]
    fn rpc_response_decodes_success_and_error() {
        let ok: RpcResponse =
            serde_json::from_str(r#"{"id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(ok.id, 1);
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let err: RpcResponse =
            serde_json::from_str(r#"{"id":2,"error":{"code":-1,"message":"boom"}}"#)
                .unwrap();
        assert_eq!(err.id, 2);
        assert!(err.result.is_none());
        assert_eq!(err.error.unwrap().message, "boom");
    }

    #[tokio::test]
    async fn spawn_nonexistent_binary_returns_error() {
        let spec = SubprocessSpec {
            plugin_id: "p1".into(),
            executable: "definitely-not-a-real-binary-xyz-123".into(),
            args: vec![],
            cwd: None,
        };
        let err = SubprocessPlugin::spawn(spec).await.unwrap_err();
        assert!(err.to_lowercase().contains("spawn"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn echo_plugin_round_trips_rpc() {
        // On unix we can use `sh -c` to provide a tiny JSON-RPC echo
        // responder. On Windows we skip this test (see cfg).
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{"id":%s,"result":{"echo":"ok"}}\n' "$id"
done
"#;
        let spec = SubprocessSpec {
            plugin_id: "echo".into(),
            executable: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
        };
        let plugin = SubprocessPlugin::spawn(spec).await.unwrap();
        let result = plugin
            .call("ping", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"echo": "ok"}));
        plugin.shutdown().await;
    }
}
