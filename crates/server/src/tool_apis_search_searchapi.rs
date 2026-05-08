//! SearchAPI.io search-provider adapter.
//!
//! SearchAPI is a paid wrapper around Google / Bing / DuckDuckGo /
//! YouTube etc. results, similar in shape to SerpAPI but with
//! different pricing tiers (free 100 searches/month at time of
//! writing). The right second-source pick when an operator wants
//! a Google-quality SERP and SerpAPI's quota is exhausted — both
//! providers in the rotation pool gives us double the monthly
//! search budget for free-tier setups.
//!
//! Wire format (https://www.searchapi.io/docs/google):
//!
//!   GET https://www.searchapi.io/api/v1/search
//!     ?engine=google
//!     &q=<query>
//!     &num=N
//!     Header: Authorization: Bearer <api_key>
//!
//!   Response: {
//!     "search_metadata": {...},
//!     "organic_results": [
//!       {"title", "link", "snippet", "position", ...},
//!       ...
//!     ]
//!   }
//!
//! 2026-05-06.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const SEARCHAPI_ENDPOINT: &str = "https://www.searchapi.io/api/v1/search";
const DEFAULT_TIMEOUT_S: u64 = 25;

/// SearchAPI publishes a generous per-second cap on paid plans
/// (5+ qps). The free tier is monthly-credit-limited, no per-
/// second ceiling published. Conservative ~4 qps pacing matches
/// the rest of the SERP-wrapping adapters and keeps gather bursts
/// polite.
const SEARCHAPI_MIN_REQUEST_GAP: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct SearchApiResponse {
    #[serde(default)]
    organic_results: Vec<SearchApiOrganicResult>,
    /// SearchAPI returns 200 with `error` populated for plan-side
    /// failures (quota exhausted, bad engine arg, etc.). Surface
    /// it verbatim so the operator can fix the underlying cause.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchApiOrganicResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
}

pub struct SearchApiSearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl SearchApiSearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(SEARCHAPI_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for SearchApiSearchApi {
    fn provider_id(&self) -> &str {
        "searchapi"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "SearchAPI api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        self.rate_limit.wait().await;
        let num = max_results.max(1).min(20).to_string();
        let resp = self
            .client
            .get(SEARCHAPI_ENDPOINT)
            .query(&[("engine", "google"), ("q", query), ("num", num.as_str())])
            .bearer_auth(&self.api_key)
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
                    "SearchAPI returned HTTP {} ({reason}); \
                     check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            } else {
                format!(
                    "SearchAPI returned HTTP {}: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            }));
        }
        let parsed: SearchApiResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        if let Some(msg) = parsed.error {
            return Err(ApiError::Storage(format!("SearchAPI: {msg}")));
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
    fn provider_id_is_searchapi() {
        assert_eq!(SearchApiSearchApi::new("k").provider_id(), "searchapi");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = SearchApiSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parses_canned_searchapi_json_response() {
        let body = r#"{
            "search_metadata": {"id": "abc"},
            "organic_results": [
                {"title": "First", "link": "https://example.com/a", "snippet": "First snippet", "position": 1},
                {"title": "Second", "link": "https://example.com/b", "snippet": ""},
                {"title": "", "link": "https://example.com/c", "snippet": "title-empty (filtered)"}
            ]
        }"#;
        let parsed: SearchApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.organic_results.len(), 3);
        assert_eq!(parsed.organic_results[0].link, "https://example.com/a");
        assert_eq!(parsed.organic_results[1].snippet, "");
        assert!(parsed.error.is_none());
    }

    #[test]
    fn surfaces_top_level_error_field_on_plan_side_failure() {
        let body = r#"{
            "error": "Monthly searches limit reached"
        }"#;
        let parsed: SearchApiResponse = serde_json::from_str(body).unwrap();
        assert_eq!(
            parsed.error.as_deref(),
            Some("Monthly searches limit reached"),
        );
    }
}
