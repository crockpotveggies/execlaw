//! SearxNG search-provider adapter.
//!
//! SearxNG is a self-hosted meta-search engine — operator runs a
//! SearxNG container alongside execlaw, points this adapter at its
//! base URL, and gets results aggregated from Google / Bing / DDG /
//! Wikipedia / etc. without sharing rate-limit ceilings with
//! anyone else. Aligns with execlaw's "no mandatory external
//! services" grounding rule (the only optional service here is the
//! operator's own SearxNG box).
//!
//! Wire format (https://docs.searxng.org/dev/search_api.html):
//!
//!   GET <base>/search?format=json&q=<query>&safesearch=0&pageno=1
//!
//!   Response: { "results": [ { "title", "url", "content", ... } ] }
//!
//! No API key required. Some SearxNG instances disable the JSON
//! format by default (admin sets `formats: [html, json]` in
//! settings.yml) — when that's the case, the adapter surfaces a
//! discriminating error so the operator knows to fix the upstream
//! config.
//!
//! 2026-05-04.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const DEFAULT_TIMEOUT_S: u64 = 20;

#[derive(Debug, Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// SearxNG calls the snippet `content`; some engines emit
    /// `pretty_url` or `description` instead but `content` is the
    /// canonical field per the docs.
    #[serde(default)]
    content: String,
}

pub struct SearxNGSearchApi {
    client: reqwest::Client,
    base_url: String,
}

impl SearxNGSearchApi {
    /// Construct from a base URL. The base URL must include scheme
    /// + host; trailing slash is normalised away. The adapter
    /// appends `/search` itself so the operator can pass the root
    /// (e.g. `https://searx.example.com`) without thinking about
    /// path joins.
    ///
    /// Validation is deferred to `search()` — the constructor never
    /// errors so the dispatcher can construct lazily without
    /// fallible plumbing.
    pub fn new(base_url: impl Into<String>) -> Self {
        let raw = base_url.into();
        let base_url = raw.trim_end_matches('/').to_owned();
        // Same realistic browser UA as the DDG client. SearxNG itself
        // doesn't bot-detect, but the engines IT proxies (Google,
        // Bing) sometimes do — and SearxNG forwards the UA through
        // to those upstreams.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, base_url }
    }

    /// Test seam: bring your own client. Production uses `new`.
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        let raw = base_url.into();
        Self {
            client,
            base_url: raw.trim_end_matches('/').to_owned(),
        }
    }
}

#[async_trait]
impl WebSearchApi for SearxNGSearchApi {
    fn provider_id(&self) -> &str {
        "searxng"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.base_url.is_empty() {
            return Err(ApiError::Validation(
                "SearxNG base_url is empty; configure it in Settings → Search".into(),
            ));
        }
        let url = format!("{}/search", self.base_url);
        // SearxNG accepts both GET-querystring and POST-form; POST
        // keeps long queries off the URL line (some operators run
        // SearxNG behind a reverse proxy with URL-length limits).
        let body = [
            ("q", query),
            ("format", "json"),
            ("safesearch", "0"),
            ("pageno", "1"),
        ];
        let resp = self
            .client
            .post(&url)
            .form(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            // 403 with "json format not enabled" is the most
            // common config issue — surface a discriminating
            // message so the operator knows to fix settings.yml
            // rather than wonder why their SearxNG box isn't
            // working.
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 403 && body.contains("json") {
                return Err(ApiError::Storage(format!(
                    "SearxNG returned 403 — the JSON format is likely disabled. \
                     Add `json` to `search.formats` in settings.yml on the SearxNG instance. \
                     Body: {}",
                    truncate(&body, 200),
                )));
            }
            return Err(ApiError::Storage(format!(
                "SearxNG returned HTTP {} for {}: {}",
                status.as_u16(),
                url,
                truncate(&body, 200),
            )));
        }
        let parsed: SearxResponse = resp
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
                snippet: if r.content.is_empty() {
                    None
                } else {
                    Some(r.content)
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
    fn provider_id_is_searxng() {
        assert_eq!(
            SearxNGSearchApi::new("https://x.example.com").provider_id(),
            "searxng"
        );
    }

    #[test]
    fn constructor_strips_trailing_slash_from_base_url() {
        let api = SearxNGSearchApi::new("https://searx.example.com/");
        assert_eq!(api.base_url, "https://searx.example.com");
        let api2 = SearxNGSearchApi::new("https://searx.example.com");
        assert_eq!(api2.base_url, "https://searx.example.com");
    }

    #[tokio::test]
    async fn empty_base_url_returns_validation_error_not_panic() {
        // Production flow: operator selects searxng but forgets to
        // fill the URL. The adapter must error cleanly so the
        // gather card surfaces "configure SearxNG URL" instead of
        // a panic or a network-error stack trace.
        let api = SearxNGSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("base_url")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Mock-server happy path: spin up a tiny TCP listener that
    /// returns a canned SearxNG JSON response. Verifies parse
    /// round-trip without depending on a real SearxNG instance.
    #[tokio::test]
    async fn parses_canned_searxng_json_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"{
            "query": "test",
            "results": [
                {"title": "First", "url": "https://example.com/a", "content": "First snippet"},
                {"title": "Second", "url": "https://example.com/b", "content": ""},
                {"title": "", "url": "https://example.com/c", "content": "Title-empty (skip)"}
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

        let api = SearxNGSearchApi::with_client(
            format!("http://{addr}"),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let results = api.search("test", 10).await.unwrap();
        // Two valid results (third is filtered out for empty title).
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet.as_deref(), Some("First snippet"));
        // Empty content → snippet is None, NOT Some("").
        assert!(results[1].snippet.is_none());
    }

    #[tokio::test]
    async fn surfaces_discriminating_error_when_json_format_disabled() {
        // SearxNG returns 403 with a body mentioning "json" when
        // the admin hasn't enabled the json output format.
        // Adapter must surface the specific cause so the operator
        // can fix settings.yml.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"{"error": "json format not enabled in this instance"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(response.as_bytes()).await.unwrap();
        });

        let api = SearxNGSearchApi::with_client(
            format!("http://{addr}"),
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        );
        let err = api.search("anything", 10).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("settings.yml") || msg.contains("JSON format"),
            "error must explain the config fix: {msg}",
        );
    }
}
