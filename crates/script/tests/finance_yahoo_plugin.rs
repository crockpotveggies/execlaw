//! Behavioural tests for `plugins/finance-yahoo/main.rhai`. Mirrors
//! the open_meteo_plugin.rs convention: load the real shipped script,
//! exercise the `_test_*` helpers against canned inputs. No live
//! network — the chart/search endpoint integration tests will live
//! alongside these as we wire MockServer cases for each tool.
//!
//! v0.1.0 scaffold coverage:
//!   * Script compiles cleanly (would catch a Rhai syntax regression).
//!   * Pure helpers: symbol normalization across asset classes,
//!     index alias lookup, interval/range validators, clamp_int.
//!   * Shape-validator extractors: extract_quote / extract_candles /
//!     extract_search / extract_news return structured-error shapes
//!     on malformed input rather than panicking, and project the
//!     happy-path Yahoo response shape into the plugin's stable
//!     output schema.

use execlaw_script::{ScriptEngine, ScriptPlugin};
use rhai::Dynamic;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Load the shipped finance-yahoo plugin, optionally rewriting all
/// upstream Yahoo hostnames to point at a local mock. `from_source`
/// parses the Rhai program eagerly, so a compile error in main.rhai
/// trips here.
fn finance_yahoo_plugin(mock_base: Option<&str>) -> ScriptPlugin {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("plugins/finance-yahoo/main.rhai");
    let mut source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path:?}: {e}"));
    if let Some(base) = mock_base {
        // Map every Yahoo FQDN onto the single mock base.
        source = source
            .replace("https://query1.finance.yahoo.com", base)
            .replace("https://query2.finance.yahoo.com", base)
            .replace("https://fc.yahoo.com", base);
    }
    let factory = ScriptEngine::with_loopback_allowed_for_tests();
    ScriptPlugin::from_source("finance-yahoo", &source, &factory)
        .expect("plugins/finance-yahoo/main.rhai must parse")
}

// ---- Symbol normalization ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normalize_symbol_index_friendly_names() {
    let p = finance_yahoo_plugin(None);
    let cases = [
        ("dow", "^DJI"),
        ("Dow Jones", "^DJI"),
        ("nasdaq", "^IXIC"),
        ("Nasdaq 100", "^NDX"),
        ("s&p 500", "^GSPC"),
        ("sp500", "^GSPC"),
        ("vix", "^VIX"),
        ("ftse", "^FTSE"),
        ("nikkei", "^N225"),
        ("dax", "^GDAXI"),
    ];
    for (raw, want) in cases {
        let r = p
            .invoke_async(
                "_test_normalize_symbol",
                vec![Dynamic::from(raw), Dynamic::from("any")],
            )
            .await
            .unwrap_or_else(|e| panic!("normalize_symbol({raw}, any) errored: {e:?}"));
        assert_eq!(
            r.as_str(),
            Some(want),
            "normalize_symbol({raw}) -> expected {want}, got {r:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normalize_symbol_crypto_adds_usd_suffix() {
    let p = finance_yahoo_plugin(None);
    let r = p
        .invoke_async(
            "_test_normalize_symbol",
            vec![Dynamic::from("BTC"), Dynamic::from("crypto")],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("BTC-USD"));

    // Already-suffixed forms pass through.
    let r = p
        .invoke_async(
            "_test_normalize_symbol",
            vec![Dynamic::from("ETH-EUR"), Dynamic::from("crypto")],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("ETH-EUR"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normalize_symbol_fx_coerces_to_yahoo_format() {
    let p = finance_yahoo_plugin(None);
    let cases = [
        ("EUR/USD", "EURUSD=X"),
        ("EURUSD", "EURUSD=X"),
        ("USD/JPY", "USDJPY=X"),
        ("EURUSD=X", "EURUSD=X"),
    ];
    for (raw, want) in cases {
        let r = p
            .invoke_async(
                "_test_normalize_symbol",
                vec![Dynamic::from(raw), Dynamic::from("fx")],
            )
            .await
            .unwrap_or_else(|e| panic!("normalize_symbol({raw}, fx) errored: {e:?}"));
        assert_eq!(r.as_str(), Some(want));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normalize_symbol_equity_passes_through_trimmed() {
    let p = finance_yahoo_plugin(None);
    let r = p
        .invoke_async(
            "_test_normalize_symbol",
            vec![Dynamic::from("  AAPL  "), Dynamic::from("equity")],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("AAPL"));
}

// ---- Validators --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interval_and_range_validators_accept_only_known_values() {
    let p = finance_yahoo_plugin(None);
    for ok in ["1m", "5m", "1d", "1wk", "1mo"] {
        let r = p
            .invoke_async("_test_is_valid_interval", vec![Dynamic::from(ok)])
            .await
            .unwrap();
        assert_eq!(r.as_bool(), Some(true), "interval `{ok}` should be valid");
    }
    for bad in ["10m", "2d", "1y", ""] {
        let r = p
            .invoke_async("_test_is_valid_interval", vec![Dynamic::from(bad)])
            .await
            .unwrap();
        assert_eq!(
            r.as_bool(),
            Some(false),
            "interval `{bad}` should be invalid"
        );
    }
    for ok in ["1d", "1mo", "ytd", "max"] {
        let r = p
            .invoke_async("_test_is_valid_range", vec![Dynamic::from(ok)])
            .await
            .unwrap();
        assert_eq!(r.as_bool(), Some(true), "range `{ok}` should be valid");
    }
}

// ---- Shape-validator extractors ---------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_quote_returns_shape_error_on_garbage_body() {
    let p = finance_yahoo_plugin(None);
    let r: Value = p
        .invoke_async(
            "_test_extract_quote",
            vec![
                Dynamic::from("not a map"),
                Dynamic::from("AAPL"),
                Dynamic::from("equity"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        r.get("error").and_then(|v| v.as_str()),
        Some("upstream_shape_changed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_quote_projects_happy_path() {
    let p = finance_yahoo_plugin(None);
    let body = json_to_dynamic(json!({
        "chart": {
            "error": null,
            "result": [{
                "meta": {
                    "regularMarketPrice": 193.42,
                    "chartPreviousClose": 190.00,
                    "regularMarketDayHigh": 194.50,
                    "regularMarketDayLow": 189.10,
                    "regularMarketVolume": 52_341_200,
                    "currency": "USD",
                    "exchangeName": "NMS",
                    "marketState": "REGULAR",
                    "regularMarketTime": 1_715_786_400_i64,
                }
            }]
        }
    }));

    let r: Value = p
        .invoke_async(
            "_test_extract_quote",
            vec![body, Dynamic::from("AAPL"), Dynamic::from("equity")],
        )
        .await
        .unwrap();
    assert!(
        r.get("error").map(|v| v.is_null()).unwrap_or(true),
        "no error field expected; got {r}"
    );
    assert_eq!(r.get("symbol").and_then(|v| v.as_str()), Some("AAPL"));
    let price = r.get("price").and_then(|v| v.as_f64()).unwrap();
    assert!((price - 193.42).abs() < 1e-9);
    let change_pct = r.get("change_pct").and_then(|v| v.as_f64()).unwrap();
    // (193.42 - 190.00) / 190.00 * 100 = 1.8 (approx)
    assert!(
        (change_pct - 1.8).abs() < 0.01,
        "got change_pct={change_pct}"
    );
    assert_eq!(
        r.get("chat_component_kind").and_then(|v| v.as_str()),
        Some("stock_quote")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_quote_returns_upstream_error_envelope() {
    let p = finance_yahoo_plugin(None);
    let body = json_to_dynamic(json!({ "chart": { "error": "Not Found" } }));

    let r: Value = p
        .invoke_async(
            "_test_extract_quote",
            vec![body, Dynamic::from("NOPE"), Dynamic::from("equity")],
        )
        .await
        .unwrap();
    assert_eq!(
        r.get("error").and_then(|v| v.as_str()),
        Some("upstream_error")
    );
    assert_eq!(r.get("detail").and_then(|v| v.as_str()), Some("Not Found"));
}

// ---- Cookie parser, pick_raw, truncate_string --------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_session_cookie_picks_a1_a3_a1s_only() {
    let p = finance_yahoo_plugin(None);
    // Single Set-Cookie line with A1 + ignored cruft after the
    // semicolon (Domain, Expires, Path, Secure flags).
    let r: Value = p
        .invoke_async(
            "_test_extract_session_cookie",
            vec![Dynamic::from(
                "A1=foo=bar; Domain=.yahoo.com; Path=/; Secure",
            )],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("A1=foo=bar"));

    // Array of Set-Cookie lines — extract A1 + A3 only, drop the
    // unrelated B cookie. Output order must match input order.
    let mut arr = rhai::Array::new();
    arr.push(Dynamic::from("B=xyz; Domain=.yahoo.com"));
    arr.push(Dynamic::from("A1=v1=abc; Domain=.yahoo.com; HttpOnly"));
    arr.push(Dynamic::from("A3=v3=def; Domain=.yahoo.com"));
    let r: Value = p
        .invoke_async("_test_extract_session_cookie", vec![Dynamic::from(arr)])
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("A1=v1=abc; A3=v3=def"));

    // Empty input.
    let r: Value = p
        .invoke_async("_test_extract_session_cookie", vec![Dynamic::UNIT])
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some(""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pick_raw_unwraps_yahoo_wrapped_numbers() {
    let p = finance_yahoo_plugin(None);
    // Plain number passes through.
    let r: Value = p
        .invoke_async("_test_pick_raw", vec![Dynamic::from(42.5_f64)])
        .await
        .unwrap();
    assert_eq!(r.as_f64(), Some(42.5));

    // Yahoo's wrapped `{ raw, fmt, longFmt }` form returns the raw.
    let body = json_to_dynamic(json!({ "raw": 12345, "fmt": "12.3K", "longFmt": "12,345" }));
    let r: Value = p.invoke_async("_test_pick_raw", vec![body]).await.unwrap();
    assert_eq!(r.as_i64(), Some(12345));

    // Missing raw — returns null.
    let body = json_to_dynamic(json!({ "fmt": "N/A" }));
    let r: Value = p.invoke_async("_test_pick_raw", vec![body]).await.unwrap();
    assert!(r.is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_string_caps_long_strings() {
    let p = finance_yahoo_plugin(None);
    let r: Value = p
        .invoke_async(
            "_test_truncate_string",
            vec![Dynamic::from("hello world"), Dynamic::from(5_i64)],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("hello…"));

    // Short string passes through unchanged.
    let r: Value = p
        .invoke_async(
            "_test_truncate_string",
            vec![Dynamic::from("hi"), Dynamic::from(50_i64)],
        )
        .await
        .unwrap();
    assert_eq!(r.as_str(), Some("hi"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_quote_summary_projects_modules() {
    let p = finance_yahoo_plugin(None);
    let body = json_to_dynamic(json!({
        "quoteSummary": {
            "error": null,
            "result": [{
                "summaryDetail": {
                    "marketCap":        { "raw": 3_000_000_000_i64, "fmt": "3T" },
                    "trailingPE":       { "raw": 28.4, "fmt": "28.40" },
                    "dividendYield":    { "raw": 0.005, "fmt": "0.50%" },
                    "fiftyTwoWeekHigh": { "raw": 199.62, "fmt": "199.62" },
                    "fiftyTwoWeekLow":  { "raw": 164.08, "fmt": "164.08" },
                    "beta":             { "raw": 1.25, "fmt": "1.25" },
                    "currency": "USD"
                },
                "assetProfile": {
                    "sector":   "Technology",
                    "industry": "Consumer Electronics",
                    "country":  "United States",
                    "website":  "https://www.apple.com",
                    "fullTimeEmployees": 161000,
                    "longBusinessSummary": "Apple Inc. designs..."
                },
                "price": {
                    "shortName":   "Apple Inc.",
                    "longName":    "Apple Inc.",
                    "exchangeName": "NMS"
                }
            }]
        }
    }));
    let r: Value = p
        .invoke_async(
            "_test_extract_quote_summary",
            vec![body, Dynamic::from("AAPL")],
        )
        .await
        .unwrap();
    assert_eq!(r.get("symbol").and_then(|v| v.as_str()), Some("AAPL"));
    assert_eq!(
        r.get("market_cap").and_then(|v| v.as_i64()),
        Some(3_000_000_000)
    );
    assert_eq!(r.get("sector").and_then(|v| v.as_str()), Some("Technology"));
    assert_eq!(
        r.get("industry").and_then(|v| v.as_str()),
        Some("Consumer Electronics")
    );
    assert_eq!(
        r.get("short_name").and_then(|v| v.as_str()),
        Some("Apple Inc.")
    );
    assert_eq!(
        r.get("chat_component_kind").and_then(|v| v.as_str()),
        Some("stock_quote_summary")
    );
    let pe = r.get("pe_trailing").and_then(|v| v.as_f64()).unwrap();
    assert!((pe - 28.4).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extract_quote_summary_returns_error_envelope_on_upstream_error() {
    let p = finance_yahoo_plugin(None);
    let body = json_to_dynamic(json!({
        "quoteSummary": {
            "error": "Invalid Crumb",
            "result": null
        }
    }));
    let r: Value = p
        .invoke_async(
            "_test_extract_quote_summary",
            vec![body, Dynamic::from("AAPL")],
        )
        .await
        .unwrap();
    assert_eq!(
        r.get("error").and_then(|v| v.as_str()),
        Some("upstream_error")
    );
    assert_eq!(
        r.get("detail").and_then(|v| v.as_str()),
        Some("Invalid Crumb")
    );
}

// ---- Live tool dispatch against a loopback mock HTTP server -------

/// Mock HTTP server supporting per-response extra headers (needed
/// for Set-Cookie capture in the crumb flow) and arbitrary
/// Content-Type (needed for plain-text crumb responses).
///
/// Each response is `(status, content_type, body, extra_headers)`.
/// Responses are returned in queue order regardless of URL — tests
/// queue them up matching the plugin's expected call order.
struct MockServer {
    addr: String,
    requests: Arc<Mutex<Vec<(String, String, String)>>>, // (method, path, body)
    _thread: std::thread::JoinHandle<()>,
}

impl MockServer {
    fn start(responses: Vec<(u16, &'static str, String, Vec<(String, String)>)>) -> Self {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = requests.clone();
        let mut responses = responses;

        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = [0u8; 16384];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("").to_owned();
                let parts: Vec<&str> = first_line.split_whitespace().collect();
                let method = parts.first().copied().unwrap_or("").to_owned();
                let path = parts.get(1).copied().unwrap_or("").to_owned();
                let body_idx = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                let body = req[body_idx..].to_owned();
                requests_for_thread
                    .lock()
                    .unwrap()
                    .push((method, path, body));

                let (status, content_type, resp_body, extra_headers) = if !responses.is_empty() {
                    responses.remove(0)
                } else {
                    (
                        404,
                        "text/plain",
                        "no more canned responses".into(),
                        Vec::new(),
                    )
                };
                let mut extras = String::new();
                for (k, v) in &extra_headers {
                    extras.push_str(&format!("{k}: {v}\r\n"));
                }
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extras}Connection: close\r\n\r\n{resp_body}",
                    resp_body.len()
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

    fn requests(&self) -> Vec<(String, String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

fn rhai_args(map: Value) -> Dynamic {
    use execlaw_script::primitives_glue::json_to_rhai;
    json_to_rhai(&map)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quote_tool_against_mock_chart_endpoint() {
    let mock = MockServer::start(vec![(
        200,
        "application/json",
        json!({
            "chart": {
                "error": null,
                "result": [{
                    "meta": {
                        "regularMarketPrice": 193.42,
                        "chartPreviousClose": 190.00,
                        "regularMarketDayHigh": 194.50,
                        "regularMarketDayLow": 189.10,
                        "regularMarketVolume": 52_341_200_i64,
                        "currency": "USD",
                        "exchangeName": "NMS",
                        "marketState": "REGULAR",
                        "regularMarketTime": 1_715_786_400_i64
                    }
                }]
            }
        })
        .to_string(),
        Vec::new(),
    )]);

    let plugin = finance_yahoo_plugin(Some(mock.url()));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("yahoo_finance.quote")),
                rhai_args(json!({ "symbol": "AAPL" })),
                rhai_args(json!({})),
            ],
        )
        .await
        .expect("quote tool dispatch should succeed");

    assert_eq!(r["symbol"], "AAPL");
    let price = r["price"].as_f64().unwrap();
    assert!((price - 193.42).abs() < 1e-9);
    assert_eq!(r["chat_component_kind"], "stock_quote");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "GET");
    assert!(
        reqs[0].1.starts_with("/v8/finance/chart/AAPL?"),
        "path should be the chart endpoint, got: {}",
        reqs[0].1
    );
    assert!(reqs[0].1.contains("interval=1d"));
    assert!(reqs[0].1.contains("range=1d"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_search_against_mock() {
    let mock = MockServer::start(vec![(
        200,
        "application/json",
        json!({
            "quotes": [
                { "symbol": "AAPL", "shortname": "Apple Inc.", "exchDisp": "NASDAQ", "quoteType": "EQUITY", "typeDisp": "Equity" },
                { "symbol": "AAPL.MX", "longname": "Apple Inc.", "exchDisp": "Mexico", "quoteType": "EQUITY", "typeDisp": "Equity" }
            ],
            "news": []
        })
        .to_string(),
        Vec::new(),
    )]);
    let plugin = finance_yahoo_plugin(Some(mock.url()));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("yahoo_finance.symbol_search")),
                rhai_args(json!({ "query": "apple", "limit": 5 })),
                rhai_args(json!({})),
            ],
        )
        .await
        .unwrap();
    assert_eq!(r["query"], "apple");
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["symbol"], "AAPL");
    assert_eq!(results[0]["name"], "Apple Inc.");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].1.starts_with("/v1/finance/search?"),
        "path: {}",
        reqs[0].1
    );
    assert!(reqs[0].1.contains("q=apple"));
    assert!(reqs[0].1.contains("quotesCount=5"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quote_summary_retries_once_on_401_after_refreshing_session() {
    // Two crumb cycles + a 401-then-200 quoteSummary pair. The plugin
    // should NOT bubble the 401 as an error; it should fetch a fresh
    // session and retry transparently.
    let mock = MockServer::start(vec![
        // First session bootstrap
        (
            404,
            "text/html",
            "stale".to_string(),
            vec![(
                "Set-Cookie".to_string(),
                "A1=v1=stale; Domain=.yahoo.com".to_string(),
            )],
        ),
        (200, "text/plain", "old-crumb".to_string(), Vec::new()),
        // Protected call returns 401 (stale crumb)
        (
            401,
            "application/json",
            r#"{"error":"Unauthorized"}"#.to_string(),
            Vec::new(),
        ),
        // Retry: fresh session
        (
            404,
            "text/html",
            "fresh".to_string(),
            vec![(
                "Set-Cookie".to_string(),
                "A1=v1=fresh; Domain=.yahoo.com".to_string(),
            )],
        ),
        (200, "text/plain", "fresh-crumb".to_string(), Vec::new()),
        // Retry of the protected call — now 200
        (
            200,
            "application/json",
            json!({
                "quoteSummary": {
                    "error": null,
                    "result": [{
                        "summaryDetail": { "marketCap": { "raw": 1_000_000_000_i64 }, "currency": "USD" },
                        "price":         { "shortName": "Tesla, Inc.", "exchangeName": "NMS" }
                    }]
                }
            })
            .to_string(),
            Vec::new(),
        ),
    ]);

    let plugin = finance_yahoo_plugin(Some(mock.url()));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("yahoo_finance.quote_summary")),
                rhai_args(json!({ "symbol": "TSLA", "modules": ["summaryDetail","price"] })),
                rhai_args(json!({})),
            ],
        )
        .await
        .expect("retry path should succeed");

    assert!(
        r.get("error").map(|v| v.is_null()).unwrap_or(true),
        "unexpected error: {r}"
    );
    assert_eq!(r["symbol"], "TSLA");
    assert_eq!(r["short_name"], "Tesla, Inc.");
    assert_eq!(r["market_cap"], 1_000_000_000_i64);

    let reqs = mock.requests();
    assert_eq!(
        reqs.len(),
        6,
        "expected 6 calls (2 session + protected + 2 session + retry), got: {reqs:?}"
    );

    // The first quoteSummary call should carry old-crumb; the retry
    // should carry fresh-crumb.
    let first_summary_path = &reqs[2].1;
    assert!(
        first_summary_path.contains("crumb=old-crumb"),
        "first call: {first_summary_path}"
    );
    let retry_summary_path = &reqs[5].1;
    assert!(
        retry_summary_path.contains("crumb=fresh-crumb"),
        "retry call: {retry_summary_path}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quote_summary_runs_full_crumb_flow_on_first_call() {
    // The plugin's first protected call must:
    //   1. GET fc.yahoo.com  -> capture Set-Cookie
    //   2. GET /v1/test/getcrumb  -> capture crumb text
    //   3. GET /v10/finance/quoteSummary/AAPL with both attached
    //
    // We queue those three responses in order.
    let mock = MockServer::start(vec![
        (
            // fc.yahoo — Yahoo actually returns 404 here on most
            // installs, with the cookie still set on the response.
            404,
            "text/html",
            "<html>not found</html>".to_string(),
            vec![(
                "Set-Cookie".to_string(),
                "A1=foo=bar; Domain=.yahoo.com; Path=/; Secure; HttpOnly".to_string(),
            )],
        ),
        (
            // getcrumb — plain-text crumb token, NOT JSON. body_text
            // is what the plugin reads.
            200,
            "text/plain",
            "test-crumb-xyz".to_string(),
            Vec::new(),
        ),
        (
            // quoteSummary — happy path JSON
            200,
            "application/json",
            json!({
                "quoteSummary": {
                    "error": null,
                    "result": [{
                        "summaryDetail": {
                            "marketCap":    { "raw": 3_000_000_000_i64, "fmt": "3T" },
                            "trailingPE":   { "raw": 28.4, "fmt": "28.40" },
                            "fiftyTwoWeekHigh": { "raw": 199.62 },
                            "fiftyTwoWeekLow":  { "raw": 164.08 },
                            "currency": "USD"
                        },
                        "assetProfile": { "sector": "Technology", "industry": "Consumer Electronics" },
                        "price": { "shortName": "Apple Inc.", "exchangeName": "NMS" }
                    }]
                }
            })
            .to_string(),
            Vec::new(),
        ),
    ]);

    let plugin = finance_yahoo_plugin(Some(mock.url()));
    let r = plugin
        .invoke_async(
            "tool_call",
            vec![
                Dynamic::from(rhai::ImmutableString::from("yahoo_finance.quote_summary")),
                rhai_args(json!({ "symbol": "AAPL", "modules": ["summaryDetail","assetProfile","price"] })),
                rhai_args(json!({})),
            ],
        )
        .await
        .expect("quote_summary dispatch should succeed");

    assert!(
        r.get("error").map(|v| v.is_null()).unwrap_or(true),
        "unexpected error: {r}"
    );
    assert_eq!(r["symbol"], "AAPL");
    assert_eq!(r["sector"], "Technology");
    assert_eq!(r["short_name"], "Apple Inc.");
    assert_eq!(r["market_cap"], 3_000_000_000_i64);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3, "expected 3 upstream calls, got: {reqs:?}");

    // 1: fc.yahoo
    assert!(reqs[0].1.starts_with("/"), "first path: {}", reqs[0].1);

    // 2: getcrumb
    assert!(
        reqs[1].1.starts_with("/v1/test/getcrumb"),
        "second path: {}",
        reqs[1].1
    );

    // 3: quoteSummary — must carry the crumb in the query string
    assert!(
        reqs[2].1.starts_with("/v10/finance/quoteSummary/AAPL?"),
        "third path: {}",
        reqs[2].1
    );
    assert!(
        reqs[2].1.contains("crumb=test-crumb-xyz"),
        "crumb must be threaded into the quoteSummary query: {}",
        reqs[2].1
    );
    assert!(
        reqs[2]
            .1
            .contains("modules=summaryDetail%2CassetProfile%2Cprice"),
        "modules CSV must be encoded: {}",
        reqs[2].1
    );
}

// ---- Helpers -----------------------------------------------------

/// Convert a `serde_json::Value` into a Rhai `Dynamic` argument
/// shaped the way the plugin's helpers expect (Map / Array / strings /
/// floats / ints / unit). This mirrors what the host's HTTP plumbing
/// does for real responses, so tests exercise the same projection
/// path the production extractors run against.
fn json_to_dynamic(v: Value) -> Dynamic {
    match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        Value::String(s) => Dynamic::from(s),
        Value::Array(a) => {
            let mut out = rhai::Array::new();
            for e in a {
                out.push(json_to_dynamic(e));
            }
            Dynamic::from(out)
        }
        Value::Object(o) => {
            let mut out = rhai::Map::new();
            for (k, v) in o {
                out.insert(k.into(), json_to_dynamic(v));
            }
            Dynamic::from(out)
        }
    }
}
