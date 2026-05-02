//! Host-injected primitives the script can call.
//!
//! The list is deliberately tight — every function here is an
//! escape hatch out of the sandbox, so each is a security +
//! maintenance commitment. Adding to this list is a deliberate
//! design decision; subtracting is safe.
//!
//! Categories:
//!   * **HTTP** — `http_get`, `http_post`, `http_get_cached`
//!   * **String** — `digits_only`, `lower`, `trim`, `hash`
//!   * **JSON** — `json_path`
//!   * **Time** — `now`
//!   * **Logging** — `log_info`, `log_warn`
//!
//! Notes on the HTTP primitives:
//!   * Bearer auth is the only credential type passed in. The
//!     access_token comes from the host's OAuth machinery via
//!     `params._oauth.<account_name>` — plugins never see the
//!     refresh_token or client_secret.
//!   * `http_get_cached` is a thin wrapper that consults the
//!     per-plugin [`crate::cache::HttpCache`] before issuing a
//!     network call. Cache key is `sha256(url + query + bearer)`
//!     so a token rotation invalidates entries naturally.

use crate::cache::{HttpCache, cache_key};
use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Map};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

/// Register every primitive against `engine`, capturing
/// `plugin_id` for log lines + the shared HTTP agent + a
/// per-plugin cache.
pub(crate) fn register(
    engine: &mut Engine,
    plugin_id: &str,
    http_agent: ureq::Agent,
    cache: Arc<HttpCache>,
) {
    let pid_for_logs = plugin_id.to_owned();

    // ---- HTTP -----------------------------------------------------

    // http_get(url, query_map, bearer) -> map | array | null
    {
        let agent = http_agent.clone();
        let pid = plugin_id.to_owned();
        engine.register_fn(
            "http_get",
            move |url: ImmutableString, query: Map, bearer: ImmutableString|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                http_get_impl(&agent, &pid, &url, &query, &bearer)
            },
        );
    }

    // http_post(url, body_map_or_value, bearer) -> map | array | null
    {
        let agent = http_agent.clone();
        let pid = plugin_id.to_owned();
        engine.register_fn(
            "http_post",
            move |url: ImmutableString, body: Dynamic, bearer: ImmutableString|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                http_post_impl(&agent, &pid, &url, body, &bearer)
            },
        );
    }

    // http_get_cached(url, query_map, bearer, ttl_secs) -> ...
    {
        let agent = http_agent.clone();
        let pid = plugin_id.to_owned();
        let cache = cache.clone();
        engine.register_fn(
            "http_get_cached",
            move |url: ImmutableString, query: Map, bearer: ImmutableString, ttl_secs: i64|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                http_get_cached_impl(&agent, &cache, &pid, &url, &query, &bearer, ttl_secs)
            },
        );
    }

    // ---- String ---------------------------------------------------

    engine.register_fn("digits_only", |s: ImmutableString| -> ImmutableString {
        s.chars().filter(|c| c.is_ascii_digit()).collect::<String>().into()
    });

    engine.register_fn("lower", |s: ImmutableString| -> ImmutableString {
        s.to_lowercase().into()
    });

    engine.register_fn("trim", |s: ImmutableString| -> ImmutableString {
        s.trim().to_owned().into()
    });

    engine.register_fn("hash", |s: ImmutableString| -> ImmutableString {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        let bytes = h.finalize();
        // First 8 bytes (16 hex chars) — short enough for an id
        // suffix, long enough to avoid collisions in any realistic
        // contact set.
        bytes[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into()
    });

    // ---- JSON path -----------------------------------------------

    engine.register_fn(
        "json_path",
        |value: Dynamic, path: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            let json = rhai_to_json(value)
                .map_err(|e| EvalAltResult::ErrorRuntime(e.into(), rhai::Position::NONE))?;
            let parsed = serde_json_path::JsonPath::parse(&path).map_err(|e| {
                EvalAltResult::ErrorRuntime(
                    format!("json_path: invalid expression '{path}': {e}").into(),
                    rhai::Position::NONE,
                )
            })?;
            let nodes = parsed.query(&json);
            // Always return an array (even single matches) — easier
            // for the script to iterate without branching on shape.
            let arr: Vec<serde_json::Value> = nodes.iter().map(|n| (*n).clone()).collect();
            Ok(json_to_rhai(&serde_json::Value::Array(arr)))
        },
    );

    // ---- Time ----------------------------------------------------

    engine.register_fn("now", || -> i64 { chrono::Utc::now().timestamp() });

    // ---- Logging -------------------------------------------------

    {
        let pid = pid_for_logs.clone();
        engine.register_fn("log_info", move |msg: ImmutableString| {
            tracing::info!(plugin_id = %pid, "{msg}");
        });
    }
    {
        let pid = pid_for_logs.clone();
        engine.register_fn("log_warn", move |msg: ImmutableString| {
            tracing::warn!(plugin_id = %pid, "{msg}");
        });
    }
}

// ---------------------------------------------------------------------------
// HTTP impls (ureq, sync).

fn http_get_impl(
    agent: &ureq::Agent,
    plugin_id: &str,
    url: &str,
    query: &Map,
    bearer: &str,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut req = agent.get(url);
    for (k, v) in map_to_query_iter(query) {
        req = req.query(&k, &v);
    }
    if !bearer.is_empty() {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let resp = req.call().map_err(|e| ureq_to_eval_err(plugin_id, "http_get", url, e))?;
    decode_response(plugin_id, url, resp)
}

fn http_post_impl(
    agent: &ureq::Agent,
    plugin_id: &str,
    url: &str,
    body: Dynamic,
    bearer: &str,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let body_json = rhai_to_json(body)
        .map_err(|e| EvalAltResult::ErrorRuntime(e.into(), rhai::Position::NONE))?;
    let mut req = agent.post(url);
    if !bearer.is_empty() {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let resp = req
        .send_json(body_json)
        .map_err(|e| ureq_to_eval_err(plugin_id, "http_post", url, e))?;
    decode_response(plugin_id, url, resp)
}

fn http_get_cached_impl(
    agent: &ureq::Agent,
    cache: &HttpCache,
    plugin_id: &str,
    url: &str,
    query: &Map,
    bearer: &str,
    ttl_secs: i64,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let pairs: Vec<(String, String)> = map_to_query_iter(query).collect();
    let query_repr = serde_json::to_string(&pairs).unwrap_or_default();
    let key = cache_key(url, &query_repr, bearer);
    if let Some(hit) = cache.get(&key) {
        return Ok(json_to_rhai(&hit));
    }
    let body = http_get_impl(agent, plugin_id, url, query, bearer)?;
    let body_json = rhai_to_json(body.clone())
        .map_err(|e| EvalAltResult::ErrorRuntime(e.into(), rhai::Position::NONE))?;
    let ttl = Duration::from_secs(ttl_secs.clamp(1, 86_400) as u64);
    cache.put(key, body_json, ttl);
    Ok(body)
}

fn decode_response(
    plugin_id: &str,
    url: &str,
    resp: ureq::Response,
) -> Result<Dynamic, Box<EvalAltResult>> {
    // Successful only — ureq surfaces non-2xx as Err already
    // (handled by ureq_to_eval_err). We shouldn't see one here.
    let body = resp.into_string().map_err(|e| {
        EvalAltResult::ErrorRuntime(
            format!("[{plugin_id}] read body {url}: {e}").into(),
            rhai::Position::NONE,
        )
    })?;
    if body.trim().is_empty() {
        return Ok(Dynamic::UNIT);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        EvalAltResult::ErrorRuntime(
            format!("[{plugin_id}] decode {url}: {e}").into(),
            rhai::Position::NONE,
        )
    })?;
    Ok(json_to_rhai(&parsed))
}

fn ureq_to_eval_err(
    plugin_id: &str,
    op: &str,
    url: &str,
    e: ureq::Error,
) -> Box<EvalAltResult> {
    let msg = match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("{op} [{plugin_id}] {url} returned {code}: {}", truncate(&body, 400))
        }
        ureq::Error::Transport(t) => {
            format!("{op} [{plugin_id}] {url}: {t}")
        }
    };
    EvalAltResult::ErrorRuntime(msg.into(), rhai::Position::NONE).into()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}…", &s[..max])
    }
}

// ---------------------------------------------------------------------------
// Conversions.

fn map_to_query_iter(m: &Map) -> impl Iterator<Item = (String, String)> + '_ {
    m.iter().map(|(k, v)| {
        let s = match v.clone().try_cast::<ImmutableString>() {
            Some(s) => s.to_string(),
            None => v.to_string(),
        };
        (k.to_string(), s)
    })
}

/// Convert a rhai `Dynamic` into a `serde_json::Value`. Supports
/// the shapes the script-tier plugin SDK actually produces:
/// nested maps + arrays of strings/ints/floats/bools/units.
pub(crate) fn rhai_to_json(d: Dynamic) -> Result<serde_json::Value, String> {
    if d.is_unit() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(b) = d.clone().try_cast::<bool>() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Some(i) = d.clone().try_cast::<i64>() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Some(f) = d.clone().try_cast::<f64>() {
        return serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| format!("cannot encode non-finite f64: {f}"));
    }
    if let Some(s) = d.clone().try_cast::<ImmutableString>() {
        return Ok(serde_json::Value::String(s.to_string()));
    }
    if let Some(s) = d.clone().try_cast::<String>() {
        return Ok(serde_json::Value::String(s));
    }
    if let Some(arr) = d.clone().try_cast::<rhai::Array>() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(rhai_to_json(item)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    if let Some(map) = d.clone().try_cast::<Map>() {
        let mut out = serde_json::Map::with_capacity(map.len());
        for (k, v) in map {
            out.insert(k.to_string(), rhai_to_json(v)?);
        }
        return Ok(serde_json::Value::Object(out));
    }
    Err(format!("unsupported rhai → json type: {}", d.type_name()))
}

/// Inverse of `rhai_to_json`. Numbers prefer i64 when they fit,
/// f64 otherwise. Null lands as Rhai's UNIT (`()`).
pub(crate) fn json_to_rhai(v: &serde_json::Value) -> Dynamic {
    match v {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                Dynamic::UNIT
            }
        }
        serde_json::Value::String(s) => Dynamic::from(ImmutableString::from(s.clone())),
        serde_json::Value::Array(items) => {
            let arr: rhai::Array = items.iter().map(json_to_rhai).collect();
            arr.into()
        }
        serde_json::Value::Object(obj) => {
            let map: Map = obj
                .iter()
                .map(|(k, v)| (k.as_str().into(), json_to_rhai(v)))
                .collect();
            map.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ScriptEngine;

    #[test]
    fn digits_only_strips_non_digits() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let v: String = engine
            .eval::<ImmutableString>(r#"digits_only("+1 (555) 123-4567")"#)
            .unwrap()
            .to_string();
        assert_eq!(v, "15551234567");
    }

    #[test]
    fn lower_and_trim_compose() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let v: String = engine
            .eval::<ImmutableString>(r#"trim(lower("  ALICE@Example.COM  "))"#)
            .unwrap()
            .to_string();
        assert_eq!(v, "alice@example.com");
    }

    #[test]
    fn hash_is_stable_for_same_input() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let a: ImmutableString = engine.eval(r#"hash("people/c12345")"#).unwrap();
        let b: ImmutableString = engine.eval(r#"hash("people/c12345")"#).unwrap();
        assert_eq!(a, b);
        // Different input → different hash.
        let c: ImmutableString = engine.eval(r#"hash("people/c99999")"#).unwrap();
        assert_ne!(a, c);
        // 16 hex chars (8 bytes).
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn now_returns_a_recent_unix_timestamp() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let t: i64 = engine.eval("now()").unwrap();
        // Sanity range: between 2024-01-01 and 2030-01-01.
        assert!(t > 1_700_000_000);
        assert!(t < 1_900_000_000);
    }

    #[test]
    fn json_path_extracts_nested_values() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let script = r#"
            let doc = #{
                "connections": [
                    #{ "names": [#{ "displayName": "Alice" }], "emails": ["a@x.com"] },
                    #{ "names": [#{ "displayName": "Bob" }], "emails": ["b@x.com"] }
                ]
            };
            json_path(doc, "$.connections[*].names[0].displayName")
        "#;
        let names: rhai::Array = engine.eval(script).unwrap();
        let strs: Vec<String> = names
            .into_iter()
            .map(|d| d.try_cast::<ImmutableString>().map(|s| s.to_string()).unwrap_or_default())
            .collect();
        assert_eq!(strs, vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn rhai_to_json_handles_nested_map_array_mix() {
        let v = rhai_to_json(Dynamic::from(rhai::Array::from([
            Dynamic::from(1_i64),
            Dynamic::from(ImmutableString::from("hi")),
            Dynamic::from({
                let mut m = Map::new();
                m.insert("k".into(), Dynamic::from(true));
                m
            }),
        ])))
        .unwrap();
        assert_eq!(v[0], 1);
        assert_eq!(v[1], "hi");
        assert_eq!(v[2]["k"], true);
    }

    #[test]
    fn json_to_rhai_round_trips_through_serde() {
        let original = serde_json::json!({
            "transport": "email",
            "handle": "alice@example.com",
            "_oauth": {"controller": "ya29.tok"},
        });
        let dynamic = json_to_rhai(&original);
        let back = rhai_to_json(dynamic).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn http_get_decodes_real_response_against_local_server() {
        // Spawn a tiny blocking server thread that handles a
        // single request and returns canned JSON.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf); // best-effort drain of the request
            let body = r#"{"ok":true,"items":[1,2,3]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("http-test");
        let script = format!(
            r#"
            let r = http_get("http://{addr}/", #{{ }}, "");
            r.ok
            "#
        );
        let v: bool = engine.eval(&script).unwrap();
        assert!(v);
    }

    /// Adversarial: an http_get against an unreachable port must
    /// surface a Rhai runtime error the caller can `try { } catch`.
    /// Without this, a network hiccup wedges the script.
    #[test]
    fn http_get_to_closed_port_surfaces_runtime_error() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("http-fail");
        let result = engine.eval::<Dynamic>(r#"http_get("http://127.0.0.1:1/", #{ }, "")"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("http_get"), "got: {err}");
        assert!(err.contains("http-fail"), "got: {err}");
    }
}
