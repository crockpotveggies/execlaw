//! Exa search-provider adapter.
//!
//! Exa (formerly Metaphor) is a neural / semantic web-search API
//! built for AI agents. Returns higher-signal results than
//! keyword-only engines for natural-language research queries —
//! the gather phase's planner-emitted sub-queries tend to fall
//! into that shape, so Exa pairs well with deep research.
//!
//! Wire format (https://exa.ai/docs/reference/search):
//!
//!   POST https://api.exa.ai/search
//!   Header: x-api-key: <api_key>
//!   Body:   {"query": "...", "numResults": N, "type": "auto"}
//!
//!   Response: {
//!     "results": [
//!       {"title": "...", "url": "...", "text": "..." (when contents requested)},
//!       ...
//!     ],
//!     "requestId": "...",
//!     "costDollars": {...}
//!   }
//!
//! We request `contents.text` so the response carries a snippet —
//! without it the gather worker would have to re-fetch each URL
//! to get any text, defeating the point of using Exa over a raw
//! crawler. Exa charges a small per-content fee on top of the
//! per-search fee for this; the trade-off is worth it because the
//! returned text is already extracted (no readability pass needed
//! on top).
//!
//! 2026-05-04.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use serde::Deserialize;
use std::time::Duration;

const EXA_ENDPOINT: &str = "https://api.exa.ai/search";
const DEFAULT_TIMEOUT_S: u64 = 25;

/// Exa doesn't publish a hard per-second cap but charges per query
/// + per content character — bursting 3 parallel calls can both
/// rate-limit AND multiply spend unnecessarily. 250 ms gap (~4 qps)
/// keeps within polite-client territory while letting a 7-step
/// gather complete in under 2 s of search wall-time.
const EXA_MIN_REQUEST_GAP: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct ExaResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    /// When `contents.text` is requested in the body, Exa returns
    /// the extracted page text under `text`. We trim + use it as
    /// the snippet. Falls back to the optional `highlights` field
    /// on results where the full text wasn't returned.
    #[serde(default)]
    text: String,
    #[serde(default)]
    highlights: Vec<String>,
}

pub struct ExaSearchApi {
    client: reqwest::Client,
    api_key: String,
    rate_limit: crate::search_rate_limit::RateLimitGate,
}

impl ExaSearchApi {
    pub fn new(api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            .user_agent(crate::tool_apis_http::DEFAULT_USER_AGENT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(EXA_MIN_REQUEST_GAP),
        }
    }

    pub fn with_client(api_key: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            rate_limit: crate::search_rate_limit::RateLimitGate::new(EXA_MIN_REQUEST_GAP),
        }
    }
}

#[async_trait]
impl WebSearchApi for ExaSearchApi {
    fn provider_id(&self) -> &str {
        "exa"
    }
    async fn search(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<SearchResult>, ApiError> {
        if self.api_key.is_empty() {
            return Err(ApiError::Validation(
                "Exa api_key is empty; configure it in Settings → Search".into(),
            ));
        }
        self.rate_limit.wait().await;
        let num_results = max_results.max(1).min(100) as i64;
        // `type: "auto"` lets Exa pick neural vs keyword per query.
        // `contents.text.maxCharacters` keeps the per-result body
        // small enough that a 7-step plan doesn't drown the
        // dispatcher in payload — the readability pass happens in
        // gather either way.
        let body = serde_json::json!({
            "query": query,
            "numResults": num_results,
            "type": "auto",
            "contents": {
                "text": {"maxCharacters": 1500},
                "highlights": {"numSentences": 2, "highlightsPerUrl": 2},
            },
        });
        let resp = self
            .client
            .post(EXA_ENDPOINT)
            .header("x-api-key", &self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 401/403 → bad/missing key. Surface the actionable
            // cause rather than a generic HTTP error so the
            // operator knows to fix the key, not chase a
            // network-layer red herring.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(ApiError::Storage(format!(
                    "Exa returned HTTP {} (key invalid or quota exhausted); \
                     check Settings → Search. Body: {}",
                    status.as_u16(),
                    truncate(&body, 200),
                )));
            }
            return Err(ApiError::Storage(format!(
                "Exa returned HTTP {}: {}",
                status.as_u16(),
                truncate(&body, 200),
            )));
        }
        let parsed: ExaResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Storage(format!("parsing JSON response: {e}")))?;
        let cap = max_results.max(1) as usize;
        let mut out = Vec::with_capacity(cap);
        for r in parsed.results.into_iter().take(cap) {
            if r.url.is_empty() || r.title.is_empty() {
                continue;
            }
            // Pick the best snippet source available: full text >
            // joined highlights > nothing. Trim aggressively so
            // the search-result list stays light — gather's
            // readability pass on the full URL is the canonical
            // text source; the snippet is just a "is this URL
            // worth fetching" hint for the gather worker.
            let snippet = if !r.text.is_empty() {
                Some(snippet_from_text(&r.text, 280))
            } else if !r.highlights.is_empty() {
                Some(r.highlights.join(" … "))
            } else {
                None
            };
            out.push(SearchResult {
                title: r.title,
                url: r.url,
                snippet,
            });
        }
        Ok(out)
    }
}

/// Trim a multiline body to a single-line snippet bounded by
/// `max_chars`. Whitespace runs collapse to single spaces.
fn snippet_from_text(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars);
    let mut last_was_ws = false;
    for ch in s.chars() {
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
        if ch.is_whitespace() {
            if !last_was_ws && !out.is_empty() {
                out.push(' ');
                last_was_ws = true;
            }
        } else {
            out.push(ch);
            last_was_ws = false;
        }
    }
    out.trim().to_owned()
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
    fn provider_id_is_exa() {
        assert_eq!(ExaSearchApi::new("k").provider_id(), "exa");
    }

    #[tokio::test]
    async fn empty_key_returns_validation_error_not_panic() {
        let api = ExaSearchApi::new("");
        let err = api.search("anything", 10).await.unwrap_err();
        match err {
            ApiError::Validation(msg) => assert!(msg.contains("api_key")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn parses_canned_exa_json_response_with_text_and_highlights() {
        let body = r#"{
            "results": [
                {"title": "Full text result", "url": "https://example.com/a", "text": "Long extracted content that needs to be trimmed to a sane snippet length but is still useful to the gather worker."},
                {"title": "Highlights only", "url": "https://example.com/b", "highlights": ["First salient sentence.", "Second salient sentence."]},
                {"title": "", "url": "https://example.com/c", "text": "title-empty (skip)"},
                {"title": "URL empty", "url": "", "text": "url-empty (skip)"}
            ]
        }"#;
        let parsed: ExaResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.results.len(), 4);
        assert!(parsed.results[0].text.contains("Long extracted"));
        assert_eq!(parsed.results[1].highlights.len(), 2);
    }

    #[test]
    fn snippet_from_text_collapses_whitespace_and_truncates_at_cap() {
        let s = "Hello\n\n  world  with    long whitespace runs and a tail beyond the cap";
        let out = snippet_from_text(s, 30);
        assert!(out.chars().count() <= 31, "should respect cap (+ellipsis): {out}");
        assert!(!out.contains("\n"), "should drop newlines: {out}");
        // Multiple spaces collapse.
        assert!(!out.contains("  "));
    }

    #[test]
    fn snippet_from_text_passthrough_when_under_cap() {
        assert_eq!(snippet_from_text("short", 100), "short");
        assert_eq!(snippet_from_text("", 100), "");
    }
}
