//! SerpAPI search-provider adapter.
//!
//! SerpAPI is a paid wrapper around Google / Bing / Yahoo / etc.
//! search results. Operator-supplied API key (free tier:
//! 100 searches/month at time of writing; paid tiers from
//! $50/mo). The right pick when an operator wants Google-quality
//! results without scraping or running their own SearxNG.
//!
//! Wire format (https://serpapi.com/search-api):
//!
//!   GET https://serpapi.com/search.json
//!     ?q=<query>
//!     &api_key=<api_key>
//!     &engine=google
//!     &num=N
//!
//!   Response: {
//!     "organic_results": [
//!       {"title", "link", "snippet", "position", ...},
//!       ...
//!     ],
//!     "search_metadata": {...},
//!     "search_information": {...}
//!   }
//!
//! 2026-05-06.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const SERPAPI_ENDPOINT: &str = "https://serpapi.com/search.json";
const DEFAULT_TIMEOUT_S: u64 = 25;

/// SerpAPI's free tier doesn't publish a per-second cap (the
/// monthly count is the limit). Conservative ~4 qps pacing keeps
/// concurrent gather workers polite without burning credit
/// budget on the burst.
const SERPAPI_MIN_REQUEST_GAP: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct SerpApiResponse {
    #[serde(default)]
    organic_results: Vec<SerpApiOrganicResult>,
    /// Populated when SerpAPI rejects the request (bad key,
    /// quota exhausted, malformed engine, etc.). Surface verbatim
    /// in the error path so the operator can fix the underlying
    /// cause without poking around the dashboard.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SerpApiOrganicResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
}

pub struct SerpApiSearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl SerpApiSearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(SERPAPI_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for SerpApiSearchApi {
    fn provider_id(&self) -> &str {
        "serpapi"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "SerpAPI api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        self.rate_limit.wait().await;
        let num = max_results.max(1).min(20).to_string();
        let resp = self
            .client
            .get(SERPAPI_ENDPOINT)
            .query(&[
                ("q", query),
                ("api_key", self.api_key.as_str()),
                ("engine", "google"),
                ("num", num.as_str()),
            ])
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let actionable = match status.as_u16() {
                401 | 403 => Some("key invalid"),
                429 => Some("rate-limited or out of monthly credits"),
                _ => None,
            };
            return Err(ApiError::Storage(if let Some(reason) = actionable {
                format!(
                    "SerpAPI returned HTTP {} ({reason}); \
                     check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            } else {
                format!(
                    "SerpAPI returned HTTP {}: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            }));
        }
        let parsed: SerpApiResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        // SerpAPI returns 200 with `error` populated for plan-side
        // failures (e.g. "Your account has run out of searches").
        // Surface those as the actionable cause, not a silent empty
        // result list.
        if let Some(msg) = parsed.error {
            return Err(ApiError::Storage(format!("SerpAPI: {msg}")));
        }
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap);
        for r in parsed.organic_results.into_iter().take(cap) {
            if r.link.is_empty() || r.title.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: r.title,
                url: r.link,
                snippet: if r.snippet.is_empty() {
                    None
                } else {
                    Some(r.snippet)
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
    fn provider_id_is_serpapi() {
        assert_eq!(SerpApiSearchApi::new("k").provider_id(), "serpapi");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = SerpApiSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parses_canned_serpapi_json_response() {
        let body = r#"{
            "search_metadata": {"id": "abc123"},
            "organic_results": [
                {"title": "First", "link": "https://example.com/a", "snippet": "First snippet", "position": 1},
                {"title": "Second", "link": "https://example.com/b", "snippet": ""},
                {"title": "", "link": "https://example.com/c", "snippet": "title-empty (filtered)"}
            ]
        }"#;
        let parsed: SerpApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.organic_results.len(), 3);
        assert_eq!(parsed.organic_results[0].link, "https://example.com/a");
        assert_eq!(parsed.organic_results[1].snippet, "");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn surfaces_top_level_error_field_from_quota_exhausted_response() {
        let body = r#"{
            "search_metadata": {"id": "abc"},
            "error": "Your account has run out of searches."
        }"#;
        let parsed: SerpApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed.error.as_deref(),
            Some("Your account has run out of searches."),
        );
        assert!(parsed.organic_results.is_empty());
    }
}
