//! Phase C — trajectory sanitization for the auto-capture worker.
//!
//! Before a turn's `(tool_use, tool_result)` chain is shown to the
//! summarizer LLM, every value flows through this pipeline. The job:
//! ensure no credential, token, header, or operator-private string
//! ever lands in a draft skill body. The summarizer's prompt + the
//! resulting skill body are downstream of this sanitizer; the
//! `crate::scanner` module is the LAST defense (rejecting writes
//! that slipped through), but the sanitizer is the FIRST.
//!
//! The pipeline is deterministic, in-process, and pure:
//!
//! 1. **Header strip.** Tool args / results that are JSON objects are
//!    walked for keys named `authorization`, `cookie`, `set-cookie`,
//!    `proxy-authorization`, `x-api-key`, `api-key`, `bearer`,
//!    `*_token`, `*_key`, `*_secret`, `*_password`, `*_credential`
//!    (case-insensitive). Matching values are replaced with the
//!    sentinel `"<<redacted-header>>"`.
//! 2. **Pattern redact.** String values that match the same shapes
//!    `crate::scanner::scan` flags (provider keys, PEM blocks, JWT
//!    shape, URLs with inline credentials) are wholesale replaced
//!    with `"<<redacted-secret>>"`.
//! 3. **High-entropy redact.** Tokens of length ≥ 32 with Shannon
//!    entropy > 4.5 bits/char that aren't sha256/UUID-shaped are
//!    replaced inline with `"<<redacted-entropy>>"`. Keeps the
//!    surrounding prose intact for the summarizer.
//! 4. **Length cap.** Every string field is hard-capped at
//!    [`MAX_FIELD_CHARS`]; longer values are truncated and
//!    suffixed with `"…[truncated]"`. Stops a single 2 MB tool
//!    result from blowing the summarizer's context window.
//!
//! Non-goals: this module does not change the tool name, ordinal,
//! or call/result pairing — those structural fields are needed by
//! the prompt builder. Only the **values** inside arg / result
//! payloads are sanitized.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Hard cap per stringified field, post-sanitization. 4 KB is
/// generous for tool args / results and keeps a 5-tool-call
/// trajectory under ~20 KB even before the summarizer's own
/// truncation.
pub const MAX_FIELD_CHARS: usize = 4096;

/// One sanitized step in the trajectory the prompt builder will
/// stitch together. `tool_name` is preserved because it's part of
/// the procedure being documented; `args_json` and `result_json`
/// are sanitized JSON values; `outcome_ok` mirrors the underlying
/// `Result<_, _>` so the summarizer knows which steps failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SanitizedStep {
    pub ordinal: u32,
    pub tool_name: String,
    pub args_json: JsonValue,
    pub result_json: JsonValue,
    pub outcome_ok: bool,
}

/// Sanitizer counters for telemetry and tests. Every field counts
/// the number of redactions performed in the corresponding category
/// across the whole trajectory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SanitizationReport {
    pub headers_redacted: usize,
    pub patterns_redacted: usize,
    pub entropy_redacted: usize,
    pub fields_truncated: usize,
}

// -----------------------------------------------------------------
// Compiled patterns. Mostly the same set the scanner uses, kept
// independent here so we don't depend on scanner internals (and so
// the sanitizer's behavior is grep-able in isolation).
// -----------------------------------------------------------------

/// Decide whether a JSON key looks like one whose VALUE is a
/// credential. Implemented as a function (not a regex) because the
/// rules — exact keyword + segment-suffix match across `_`/`-`/`.`
/// separators, case-insensitive — are clearer in code than in a
/// single regex alt list, and a few edge cases (`x_api_key`,
/// `db.master.password`) used to slip past the regex.
fn is_suspicious_key(k: &str) -> bool {
    // Normalize separators to `_` and lowercase for segment-by-segment
    // analysis. We match either the full normalized string against a
    // small allowlist, or any segment-suffix against a credential token.
    let normalized: String = k
        .chars()
        .map(|c| {
            if c == '-' || c == '.' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    // Exact full-key matches (HTTP-header shapes the agent shouldn't
    // pass through).
    if matches!(
        normalized.as_str(),
        "authorization" | "proxy_authorization" | "cookie" | "set_cookie" | "bearer"
    ) {
        return true;
    }
    let segments: Vec<&str> = normalized.split('_').filter(|s| !s.is_empty()).collect();
    if let Some(last) = segments.last() {
        // Last segment is a credential noun on its own.
        if matches!(*last, "token" | "secret" | "password" | "credential") {
            return true;
        }
        // `*_key` shapes: only flag when prefixed by a credential
        // noun (api_key, access_key, private_key, secret_key,
        // x_api_key, master_api_key, etc.). A bare "key" or
        // "primary_key" isn't a credential.
        if *last == "key" {
            for seg in &segments[..segments.len() - 1] {
                if matches!(*seg, "api" | "access" | "private" | "secret") {
                    return true;
                }
            }
        }
    }
    false
}

static OPENAI_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bsk-(?:ant-|proj-|or-|live-|test-)?[A-Za-z0-9_-]{20,}\b").unwrap());
static GITHUB_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b").unwrap());
static AWS_ACCESS_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap());
static SLACK_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap());
static GOOGLE_API_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bAIza[0-9A-Za-z_-]{35}\b").unwrap());
static PEM_PRIVATE_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP |ENCRYPTED )?PRIVATE KEY-----").unwrap()
});
static JWT_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b").unwrap()
});
static URL_WITH_CREDS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^/\s:@]+:[^/\s@]+@[^\s]+").unwrap());

static SHA256_HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-fA-F0-9]{64}$").unwrap());
static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$")
        .unwrap()
});
static TOKEN_SPLIT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\s,;<>{}()\[\]\u{2018}\u{2019}\u{201C}\u{201D}'\u{0022}]+").unwrap()
});

const ENTROPY_MIN_LEN: usize = 32;
const ENTROPY_THRESHOLD: f64 = 4.5;

const SENTINEL_HEADER: &str = "<<redacted-header>>";
const SENTINEL_SECRET: &str = "<<redacted-secret>>";
const SENTINEL_ENTROPY: &str = "<<redacted-entropy>>";

/// Sanitize a single `(tool_use, tool_result)` pair.
pub fn sanitize_step(
    ordinal: u32,
    tool_name: &str,
    args_json: &JsonValue,
    result: &Result<JsonValue, String>,
    report: &mut SanitizationReport,
) -> SanitizedStep {
    let args = sanitize_value(args_json, report);
    let (result_json, outcome_ok) = match result {
        Ok(v) => (sanitize_value(v, report), true),
        Err(s) => {
            // Sanitize error strings too — error paths often leak
            // raw API responses verbatim.
            let s = sanitize_string(s, report);
            (JsonValue::String(s), false)
        }
    };
    SanitizedStep {
        ordinal,
        tool_name: tool_name.to_string(),
        args_json: args,
        result_json,
        outcome_ok,
    }
}

fn sanitize_value(v: &JsonValue, report: &mut SanitizationReport) -> JsonValue {
    match v {
        JsonValue::String(s) => JsonValue::String(sanitize_string(s, report)),
        JsonValue::Array(arr) => {
            JsonValue::Array(arr.iter().map(|x| sanitize_value(x, report)).collect())
        }
        JsonValue::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if is_suspicious_key(k) {
                    report.headers_redacted += 1;
                    out.insert(k.clone(), JsonValue::String(SENTINEL_HEADER.into()));
                } else {
                    out.insert(k.clone(), sanitize_value(val, report));
                }
            }
            JsonValue::Object(out)
        }
        // Numbers, bool, null are kept verbatim.
        other => other.clone(),
    }
}

fn sanitize_string(s: &str, report: &mut SanitizationReport) -> String {
    // Pattern pass first — wholesale-replace strings that match a
    // known credential shape. This catches the case where the WHOLE
    // value is a credential (Authorization header value, env-var
    // dump, stringified token).
    if matches_known_credential(s) {
        report.patterns_redacted += 1;
        return SENTINEL_SECRET.into();
    }
    // Per-token pass — replace high-entropy tokens INLINE so the
    // surrounding prose (e.g. "the response was {body, status: 200,
    // x: <token>}") survives for the summarizer.
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut redacted_any = false;
    for sep in TOKEN_SPLIT.find_iter(s) {
        let token = &s[cursor..sep.start()];
        if !token.is_empty() {
            if let Some(replacement) = redact_token_if_secret(token, report) {
                out.push_str(&replacement);
                redacted_any |= replacement == SENTINEL_SECRET || replacement == SENTINEL_ENTROPY;
            } else {
                out.push_str(token);
            }
        }
        out.push_str(&s[sep.start()..sep.end()]);
        cursor = sep.end();
    }
    if cursor < s.len() {
        let token = &s[cursor..];
        if !token.is_empty() {
            if let Some(replacement) = redact_token_if_secret(token, report) {
                out.push_str(&replacement);
                redacted_any |= replacement == SENTINEL_SECRET || replacement == SENTINEL_ENTROPY;
            } else {
                out.push_str(token);
            }
        }
    }
    let _ = redacted_any;
    truncate_if_long(out, report)
}

/// Whole-string credential check.
///
/// Returns true only when the *trimmed* input is itself a single
/// credential value (so an Authorization-header style value like
/// `"Bearer ghp_AbCd..."` returns true via the per-token pass, NOT
/// via this whole-string check, preserving the surrounding prose).
fn matches_known_credential(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let whole = |re: &Regex| -> bool {
        re.find(s)
            .map(|m| m.start() == 0 && m.end() == s.len())
            .unwrap_or(false)
    };
    whole(&OPENAI_KEY)
        || whole(&GITHUB_TOKEN)
        || whole(&AWS_ACCESS_KEY)
        || whole(&SLACK_TOKEN)
        || whole(&GOOGLE_API_KEY)
        || whole(&JWT_TOKEN)
        || whole(&URL_WITH_CREDS)
        // PEM keys span multiple lines — match if the BEGIN header
        // is anywhere; the rest of the body is just material we want
        // to redact wholesale regardless of leading/trailing prose.
        || PEM_PRIVATE_KEY.is_match(s)
}

fn redact_token_if_secret(token: &str, report: &mut SanitizationReport) -> Option<String> {
    // Per-token known-pattern check (catches credentials embedded in
    // larger strings like "Bearer ghp_xxx more text").
    if matches_known_credential(token) {
        report.patterns_redacted += 1;
        return Some(SENTINEL_SECRET.into());
    }
    if token.len() < ENTROPY_MIN_LEN {
        return None;
    }
    if SHA256_HEX.is_match(token) || UUID.is_match(token) {
        return None;
    }
    let punct = token.chars().filter(|c| !c.is_alphanumeric()).count();
    if punct * 4 > token.len() {
        return None;
    }
    if shannon_entropy(token) > ENTROPY_THRESHOLD {
        report.entropy_redacted += 1;
        return Some(SENTINEL_ENTROPY.into());
    }
    None
}

fn shannon_entropy(s: &str) -> f64 {
    use std::collections::HashMap;
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = s.chars().count() as f64;
    if len == 0.0 {
        return 0.0;
    }
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

fn truncate_if_long(s: String, report: &mut SanitizationReport) -> String {
    if s.chars().count() <= MAX_FIELD_CHARS {
        return s;
    }
    report.fields_truncated += 1;
    let mut out: String = s.chars().take(MAX_FIELD_CHARS).collect();
    out.push_str("…[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sanitize_one(
        args: JsonValue,
        result: Result<JsonValue, String>,
    ) -> (SanitizedStep, SanitizationReport) {
        let mut r = SanitizationReport::default();
        let s = sanitize_step(1, "test_tool", &args, &result, &mut r);
        (s, r)
    }

    // --- header strip ---

    #[test]
    fn header_keys_get_value_redacted_regardless_of_inner_shape() {
        let (s, r) = sanitize_one(
            json!({"Authorization": "Bearer abc.def.ghi"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["Authorization"], "<<redacted-header>>");
        assert!(r.headers_redacted >= 1);
    }

    #[test]
    fn header_match_is_case_insensitive_and_handles_dash_underscore_variants() {
        let (s, _) = sanitize_one(
            json!({
                "AUTHORIZATION": "x",
                "X-Api-Key": "y",
                "x_api_key": "z",
                "set-cookie": "session=abc",
                "user_token": "t",
                "stripe_secret": "s",
                "db_password": "p",
            }),
            Ok(json!({})),
        );
        for k in [
            "AUTHORIZATION",
            "X-Api-Key",
            "x_api_key",
            "set-cookie",
            "user_token",
            "stripe_secret",
            "db_password",
        ] {
            assert_eq!(
                s.args_json[k], "<<redacted-header>>",
                "key {k} must be redacted"
            );
        }
    }

    #[test]
    fn non_secret_keys_pass_through() {
        let (s, r) = sanitize_one(
            json!({"name": "alice", "count": 3, "ok": true}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["name"], "alice");
        assert_eq!(s.args_json["count"], 3);
        assert_eq!(s.args_json["ok"], true);
        assert_eq!(r.headers_redacted, 0);
    }

    // --- known pattern redact ---

    #[test]
    fn provider_key_in_string_value_is_wholesale_redacted() {
        let (s, r) = sanitize_one(
            json!({"note": "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["note"], "<<redacted-secret>>");
        assert!(r.patterns_redacted >= 1);
    }

    #[test]
    fn pem_private_key_is_redacted() {
        let (s, _) = sanitize_one(
            json!({"key": "-----BEGIN RSA PRIVATE KEY-----\nMIIE…"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["key"], "<<redacted-secret>>");
    }

    #[test]
    fn jwt_in_value_is_redacted() {
        let (s, _) = sanitize_one(
            json!({"tok": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ4In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["tok"], "<<redacted-secret>>");
    }

    #[test]
    fn credential_embedded_in_prose_is_redacted_inline() {
        // "I authenticated with Bearer ghp_xxxxx and got 200" → the
        // ghp_ token gets replaced inline; surrounding prose survives.
        let (s, r) = sanitize_one(
            json!({
                "log": "I authenticated with Bearer ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234 and got 200"
            }),
            Ok(json!({})),
        );
        let log = s.args_json["log"].as_str().unwrap();
        assert!(!log.contains("ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234"));
        assert!(log.contains("Bearer"));
        assert!(log.contains("got 200"));
        assert!(r.patterns_redacted >= 1);
    }

    #[test]
    fn url_with_inline_creds_is_redacted() {
        let (s, _) = sanitize_one(
            json!({"dsn": "postgres://admin:hunter2@db.internal:5432/app"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["dsn"], "<<redacted-secret>>");
    }

    // --- entropy redact ---

    #[test]
    fn high_entropy_token_is_redacted_inline() {
        let (s, r) = sanitize_one(
            json!({"resp": "X9aB2cD4eF6gH8iJ0kL2mN4oP6qR8sT0uV2wX4yZ6 then succeeded"}),
            Ok(json!({})),
        );
        let resp = s.args_json["resp"].as_str().unwrap();
        assert!(resp.contains("<<redacted-entropy>>"));
        assert!(resp.contains("succeeded"));
        assert!(r.entropy_redacted >= 1);
    }

    #[test]
    fn sha256_and_uuid_pass_through_entropy_check() {
        let (s, r) = sanitize_one(
            json!({
                "sha": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "id": "550e8400-e29b-41d4-a716-446655440000"
            }),
            Ok(json!({})),
        );
        assert_eq!(
            s.args_json["sha"],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(s.args_json["id"], "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(r.entropy_redacted, 0);
    }

    // --- truncation ---

    #[test]
    fn over_length_string_truncates_and_marks() {
        let big = "x".repeat(MAX_FIELD_CHARS + 100);
        let (s, r) = sanitize_one(json!({"body": big}), Ok(json!({})));
        let truncated = s.args_json["body"].as_str().unwrap();
        assert!(truncated.ends_with("…[truncated]"));
        assert!(truncated.chars().count() <= MAX_FIELD_CHARS + 20);
        assert_eq!(r.fields_truncated, 1);
    }

    // --- recursion ---

    #[test]
    fn nested_arrays_and_objects_are_walked() {
        let (s, r) = sanitize_one(
            json!({
                "outer": {
                    "inner": [
                        {"Authorization": "Bearer x"},
                        {"normal": "prose"},
                        "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890"
                    ]
                }
            }),
            Ok(json!({})),
        );
        assert_eq!(
            s.args_json["outer"]["inner"][0]["Authorization"],
            "<<redacted-header>>"
        );
        assert_eq!(s.args_json["outer"]["inner"][1]["normal"], "prose");
        assert_eq!(s.args_json["outer"]["inner"][2], "<<redacted-secret>>");
        assert!(r.headers_redacted >= 1);
        assert!(r.patterns_redacted >= 1);
    }

    // --- result + error ---

    #[test]
    fn err_outcome_string_is_sanitized_and_outcome_flag_false() {
        let (s, _) = sanitize_one(
            json!({}),
            Err("auth failed: token=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890".into()),
        );
        assert!(!s.outcome_ok);
        let err = s.result_json.as_str().unwrap();
        assert!(!err.contains("ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890"));
    }

    #[test]
    fn ok_outcome_passes_outcome_flag_true() {
        let (s, _) = sanitize_one(json!({}), Ok(json!({"ok": true})));
        assert!(s.outcome_ok);
        assert_eq!(s.result_json["ok"], true);
    }

    // --- adversarial ---

    #[test]
    fn url_in_a_field_named_url_is_not_falsely_redacted() {
        // A normal URL value (no inline creds) must pass through —
        // the worker frequently sees URLs as legitimate tool args.
        let (s, r) = sanitize_one(
            json!({"url": "https://api.example.com/v1/items/42"}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["url"], "https://api.example.com/v1/items/42");
        assert_eq!(r.patterns_redacted, 0);
    }

    #[test]
    fn ordinary_prose_is_unchanged() {
        let prose = "The user wants to find a contact named Alice in the address book.";
        let (s, r) = sanitize_one(json!({"intent": prose}), Ok(json!({})));
        assert_eq!(s.args_json["intent"], prose);
        assert_eq!(r, SanitizationReport::default());
    }

    #[test]
    fn null_and_numeric_values_are_preserved() {
        // Pick a fractional value that doesn't trip clippy::approx_constant
        // (3.14 / 3.1415 are flagged as "did you mean PI?" — we just want
        // a non-integer JSON number; the actual value is irrelevant).
        let (s, _) = sanitize_one(
            json!({"x": null, "n": 42, "f": 1.5, "b": false}),
            Ok(json!({})),
        );
        assert_eq!(s.args_json["x"], JsonValue::Null);
        assert_eq!(s.args_json["n"], 42);
        assert_eq!(s.args_json["f"], 1.5);
        assert_eq!(s.args_json["b"], false);
    }
}
