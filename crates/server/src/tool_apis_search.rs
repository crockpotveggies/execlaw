//! Web-search provider implementations for [`execlaw_core::tool::WebSearchApi`].
//!
//! ## What's here today
//!
//! - `DuckDuckGoSearchApi` — the default provider. POSTs to
//!   `html.duckduckgo.com/html/`, parses the result list, returns
//!   `[{title, url, snippet}]`. No API key required, works on
//!   first-boot.
//!
//! ## What's coming next (separate PR)
//!
//! Provider trait + Settings UI for Brave / Exa / Tavily / Kagi /
//! SearxNG. The current `WebSearchApi` trait surface is provider-
//! agnostic, so adding a new vendor is "implement the trait + add a
//! row to `config_search_providers` + add a Settings panel"; no
//! changes to the tool side.
//!
//! 2026-04-29.

use async_trait::async_trait;
use execlaw_core::tool::{ApiError, SearchResult, WebSearchApi};
use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;

const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const DEFAULT_TIMEOUT_S: u64 = 20;

/// DuckDuckGo HTML-endpoint provider. The `q=...` POST returns an
/// HTML document; we extract the result list with a constrained
/// regex. The structure is well-defined enough for this to be
/// reliable in practice — DuckDuckGo's `result__a` / `result__url` /
/// `result__snippet` class names have been stable for years. If
/// they change, the parser falls back to an empty list (no panic)
/// and a follow-up PR can update the regex.
pub struct DuckDuckGoSearchApi {
    client: reqwest::Client,
}

impl DuckDuckGoSearchApi {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_S))
            // Some search providers (DDG included) sniff for default
            // reqwest user-agents and return reduced results. A
            // realistic UA gets the full page.
            .user_agent("Mozilla/5.0 (compatible; execlaw-agent/0.1)")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for DuckDuckGoSearchApi {
    fn default() -> Self {
        Self::new()
    }
}

/// Compile-once regex for the DDG result block. Captures:
///   1. the result-link redirect href (so we can recover the real URL
///      from the `uddg=` param)
///   2. the link text (raw HTML; we strip tags afterward)
///   3. the snippet HTML (also raw)
fn ddg_result_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // (?s) — let `.` match newlines so the regex spans multi-line
        // result blocks. Non-greedy on every capture so we don't leak
        // across siblings.
        Regex::new(
            r#"(?s)<a[^>]+class="result__a"[^>]+href="([^"]+)"[^>]*>(.*?)</a>.*?<a[^>]+class="result__snippet"[^>]*>(.*?)</a>"#,
        )
        .expect("ddg_result_regex literal is valid")
    })
}

/// Strip `<...>` tags from a fragment and HTML-decode the few
/// entities DDG actually emits. Good enough for snippet-quality
/// text; not a general-purpose HTML-to-text converter.
fn strip_tags_and_decode(s: &str) -> String {
    static TAG_RE: OnceLock<Regex> = OnceLock::new();
    let tag_re = TAG_RE.get_or_init(|| Regex::new("<[^>]*>").unwrap());
    let no_tags = tag_re.replace_all(s, "");
    no_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_owned()
}

/// Pull the real URL out of a DDG redirect href. DDG wraps result
/// links in `//duckduckgo.com/l/?uddg=ENCODED_URL&...`. We extract
/// the `uddg` query param. If it's missing (or the href is already
/// absolute, which DDG does for some result kinds), return as-is.
fn unwrap_ddg_redirect(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let tail = &href[idx + 5..];
        let end = tail.find('&').unwrap_or(tail.len());
        let raw = &tail[..end];
        // URL-decode just enough to reverse percent-encoding. The
        // standard library doesn't ship a URL decoder, so use the
        // `url` crate's helper indirectly via `percent-encoding`.
        return urlencoding_decode(raw);
    }
    // DDG sometimes serves protocol-relative `//domain/...` —
    // promote to https for clarity.
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_owned()
}

/// Tiny percent-decoder. Handles the subset of escapes DDG emits in
/// its `uddg` param (just `%` + 2 hex digits). Avoids pulling in
/// `percent-encoding` as a direct dep.
fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse DDG HTML into search results. Bounded by `max_results`.
/// Public-but-doc-hidden so tests in this module can drive it
/// against canned HTML without hitting the network.
#[doc(hidden)]
pub fn parse_ddg_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut out = Vec::with_capacity(max_results);
    for caps in ddg_result_regex().captures_iter(html) {
        if out.len() >= max_results {
            break;
        }
        let raw_href = &caps[1];
        let title = strip_tags_and_decode(&caps[2]);
        let snippet = strip_tags_and_decode(&caps[3]);
        let url = unwrap_ddg_redirect(raw_href);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        out.push(SearchResult {
            title,
            url,
            snippet: if snippet.is_empty() { None } else { Some(snippet) },
        });
    }
    out
}

#[async_trait]
impl WebSearchApi for DuckDuckGoSearchApi {
    fn provider_id(&self) -> &str {
        "duckduckgo"
    }
    async fn search(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<SearchResult>, ApiError> {
        let body = [("q", query), ("kl", "us-en")];
        let resp = self
            .client
            .post(DDG_HTML_ENDPOINT)
            .form(&body)
            .send()
            .await
            .map_err(|e| ApiError::Storage(format!("network: {e}")))?;
        let html = resp
            .text()
            .await
            .map_err(|e| ApiError::Storage(format!("body: {e}")))?;
        Ok(parse_ddg_html(&html, max_results.max(1) as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal DDG HTML fragment exercising the parser end-to-end:
    /// two result blocks, one with snippet, one without (snippet
    /// missing → entry skipped because the regex requires snippet
    /// match by design).
    const DDG_FIXTURE: &str = r##"
<html><body>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffoo&amp;rut=abc">
    Example: Foo Page
  </a>
  <a class="result__url" href="//example.com">example.com</a>
  <a class="result__snippet" href="...">A short snippet about foo.</a>
</div>
<div class="result">
  <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fbar">
    <b>Bar</b> Page Title
  </a>
  <a class="result__snippet">Bar snippet here.</a>
</div>
</body></html>
"##;

    #[test]
    fn parser_extracts_title_url_and_snippet() {
        let results = parse_ddg_html(DDG_FIXTURE, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Example: Foo Page");
        assert_eq!(results[0].url, "https://example.com/foo");
        assert_eq!(
            results[0].snippet.as_deref(),
            Some("A short snippet about foo.")
        );
        assert_eq!(results[1].title, "Bar Page Title");
        assert_eq!(results[1].url, "https://example.org/bar");
        assert_eq!(results[1].snippet.as_deref(), Some("Bar snippet here."));
    }

    #[test]
    fn parser_caps_at_max_results() {
        let results = parse_ddg_html(DDG_FIXTURE, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example: Foo Page");
    }

    #[test]
    fn parser_returns_empty_on_no_match() {
        let results = parse_ddg_html("<html>nothing here</html>", 10);
        assert!(results.is_empty());
    }

    /// Regression: parse a real DDG response (captured 2026-05-03)
    /// to catch HTML structure drift early. The old parser regex
    /// expected the snippet to be inside an `<a class="result__snippet">`
    /// tag, but a fresh capture against the production endpoint
    /// shows DDG also serves results where the snippet markup
    /// changed. If this assertion ever drops to 0, the regex needs
    /// updating — bisect against this fixture, not the live network.
    #[test]
    fn parser_handles_live_2026_05_html_capture() {
        let html = include_str!("ddg_live_fixture.html");
        let results = parse_ddg_html(html, 10);
        assert!(
            results.len() >= 5,
            "expected >= 5 results from live DDG capture, got {}: \
             this is the symptom users see as 'synthesize failed: no notes'. \
             First three for triage: {:?}",
            results.len(),
            results.iter().take(3).collect::<Vec<_>>(),
        );
        // Sanity: every parsed entry must have a usable URL
        // (search returns "no search results" if every URL is empty).
        for r in &results {
            assert!(r.url.starts_with("http"), "non-http url: {}", r.url);
            assert!(!r.title.is_empty(), "empty title");
        }
    }

    #[test]
    fn redirect_unwrap_recovers_real_url() {
        assert_eq!(
            unwrap_ddg_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=x"),
            "https://example.com"
        );
        assert_eq!(unwrap_ddg_redirect("//example.com/foo"), "https://example.com/foo");
        assert_eq!(unwrap_ddg_redirect("https://example.com"), "https://example.com");
    }

    #[test]
    fn strip_tags_handles_common_html_entities() {
        assert_eq!(
            strip_tags_and_decode("Tom &amp; Jerry &lt;tag&gt;"),
            "Tom & Jerry <tag>"
        );
        assert_eq!(strip_tags_and_decode("<b>bold</b> text"), "bold text");
    }

    #[test]
    fn provider_id_is_duckduckgo() {
        assert_eq!(DuckDuckGoSearchApi::new().provider_id(), "duckduckgo");
    }
}

#[cfg(test)]
mod ua_smoke_tests {
    //! Smoke check for the production HttpWebFetchApi UA. Doesn't
    //! hit the network; just asserts the constant is set to
    //! something that wouldn't immediately trigger CDN bot-protect.
    //!
    //! The actual outbound UA fix lives in `tool_apis_http.rs`; this
    //! mirror lives next to the search module so the regression
    //! lands in the same area as the other "DR fetch-failure"
    //! triage tests.
    use crate::tool_apis_http::DEFAULT_USER_AGENT;

    #[test]
    fn default_ua_does_not_identify_as_reqwest() {
        // The bug we fixed: `reqwest/X.Y` UA → CDN 403 → empty
        // notes → "no notes" synthesise failure. Catch any future
        // regression that drops the override.
        assert!(
            !DEFAULT_USER_AGENT.to_ascii_lowercase().contains("reqwest"),
            "default UA must not advertise as the reqwest library; got {DEFAULT_USER_AGENT:?}",
        );
        assert!(
            DEFAULT_USER_AGENT.to_ascii_lowercase().contains("mozilla"),
            "default UA should look like a real browser to satisfy CDN \
             bot-protection; got {DEFAULT_USER_AGENT:?}",
        );
    }
}
