//! Integration tests for the `host_create_attachment` Rhai binding.
//!
//! Plugs a stub `HostCapabilities` into a `ScriptEngine`, then runs
//! a tiny Rhai script that calls the binding with various input
//! shapes (raw base64, `data:` URL, oversized, malformed). The stub
//! captures the bytes the plugin passed in so we can assert the
//! binding decoded its input correctly.

use async_trait::async_trait;
use execlaw_script::{
    AttachmentBytes, CreatedArtifact, HostCapError, HostCapabilities, InboundMessage, RouteOutcome,
    ScriptEngine, ScriptPlugin, WsFrameHandler, WsSubscriptionHandle,
};
use rhai::Dynamic;
use std::sync::{Arc, Mutex};

/// Stub caps that records every `create_artifact_attachment` call.
struct RecordingCaps {
    log: Arc<Mutex<Vec<(String, String, String, Vec<u8>, Option<i64>)>>>,
}

#[async_trait]
impl HostCapabilities for RecordingCaps {
    async fn sidecar_url(&self, _: &str) -> Option<String> {
        None
    }
    async fn ws_subscribe_with_init(
        &self,
        _: String,
        _: Vec<(String, String)>,
        _: Vec<String>,
        _: WsFrameHandler,
    ) -> Result<WsSubscriptionHandle, HostCapError> {
        unreachable!("stub")
    }
    async fn route_inbound(&self, _: InboundMessage) -> Result<RouteOutcome, HostCapError> {
        unreachable!("stub")
    }
    async fn get_attachment_bytes_b64(&self, _: &str) -> Result<AttachmentBytes, HostCapError> {
        unreachable!("stub")
    }
    async fn vault_get(&self, _: &str, _: &str) -> Result<Option<String>, HostCapError> {
        Ok(None)
    }
    async fn vault_put(&self, _: &str, _: &str, _: &str) -> Result<(), HostCapError> {
        Ok(())
    }
    async fn vault_delete(&self, _: &str, _: &str) -> Result<bool, HostCapError> {
        Ok(false)
    }
    async fn create_artifact_attachment(
        &self,
        plugin_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: Vec<u8>,
        ttl_seconds: Option<i64>,
    ) -> Result<CreatedArtifact, HostCapError> {
        let mut log = self.log.lock().unwrap();
        log.push((
            plugin_id.to_owned(),
            filename.to_owned(),
            mime_type.to_owned(),
            bytes.clone(),
            ttl_seconds,
        ));
        Ok(CreatedArtifact {
            attachment_id: "att-fixed-id".to_owned(),
            sha256: "deadbeef".to_owned(),
            size_bytes: bytes.len() as u64,
        })
    }
}

fn engine_with_recorder() -> (
    ScriptEngine,
    Arc<Mutex<Vec<(String, String, String, Vec<u8>, Option<i64>)>>>,
) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let caps: Arc<dyn HostCapabilities> = Arc::new(RecordingCaps { log: log.clone() });
    let factory = ScriptEngine::new();
    factory.set_host_capabilities(caps).ok();
    (factory, log)
}

const SCRIPT: &str = r#"
fn create_from_raw_b64(payload, mime, filename, ttl) {
    host_create_attachment(payload, mime, filename, ttl)
}

fn create_from_data_url(payload, mime, filename, ttl) {
    host_create_attachment(payload, mime, filename, ttl)
}

fn create_oversize(payload, mime, filename, ttl) {
    host_create_attachment(payload, mime, filename, ttl)
}
"#;

fn load(engine: &ScriptEngine) -> ScriptPlugin {
    ScriptPlugin::from_source("test-plugin", SCRIPT, engine).expect("test script must parse")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_base64_round_trips_to_caps() {
    let (engine, log) = engine_with_recorder();
    let plugin = load(&engine);
    // base64 of "PNG-bytes" — 9 bytes.
    let r = plugin
        .invoke_async(
            "create_from_raw_b64",
            vec![
                Dynamic::from(rhai::ImmutableString::from("UE5HLWJ5dGVz")),
                Dynamic::from(rhai::ImmutableString::from("image/png")),
                Dynamic::from(rhai::ImmutableString::from("forecast.png")),
                Dynamic::from(3600_i64),
            ],
        )
        .await
        .expect("call should succeed");
    assert_eq!(r["attachment_id"], "att-fixed-id");
    assert_eq!(r["size_bytes"], 9);
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    let (pid, filename, mime, bytes, ttl) = &log[0];
    assert_eq!(pid, "test-plugin");
    assert_eq!(filename, "forecast.png");
    assert_eq!(mime, "image/png");
    assert_eq!(bytes, b"PNG-bytes");
    assert_eq!(*ttl, Some(3600));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_url_strips_prefix_before_decoding() {
    let (engine, log) = engine_with_recorder();
    let plugin = load(&engine);
    let _r = plugin
        .invoke_async(
            "create_from_data_url",
            vec![
                Dynamic::from(rhai::ImmutableString::from(
                    "data:image/png;base64,UE5HLWJ5dGVz",
                )),
                Dynamic::from(rhai::ImmutableString::from("image/png")),
                Dynamic::from(rhai::ImmutableString::from("chart.png")),
                Dynamic::from(0_i64),
            ],
        )
        .await
        .expect("call should succeed");
    let log = log.lock().unwrap();
    assert_eq!(log[0].3, b"PNG-bytes", "data URL prefix must be stripped");
    assert_eq!(log[0].4, None, "ttl=0 collapses to None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_payload_errors_before_touching_caps() {
    let (engine, log) = engine_with_recorder();
    let plugin = load(&engine);
    // 11 MiB of zero bytes → 14.7 MiB base64. Way over the 10 MiB cap.
    use base64::Engine as _;
    let raw = vec![0u8; 11 * 1024 * 1024];
    let payload = base64::engine::general_purpose::STANDARD.encode(&raw);
    let err = plugin
        .invoke_async(
            "create_oversize",
            vec![
                Dynamic::from(rhai::ImmutableString::from(payload)),
                Dynamic::from(rhai::ImmutableString::from("application/octet-stream")),
                Dynamic::from(rhai::ImmutableString::from("big.bin")),
                Dynamic::from(0_i64),
            ],
        )
        .await
        .expect_err("oversize should error");
    let msg = err.to_string();
    assert!(
        msg.contains("exceeds max"),
        "expected size-cap error, got: {msg}"
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "caps must not have been called"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_ttl_is_rejected() {
    let (engine, log) = engine_with_recorder();
    let plugin = load(&engine);
    let err = plugin
        .invoke_async(
            "create_from_raw_b64",
            vec![
                Dynamic::from(rhai::ImmutableString::from("UE5HLWJ5dGVz")),
                Dynamic::from(rhai::ImmutableString::from("image/png")),
                Dynamic::from(rhai::ImmutableString::from("forecast.png")),
                Dynamic::from(-5_i64),
            ],
        )
        .await
        .expect_err("negative ttl should error");
    let msg = err.to_string();
    assert!(
        msg.contains("ttl_seconds"),
        "expected ttl error, got: {msg}"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_base64_returns_clean_error() {
    let (engine, log) = engine_with_recorder();
    let plugin = load(&engine);
    let err = plugin
        .invoke_async(
            "create_from_raw_b64",
            vec![
                // Whitespace-laden garbage that won't even pad correctly.
                Dynamic::from(rhai::ImmutableString::from("!!! not base64 !!!")),
                Dynamic::from(rhai::ImmutableString::from("image/png")),
                Dynamic::from(rhai::ImmutableString::from("forecast.png")),
                Dynamic::from(0_i64),
            ],
        )
        .await
        .expect_err("malformed b64 should error");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid base64"),
        "expected base64 error, got: {msg}"
    );
    assert!(log.lock().unwrap().is_empty());
}
