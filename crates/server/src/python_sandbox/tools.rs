//! `ToolImpl` implementations for the four python-sandbox tools.
//!
//! Each impl is a tiny adapter: it parses args, delegates to the
//! shared [`PythonSandboxService`], serializes the result. No
//! business logic lives here — Phase 2b's KernelPool and Phase 5's
//! streaming publish are the substance; this layer just satisfies
//! the `ToolImpl` contract the host's dispatch expects.
//!
//! Registration: the manifest declares each tool with
//! `host_implemented = true`, which means the catalog entry
//! (schema + trust_floor + description) comes from `plugin.toml`
//! but the dispatch path looks for a builtin with the matching
//! name. [`python_sandbox_tools`] returns the vec to pass into
//! `register_builtins`.

use crate::python_sandbox::client::DEFAULT_EXECUTE_TIMEOUT;
use crate::python_sandbox::service::PythonSandboxService;
use async_trait::async_trait;
use execlaw_core::tool::{
    Capability, ToolCtx, ToolDescriptor, ToolImpl, ToolLatency, ToolOutcome, ToolSource,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Trust classes allowed to invoke any python.* tool. Aligns with
/// the manifest's `trust_floor = "Controller"`; this slice is the
/// dispatch-layer enforcement to match.
fn controller_only() -> Vec<String> {
    vec!["Controller".into()]
}

/// All four tools share this descriptor skeleton — same trust gate,
/// same source tag. Per-tool fields (name / description / schema /
/// latency / capabilities) fill in via the builder below.
fn make_descriptor(
    name: &str,
    description: &str,
    schema: Value,
    latency: ToolLatency,
    caps: Vec<Capability>,
) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: description.into(),
        schema,
        source: ToolSource::Builtin,
        latency,
        capabilities: caps,
        default_allowed_classes: controller_only(),
    }
}

// =====================================================================
// python.execute
// =====================================================================

#[derive(Debug, Deserialize)]
struct ExecuteArgs {
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct PythonExecuteTool {
    descriptor: ToolDescriptor,
    service: Arc<PythonSandboxService>,
}

impl PythonExecuteTool {
    pub fn new(service: Arc<PythonSandboxService>) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 200_000
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 100,
                    "maximum": 600_000,
                    "default": 30_000
                }
            },
            "required": ["code"],
            "additionalProperties": false
        });
        Self {
            descriptor: make_descriptor(
                "python.execute",
                "Execute Python in a persistent per-conversation sandbox. State \
                 (variables, dataframes, imports) survives across turns. \
                 \n\nFILES: anything that arrives via the conversation's \
                 attachment list (user-uploaded files OR results from \
                 `web_fetch(url, as_attachment: true)`) is auto-mounted at \
                 /work/<convo>/uploads/<filename> before this tool runs. Write \
                 user-facing outputs to /work/<convo>/outputs/ — anything \
                 written there is auto-published as a chat attachment chip. \
                 \n\nNETWORK: the sandbox runs --network none. Python CANNOT \
                 fetch URLs (`pd.read_csv('https://…')` will hang and time out). \
                 To ingest a remote dataset: first call \
                 `web_fetch(url, as_attachment: true)` — that lands the bytes \
                 as an attachment which auto-hydrates into \
                 /work/<convo>/uploads/<filename> for the NEXT execute call. \
                 Then `pd.read_csv('/work/uploads/<filename>')` reads it locally. \
                 \n\nPACKAGES: pandas, polars, duckdb, pyarrow, numpy, openpyxl, \
                 ipython, httpx. Matplotlib is NOT installed — for charts, \
                 compute the aggregated data in Python (e.g. \
                 `df.groupby('species').size().to_dict()`) then call \
                 chart.render with the resulting series.",
                schema,
                ToolLatency::High,
                vec![Capability::AttachmentSend],
            ),
            service,
        }
    }
}

#[async_trait]
impl ToolImpl for PythonExecuteTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn invoke(&self, ctx: ToolCtx, args: Value) -> ToolOutcome {
        let args: ExecuteArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err("invalid_argument", e.to_string()),
        };
        let timeout = args
            .timeout_ms
            .map(|ms| Duration::from_millis(ms.clamp(100, 600_000)))
            .unwrap_or(DEFAULT_EXECUTE_TIMEOUT);

        // Hydrate this conversation's attachments into /work/<convo>/
        // uploads/ on first execute. Idempotent; subsequent executes
        // skip the DB query + disk walk via the service's cache.
        // Hydration failures are best-effort — log + continue, so a
        // missing blob doesn't block all execute calls. The kernel
        // just won't see that file under /work/uploads/.
        if let Err(e) = self.service.hydrate_if_needed(&ctx.conversation_id).await {
            tracing::warn!(
                ?e,
                convo = %ctx.conversation_id,
                "python_sandbox hydration failed; proceeding without uploaded files"
            );
        }

        let kernel = match self.service.pool().ensure_for(&ctx.conversation_id).await {
            Ok(k) => k,
            Err(e) => return ToolOutcome::err("kernel_spawn_failed", e.to_string()),
        };
        let result = match self
            .service
            .pool()
            .client()
            .execute(&kernel, &args.code, timeout)
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutcome::err("execute_failed", e.to_string()),
        };
        // Inject `chat_component_kind` so the SPA's chatComponentRegistry
        // routes this tool_result to PythonExecuteComponent rather than
        // the default raw-JSON renderer. Sibling field of the
        // ExecuteResult fields — doesn't disturb the existing shape.
        let mut value = match serde_json::to_value(&result) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::err("serialize_result_failed", e.to_string()),
        };
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "chat_component_kind".to_string(),
                Value::String("python_execute".to_string()),
            );
        }
        ToolOutcome::Ok(value)
    }
}

// =====================================================================
// python.reset
// =====================================================================

pub struct PythonResetTool {
    descriptor: ToolDescriptor,
    service: Arc<PythonSandboxService>,
}

impl PythonResetTool {
    pub fn new(service: Arc<PythonSandboxService>) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        });
        Self {
            descriptor: make_descriptor(
                "python.reset",
                "Soft-reset the conversation's Python kernel — clears variables, \
                 imports, and module state without restarting the container. \
                 Files in /work are NOT affected.",
                schema,
                ToolLatency::Low,
                vec![],
            ),
            service,
        }
    }
}

#[async_trait]
impl ToolImpl for PythonResetTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        match self.service.pool().reset(&ctx.conversation_id).await {
            Ok(()) => ToolOutcome::Ok(json!({ "ok": true })),
            Err(e) => ToolOutcome::err("reset_failed", e.to_string()),
        }
    }
}

// =====================================================================
// python.interrupt
// =====================================================================

pub struct PythonInterruptTool {
    descriptor: ToolDescriptor,
    service: Arc<PythonSandboxService>,
}

impl PythonInterruptTool {
    pub fn new(service: Arc<PythonSandboxService>) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        });
        Self {
            descriptor: make_descriptor(
                "python.interrupt",
                "Host-internal: send a KeyboardInterrupt to the conversation's \
                 currently-executing Python cell. Wired to the SPA's stop button.",
                schema,
                ToolLatency::Low,
                vec![],
            ),
            service,
        }
    }
}

#[async_trait]
impl ToolImpl for PythonInterruptTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        match self.service.pool().interrupt(&ctx.conversation_id).await {
            Ok(()) => ToolOutcome::Ok(json!({ "ok": true })),
            Err(e) => ToolOutcome::err("interrupt_failed", e.to_string()),
        }
    }
}

// =====================================================================
// python.list_files
// =====================================================================

pub struct PythonListFilesTool {
    descriptor: ToolDescriptor,
    service: Arc<PythonSandboxService>,
}

impl PythonListFilesTool {
    pub fn new(service: Arc<PythonSandboxService>) -> Self {
        let schema = json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        });
        Self {
            descriptor: make_descriptor(
                "python.list_files",
                "List the current contents of /work/<convo>/uploads/ and \
                 /work/<convo>/outputs/. Cheap; does no kernel work. Useful \
                 after idle eviction to recover what files are still on disk.",
                schema,
                ToolLatency::Low,
                vec![],
            ),
            service,
        }
    }
}

#[async_trait]
impl ToolImpl for PythonListFilesTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }
    async fn invoke(&self, ctx: ToolCtx, _args: Value) -> ToolOutcome {
        let convo_root = self.service.work_root().join(ctx.conversation_id.as_str());
        let uploads = list_dir(&convo_root.join("uploads"));
        let outputs = list_dir(&convo_root.join("outputs"));
        ToolOutcome::Ok(json!({
            "uploads": uploads,
            "outputs": outputs,
        }))
    }
}

fn list_dir(dir: &std::path::Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push(json!({
            "name": name,
            "size": meta.len(),
            "mime": super::service::PLUGIN_ID, // placeholder, see below
        }));
        // The MIME placeholder above is wrong — fixing inline:
        if let Some(last) = out.last_mut() {
            if let Some(obj) = last.as_object_mut() {
                obj.insert(
                    "mime".to_string(),
                    Value::String(guess_mime_for_listing(&name)),
                );
            }
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

fn guess_mime_for_listing(name: &str) -> String {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "parquet" => "application/vnd.apache.parquet",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

// =====================================================================
// Builder
// =====================================================================

/// Return all four python_sandbox tools as the `register_builtins`
/// expects. Each tool holds an `Arc<PythonSandboxService>` so calls
/// from different concurrent turns share the same kernel pool +
/// output watcher.
pub fn python_sandbox_tools(service: Arc<PythonSandboxService>) -> Vec<Arc<dyn ToolImpl>> {
    vec![
        Arc::new(PythonExecuteTool::new(service.clone())),
        Arc::new(PythonResetTool::new(service.clone())),
        Arc::new(PythonInterruptTool::new(service.clone())),
        Arc::new(PythonListFilesTool::new(service)),
    ]
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::Database;
    use execlaw_core::db::DbConfig;
    use execlaw_core::migrations::MigrationRunner;

    fn service_for_test() -> Arc<PythonSandboxService> {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&artifacts).unwrap();
        // Bogus URL — we only use the service for non-network calls
        // (list_files reads from disk). Tests that need execute use
        // a real gateway via EXECLAW_TEST_GATEWAY_URL.
        PythonSandboxService::new(
            String::from("http://127.0.0.1:1"),
            work,
            artifacts,
            db,
            crate::events::EventBus::new(),
        )
        .expect("service constructs (watcher only touches disk)")
    }

    #[test]
    fn python_sandbox_tools_returns_all_four_with_distinct_names() {
        // Need a tokio runtime so OutputWatcher::start's
        // spawn_blocking-free path is satisfied (the watcher spawns
        // a tokio task on init).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let svc = service_for_test();
        let tools = python_sandbox_tools(svc);
        let names: Vec<&str> = tools.iter().map(|t| t.descriptor().name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "python.execute",
                "python.reset",
                "python.interrupt",
                "python.list_files"
            ]
        );
    }

    #[test]
    fn every_tool_gates_to_controller_only() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let svc = service_for_test();
        for tool in python_sandbox_tools(svc) {
            assert_eq!(
                tool.descriptor().default_allowed_classes,
                vec!["Controller".to_string()],
                "tool {} must gate to Controller only",
                tool.descriptor().name
            );
        }
    }

    #[test]
    fn execute_tool_requires_attachment_send_capability() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let svc = service_for_test();
        let tools = python_sandbox_tools(svc);
        let exec = tools
            .iter()
            .find(|t| t.descriptor().name == "python.execute")
            .unwrap();
        assert!(
            exec.descriptor()
                .capabilities
                .iter()
                .any(|c| matches!(c, Capability::AttachmentSend)),
            "python.execute should declare AttachmentSend so its outputs surface as chips"
        );
    }

    #[tokio::test]
    async fn list_files_returns_uploads_and_outputs_from_disk() {
        let svc = service_for_test();
        // Lay down files in BOTH dirs for one convo.
        let convo = "list-test-convo";
        let uploads = svc.work_root().join(convo).join("uploads");
        let outputs = svc.work_root().join(convo).join("outputs");
        std::fs::create_dir_all(&uploads).unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(uploads.join("sales.csv"), b"a,b\n1,2\n").unwrap();
        std::fs::write(outputs.join("region_summary.csv"), b"r,t\nW,1\n").unwrap();
        std::fs::write(outputs.join("chart.png"), b"\x89PNG\r\n").unwrap();

        let tools = python_sandbox_tools(svc.clone());
        let list = tools
            .iter()
            .find(|t| t.descriptor().name == "python.list_files")
            .unwrap();
        let ctx = ToolCtx::empty(
            execlaw_core::ids::ConversationId::from(convo),
            "Controller",
            Arc::new(execlaw_core::tool::SystemClock),
        );
        let out = list.invoke(ctx, json!({})).await;
        match out {
            ToolOutcome::Ok(v) => {
                let uploads = v["uploads"].as_array().unwrap();
                let outputs = v["outputs"].as_array().unwrap();
                assert_eq!(uploads.len(), 1);
                assert_eq!(uploads[0]["name"], "sales.csv");
                assert_eq!(uploads[0]["mime"], "text/csv");
                assert_eq!(outputs.len(), 2);
                // Sorted by name: chart.png, region_summary.csv
                assert_eq!(outputs[0]["name"], "chart.png");
                assert_eq!(outputs[0]["mime"], "image/png");
                assert_eq!(outputs[1]["name"], "region_summary.csv");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Live end-to-end: PythonExecuteTool against a real gateway.
    /// Gated on EXECLAW_TEST_GATEWAY_URL so `cargo test` doesn't
    /// require Docker. Pairs with the existing
    /// live_execute_parity_with_python_smoke in client.rs but
    /// exercises the dispatch layer (ToolImpl::invoke) rather than
    /// the bare client.
    #[tokio::test]
    async fn live_execute_tool_round_trip() {
        let Ok(url) = std::env::var("EXECLAW_TEST_GATEWAY_URL") else {
            return;
        };
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&artifacts).unwrap();

        let svc =
            PythonSandboxService::new(url, work, artifacts, db, crate::events::EventBus::new())
                .unwrap();
        let tools = python_sandbox_tools(svc.clone());
        let exec = tools
            .iter()
            .find(|t| t.descriptor().name == "python.execute")
            .unwrap();

        let ctx = ToolCtx::empty(
            execlaw_core::ids::ConversationId::from("live-tool-test"),
            "Controller",
            Arc::new(execlaw_core::tool::SystemClock),
        );
        let out = exec
            .invoke(ctx, json!({"code": "1 + 1", "timeout_ms": 10_000}))
            .await;
        let result = match out {
            ToolOutcome::Ok(v) => v,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(result["status"], "ok");
        assert_eq!(result["execution_count"], 1);
        // SPA dispatch hint: the chat-component registry routes this
        // tool_result to PythonExecuteComponent.tsx via this kind.
        assert_eq!(result["chat_component_kind"], "python_execute");
        // outputs[0] is ExecuteResult kind with text/plain "2".
        let outputs = result["outputs"].as_array().unwrap();
        let er = outputs
            .iter()
            .find(|o| o["kind"] == "execute_result")
            .expect("execute_result output");
        let bundle = er["bundle"].as_array().unwrap();
        let plain = bundle
            .iter()
            .find(|m| m["mime_type"] == "text/plain")
            .expect("text/plain");
        assert_eq!(plain["data"], "2");

        // Clean up: drop the conversation's kernel so the live test
        // suite stays at baseline.
        svc.pool()
            .drop_kernel(&execlaw_core::ids::ConversationId::from("live-tool-test"))
            .await
            .ok();
    }

    #[tokio::test]
    async fn execute_invalid_args_returns_clear_error() {
        let svc = service_for_test();
        let tools = python_sandbox_tools(svc);
        let exec = tools
            .iter()
            .find(|t| t.descriptor().name == "python.execute")
            .unwrap();
        let ctx = ToolCtx::empty(
            execlaw_core::ids::ConversationId::from("c"),
            "Controller",
            Arc::new(execlaw_core::tool::SystemClock),
        );
        // Missing required `code` field.
        let out = exec.invoke(ctx, json!({"timeout_ms": 1000})).await;
        match out {
            ToolOutcome::Err { code, .. } => {
                assert_eq!(code, "invalid_argument");
            }
            other => panic!("expected Err(invalid_argument), got {other:?}"),
        }
    }
}
