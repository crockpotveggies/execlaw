//! Perplexity Search API adapter.
//!
//! Perplexity exposes a search-only endpoint (separate from
//! the Sonar chat completions API): it returns ranked URLs +
//! snippets curated for LLM use, without an attached answer
//! summary. That's exactly what the gather phase wants — the
//! synthesizer subagent provides its own summary.
//!
//! Wire format (https://docs.perplexity.ai/api-reference/search-post):
//!
//!   POST https://api.perplexity.ai/search
//!     Header: Authorization: Bearer pplx-<api_key>
//!     Body:   {"query": "...", "max_results": N}
//!
//!   Response: {
//!     "results": [{"title", "url", "snippet", "date"}, ...]
//!   }
//!
//! 2026-05-06.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const PERPLEXITY_ENDPOINT: &str = "https://api.perplexity.ai/search";
const DEFAULT_TIMEOUT_S: u64 = 25;

/// Perplexity's published rate limit varies by plan tier; the
/// free dev key is documented at "around 50 RPM". Conservative
/// 250ms (~4 qps) leaves plenty of headroom even on the free
/// tier and matches the rest of the per-adapter gates so a
/// gather burst doesn't disproportionately favor any provider.
const PERPLEXITY_MIN_REQUEST_GAP: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct PerplexityResponse {
    #[serde(default)]
    results: Vec<PerplexityResult>,
}

#[derive(Debug, Deserialize)]
struct PerplexityResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// Perplexity returns the per-result excerpt as `snippet`. Some
    /// older response variants used `description`; we accept both
    /// via a fallback so a documentation drift doesn't silently
    /// drop snippets.
    #[serde(default, alias = "description")]
    snippet: String,
}

pub struct PerplexitySearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl PerplexitySearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(PERPLEXITY_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for PerplexitySearchApi {
    fn provider_id(&self) -> &str {
        "perplexity"
    }
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "Perplexity api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        self.rate_limit.wait().await;
        let max_results_clamped = max_results.max(1).min(20) as i64;
        let body = serde_json::json!({
            "query": query,
            "max_results": max_results_clamped,
        });
        let resp = self
            .client
            .post(PERPLEXITY_ENDPOINT)
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let actionable = match status.as_u16() {
                401 | 403 => Some("key invalid"),
                429 => Some("rate-limited or quota exhausted"),
                _ => None,
            };
            return Err(ApiError::Storage(if let Some(reason) = actionable {
                format!(
                    "Perplexity returned HTTP {} ({reason}); \
                     check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            } else {
                format!(
                    "Perplexity returned HTTP {}: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )
            }));
        }
        let parsed: PerplexityResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap);
        for r in parsed.results.into_iter().take(cap) {
            if r.url.is_empty() || r.title.is_empty() {
                continue;
            }
            out.push(SearchResult {
                title: r.title,
                url: r.url,
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
    fn provider_id_is_perplexity() {
        assert_eq!(PerplexitySearchApi::new("k").provider_id(), "perplexity");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = PerplexitySearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parses_canned_perplexity_json_response() {
        let body = r#"{
            "results": [
                {"title": "First", "url": "https://example.com/a", "snippet": "First snippet"},
                {"title": "Second", "url": "https://example.com/b", "snippet": ""},
                {"title": "", "url": "https://example.com/c", "snippet": "title-empty (filtered)"}
            ]
        }"#;
        let parsed: PerplexityResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results.len(), 3);
        assert_eq!(parsed.results[0].url, "https://example.com/a");
        assert_eq!(parsed.results[1].snippet, "");
    }

    #[test]
    fn accepts_description_alias_for_snippet() {
        // Older / alternate response shape uses `description`
        // instead of `snippet`. Make sure the alias path works
        // so a doc drift doesn't silently drop snippets.
        let body = r#"{
            "results": [
                {"title": "T", "url": "https://example.com/a", "description": "via alias"}
            ]
        }"#;
        let parsed: PerplexityResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results[0].snippet, "via alias");
    }
}
