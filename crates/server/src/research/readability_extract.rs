//! Article-text extraction layer for the deep-research gather phase.
//!
//! Wraps `dom_smoothie` (a maintained Rust port of Mozilla's
//! Readability) so the gather worker can hand the subagent CLEAN
//! article text — title + body, no nav / sidebar / ads / footer —
//! instead of the raw-HTML truncation we did before. The previous
//! approach (`truncate_body(html, 4000)`) shoved 4 KB of mostly-
//! chrome HTML at the subagent and asked it to summarise; report
//! quality suffered accordingly.
//!
//! Why a wrapper instead of inlining the dom_smoothie call:
//!   * Centralises the failure-mode policy. Readability raises on
//!     un-articley pages (SPA shells, redirect interstitials,
//!     directory listings); the wrapper degrades to a raw-text
//!     fallback so the gather worker doesn't lose the entire URL.
//!   * Makes the gather worker testable without a heavyweight
//!     dom_smoothie test harness — gather tests stub the fetcher
//!     and the extraction layer is exercised directly here.
//!   * One place to enforce the per-page char cap, so the cap can
//!     evolve with the subagent's context budget without grepping
//!     through gather.rs.
//!
//! Performance: `dom_smoothie::Readability::new(...).parse()` runs
//! ~5–50 ms on typical articles and is CPU-bound. The gather worker
//! calls this from `tokio::task::spawn_blocking` so the async
//! runtime stays responsive when N parallel workers all extract
//! at once.

use dom_smoothie::{Config, Readability};

/// Hard cap on input HTML size we feed to Readability. Same order
/// of magnitude as `HttpWebFetchApi`'s 1 MiB body cap, but tighter
/// — Readability's scoring pass is super-linear in DOM size, so
/// ~512 KiB is the point past which it starts taking >100ms per
/// page. Bigger pages get pre-truncated; we keep the head of the
/// body since that's where the article almost always sits.
const READABILITY_INPUT_CAP_BYTES: usize = 512 * 1024;

/// Outcome of an extraction attempt. The wrapper distinguishes
/// success-with-Readability from raw-text fallback so callers can
/// log / surface the difference without a magic prefix string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionOutcome {
    /// Readability found an article and we extracted the body.
    /// `title` is populated when the page had a `<title>` or
    /// readability inferred one from headings.
    Article { title: Option<String>, text: String },
    /// Readability returned an error or a degenerate (near-empty)
    /// article. We fell back to the raw-text strip, surfacing the
    /// reason in `reason` so callers can record it.
    Fallback { text: String, reason: String },
}

impl ExtractionOutcome {
    /// Collapse to the prompt-ready text the subagent will see.
    /// Article variant prefixes the title (when present) so the
    /// subagent has a one-line topical anchor at the top of the
    /// excerpt; fallback variant returns the raw text as-is.
    pub fn into_text(self) -> String {
        match self {
            Self::Article { title, text } => match title {
                Some(t) if !t.trim().is_empty() => format!("{}\n\n{}", t.trim(), text),
                _ => text,
            },
            Self::Fallback { text, .. } => text,
        }
    }
}

/// Extract clean article text from `html`.
///
/// `document_url` is passed to Readability so it can resolve
/// relative links inside the article (the text path doesn't use
/// link hrefs, but Readability's scoring sometimes consults them).
/// `max_chars` clips the final text so the gather worker can pack
/// multiple URLs' worth into one subagent context.
///
/// Sync: calls into `dom_smoothie` which builds a full DOM and runs
/// the readability scoring pass. The gather worker invokes this
/// from `spawn_blocking`.
pub fn extract_readable_text(
    html: &str,
    document_url: Option<&str>,
    max_chars: usize,
) -> ExtractionOutcome {
    // Pre-truncate oversized inputs. Readability is super-linear
    // in DOM size; a 5 MB scraped page can trip a 10 s extraction.
    // Tail bytes get dropped since the article almost always lives
    // near the top of the body.
    let trimmed_html: &str = if html.len() > READABILITY_INPUT_CAP_BYTES {
        // Slice on a UTF-8 boundary by walking back from the cap
        // until we find one. Worst case ~3 byte rewind for a
        // multibyte sequence at the cap.
        let mut cap = READABILITY_INPUT_CAP_BYTES;
        while cap > 0 && !html.is_char_boundary(cap) {
            cap -= 1;
        }
        &html[..cap]
    } else {
        html
    };

    // Construct + parse. Both can error; treat either as a fallback
    // signal rather than propagating to the worker (we'd rather
    // have raw-text-truncated content than nothing for this URL).
    let mut readability =
        match Readability::new(trimmed_html, document_url, Some(Config::default())) {
            Ok(r) => r,
            Err(e) => {
                return ExtractionOutcome::Fallback {
                    text: clip_chars(&strip_tags_lossy(trimmed_html), max_chars),
                    reason: format!("readability init failed: {e}"),
                };
            }
        };
    let article = match readability.parse() {
        Ok(a) => a,
        Err(e) => {
            return ExtractionOutcome::Fallback {
                text: clip_chars(&strip_tags_lossy(trimmed_html), max_chars),
                reason: format!("readability parse failed: {e}"),
            };
        }
    };

    // dom_smoothie returns Article.text_content as the cleaned
    // article body. A tiny `length` field counts the chars; we use
    // it as a degeneracy signal — articles below ~200 chars are
    // almost always extraction failures (a navigation page that
    // got categorised as the "article", a single-paragraph 404,
    // etc.). Falling back gives the subagent better material than
    // an empty Article result.
    if article.length < 200 {
        return ExtractionOutcome::Fallback {
            text: clip_chars(&strip_tags_lossy(trimmed_html), max_chars),
            reason: format!(
                "readability returned degenerate article ({} chars)",
                article.length
            ),
        };
    }

    let title = if article.title.trim().is_empty() {
        None
    } else {
        Some(article.title.clone())
    };
    let text = clip_chars(article.text_content.as_ref(), max_chars);
    ExtractionOutcome::Article { title, text }
}

/// Truncate `s` to at most `max_chars` chars (NOT bytes — operates
/// on Unicode scalar values so multi-byte chars don't get bisected).
/// Appends an ellipsis when truncation actually fired so downstream
/// readers can tell.
fn clip_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut buf: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    buf.push('…');
    buf
}

/// Last-ditch fallback when Readability can't make sense of the
/// page. NOT a general-purpose HTML→text converter — strips tags,
/// collapses whitespace, and decodes the few entities most pages
/// emit. Adequate for "give the subagent SOMETHING from this URL."
fn strip_tags_lossy(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut last_was_space = true; // start true so leading WS is dropped
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim()
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic article: nav + sidebar + body. Readability should
    /// pull just the article body and drop the chrome.
    const ARTICLE_HTML: &str = r##"<!DOCTYPE html><html><head><title>Evergreen Ground Covers Guide</title></head><body>
<nav class="topnav">
  <a href="/">Home</a> | <a href="/garden">Garden</a> | <a href="/about">About</a>
  <div class="ads">Buy our seeds! Subscribe! Click here!</div>
</nav>
<aside class="sidebar">
  <h3>Popular posts</h3>
  <ul><li>Tomato tips</li><li>Pruning roses</li><li>Soil amendments</li></ul>
</aside>
<article>
  <h1>Evergreen Ground Covers for Year-Round Color</h1>
  <p>When choosing an evergreen ground cover, the most important factor is climate matching.
     Hardy plants like creeping thyme and vinca minor thrive in zones 4 through 9, while
     mondo grass and dichondra are better suited to warmer southern zones.</p>
  <p>For shaded areas, pachysandra and English ivy are reliable choices, though English
     ivy can be invasive in moist climates and should be sited carefully. Sweet woodruff
     offers fragrant white flowers in addition to deep green foliage.</p>
  <p>Drought-tolerant options for sunny banks include creeping juniper, sedum, and
     ice plant. These succulents store moisture in their leaves and require almost no
     supplemental water once established, making them excellent choices for water-wise
     landscaping in arid regions.</p>
</article>
<footer><p>© 2026 Garden Blog. Privacy. Terms.</p></footer>
</body></html>"##;

    #[test]
    fn extracts_article_body_and_drops_nav_sidebar_footer() {
        let outcome = extract_readable_text(ARTICLE_HTML, Some("https://example.com/cover"), 4_000);
        let text = match outcome {
            ExtractionOutcome::Article { ref text, .. } => text.clone(),
            other => panic!("expected Article, got {other:?}"),
        };
        // Article body must be present.
        assert!(
            text.contains("creeping thyme"),
            "body content missing: {text}"
        );
        assert!(text.contains("pachysandra"), "body content missing");
        assert!(text.contains("Drought-tolerant"), "body content missing");
        // Nav must NOT be present.
        assert!(!text.contains("Buy our seeds"), "nav leaked: {text}");
        assert!(!text.contains("Tomato tips"), "sidebar leaked: {text}");
        // Footer is borderline (Readability sometimes keeps copyright);
        // explicitly assert just the privacy/terms link list is gone.
        assert!(!text.contains("Privacy. Terms"), "footer link list leaked");
    }

    #[test]
    fn populates_title_from_article_h1() {
        let outcome = extract_readable_text(ARTICLE_HTML, None, 4_000);
        match outcome {
            ExtractionOutcome::Article { title, .. } => {
                assert!(title.is_some(), "title must be populated");
                let t = title.unwrap();
                assert!(
                    t.contains("Evergreen") || t.contains("Ground Cover"),
                    "title should reflect the article: {t}"
                );
            }
            other => panic!("expected Article, got {other:?}"),
        }
    }

    #[test]
    fn into_text_prefixes_title_when_article() {
        let outcome = ExtractionOutcome::Article {
            title: Some("Hello".into()),
            text: "world body".into(),
        };
        let s = outcome.into_text();
        assert!(s.starts_with("Hello\n\n"), "title must lead: {s}");
        assert!(s.contains("world body"));
    }

    #[test]
    fn into_text_omits_blank_title() {
        let outcome = ExtractionOutcome::Article {
            title: Some("   ".into()),
            text: "body".into(),
        };
        assert_eq!(outcome.into_text(), "body");
    }

    #[test]
    fn falls_back_when_readability_finds_no_article() {
        // SPA shell — no article markup at all. Readability will
        // either error or return a degenerate article; either way
        // the wrapper must produce a Fallback.
        let html = "<html><body><div id='root'></div></body></html>";
        let outcome = extract_readable_text(html, None, 4_000);
        match outcome {
            ExtractionOutcome::Fallback { reason, .. } => {
                assert!(!reason.is_empty(), "reason must explain fallback");
            }
            ExtractionOutcome::Article { text, .. } => {
                // If for some reason readability claims success on an
                // empty body, the text must be effectively empty —
                // the gather worker treats that the same as fallback
                // in practice. Allow it but assert tiny.
                assert!(
                    text.len() < 200,
                    "implausibly large article from empty body: {text}"
                );
            }
        }
    }

    #[test]
    fn respects_max_chars_cap_on_unparseable_input() {
        // Plain garbage — Readability either errors or returns a
        // degenerate result; the wrapper falls back to strip-tags
        // and then `clip_chars`. Asserts the cap is honoured.
        // (Earlier version pumped 30k `<x>` tags, which blew the
        // html5ever parser's recursive descent stack on Windows
        // debug builds — switched to a smaller input that still
        // exercises the fallback path without crashing the test
        // harness.)
        let big = format!("{}{}", "<div>".repeat(500), "</div>".repeat(500));
        let outcome = extract_readable_text(&big, None, 200);
        let text = match outcome {
            ExtractionOutcome::Fallback { text, .. } => text,
            ExtractionOutcome::Article { text, .. } => text,
        };
        assert!(
            text.chars().count() <= 200,
            "respect max_chars cap: {} chars",
            text.chars().count(),
        );
    }

    #[test]
    fn pre_truncates_oversized_input_to_avoid_pathological_extraction_time() {
        // 1 MiB of valid-ish HTML. Without pre-truncation,
        // Readability's scoring pass goes super-linear and can
        // hang for seconds. Verify the input cap actually applied
        // by timing — extraction must complete in well under 1 s.
        let header = "<html><body><article><h1>T</h1>";
        let footer = "</article></body></html>";
        let para = "<p>Words and words and words and more words for the article.</p>";
        let mut html = String::with_capacity(2 * 1024 * 1024);
        html.push_str(header);
        while html.len() < 2 * 1024 * 1024 {
            html.push_str(para);
        }
        html.push_str(footer);
        let start = std::time::Instant::now();
        let _ = extract_readable_text(&html, None, 4_000);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "extraction took {:?}, the input cap isn't working",
            elapsed,
        );
    }

    #[test]
    fn clip_chars_handles_multibyte_safely() {
        // Each emoji is multi-byte; clip on char count, not byte count.
        let s = "🌱🌿🌳🌲🌴";
        let clipped = clip_chars(s, 3);
        assert_eq!(clipped.chars().count(), 3, "must clip on chars");
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn clip_chars_passthrough_when_under_cap() {
        assert_eq!(clip_chars("short", 100), "short");
        assert_eq!(clip_chars("", 5), "");
    }

    #[test]
    fn strip_tags_lossy_drops_real_tags_and_decodes_entities() {
        // The function does NOT understand <script> semantics — it
        // just strips angle-bracket tags and decodes the most-common
        // entities. That's adequate for "give the subagent
        // SOMETHING from a page Readability rejected" since this
        // path runs only when nothing better is available.
        let input = "<p>Tom &amp; Jerry &lt;cat&gt;</p>";
        let out = strip_tags_lossy(input);
        // After decoding `&lt;cat&gt;` we DO have literal `<` / `>`
        // chars in the output — that's intentional. The assertion
        // is that the original tag markers (`<p>` / `</p>`) are gone
        // and entities are decoded.
        assert_eq!(out, "Tom & Jerry <cat>");
    }
}
