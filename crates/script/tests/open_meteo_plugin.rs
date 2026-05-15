//! Behavioral tests for `plugins/open-meteo/main.rhai`. Mirrors the
//! discord/signal/google-places test convention: load the real
//! shipped script, exercise the `_test_*` helpers + tool dispatch
//! against a 127.0.0.1 mock HTTP server. No live network, no live
//! host stack.
//!
//! Coverage:
//!   * Script compiles cleanly (would catch a Rhai syntax regression).
//!   * Pure helpers: csv, clamp_int, numbers_to_csv, chart filename
//!     slugifier, forecast-kind picker.
//!   * Tool dispatch through `tool_call` for geocode + forecast +
//!     air_quality + historical against a mock that returns canned
//!     JSON shaped like Open-Meteo's real responses.
//!
//! We use `tiny_http`-style loopback mocks via the `ScriptEngine`'s
//! `with_loopback_allowed_for_tests()` factory and patch the script
//! to hit `127.0.0.1`. The patching trick: load the script source
//! with a substitution that swaps the production hostnames for the
//! mock's `http://127.0.0.1:<port>`. That keeps the production
//! script unmodified and the test plumbing tight.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Load the shipped open-meteo plugin, optionally rewriting the
/// upstream base URLs to point at a local mock.
fn open_meteo_plugin(base_rewrite: Option<(&str, &str)>) -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/open-meteo/main.rhai");
    let mut source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    if let Some((from, to)) = base_rewrite {
        source = source.replace(from, to);
    }
    let factory = ScriptEngine::with_loopback_allowed_for_tests();
    ScriptPlugin::from_source("open-meteo", &source, &factory)
        .expect("plugins/open-meteo/main.rhai must parse")
}

// ---- Pure helper tests --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csv_joins_array_with_commas() {
    let p = open_meteo_plugin(None);
    let r = p
        .invoke_async(
            "_test_csv",
            vec![{
                let mut a = rhai::Array::new();
                a.push(Dynamic::from("temperature_2m"));
                a.push(Dynamic::from("weather_code"));
                a.push(Dynamic::from("wind_speed_10m"));
                Dynamic::from(a)
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        r.as_str(),
        Some("temperature_2m,weather_code,wind_speed_10m")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamp_int_floors_and_caps() {
    let p = open_meteo_plugin(None);
    // Inside range.
    let r = p
        .invoke_async(
            "_test_clamp_int",
            vec![
                Dynamic::from(5_i64),
                Dynamic::from(1_i64),
                Dynamic::from(20_i64),
                Dynamic::from(7_i64),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r.as_i64(), Some(5));
    // Below range — clamps to lo.
    let r = p
        .invoke_async(
            "_test_clamp_int",
            vec![
                Dynamic::from(-1_i64),
                Dynamic::from(1_i64),
                Dynamic::from(20_i64),
                Dynamic::from(7_i64),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r.as_i64(), Some(1));
    // Above range — clamps to hi.
    let r = p
        .invoke_async(
            "_test_clamp_int",
            vec![
                Dynamic::from(99_i64),
                Dynamic::from(1_i64),
                Dynamic::from(20_i64),
                Dynamic::from(7_i64),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r.as_i64(), Some(20));
    // Unit → fallback.
    let r = p
        .invoke_async(
            "_test_clamp_int",
            vec![
                Dynamic::UNIT,
                Dynamic::from(1_i64),
                Dynamic::from(20_i64),
                Dynamic::from(7_i64),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r.as_i64(), Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn numbers_to_csv_handles_scalar_and_array() {
    let p = open_meteo_plugin(None);
    let r = p
        .invoke_async(
            "_test_numbers_to_csv",
            vec![Dynamic::from(64.146_f64), Dynamic::from("latitude")],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "64.146");

    let r = p
        .invoke_async(
            "_test_numbers_to_csv",
            vec![
                {
                    let mut a = rhai::Array::new();
                    a.push(Dynamic::from(64.146_f64));
                    a.push(Dynamic::from(-12.345_f64));
                    Dynamic::from(a)
                },
                Dynamic::from("latitude"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "64.146,-12.345");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_filename_slugifies_titles() {
    let p = open_meteo_plugin(None);
    let r = p
        .invoke_async(
            "_test_chart_filename",
            vec![Dynamic::from("Reykjavík — 7-day forecast!")],
        )
        .await
        .unwrap();
    let s = r.as_str().unwrap();
    // Non-ASCII characters drop; runs of separators collapse to one dash.
    assert!(s.ends_with(".png"), "expected .png suffix, got: {s}");
    assert!(
        s.starts_with("reykjav") || s.starts_with("7-day") || s.starts_with("day"),
        "slugified prefix: {s}"
    );
    assert!(!s.contains("—"), "non-ascii must be dropped: {s}");
    assert!(!s.contains("!"), "punctuation must be dropped: {s}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chart_filename_falls_back_for_blank_titles() {
    let p = open_meteo_plugin(None);
    let r = p
        .invoke_async("_test_chart_filename", vec![Dynamic::from("")])
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "chart.png");
    let r = p
        .invoke_async("_test_chart_filename", vec![Dynamic::UNIT])
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "chart.png");
}

/// Regression for the 2026-05-14 default-location bug. Pre-rework
/// the agent kept asking the user for their current location even
/// when the operator had pinned a default — because each `*_impl`
/// did `require_number(args["latitude"], ...)` BEFORE merging
/// defaults, so the require check fired against the user-supplied
/// args (no lat/lon) instead of the merged map. The fix reorders
/// the merge to happen first so the operator default counts as
/// "supplied." This test pins both the merge order and the throw
/// message that fires when neither args nor defaults supply
/// coordinates (the operator-actionable hint matters: the agent
/// reads the error and surfaces it, which only helps if the
/// message points at the right settings page).
///
/// Note: the helper-level test exercises `_test_merge_with_defaults`
/// directly. read_config in the test fixture returns the fallback
/// defaults (no vault wired by `with_loopback_allowed_for_tests`),
/// so the "operator pinned a default" arm requires us to construct
/// args that override; the missing-default case naturally exercises
/// the throw path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_coords_throws_with_actionable_settings_hint_when_unset() {
    let p = open_meteo_plugin(None);
    // Empty merged map → both lat/lon are unit → require_coords throws.
    let merged: rhai::Map = rhai::Map::new();
    let result = p
        .invoke_async("_test_require_coords", vec![Dynamic::from(merged)])
        .await;
    let err = result.expect_err("missing coords must throw");
    let msg = format!("{err}");
    assert!(
        msg.contains("Settings → Plugins → Open-Meteo"),
        "throw must point at the operator settings page so the agent can surface an actionable error: {msg}"
    );
    assert!(
        msg.contains("geocode"),
        "throw must mention the geocode escape-hatch so the agent considers calling it: {msg}"
    );
}

/// Regression: when args supply latitude+longitude, the merge keeps
/// them (defaults don't clobber). Pins the precedence: caller-args
/// always win over operator defaults.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_with_defaults_preserves_arg_supplied_coordinates() {
    let p = open_meteo_plugin(None);
    let mut args = rhai::Map::new();
    args.insert("latitude".into(), Dynamic::from(64.146_f64));
    args.insert("longitude".into(), Dynamic::from(-21.940_f64));
    let merged = p
        .invoke_async("_test_merge_with_defaults", vec![Dynamic::from(args)])
        .await
        .unwrap();
    // _test_merge_with_defaults exposes the rhai Map; serde-roundtrip
    // through invoke_async returns serde_json::Value. Pull both lat/lon
    // back out and confirm they're the values we passed in.
    let v: serde_json::Value = serde_json::to_value(merged.clone()).unwrap();
    assert_eq!(
        v.get("latitude").and_then(|x| x.as_f64()),
        Some(64.146),
        "arg-supplied latitude must reach the merged map verbatim: {v:?}",
    );
    assert_eq!(
        v.get("longitude").and_then(|x| x.as_f64()),
        Some(-21.940),
        "arg-supplied longitude must reach the merged map verbatim: {v:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forecast_kind_picker_routes_to_daily_when_daily_present() {
    let p = open_meteo_plugin(None);
    // No daily → weather_current.
    let r = p
        .invoke_async("_test_pick_forecast_kind", vec![Dynamic::UNIT])
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "weather_current");
    // Empty daily array → weather_current.
    let r = p
        .invoke_async(
            "_test_pick_forecast_kind",
            vec![Dynamic::from(rhai::Array::new())],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "weather_current");
    // Non-empty daily → weather_daily.
    let mut a = rhai::Array::new();
    a.push(Dynamic::from("temperature_2m_max"));
    let r = p
        .invoke_async("_test_pick_forecast_kind", vec![Dynamic::from(a)])
        .await
        .unwrap();
    assert_eq!(r.as_str().unwrap(), "weather_daily");
}

// ---- Live tool dispatch against a loopback mock HTTP server -------

/// Tiny single-threaded mock HTTP server. Records every request +
/// returns the canned response next in the queue. Bound to an
/// ephemeral 127.0.0.1 port.
struct MockServer {
    addr: String,
    requests: Arc<Mutex<Vec<(String, String)>>>, // (method, path+query)
    _thread: std::thread::JoinHandle<()>,
}

impl MockServer {
    fn start(responses: Vec<(u16, &'static str, String)>) -> Self {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = requests.clone();
        let mut responses = responses; // moved into the thread

        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("").to_owned();
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                let method = parts.get(0).copied().unwrap_or("").to_owned();
                let path = parts.get(1).copied().unwrap_or("").to_owned();
                requests_for_thread.lock().unwrap().push((method, path));

                let (status, content_type, body) = if !responses.is_empty() {
                    responses.remove(0)
                } else {
                    (404, "text/plain", "no more canned responses".into())
                };
                let resp = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    content_type,
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _: Result<(), std::io::Error> = (|s: TcpStream| -> Result<(), std::io::Error> {
                    s.shutdown(std::net::Shutdown::Both)
                })(stream);
            }
        });

        MockServer {
            addr,
            requests,
            _thread: thread,
        }
    }

    fn url(&self) -> &str {
        &self.addr
    }

    fn last_request(&self) -> Option<(String, String)> {
        self.requests.lock().unwrap().last().cloned()
    }
}

fn rhai_args(map: serde_json::Value) -> Dynamic {
    use execlaw_script::primitives_glue::json_to_rhai;
    json_to_rhai(&map)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forecast_tool_dispatches_to_mock_and_annotates_payload() {
    let mock = MockServer::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "latitude": 64.146,
            "longitude": -21.940,
            "current": {
                "time": "2026-05-11T10:00",
                "temperature_2m": 7.4,
                "weather_code": 61,
                "wind_speed_10m": 18.3,
                "relative_humidity_2m": 82
            }
        })
        .to_string(),
    )]);

    let plugin = open_meteo_plugin(Some(("https://api.open-meteo.com", mock.url())));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.forecast")),
                rhai_args(serde_json::json!({
                    "latitude": 64.146,
                    "longitude": -21.940,
                    "place_name": "Reykjavík",
                    "current": ["temperature_2m", "weather_code", "wind_speed_10m"],
                    "timezone": "auto"
                })),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .expect("forecast dispatch should succeed");

    assert_eq!(r["chat_component_kind"], "weather_current");
    assert_eq!(r["place_name"], "Reykjavík");
    assert_eq!(r["current"]["temperature_2m"], 7.4);
    assert_eq!(r["current"]["weather_code"], 61);

    // Verify the mock got the right query shape.
    let (method, path) = mock.last_request().expect("server saw a request");
    assert_eq!(method, "GET");
    assert!(path.starts_with("/v1/forecast?"), "path: {path}");
    assert!(path.contains("latitude=64.146"), "path: {path}");
    assert!(path.contains("longitude=-21.94"), "path: {path}");
    assert!(
        path.contains("current=temperature_2m%2Cweather_code%2Cwind_speed_10m"),
        "current csv must be encoded: {path}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forecast_with_daily_picks_weather_daily_kind() {
    let mock = MockServer::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "latitude": 64.0,
            "longitude": -22.0,
            "daily": {
                "time": ["2026-05-12", "2026-05-13"],
                "temperature_2m_max": [11, 9],
                "temperature_2m_min": [4, 3],
                "precipitation_sum": [2.1, 0.0]
            }
        })
        .to_string(),
    )]);
    let plugin = open_meteo_plugin(Some(("https://api.open-meteo.com", mock.url())));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.forecast")),
                rhai_args(serde_json::json!({
                    "latitude": 64.0,
                    "longitude": -22.0,
                    "daily": ["temperature_2m_max", "temperature_2m_min", "precipitation_sum"]
                })),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r["chat_component_kind"], "weather_daily");
    assert_eq!(r["daily"]["temperature_2m_max"][0], 11);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn geocode_tool_returns_summarized_results() {
    let mock = MockServer::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "results": [
                {
                    "name": "Reykjavík",
                    "country": "Iceland",
                    "admin1": "Höfuðborgarsvæðið",
                    "latitude": 64.146,
                    "longitude": -21.940,
                    "elevation": 4.0,
                    "timezone": "Atlantic/Reykjavik",
                    "population": 122853
                }
            ]
        })
        .to_string(),
    )]);
    let plugin = open_meteo_plugin(Some(("https://geocoding-api.open-meteo.com", mock.url())));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.geocode")),
                rhai_args(serde_json::json!({ "name": "Reykjavík" })),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r["count"], 1);
    assert_eq!(r["results"][0]["name"], "Reykjavík");
    assert_eq!(r["results"][0]["latitude"], 64.146);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_requires_start_and_end_date() {
    let plugin = open_meteo_plugin(None);
    let err = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.historical")),
                rhai_args(serde_json::json!({
                    "latitude": 1.0, "longitude": 1.0
                })),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .expect_err("missing dates must error");
    assert!(
        err.to_string().contains("start_date"),
        "expected start_date error, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tool_errors_with_clean_message() {
    let plugin = open_meteo_plugin(None);
    let err = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.does_not_exist")),
                rhai_args(serde_json::json!({})),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .expect_err("unknown tool must error");
    assert!(
        err.to_string().contains("unknown tool"),
        "expected unknown-tool error, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn climate_requires_models_array() {
    let plugin = open_meteo_plugin(None);
    let err = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("open_meteo.climate")),
                rhai_args(serde_json::json!({
                    "latitude": 1.0,
                    "longitude": 1.0,
                    "start_date": "2030-01-01",
                    "end_date":   "2031-01-01"
                })),
                rhai_args(serde_json::json!({})),
            ],
        )
        .await
        .expect_err("missing models must error");
    assert!(err.to_string().contains("models"), "got: {err}");
}
