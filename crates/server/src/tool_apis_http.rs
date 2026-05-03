//! HTTP-backed implementation of [`execlaw_core::tool::WebFetchApi`].
//!
//! Lives in `server` (not `core`) because `core` deliberately doesn't
//! pull in `reqwest` — keeping the durability heart of the project
//! light. The dispatch layer constructs an `HttpWebFetchApi` per
//! turn and stuffs it into `ToolCtx::web_fetch` when the tool's
//! descriptor declared `Capability::WebFetch`.
//!
//! ## Security boundary
//!
//! The implementation is the SSRF defense. The trait surface (a
//! single `get(url)` method) gives the tool body no way to reach
//! around it. Specific guards:
//!
//!   * Scheme allowlist: `http` / `https` only. `file://`,
//!     `gopher://`, `data://`, etc. — all rejected up front.
//!   * Host check (IP literal): if the URL's host is an IP
//!     literal, it must not be loopback / private / link-local /
//!     multicast / broadcast / documentation. Default-deny.
//!   * Host check (`localhost`): rejected unless the impl was
//!     constructed with `allow_loopback(true)` (tests only).
//!   * Content-Type allowlist: only `text/*`, `application/json`,
//!     `application/xml`, `application/ld+json`, `application/atom+xml`,
//!     `application/rss+xml`. Binary content is rejected before we
//!     read the body.
//!   * Size cap: 1 MiB by default. Larger responses are read up to
//!     the cap and the resulting `WebFetchResponse.truncated` is
//!     `true` so the LLM knows the body is incomplete.
//!   * Timeout: 30 s on the whole request.
//!
//! Known limitation (documented in the audit doc): DNS-based hosts
//! that resolve to private IPs aren't pre-resolved here. A future
//! iteration can add pre-resolve + reject-if-private. The current
//! implementation relies on the operator's network layer (firewall /
//! egress proxy) to enforce the harder boundary.
//!
//! 2026-04-29.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, WebFetchApi, WebFetchResponse};
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

/// Default body cap. 1 MiB is enough for an article, an OpenAPI doc,
/// a moderately sized JSON feed; larger payloads are typically not
/// what an agent legitimately wants to read inline.
pub const DEFAULT_MAX_BYTES: usize = 1_048_576;

/// Default per-request timeout.
pub const DEFAULT_TIMEOUT_S: u64 = 30;

/// Default User-Agent string for outbound HTTP. Identifies as a
/// recent Firefox so CDN bot-protection layers (Cloudflare, Akamai,
/// AWS WAF, Fastly, etc.) don't 403/406 the request the way they
/// reflexively do for the default `reqwest/X.Y` UA. The version is
/// intentionally stable-ish (mid-2026 ESR) so we don't have to
/// chase Firefox release-train churn; the bot-detection rules care
/// about "looks like a real browser" much more than precise version
/// matching.
///
/// Operators uneasy about UA-spoofing can override via
/// `HttpWebFetchApi::with_client(...)` and pass their own configured
/// reqwest client. The default is "stop the silent gather-failure
/// surface that motivated this fix"; the override is for purists.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Content types `web_fetch` is willing to read. Binary types fail
/// fast so the LLM doesn't get a base64-encoded image dumped into
/// context. Operators who need broader types can register a plugin
/// tool with its own validation.
pub const ALLOWED_CONTENT_TYPE_PREFIXES: &[&str] = &[
    "text/",
    "application/json",
    "application/xml",
    "application/ld+json",
    "application/atom+xml",
    "application/rss+xml",
];

pub struct HttpWebFetchApi {
    client: reqwest::Client,
    max_bytes: usize,
    timeout: Duration,
    allow_loopback: bool,
}

impl HttpWebFetchApi {
    /// Production constructor: SSRF guard on, 1 MiB cap, 30 s
    /// timeout, fresh reqwest client with no redirect surprises
    /// (10-redirect cap, default).
    ///
    /// Sets a realistic browser User-Agent. Without one, the default
    /// reqwest UA (`reqwest/0.12.x`) is recognized as a bot by most
    /// CDN / WAF stacks (Cloudflare, Akamai, Fastly bot-protect) and
    /// returns 403/406 — which silently torpedoed the deep-research
    /// gather phase (every URL fetch failed, every note ended up
    /// empty). Operator can override via `with_client` if they need
    /// a different identity (e.g. a corporate-policy UA).
    ///
    /// Also sets `Accept` and `Accept-Language` headers since some
    /// sites also bot-detect on those being absent.
    pub fn new() -> Self {
        use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE};
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(DEFAULT_USER_AGENT)
            .default_headers(headers)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            allow_loopback: false,
        }
    }

    /// Bring your own client (custom proxy, custom user agent, etc.).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            max_bytes: DEFAULT_MAX_BYTES,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            allow_loopback: false,
        }
    }

    /// Override the response-size cap.
    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }

    /// Override the request timeout.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Test seam — flip this `true` so unit tests can reach a
    /// `127.0.0.1:0`-bound local mock without the SSRF guard
    /// rejecting it. Production code never calls this.
    pub fn allow_loopback(mut self, yes: bool) -> Self {
        self.allow_loopback = yes;
        self
    }
}

impl Default for HttpWebFetchApi {
    fn default() -> Self {
        Self::new()
    }
}

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
                // 169.254/16 covered by is_link_local;
                // 0.0.0.0/8 we treat as unspecified.
                // Carrier-grade NAT 100.64/10:
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ULA fc00::/7 — `is_unique_local` is unstable, so
                // check the first byte directly.
                || (v6.octets()[0] & 0xfe) == 0xfc
                // Link-local fe80::/10 — `is_unicast_link_local` is
                // unstable; check first 10 bits.
                || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
        }
    }
}

/// Validate a URL's scheme + host. Returns the parsed `Url` on
/// success.
fn validate_url(
    url_str: &str,
    allow_loopback: bool,
) -> Result<reqwest::Url, ApiError> {
    let url = reqwest::Url::parse(url_str)
        .map_err(|e| ApiError::Validation(format!("invalid URL: {e}")))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ApiError::Validation(format!(
                "scheme {other:?} not allowed; only http and https are permitted"
            )));
        }
    }
    use url::Host;
    let host = url
        .host()
        .ok_or_else(|| ApiError::Validation("URL has no host".into()))?;
    match host {
        Host::Domain(d) => {
            let lower = d.to_ascii_lowercase();
            if !allow_loopback
                && (lower == "localhost" || lower.ends_with(".localhost"))
            {
                return Err(ApiError::Validation(
                    "localhost addresses are not allowed".into(),
                ));
            }
            // Defense-in-depth: a domain string that happens to be a
            // dotted-quad ("0177.0.0.1" etc. unusual encodings) gets
            // a final IpAddr parse attempt. The `Host::Domain` arm
            // covers the common case; this catches the weird ones.
            if let Ok(ip) = IpAddr::from_str(d)
                && is_private_or_local_ip(&ip)
                && !allow_loopback
            {
                return Err(ApiError::Validation(format!(
                    "IP address {ip} is in a private / loopback / link-local range"
                )));
            }
        }
        Host::Ipv4(v4) => {
            let ip = IpAddr::V4(v4);
            if !allow_loopback && is_private_or_local_ip(&ip) {
                return Err(ApiError::Validation(format!(
                    "IP address {v4} is in a private / loopback / link-local range"
                )));
            }
        }
        Host::Ipv6(v6) => {
            let ip = IpAddr::V6(v6);
            if !allow_loopback && is_private_or_local_ip(&ip) {
                return Err(ApiError::Validation(format!(
                    "IP address {v6} is in a private / loopback / link-local range"
                )));
            }
        }
    }
    Ok(url)
}

fn content_type_allowed(ct: &str) -> bool {
    let ct_lower = ct.to_ascii_lowercase();
    let primary = ct_lower
        .split(';')
        .next()
        .unwrap_or(&ct_lower)
        .trim();
    ALLOWED_CONTENT_TYPE_PREFIXES
        .iter()
        .any(|p| primary.starts_with(p))
}

#[async_trait]
impl WebFetchApi for HttpWebFetchApi {
    async fn get(&self, url: &str) -> Result<WebFetchResponse, ApiError> {
        let parsed = validate_url(url, self.allow_loopback)?;
        let resp = self
            .client
            .get(parsed)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network error: {e}")))?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        // Re-validate the FINAL url after redirects. Cheap defense
        // against a server redirecting us into a private network.
        validate_url(&final_url, self.allow_loopback)?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        if let Some(ct) = &content_type
            && !content_type_allowed(ct)
        {
            return Err(ApiError::Validation(format!(
                "content-type {ct:?} not allowed; only text and structured-data MIME \
                 types are permitted"
            )));
        }
        // Stream bytes up to the cap so a malicious server can't
        // OOM us with `Content-Length: 999999999999`.
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::with_capacity(8_192);
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| ApiError::Storage(format!("body read: {e}")))?;
            let remaining = self.max_bytes.saturating_sub(buf.len());
            if chunk.len() > remaining {
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
            if buf.len() >= self.max_bytes {
                truncated = true;
                break;
            }
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        Ok(WebFetchResponse {
            final_url,
            status,
            content_type,
            body,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_rejects_non_http_schemes() {
        for s in [
            "file:///etc/passwd",
            "data:text/plain;base64,SGVsbG8=",
            "gopher://example.com",
            "ftp://example.com",
        ] {
            let err = validate_url(s, false).unwrap_err();
            match err {
                ApiError::Validation(msg) => assert!(msg.contains("scheme")),
                other => panic!("expected Validation, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_url_rejects_localhost_by_default() {
        let err =
            validate_url("http://localhost:8080/path", false).unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("localhost")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn validate_url_allows_localhost_when_test_seam_set() {
        validate_url("http://localhost:8080/path", true).unwrap();
    }

    #[test]
    fn validate_url_rejects_private_ipv4_literals() {
        for s in [
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://172.16.0.1/x",
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest", // AWS metadata!
        ] {
            let err = validate_url(s, false).unwrap_err();
            match err {
                ApiError::Validation(msg) => {
                    assert!(msg.contains("private") || msg.contains("loopback"));
                }
                other => panic!("expected Validation, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_url_rejects_loopback_ipv6() {
        let err = validate_url("http://[::1]/x", false).unwrap_err();
        match err {
            ApiError::Validation(_) => {}
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn validate_url_allows_public_https() {
        validate_url("https://example.com/foo", false).unwrap();
    }

    #[test]
    fn content_type_allowlist_admits_textual_responses() {
        assert!(content_type_allowed("text/html; charset=UTF-8"));
        assert!(content_type_allowed("application/json"));
        assert!(content_type_allowed("application/xml"));
        assert!(content_type_allowed("text/plain"));
    }

    #[test]
    fn content_type_allowlist_rejects_binary() {
        assert!(!content_type_allowed("image/png"));
        assert!(!content_type_allowed("video/mp4"));
        assert!(!content_type_allowed("application/octet-stream"));
        assert!(!content_type_allowed("application/pdf"));
    }

    /// End-to-end test against a tiny in-process HTTP server: small
    /// 200 response with a `text/plain` body comes through clean.
    #[tokio::test]
    async fn fetch_against_local_mock_returns_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
            let _ = sock.write_all(resp).await;
        });
        let api = HttpWebFetchApi::new().allow_loopback(true);
        let resp = api.get(&format!("http://{addr}/")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "hello");
        assert_eq!(resp.content_type.as_deref(), Some("text/plain"));
        assert!(!resp.truncated);
    }

    /// Body size cap kicks in. Server returns 100 bytes; cap is 16.
    /// Resulting `truncated` flag is true and body is exactly 16 bytes.
    #[tokio::test]
    async fn fetch_truncates_body_when_cap_exceeded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body = "a".repeat(100);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let api = HttpWebFetchApi::new().allow_loopback(true).max_bytes(16);
        let resp = api.get(&format!("http://{addr}/")).await.unwrap();
        assert!(resp.truncated);
        assert_eq!(resp.body.len(), 16);
    }

    /// Server returns binary content-type; the impl rejects without
    /// reading the body.
    #[tokio::test]
    async fn fetch_rejects_binary_content_type() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 0\r\n\r\n";
            let _ = sock.write_all(resp).await;
        });
        let api = HttpWebFetchApi::new().allow_loopback(true);
        let err = api.get(&format!("http://{addr}/")).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("content-type")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}
