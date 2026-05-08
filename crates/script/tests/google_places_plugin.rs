//! Behavioral tests for `plugins/google-places/main.rhai`.
//!
//! Loads the real shipped script + exercises pure helper functions
//! (`summary_of` via `_test_summary`, `clamp_max_results`,
//! `filter_by_min_rating`, `mask`). The HTTP-driven tool calls
//! aren't exercised here — they need a real Google Places API key
//! and the host's http_post binding wired to ureq, which is
//! covered by manual integration testing once the plugin is
//! installed.
//!
//! These tests catch regressions in:
//!   * Place → summary mapping (camelCase → snake_case, missing
//!     fields → Unit, displayName.text unwrap)
//!   * Max-results clamping (1..=20, default-fallback path,
//!     int-vs-string coercion)
//!   * Min-rating filtering (numeric coercion, range validation,
//!     order preservation)
//!   * Masking helper (full-mask short keys, suffix-preserve long)

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use std::path::PathBuf;

fn google_places_plugin() -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/google-places/main.rhai");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    let factory = ScriptEngine::new();
    ScriptPlugin::from_source("google-places", &source, &factory)
        .expect("plugins/google-places/main.rhai must parse")
}

async fn invoke_one_str(
    plugin: &ScriptPlugin,
    fn_name: &'static str,
    arg: &str,
) -> serde_json::Value {
    plugin
        .invoke_async(
            fn_name,
            vec![Dynamic::from(rhai::ImmutableString::from(arg))],
        )
        .await
        .unwrap_or_else(|e| panic!("{fn_name}('{arg}') failed: {e}"))
}

async fn invoke_clamp(
    plugin: &ScriptPlugin,
    arg: i64,
    fallback: i64,
) -> serde_json::Value {
    plugin
        .invoke_async(
            "_test_clamp_max_results",
            vec![Dynamic::from(arg), Dynamic::from(fallback)],
        )
        .await
        .unwrap_or_else(|e| panic!("_test_clamp_max_results({arg}, {fallback}) failed: {e}"))
}

async fn invoke_clamp_unit(plugin: &ScriptPlugin, fallback: i64) -> serde_json::Value {
    plugin
        .invoke_async(
            "_test_clamp_max_results",
            vec![Dynamic::UNIT, Dynamic::from(fallback)],
        )
        .await
        .unwrap_or_else(|e| panic!("_test_clamp_max_results((), {fallback}) failed: {e}"))
}

async fn invoke_filter(
    plugin: &ScriptPlugin,
    places_json: &str,
    min_rating_or_empty: &str,
    max_results: i64,
) -> serde_json::Value {
    plugin
        .invoke_async(
            "_test_filter_by_min_rating",
            vec![
                Dynamic::from(rhai::ImmutableString::from(places_json)),
                Dynamic::from(rhai::ImmutableString::from(min_rating_or_empty)),
                Dynamic::from(max_results),
            ],
        )
        .await
        .unwrap_or_else(|e| panic!("_test_filter_by_min_rating failed: {e}"))
}

// ---- summary_of ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_unwraps_display_name_text() {
    // Google Places returns name as `displayName: { text, languageCode }`.
    // The summary must unwrap to a flat string so the agent doesn't
    // see nested objects in tool output.
    let plugin = google_places_plugin();
    let raw = r#"{
        "id": "ChIJN1t_tDeuEmsRUsoyG83frY4",
        "displayName": {"text": "Pelusa Cafe", "languageCode": "en"},
        "formattedAddress": "123 Main St, Vancouver",
        "rating": 4.6,
        "userRatingCount": 312,
        "priceLevel": "PRICE_LEVEL_MODERATE",
        "types": ["cafe", "food"],
        "googleMapsUri": "https://maps.google.com/?cid=1"
    }"#;
    let r = invoke_one_str(&plugin, "_test_summary", raw).await;
    assert_eq!(r["place_id"], "ChIJN1t_tDeuEmsRUsoyG83frY4");
    assert_eq!(r["name"], "Pelusa Cafe");
    assert_eq!(r["formatted_address"], "123 Main St, Vancouver");
    assert_eq!(r["rating"], 4.6);
    assert_eq!(r["user_rating_count"], 312);
    assert_eq!(r["price_level"], "PRICE_LEVEL_MODERATE");
    assert!(r["types"].is_array());
    assert_eq!(r["google_maps_uri"], "https://maps.google.com/?cid=1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_handles_missing_fields_as_unit() {
    // Essentials-tier response omits hours/website/phone. Summary
    // must surface them as null (Unit), not throw or insert
    // garbage defaults.
    let plugin = google_places_plugin();
    let raw = r#"{
        "id": "X",
        "displayName": {"text": "Bare Place"},
        "formattedAddress": "1 St"
    }"#;
    let r = invoke_one_str(&plugin, "_test_summary", raw).await;
    assert_eq!(r["name"], "Bare Place");
    assert!(r["rating"].is_null(), "missing rating → null, not 0");
    assert!(r["website_uri"].is_null());
    assert!(r["national_phone_number"].is_null());
    assert!(r["price_level"].is_null());
    assert!(r["hours"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_unwraps_weekday_descriptions() {
    let plugin = google_places_plugin();
    let raw = r#"{
        "id": "X",
        "displayName": {"text": "P"},
        "regularOpeningHours": {
            "weekdayDescriptions": ["Monday: 7AM-4PM", "Tuesday: 7AM-4PM"]
        }
    }"#;
    let r = invoke_one_str(&plugin, "_test_summary", raw).await;
    let hours = r["hours"].as_array().expect("hours array");
    assert_eq!(hours.len(), 2);
    assert_eq!(hours[0], "Monday: 7AM-4PM");
}

// ---- clamp_max_results --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamp_max_results_passes_in_range() {
    let plugin = google_places_plugin();
    assert_eq!(invoke_clamp(&plugin, 1, 5).await, 1);
    assert_eq!(invoke_clamp(&plugin, 5, 5).await, 5);
    assert_eq!(invoke_clamp(&plugin, 10, 5).await, 10);
    assert_eq!(invoke_clamp(&plugin, 20, 5).await, 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamp_max_results_clamps_above_limit() {
    let plugin = google_places_plugin();
    // Over-20 caps to 20 (Google API hard limit + our cost-control
    // ceiling).
    assert_eq!(invoke_clamp(&plugin, 21, 5).await, 20);
    assert_eq!(invoke_clamp(&plugin, 1000, 5).await, 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamp_max_results_clamps_below_floor() {
    let plugin = google_places_plugin();
    assert_eq!(invoke_clamp(&plugin, 0, 5).await, 1);
    assert_eq!(invoke_clamp(&plugin, -1, 5).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clamp_max_results_unit_falls_back() {
    // Agent omits max_results → use the configured default.
    let plugin = google_places_plugin();
    assert_eq!(invoke_clamp_unit(&plugin, 5).await, 5);
    assert_eq!(invoke_clamp_unit(&plugin, 7).await, 7);
}

// ---- filter_by_min_rating -----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_drops_below_min_rating() {
    let plugin = google_places_plugin();
    let raw = r#"[
        {"id":"a","rating":4.8},
        {"id":"b","rating":3.2},
        {"id":"c","rating":4.5},
        {"id":"d","rating":2.0}
    ]"#;
    // min_rating=4.0 → keep a + c.
    let r = invoke_filter(&plugin, raw, "4", 10).await;
    let arr = r.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "a");
    assert_eq!(arr[1]["id"], "c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_caps_at_max_results() {
    let plugin = google_places_plugin();
    let raw = r#"[
        {"id":"a","rating":5},
        {"id":"b","rating":5},
        {"id":"c","rating":5},
        {"id":"d","rating":5},
        {"id":"e","rating":5}
    ]"#;
    let r = invoke_filter(&plugin, raw, "", 3).await;
    let arr = r.as_array().expect("array");
    assert_eq!(arr.len(), 3, "max_results should cap the output");
    assert_eq!(arr[0]["id"], "a");
    assert_eq!(arr[1]["id"], "b");
    assert_eq!(arr[2]["id"], "c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_treats_missing_rating_as_zero() {
    let plugin = google_places_plugin();
    let raw = r#"[
        {"id":"a","rating":4.5},
        {"id":"b"},
        {"id":"c","rating":3.5}
    ]"#;
    let r = invoke_filter(&plugin, raw, "1", 10).await;
    let arr = r.as_array().expect("array");
    // a (4.5) + c (3.5) survive >= 1; b (no rating → 0) drops.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "a");
    assert_eq!(arr[1]["id"], "c");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_no_min_rating_keeps_all_up_to_cap() {
    let plugin = google_places_plugin();
    let raw = r#"[
        {"id":"a","rating":1},
        {"id":"b"},
        {"id":"c","rating":2}
    ]"#;
    // Empty min_rating string → no filter; cap=10 keeps all.
    let r = invoke_filter(&plugin, raw, "", 10).await;
    let arr = r.as_array().expect("array");
    assert_eq!(arr.len(), 3);
}

// ---- parse_min_rating + coerce_to_float ---------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_min_rating_throws_above_5() {
    let plugin = google_places_plugin();
    let r = plugin
        .invoke_async(
            "_test_parse_min_rating",
            vec![Dynamic::from(6.0_f64)],
        )
        .await;
    match r {
        Ok(v) => panic!("expected throw for min_rating=6.0; got Ok({v})"),
        Err(e) => assert!(
            e.to_string().contains("between 0 and 5"),
            "unexpected error: {e}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parse_min_rating_throws_below_0() {
    let plugin = google_places_plugin();
    let r = plugin
        .invoke_async(
            "_test_parse_min_rating",
            vec![Dynamic::from(-0.1_f64)],
        )
        .await;
    assert!(
        r.is_err(),
        "expected throw for min_rating=-0.1; got Ok({:?})",
        r.ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coerce_to_float_handles_decimal_string() {
    // Critical for nearby — agents often serialise lat/lng as
    // strings like "49.2827" / "-123.1207". Without float-string
    // parsing we'd reject them.
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "_test_coerce_to_float", "49.2827").await;
    let v = r.as_f64().expect("expected JSON number");
    assert!((v - 49.2827).abs() < 1e-9, "expected 49.2827, got {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coerce_to_float_handles_negative_string() {
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "_test_coerce_to_float", "-123.1207").await;
    let v = r.as_f64().expect("expected JSON number");
    assert!((v + 123.1207).abs() < 1e-9, "expected -123.1207, got {v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coerce_to_float_handles_integer_string() {
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "_test_coerce_to_float", "5").await;
    let v = r.as_f64().expect("expected JSON number");
    assert!((v - 5.0).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coerce_to_float_rejects_garbage_string() {
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "_test_coerce_to_float", "not a number").await;
    assert!(r.is_null(), "expected null for garbage, got {r}");
}

// ---- mask ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_hides_all_but_trailing_4() {
    let plugin = google_places_plugin();
    // Google API keys are typically 39 chars (AIza-prefixed); the
    // masked form preserves the last 4 so operators can confirm
    // the right key without exposing the secret.
    let r = invoke_one_str(&plugin, "mask", "AIzaSyABCDEFGHIJ1234567890abcdef").await;
    assert!(r.as_str().unwrap().ends_with("cdef"));
    assert!(r.as_str().unwrap().starts_with("****"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_short_input_fully_obscured() {
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "mask", "key").await;
    assert_eq!(r, "***");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_empty_passes_through() {
    let plugin = google_places_plugin();
    let r = invoke_one_str(&plugin, "mask", "").await;
    assert_eq!(r, "");
}
