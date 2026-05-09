//! Shared text-extraction utilities used by every adapter impl.
//!
//! All functions are pure, deterministic, no I/O. Heavy use of
//! pre-compiled regexes via `once_cell::sync::Lazy`.

use once_cell::sync::Lazy;
use regex::Regex;

/// Split a string into `(reasoning, visible)` by stripping
/// `<think>...</think>` blocks. Multiple blocks are concatenated
/// into the reasoning portion (separated by `\n\n`); everything
/// outside the blocks lands in `visible`.
///
/// If no `<think>` tag is present, returns `(None, original)`.
/// Streaming is NOT handled here — callers using streaming should
/// continue to use `crate::think_filter::ThinkBlockFilter` (in
/// `execlaw-server`) which tracks state across chunks.
pub fn split_think_block(s: &str) -> (Option<String>, String) {
    static THINK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<think>(.*?)</think>").unwrap());
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut visible = String::with_capacity(s.len());
    let mut last = 0usize;
    for cap in THINK.captures_iter(s) {
        let mat = cap.get(0).unwrap();
        let inner = cap.get(1).unwrap().as_str().trim();
        if !inner.is_empty() {
            reasoning_parts.push(inner.to_string());
        }
        visible.push_str(&s[last..mat.start()]);
        last = mat.end();
    }
    if last == 0 {
        return (None, s.to_string());
    }
    visible.push_str(&s[last..]);
    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n\n"))
    };
    (reasoning, visible)
}

/// Strip Qwen3.5's literal "Thinking Process:" preamble.
///
/// The block typically ends at the first blank line followed by the
/// actual structured output, OR at the first `{` if structured. To
/// stay simple + safe across cases, we use the heuristic: find the
/// first occurrence after a `Thinking Process:` line and slice from
/// the next blank line; if no blank line, slice from the first
/// occurrence of `{`, `[`, `"`, or a markdown header marker `#`.
///
/// Returns the stripped text. If no preamble is detected, returns
/// the input unchanged.
pub fn strip_thinking_process_preamble(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let needle = "thinking process:";
    let Some(start) = lower.find(needle) else {
        return s.to_string();
    };
    // Only strip if it's at the start (after whitespace) or after a
    // newline — avoid mauling text that legitimately mentions the
    // phrase mid-prose.
    if start > 0 {
        let before = &s[..start];
        if !before
            .chars()
            .rev()
            .take_while(|c| *c != '\n')
            .all(|c| c.is_whitespace())
        {
            return s.to_string();
        }
    }
    let after = &s[start + needle.len()..];
    // Prefer a blank-line break.
    if let Some(idx) = after.find("\n\n") {
        return after[idx + 2..].trim_start().to_string();
    }
    // Fallback: first structural marker.
    let mut best: Option<usize> = None;
    for marker in ['{', '[', '#'] {
        if let Some(p) = after.find(marker) {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
    }
    if let Some(p) = best {
        return after[p..].to_string();
    }
    after.trim_start().to_string()
}

/// Strip a leading ```` ```json ```` (or bare ```` ``` ````) fence
/// + matching trailing fence. Returns input verbatim if no fence is
/// detected.
pub fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim();
    let mut working = trimmed.to_string();
    // Common leading fences. `\n`-prefix variants caught by the
    // trim above; case-insensitive lang hint normalized to lowercase.
    for prefix in [
        "```json", "```JSON", "```Json", "```yaml", "```YAML", "```text", "```",
    ] {
        if let Some(rest) = working.strip_prefix(prefix) {
            working = rest.trim_start().to_string();
            break;
        }
    }
    if let Some(end) = working.rfind("```") {
        working.truncate(end);
    }
    working.trim().to_string()
}

/// Extract a balanced JSON object substring starting at the FIRST
/// `{` whose matching `}` makes the slice parse as `T`. Returns
/// `None` if no candidate parses.
///
/// Designed for cases where a model emits prose THEN JSON (or JSON
/// with `}` characters inside string literals). Handles escape
/// sequences and quoted-string state correctly.
pub fn find_balanced_json<T: serde::de::DeserializeOwned>(s: &str) -> Option<String> {
    for (pos, _) in s.match_indices('{') {
        if let Some(end) = matching_brace_end(s, pos) {
            let slice = &s[pos..=end];
            if serde_json::from_str::<T>(slice).is_ok() {
                return Some(slice.to_string());
            }
        }
    }
    None
}

fn matching_brace_end(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Plan {
        #[allow(dead_code)]
        thesis: String,
    }

    // --- think block ---

    #[test]
    fn split_think_block_extracts_single_block() {
        let (r, v) = split_think_block("<think>I should plan</think>The answer is 42.");
        assert_eq!(r.as_deref(), Some("I should plan"));
        assert_eq!(v, "The answer is 42.");
    }

    #[test]
    fn split_think_block_handles_multiple_blocks() {
        let (r, v) =
            split_think_block("<think>step 1</think>visible<think>step 2</think>more visible");
        assert_eq!(r.as_deref(), Some("step 1\n\nstep 2"));
        assert_eq!(v, "visiblemore visible");
    }

    #[test]
    fn split_think_block_passthrough_when_no_tag() {
        let (r, v) = split_think_block("plain text");
        assert!(r.is_none());
        assert_eq!(v, "plain text");
    }

    #[test]
    fn split_think_block_handles_multiline() {
        let s = "<think>line one\nline two\nline three</think>after";
        let (r, v) = split_think_block(s);
        assert!(r.unwrap().contains("line two"));
        assert_eq!(v, "after");
    }

    // --- preamble ---

    #[test]
    fn strip_thinking_process_preamble_with_blank_line_break() {
        let s = "Thinking Process:\n1. think\n2. plan\n\n{\"x\":1}";
        assert_eq!(strip_thinking_process_preamble(s), "{\"x\":1}");
    }

    #[test]
    fn strip_thinking_process_preamble_with_brace_marker() {
        let s = "Thinking Process: weighing options before {\"x\":1}";
        assert_eq!(strip_thinking_process_preamble(s), "{\"x\":1}");
    }

    #[test]
    fn strip_thinking_process_preamble_passthrough_when_mid_prose() {
        // Don't maul a sentence that legitimately mentions the phrase.
        let s = "The user described their thinking process: it was iterative.";
        assert_eq!(strip_thinking_process_preamble(s), s);
    }

    // --- fences ---

    #[test]
    fn strip_code_fences_removes_lang_hint_pair() {
        assert_eq!(strip_code_fences("```json\n{\"x\":1}\n```"), "{\"x\":1}");
    }

    #[test]
    fn strip_code_fences_removes_bare_pair() {
        assert_eq!(strip_code_fences("```\nplain\n```"), "plain");
    }

    #[test]
    fn strip_code_fences_passthrough_when_absent() {
        assert_eq!(strip_code_fences("no fence"), "no fence");
    }

    // --- balanced json ---

    #[test]
    fn find_balanced_json_picks_first_valid_object() {
        let s = r#"prelude {bad} more {"thesis":"t"} trailing"#;
        let got = find_balanced_json::<Plan>(s).unwrap();
        assert_eq!(got, r#"{"thesis":"t"}"#);
    }

    #[test]
    fn find_balanced_json_handles_strings_with_escaped_braces() {
        let s = r#"{"thesis":"contains \"} fake close\" and continues"}"#;
        let got = find_balanced_json::<Plan>(s);
        assert!(got.is_some());
    }

    #[test]
    fn find_balanced_json_handles_nested_braces() {
        let s = r#"prefix {"thesis":"t","extra":{"a":1,"b":{"c":2}}} suffix"#;
        let got = find_balanced_json::<Plan>(s).unwrap();
        assert_eq!(got, r#"{"thesis":"t","extra":{"a":1,"b":{"c":2}}}"#);
    }

    #[test]
    fn find_balanced_json_returns_none_when_nothing_parses() {
        let s = "no braces at all";
        assert!(find_balanced_json::<Plan>(s).is_none());
    }
}
