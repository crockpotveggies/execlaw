//! End-to-end test for the `host_render_chart` Rhai binding.
//!
//! A Rhai script builds a tiny line-chart spec, asks the host to
//! render it, and the test asserts the returned map carries:
//!   * a non-empty `attachment_id`
//!   * a valid SVG payload (starts with `<svg`)
//!   * a PNG data-URL whose decoded bytes carry the PNG magic header
//!
//! Uses a stub `HostCapabilities` that just records the bytes passed
//! to `create_artifact_attachment` so we can verify the renderer
//! actually shipped a real PNG (not garbage).

use async_trait::async_trait;
use execlaw_script::{
    AttachmentBytes, CreatedArtifact, HostCapError, HostCapabilities, InboundMessage,
    RouteOutcome, ScriptEngine, ScriptPlugin, WsFrameHandler, WsSubscriptionHandle,
};
use rhai::Dynamic;
use std::sync::{Arc, Mutex};

struct CapturingCaps {
    last_bytes: Arc<Mutex<Option<Vec<u8>>>>,
}

#[async_trait]
impl HostCapabilities for CapturingCaps {
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
        _plugin_id: &str,
        _filename: &str,
        _mime: &str,
        bytes: Vec<u8>,
        _ttl: Option<i64>,
    ) -> Result<CreatedArtifact, HostCapError> {
        let mut slot = self.last_bytes.lock().unwrap();
        *slot = Some(bytes.clone());
        Ok(CreatedArtifact {
            attachment_id: "att-chart-1".into(),
            sha256: "fakesha".into(),
            size_bytes: bytes.len() as u64,
        })
    }
}

const SCRIPT: &str = r#"
fn render(spec_json, width, height) {
    host_render_chart(spec_json, width, height, "chart.png", 0)
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_chart_to_svg_and_png_attachment() {
    let last_bytes = Arc::new(Mutex::new(None));
    let caps: Arc<dyn HostCapabilities> = Arc::new(CapturingCaps {
        last_bytes: last_bytes.clone(),
    });
    let factory = ScriptEngine::new();
    factory.set_host_capabilities(caps).ok();
    let plugin = ScriptPlugin::from_source("test-plugin", SCRIPT, &factory)
        .expect("script must parse");

    // Build a simple 6-point line-chart spec in Rust, hand the JSON
    // string to the Rhai-side `render(...)` helper. We use Rust here
    // (rather than constructing the spec in Rhai) because the spec
    // shape is tested directly by execlaw-charting; this test cares
    // about the binding's plumbing, not Rhai-side spec construction.
    let spec = serde_json::json!({
        "title": "Test temperature",
        "kind": "line",
        "x_label": "Hour",
        "y_label": "°C",
        "time_axis": false,
        "series": [
            {
                "name": "Temp",
                "points": [
                    {"x": 0.0, "y": 12.0},
                    {"x": 1.0, "y": 14.0},
                    {"x": 2.0, "y": 17.0},
                    {"x": 3.0, "y": 19.0},
                    {"x": 4.0, "y": 18.0},
                    {"x": 5.0, "y": 16.0}
                ]
            }
        ],
        "y_unit": "°C"
    });
    let spec_str = serde_json::to_string(&spec).unwrap();

    let r = plugin
        .invoke_async(
            "render",
            vec![
                Dynamic::from(rhai::ImmutableString::from(spec_str)),
                Dynamic::from(600_i64),
                Dynamic::from(360_i64),
            ],
        )
        .await
        .expect("render should succeed");

    assert_eq!(r["attachment_id"], "att-chart-1");
    let svg = r["svg"].as_str().expect("svg must be a string");
    assert!(svg.starts_with("<svg"), "svg must start with <svg, got: {}", &svg[..40.min(svg.len())]);
    assert!(svg.contains("Test temperature"), "svg must contain title");
    let data_url = r["png_data_url"]
        .as_str()
        .expect("png_data_url must be a string");
    assert!(
        data_url.starts_with("data:image/png;base64,"),
        "data url shape: {}",
        &data_url[..40]
    );

    // Verify the host received real PNG bytes (not the SVG string).
    let bytes = last_bytes.lock().unwrap().clone().expect("caps received bytes");
    assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4e, 0x47], "PNG magic header");
    assert_eq!(
        r["size_bytes"].as_i64().unwrap() as usize,
        bytes.len(),
        "size_bytes matches"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_malformed_spec_with_clean_error() {
    let last_bytes = Arc::new(Mutex::new(None));
    let caps: Arc<dyn HostCapabilities> = Arc::new(CapturingCaps {
        last_bytes: last_bytes.clone(),
    });
    let factory = ScriptEngine::new();
    factory.set_host_capabilities(caps).ok();
    let plugin = ScriptPlugin::from_source("test-plugin", SCRIPT, &factory).unwrap();
    let err = plugin
        .invoke_async(
            "render",
            vec![
                Dynamic::from(rhai::ImmutableString::from("not json")),
                Dynamic::from(600_i64),
                Dynamic::from(360_i64),
            ],
        )
        .await
        .expect_err("invalid spec must error");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid spec_json"),
        "expected spec_json error, got: {msg}"
    );
    assert!(
        last_bytes.lock().unwrap().is_none(),
        "renderer must not have stored anything"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamps_oversize_dimensions() {
    let last_bytes = Arc::new(Mutex::new(None));
    let caps: Arc<dyn HostCapabilities> = Arc::new(CapturingCaps {
        last_bytes: last_bytes.clone(),
    });
    let factory = ScriptEngine::new();
    factory.set_host_capabilities(caps).ok();
    let plugin = ScriptPlugin::from_source("test-plugin", SCRIPT, &factory).unwrap();
    let spec = serde_json::json!({
        "title": "tiny",
        "kind": "line",
        "time_axis": false,
        "series": [{"name": "a", "points": [{"x": 0.0, "y": 1.0}, {"x": 1.0, "y": 2.0}]}]
    });
    let r = plugin
        .invoke_async(
            "render",
            vec![
                Dynamic::from(rhai::ImmutableString::from(serde_json::to_string(&spec).unwrap())),
                Dynamic::from(99_999_i64), // clamps to 2400
                Dynamic::from(0_i64),       // clamps to default 400
            ],
        )
        .await
        .expect("render should succeed");
    assert_eq!(r["width"].as_i64().unwrap(), 2400, "width clamped to MAX_DIM");
    assert_eq!(r["height"].as_i64().unwrap(), 400, "height = default");
}
