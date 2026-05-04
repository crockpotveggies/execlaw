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
use scraper::{Html, Selector};
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

/// Compile-once CSS selectors for the DDG result list. We address
/// the result block once and pull title/url/snippet from descendants
/// rather than relying on a single regex that requires the snippet
/// to come AFTER the link in source order — DDG occasionally
/// reorders or interleaves trackers between the link and the
/// snippet, which broke the previous regex parser silently.
///
/// 2026-05-03 (rev 5): swapped from `regex::Regex` to `scraper`'s
/// CSS-selector parser over `html5ever`. Robust to whitespace,
/// reorderings, attribute changes, and most class-name drift; a
/// future DDG markup tweak that does invalidate the selectors fails
/// loud (the `parser_handles_live_2026_05_html_capture` regression
/// test catches it before users do).
fn ddg_selectors() -> &'static DdgSelectors {
    static S: OnceLock<DdgSelectors> = OnceLock::new();
    S.get_or_init(DdgSelectors::compile)
}

struct DdgSelectors {
    /// Each result block lives under `div.result`. We anchor on the
    /// block so child queries are scoped — a `result__snippet` from
    /// a different block can't bleed in.
    result: Selector,
    /// Link + title text live in `a.result__a` (href + inner text).
    link: Selector,
    /// Snippet text. DDG's HTML serves this as `a.result__snippet`
    /// today but also occasionally as a `<div>` with the same class
    /// in newer captures — selecting on the class alone keeps both
    /// shapes working.
    snippet: Selector,
}

impl DdgSelectors {
    fn compile() -> Self {
        Self {
            // `Selector::parse` returns a Result; the literals here
            // are guaranteed-valid CSS so the unwrap is safe. Test
            // coverage in this module catches any future typo.
            result: Selector::parse("div.result").expect("static .result selector"),
            link: Selector::parse("a.result__a").expect("static a.result__a selector"),
            snippet: Selector::parse(".result__snippet").expect("static .result__snippet selector"),
        }
    }
}

/// Decode the few entities DDG emits in plain text fragments. The
/// CSS selector path returns inner text via `Element.text()` which
/// already collapses tags but not entities — the parser feeds them
/// through unchanged, so we decode the small fixed set DDG produces
/// here to keep the result clean.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_owned()
}

/// Collapse internal whitespace runs in a text fragment to single
/// spaces. `scraper`'s `text()` iterator already returns nodes
/// trimmed of nothing — runs of newlines / tabs from indented HTML
/// become noise in the snippet field if we don't squeeze them.
fn squeeze_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_ws {
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
///
/// Implementation: scoped CSS-selector walk over `html5ever`'s DOM
/// (via the `scraper` crate). For each `div.result` block we pull
/// the first `a.result__a` (title + href) and the first
/// `.result__snippet` (snippet text). Blocks missing a usable href
/// or title are skipped silently.
#[doc(hidden)]
pub fn parse_ddg_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let sel = ddg_selectors();
    let mut out = Vec::with_capacity(max_results);
    for result_block in document.select(&sel.result) {
        if out.len() >= max_results {
            break;
        }
        let Some(link) = result_block.select(&sel.link).next() else {
            continue;
        };
        let raw_href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };
        let title = decode_entities(&squeeze_whitespace(
            &link.text().collect::<String>(),
        ));
        if title.is_empty() {
            continue;
        }
        let url = unwrap_ddg_redirect(raw_href);
        if url.is_empty() {
            continue;
        }
        let snippet = result_block
            .select(&sel.snippet)
            .next()
            .map(|s| decode_entities(&squeeze_whitespace(&s.text().collect::<String>())))
            .filter(|s| !s.is_empty());
        out.push(SearchResult {
            title,
            url,
            snippet,
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

    /// Drift simulation: exercise variants the regex parser would
    /// have silently swallowed but that the scraper-based parser
    /// must handle.
    ///   1. Snippet served as `<div class="result__snippet">` (DDG
    ///      has occasionally swapped `<a>`-shaped snippets for
    ///      div-shaped ones in newer captures).
    ///   2. Trackers / `<span>`s interleaved between the link and
    ///      the snippet (would break the regex's `.*?` chain).
    ///   3. Extra attributes in arbitrary order on the link element.
    #[test]
    fn parser_handles_markup_drift_variants() {
        let html = r##"
<html><body>
<div class="result">
  <span class="tracker"><img src="t.gif"/></span>
  <a class="result__a" rel="nofollow" data-test="x" href="https://example.com/a">Title A</a>
  <span class="meta">3 days ago</span>
  <div class="result__snippet">Snippet for A served as a div</div>
</div>
<div class="result">
  <a href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fb&amp;rut=zz" class="result__a">Title B</a>
  <a class="result__snippet" href="x">Snippet for B served as an anchor</a>
</div>
</body></html>
"##;
        let results = parse_ddg_html(html, 10);
        assert_eq!(results.len(), 2, "drift variants must both parse");
        assert_eq!(results[0].title, "Title A");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(
            results[0].snippet.as_deref(),
            Some("Snippet for A served as a div"),
        );
        assert_eq!(results[1].title, "Title B");
        assert_eq!(results[1].url, "https://example.com/b");
        assert_eq!(
            results[1].snippet.as_deref(),
            Some("Snippet for B served as an anchor"),
        );
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
    fn decode_entities_handles_common_html_entities() {
        // The new scraper-based parser pulls inner text via
        // `Element.text()` which already drops tag markup;
        // `decode_entities` is the post-processing step that
        // turns the few entities DDG emits back into their literal
        // characters. The old `strip_tags_and_decode` helper went
        // away with the regex parser.
        assert_eq!(
            decode_entities("Tom &amp; Jerry &lt;tag&gt;"),
            "Tom & Jerry <tag>"
        );
        // Bare entities round-trip too.
        assert_eq!(decode_entities("Brian&#39;s page"), "Brian's page");
        assert_eq!(decode_entities("a &nbsp; b"), "a   b");
    }

    #[test]
    fn squeeze_whitespace_collapses_runs_of_indentation() {
        assert_eq!(
            squeeze_whitespace("  hello\n   world  \t\t"),
            "hello world"
        );
        assert_eq!(squeeze_whitespace(""), "");
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
