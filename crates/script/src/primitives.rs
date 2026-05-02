//! Host-injected primitives the script can call.
//!
//! The list is deliberately tight — every function here is an
//! escape hatch out of the sandbox, so each is a security +
//! maintenance commitment. Adding to this list is a deliberate
//! design decision; subtracting is safe.
//!
//! Categories:
//!   * **HTTP** — `http_get`, `http_post`, `http_patch`, `http_delete`,
//!     `http_get_cached`
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
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// Register every primitive against `engine`, capturing
/// `plugin_id` for log lines + the shared HTTP agent + a
/// per-plugin cache + the SSRF allow_loopback flag (false in
/// production; true only for tests against 127.0.0.1 mocks).
pub(crate) fn register(
    engine: &mut Engine,
    plugin_id: &str,
    http_agent: ureq::Agent,
    cache: Arc<HttpCache>,
    allow_loopback: bool,
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
                http_get_impl(&agent, &pid, &url, &query, &bearer, allow_loopback)
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
                http_post_impl(&agent, &pid, &url, body, &bearer, allow_loopback)
            },
        );
    }

    // http_patch(url, body_map_or_value, bearer) -> map | array | null
    //
    // Same shape as http_post but issues a PATCH — used by APIs
    // that take partial-update bodies (e.g. Google Calendar's
    // PATCH /calendars/{id}/events/{eventId}). ureq doesn't expose
    // a `.patch()` shortcut, so we go through `.request("PATCH", url)`.
    {
        let agent = http_agent.clone();
        let pid = plugin_id.to_owned();
        engine.register_fn(
            "http_patch",
            move |url: ImmutableString, body: Dynamic, bearer: ImmutableString|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                http_patch_impl(&agent, &pid, &url, body, &bearer, allow_loopback)
            },
        );
    }

    // http_delete(url, query_map, bearer) -> map | unit
    //
    // Issues a DELETE with optional query-string params (Google
    // Calendar's DELETE accepts `sendNotifications` etc). Most
    // DELETE endpoints return 204 No Content; `decode_response`
    // already returns Dynamic::UNIT on an empty body so the script
    // can branch with `if r == ()`.
    {
        let agent = http_agent.clone();
        let pid = plugin_id.to_owned();
        engine.register_fn(
            "http_delete",
            move |url: ImmutableString, query: Map, bearer: ImmutableString|
                  -> Result<Dynamic, Box<EvalAltResult>> {
                http_delete_impl(&agent, &pid, &url, &query, &bearer, allow_loopback)
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
                http_get_cached_impl(&agent, &cache, &pid, &url, &query, &bearer, ttl_secs, allow_loopback)
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

    // unix_to_rfc3339(unix_secs: i64) -> "YYYY-MM-DDTHH:MM:SSZ"
    // — formatted UTC. Date math is gnarly to do in Rhai; lean
    // on chrono so plugins don't have to ship a Hinnant
    // civil-from-days implementation in script.
    engine.register_fn("unix_to_rfc3339", |unix: i64| -> ImmutableString {
        chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default()
            .into()
    });

    // ---- URL encoding -------------------------------------------

    // url_encode(s) -> percent-encoded RFC 3986 path segment.
    // For Calendar-API-style ids that contain '@' / ':' / '/'
    // when interpolated into a URL path. Same as JS
    // encodeURIComponent for the typical case.
    engine.register_fn("url_encode", |s: ImmutableString| -> ImmutableString {
        // Allowed unreserved per RFC 3986: ALPHA / DIGIT / -._~
        let mut out = String::with_capacity(s.len());
        for b in s.as_bytes() {
            let c = *b;
            let unreserved = c.is_ascii_alphanumeric()
                || c == b'-' || c == b'.' || c == b'_' || c == b'~';
            if unreserved {
                out.push(c as char);
            } else {
                out.push_str(&format!("%{c:02X}"));
            }
        }
        out.into()
    });

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
    allow_loopback: bool,
) -> Result<Dynamic, Box<EvalAltResult>> {
    validate_url(plugin_id, "http_get", url, allow_loopback)?;
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
    allow_loopback: bool,
) -> Result<Dynamic, Box<EvalAltResult>> {
    validate_url(plugin_id, "http_post", url, allow_loopback)?;
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

fn http_patch_impl(
    agent: &ureq::Agent,
    plugin_id: &str,
    url: &str,
    body: Dynamic,
    bearer: &str,
    allow_loopback: bool,
) -> Result<Dynamic, Box<EvalAltResult>> {
    validate_url(plugin_id, "http_patch", url, allow_loopback)?;
    let body_json = rhai_to_json(body)
        .map_err(|e| EvalAltResult::ErrorRuntime(e.into(), rhai::Position::NONE))?;
    let mut req = agent.request("PATCH", url);
    if !bearer.is_empty() {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let resp = req
        .send_json(body_json)
        .map_err(|e| ureq_to_eval_err(plugin_id, "http_patch", url, e))?;
    decode_response(plugin_id, url, resp)
}

fn http_delete_impl(
    agent: &ureq::Agent,
    plugin_id: &str,
    url: &str,
    query: &Map,
    bearer: &str,
    allow_loopback: bool,
) -> Result<Dynamic, Box<EvalAltResult>> {
    validate_url(plugin_id, "http_delete", url, allow_loopback)?;
    let mut req = agent.delete(url);
    for (k, v) in map_to_query_iter(query) {
        req = req.query(&k, &v);
    }
    if !bearer.is_empty() {
        req = req.set("Authorization", &format!("Bearer {bearer}"));
    }
    let resp = req
        .call()
        .map_err(|e| ureq_to_eval_err(plugin_id, "http_delete", url, e))?;
    decode_response(plugin_id, url, resp)
}

/// SSRF guard for the script-tier HTTP primitives. Mirrors the
/// validation in `crates/server/src/tool_apis_http.rs::validate_url`
/// — a script must not have MORE permissive HTTP than the native
/// `web_fetch` tool. Rejected:
///
///   * non-http(s) schemes (file://, gopher://, …)
///   * loopback (127/8, ::1, "localhost")
///   * private IPv4 ranges (10/8, 172.16/12, 192.168/16)
///   * link-local (169.254/16, fe80::/10) — incl. cloud metadata
///   * carrier-grade NAT (100.64/10)
///   * ULA (fc00::/7), multicast, broadcast, unspecified, documentation
///   * weird encodings: a hostname that parses as a private IP
///
/// Tests should opt out via `with_http_agent` + a different
/// loopback-allowed primitives module if they need to point at
/// 127.0.0.1 mocks. The existing test pattern uses an in-process
/// `127.0.0.1:0` listener — those tests construct the
/// `ScriptEngine` test-side (see `engine.rs::with_http_agent`)
/// but rely on the loopback-allowance flag. Keep that in sync.
fn validate_url(
    plugin_id: &str,
    op: &str,
    url_str: &str,
    allow_loopback: bool,
) -> Result<(), Box<EvalAltResult>> {
    let url = url::Url::parse(url_str).map_err(|e| {
        EvalAltResult::ErrorRuntime(
            format!("{op} [{plugin_id}] invalid URL '{url_str}': {e}").into(),
            rhai::Position::NONE,
        )
    })?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(EvalAltResult::ErrorRuntime(
                format!("{op} [{plugin_id}] scheme '{other}' not allowed; only http(s)").into(),
                rhai::Position::NONE,
            )
            .into());
        }
    }
    let host = url.host().ok_or_else(|| {
        Box::new(EvalAltResult::ErrorRuntime(
            format!("{op} [{plugin_id}] URL has no host: {url_str}").into(),
            rhai::Position::NONE,
        ))
    })?;
    use url::Host;
    let bad = |reason: &str| -> Box<EvalAltResult> {
        EvalAltResult::ErrorRuntime(
            format!("{op} [{plugin_id}] {reason}: {url_str}").into(),
            rhai::Position::NONE,
        )
        .into()
    };
    match host {
        Host::Domain(d) => {
            let lower = d.to_ascii_lowercase();
            if !allow_loopback
                && (lower == "localhost" || lower.ends_with(".localhost"))
            {
                return Err(bad("loopback hostname not allowed"));
            }
            // Defense-in-depth: a hostname that's actually a
            // dotted-quad in unusual encoding ("0177.0.0.1" etc.)
            // gets a final IpAddr parse attempt.
            if let Ok(ip) = IpAddr::from_str(d)
                && !allow_loopback
                && is_private_or_local_ip(&ip)
            {
                return Err(bad("private/loopback/link-local IP not allowed"));
            }
        }
        Host::Ipv4(v4) => {
            let ip = IpAddr::V4(v4);
            if !allow_loopback && is_private_or_local_ip(&ip) {
                return Err(bad("private/loopback/link-local IP not allowed"));
            }
        }
        Host::Ipv6(v6) => {
            let ip = IpAddr::V6(v6);
            if !allow_loopback && is_private_or_local_ip(&ip) {
                return Err(bad("private/loopback/link-local IP not allowed"));
            }
        }
    }
    Ok(())
}

/// Mirror of `tool_apis_http::is_private_or_local_ip`. Stays in
/// sync by hand — duplicated rather than depended-on because
/// extracting to a shared crate would create a server → script
/// dep going the wrong direction, and the function is small +
/// stable.
fn is_private_or_local_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.octets()[0] & 0xfe) == 0xfc
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
        }
    }
}

// Eight homogeneous args, single call site — bundling into a
// struct just to silence the lint adds churn without clarity.
#[allow(clippy::too_many_arguments)]
fn http_get_cached_impl(
    agent: &ureq::Agent,
    cache: &HttpCache,
    plugin_id: &str,
    url: &str,
    query: &Map,
    bearer: &str,
    ttl_secs: i64,
    allow_loopback: bool,
) -> Result<Dynamic, Box<EvalAltResult>> {
    let pairs: Vec<(String, String)> = map_to_query_iter(query).collect();
    let query_repr = serde_json::to_string(&pairs).unwrap_or_default();
    let key = cache_key(url, &query_repr, bearer);
    if let Some(hit) = cache.get(&key) {
        return Ok(json_to_rhai(&hit));
    }
    let body = http_get_impl(agent, plugin_id, url, query, bearer, allow_loopback)?;
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
    fn unix_to_rfc3339_formats_known_timestamp() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        // 1700000000 = 2023-11-14T22:13:20Z
        let s: ImmutableString = engine
            .eval("unix_to_rfc3339(1700000000)")
            .unwrap();
        assert_eq!(s.as_str(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn url_encode_percent_encodes_reserved_characters() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("test");
        let cases: &[(&str, &str)] = &[
            ("user@example.com", "user%40example.com"),
            ("a/b:c", "a%2Fb%3Ac"),
            ("hello world", "hello%20world"),
            ("plain", "plain"),
            ("a-b.c_d~e", "a-b.c_d~e"),
        ];
        for (input, want) in cases {
            let script = format!(r#"url_encode("{input}")"#);
            let got: ImmutableString = engine.eval(&script).unwrap();
            assert_eq!(got.as_str(), *want, "input={input}");
        }
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
        // Test mock is on 127.0.0.1, so opt out of the SSRF guard
        // for this test only — production never uses this constructor.
        let factory = ScriptEngine::with_loopback_allowed_for_tests();
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
    /// With the SSRF guard ON (default), 127.0.0.1 is rejected
    /// BEFORE the connect — the error message still names http_get
    /// + the plugin id, so the contract holds either way.
    #[test]
    fn http_get_to_closed_port_surfaces_runtime_error() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("http-fail");
        let result = engine.eval::<Dynamic>(r#"http_get("http://127.0.0.1:1/", #{ }, "")"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("http_get"), "got: {err}");
        assert!(err.contains("http-fail"), "got: {err}");
    }

    /// SSRF guard pins: production constructor rejects loopback /
    /// private / link-local + non-http schemes BEFORE any network
    /// call. Mirrors the contract in tool_apis_http::validate_url
    /// — a script must not have MORE permissive HTTP than the
    /// native web_fetch tool.
    #[test]
    fn ssrf_guard_rejects_loopback_in_production_default() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("ssrf-test");
        for url in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/", // AWS metadata
            "http://[::1]/",
            "http://[fe80::1]/",
        ] {
            let script = format!(r#"http_get("{url}", #{{ }}, "")"#);
            let err = engine
                .eval::<Dynamic>(&script)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("not allowed"),
                "URL {url} should be SSRF-rejected; got: {err}"
            );
        }
    }

    #[test]
    fn ssrf_guard_rejects_non_http_schemes() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("ssrf-test");
        for url in ["file:///etc/passwd", "gopher://x/", "ftp://x/"] {
            let script = format!(r#"http_get("{url}", #{{ }}, "")"#);
            let err = engine
                .eval::<Dynamic>(&script)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("not allowed") && err.contains("http"),
                "URL {url} should be scheme-rejected; got: {err}"
            );
        }
    }

    #[test]
    fn ssrf_guard_allows_public_addresses() {
        let factory = ScriptEngine::new();
        let engine = factory.build_for_plugin("ssrf-test");
        // No DNS resolution happens at validate-time — we just
        // accept the hostname. Confirm parsing + validation pass
        // for the realistic public-API hostnames a plugin uses.
        // (Actual connection would fail in a sealed test env, so
        // we wrap in try/catch and look at where it failed.)
        let script = r#"
            try {
                http_get("https://people.googleapis.com/v1/people/me/connections", #{ }, "")
            } catch(e) {
                // Connect error / TLS error / DNS error from ureq
                // is fine — we just want to confirm the SSRF guard
                // didn't fire FIRST.
                "passed-ssrf:" + e
            }
        "#;
        let result = engine.eval::<Dynamic>(script).unwrap();
        let s = result.to_string();
        // Either the call returned successfully (unlikely in
        // sandboxed CI) OR our catch fired with an error from
        // BEYOND the SSRF guard.
        if s.starts_with("passed-ssrf:") {
            assert!(
                !s.contains("not allowed"),
                "public hostname should pass SSRF guard; got: {s}"
            );
        }
    }
}
