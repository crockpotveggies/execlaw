//! Websurfx search-provider adapter.
//!
//! Websurfx is a self-hosted Rust-native meta-search engine —
//! similar in spirit to SearxNG but with its own codebase, config
//! format, and faster cold-start. Operator runs a Websurfx
//! container alongside execlaw, points this adapter at its base
//! URL, and gets aggregated results without an API key. Aligns
//! with execlaw's "no mandatory external services" rule.
//!
//! Wire format (https://github.com/neon-mmd/websurfx):
//!
//!   GET <base>/search?q=<query>&page=<page>&format=json
//!
//!   Response: {
//!     "results": [
//!       {"url", "title", "description", "engine", ...},
//!       ...
//!     ],
//!     ...
//!   }
//!
//! Older Websurfx releases only served HTML on `/search`; the
//! JSON endpoint shipped in 1.x. If the operator's instance
//! returns text/html we surface a discriminating error pointing
//! at the upgrade path rather than a confusing parse failure.
//!
//! 2026-05-06.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const DEFAULT_TIMEOUT_S: u64 = 20;

#[derive(Debug, Deserialize)]
struct WebsurfxResponse {
    #[serde(default)]
    results: Vec<WebsurfxResult>,
}

#[derive(Debug, Deserialize)]
struct WebsurfxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// Websurfx calls the snippet `description`. Older / fork
    /// builds occasionally use `content` (mirroring SearxNG's
    /// field name) — accept that as an alias so a doc drift
    /// doesn't silently drop snippets.
    #[serde(default, alias = "content")]
    description: String,
}

pub struct WebsurfxSearchApi {
    client: reqwest::Client,
    base_url: String,
}

impl WebsurfxSearchApi {
    /// Construct from a base URL. Must include scheme + host;
    /// trailing slash is normalised away. The adapter appends
    /// `/search` itself so the operator can pass the root (e.g.
    /// `http://websurfx.local:8080`) without thinking about path
    /// joins.
    pub fn new(base_url: impl Into<String>) -> Self {
        let raw = base_url.into();
        let base_url = raw.trim_end_matches('/').to_owned();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, base_url }
    }

    /// Test seam: bring your own client.
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        let raw = base_url.into();
        Self {
            client,
            base_url: raw.trim_end_matches('/').to_owned(),
        }
    }
}

#[async_trait]
impl WebSearchApi for WebsurfxSearchApi {
    fn provider_id(&self) -> &str {
        "websurfx"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.base_url.is_empty() {
            return Err(ApiError::Validation(
                "Websurfx base_url is empty; configure it in Settings → Search".into(),
            ));
        }
        let url = format!("{}/search", self.base_url);
        let resp = self
            .client
            .get(&url)
            .query(&[("q", query), ("page", "1"), ("format", "json")])
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Storage(format!(
                "Websurfx returned HTTP {} for {}: {}",
                status.as_u16(),
                url,
                truncate(&body, 200),
            )));
        }
        // If the upstream is an older Websurfx build that doesn't
        // support `format=json`, it returns 200 with text/html.
        // Surface a discriminating error so the operator knows to
        // upgrade rather than wonder about a parse failure.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("text/html") {
            return Err(ApiError::Storage(
                "Websurfx returned HTML — your instance is likely on an older release \
                 that doesn't support `?format=json`. Upgrade to Websurfx 1.x or \
                 switch to SearxNG."
                    .into(),
            ));
        }
        let parsed: WebsurfxResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap.min(parsed.results.len()));
        for r in parsed.results.into_iter().take(cap) {
            if r.url.is_empty() || r.title.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: r.title,
                url: r.url,
                snippet: if r.description.is_empty() {
                    None
                } else {
                    Some(r.description)
                },
            });
        }
        Ok(out)
    }
}

fn truncate(s: &str, max: usize) -> String {
    let trimmed: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_is_websurfx() {
        assert_eq!(
            WebsurfxSearchApi::new("http://x.example.com").provider_id(),
            "websurfx",
        );
    }

    #[test]
    fn constructor_strips_trailing_slash_from_base_url() {
        let api = WebsurfxSearchApi::new("http://websurfx.local:8080/");
        assert_eq!(api.base_url, "http://websurfx.local:8080");
        let api2 = WebsurfxSearchApi::new("http://websurfx.local:8080");
        assert_eq!(api2.base_url, "http://websurfx.local:8080");
    }

    #[tokio::test]
    async fn empty_base_url_returns_validation_error_not_panic() {
        let api = WebsurfxSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("base_url")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_canned_websurfx_json_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"{
            "query": "test",
            "results": [
                {"title": "First", "url": "https://example.com/a", "description": "First snippet"},
                {"title": "Second", "url": "https://example.com/b", "description": ""},
                {"title": "", "url": "https://example.com/c", "description": "Title-empty (skip)"}
            ]
        }"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(response.as_bytes()).await.unwrap();
        });

        let api = WebsurfxSearchApi::with_client(
            format!("http://{addr}"),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let results = api.search("test", 10).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet.as_deref(), Some("First snippet"));
        assert!(results[1].snippet.is_none());
    }

    #[tokio::test]
    async fn surfaces_discriminating_error_when_instance_returns_html() {
        // Older Websurfx releases serve HTML on /search regardless
        // of `format=json`. Adapter must surface the upgrade hint
        // rather than fail at JSON parse.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = "<html><body><h1>Websurfx</h1></body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(response.as_bytes()).await.unwrap();
        });

        let api = WebsurfxSearchApi::with_client(
            format!("http://{addr}"),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let err = api.search("anything", 10).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Websurfx returned HTML")
                && (msg.contains("upgrade") || msg.contains("Upgrade")),
            "error must explain the upgrade path: {msg}",
        );
    }

    #[test]
    fn accepts_content_alias_for_description() {
        // Some Websurfx forks emit `content` instead of
        // `description`. The serde alias keeps the snippet path
        // working without code change.
        let body = r#"{
            "results": [
                {"title": "T", "url": "https://example.com/a", "content": "via alias"}
            ]
        }"#;
        let parsed: WebsurfxResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results[0].description, "via alias");
    }
}
