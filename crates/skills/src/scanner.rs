//! Deterministic in-process secret scanner for skill writes.
//!
//! The job: prevent the *common, accidental* cases of operators or
//! agents pasting credentials into skill bodies. Determined exfiltration
//! is properly the threat model for the vault and the audit log, not
//! this module — see the `What this is NOT` section.
//!
//! ## Pipeline
//!
//! Every text field on every write goes through this pipeline in order:
//!
//! 1. **Vault-syntax extraction** — `{{vault:name}}` references are
//!    pulled out and replaced with placeholders BEFORE any other check.
//!    Legitimate secret references are invisible to the rest of the
//!    scanner. The vault-resolved value is never passed through the
//!    scanner.
//! 2. **Known-pattern matcher** — a precompiled `regex::RegexSet` of
//!    well-known credential shapes (provider API keys, PEM private
//!    keys, JWTs, URLs with inline credentials).
//! 3. **High-entropy heuristic** — for every token of length ≥ 32, we
//!    compute Shannon entropy. Tokens > 4.5 bits/char are flagged
//!    *unless* they match an allowlist shape (sha256 hex, UUID).
//! 4. **Frontmatter key heuristic** — walks the parsed JSON looking for
//!    keys that smell like credentials (`token`, `key`, `secret`,
//!    `password`, `credential`, `api_key`) carrying non-trivial string
//!    values.
//!
//! ## Verdict
//!
//! Returns [`ScanVerdict::Clean`] or [`ScanVerdict::Suspicious`] with a
//! list of [`Finding`]s. Each finding carries its `Severity` (Block /
//! Warn) and the [`Strictness`] mode the caller passed determines
//! whether `Warn`-severity findings collapse to a block or just log.
//!
//! Defaults locked at 2026-05-02:
//!   * `Strict` for `agent` and `plugin` writers (block on any finding)
//!   * `Warn`   for `admin` writers (write proceeds; admin can override)
//!
//! ## What this is NOT
//!
//! - A malware scanner.
//! - A judgment call from an LLM. Pure regex + entropy + JSON walk.
//! - A network call. Runs in microseconds, in-process, deterministically.
//! - Foolproof against adversarial encoding (base64, custom formats,
//!   tool composition that produces secrets at runtime). Those are
//!   threats the vault and audit log handle.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Strictness mode applied at scan time. Determined by who's writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// Any finding (Block OR Warn severity) collapses to a Suspicious
    /// verdict. Agent and plugin writes use this by default.
    Strict,
    /// Only Block-severity findings collapse to Suspicious. Warn-only
    /// findings still appear in the verdict's findings list (so the
    /// caller can log them) but the overall verdict is Clean. Admin
    /// writes use this by default.
    Warn,
}

/// Severity attached to each finding, evaluated against the caller's
/// `Strictness` to compute the overall verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Always blocks under any strictness. Provider keys, PEM private
    /// keys, JWTs, inline-credential URLs.
    Block,
    /// Blocks under `Strict`; logged-only under `Warn`. High-entropy
    /// tokens, suspicious frontmatter keys.
    Warn,
}

/// A single detection. The `matched` field carries a *redacted*
/// preview, never the raw value, so audit logs can be safely shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub field: String,
    pub span: (usize, usize),
    /// Redacted preview suitable for logs. Format: first 4 chars +
    /// "...***...({n} chars)".
    pub matched: String,
    pub severity: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// One of the precompiled known-credential regexes hit.
    KnownPattern,
    /// A token with high Shannon entropy that didn't match the allowlist.
    HighEntropy,
    /// A URL with embedded `user:pass@host` credentials.
    InlineCredential,
    /// A frontmatter JSON key that looks like a credential carrying a
    /// non-trivial string value.
    SuspiciousFrontmatterKey,
}

/// Input bundle passed to [`scan`]. All fields are borrowed so the
/// caller doesn't have to own the data twice.
pub struct ScanInput<'a> {
    pub body_md: &'a str,
    pub description: &'a str,
    pub frontmatter_json: &'a str,
    pub resources: &'a [(String, &'a [u8])],
}

/// Verdict returned by [`scan`].
///
/// - `Clean { warnings }` — the caller may proceed. `warnings` carries
///   any Warn-severity findings that did NOT block under the caller's
///   strictness mode, so the caller can still emit them to the audit
///   trail. Empty when the scan was truly clean.
/// - `Suspicious { findings }` — the caller must NOT proceed. Carries
///   every finding the scanner produced (Block + Warn) so the caller
///   has the full diagnostic set for the rejection event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanVerdict {
    Clean { warnings: Vec<Finding> },
    Suspicious { findings: Vec<Finding> },
}

impl ScanVerdict {
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::Clean { .. })
    }

    /// All findings, regardless of which variant. Useful for callers
    /// that want to log every detection without branching.
    pub fn findings(&self) -> &[Finding] {
        match self {
            Self::Clean { warnings } => warnings,
            Self::Suspicious { findings } => findings,
        }
    }
}

// -----------------------------------------------------------------
// Compiled patterns. One per credential shape, kept individual rather
// than fused into a `RegexSet` because we want span info per match
// (RegexSet only tells us which patterns matched, not where).
// -----------------------------------------------------------------

static VAULT_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{\{vault:[A-Za-z0-9_/.-]+\}\}").unwrap());

static OPENAI_KEY: Lazy<Regex> = Lazy::new(|| {
    // Anthropic uses sk-ant-... (40+ chars), OpenAI uses sk-... (48+).
    // Pattern matches both shapes plus near-variants (sk-proj-, sk-or-).
    Regex::new(r"\bsk-(?:ant-|proj-|or-|live-|test-)?[A-Za-z0-9_-]{20,}\b").unwrap()
});

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
    // Three base64url segments separated by dots; first segment starts
    // with `eyJ` (the JSON `{"` prefix base64-encoded). False positives
    // are possible but rare in skill bodies.
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b").unwrap()
});

static URL_WITH_CREDS: Lazy<Regex> = Lazy::new(|| {
    // scheme://user:pass@host... — captures the credential portion.
    Regex::new(r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^/\s:@]+:[^/\s@]+@[^\s]+").unwrap()
});

static SHA256_HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-fA-F0-9]{64}$").unwrap());

static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$")
        .unwrap()
});

static SUSPICIOUS_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:^|[._-])(token|secret|password|credential|api[_-]?key|access[_-]?key|private[_-]?key)$")
        .unwrap()
});

// Tokens are the unit of high-entropy scanning. Split on whitespace
// and a small set of common punctuation that wouldn't appear in a
// secret string itself.
static TOKEN_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\s,;<>{}()\[\]\u{2018}\u{2019}\u{201C}\u{201D}'\u{0022}]+").unwrap());

const ENTROPY_MIN_LEN: usize = 32;
const ENTROPY_THRESHOLD: f64 = 4.5;

/// The scanner entrypoint. Pure: no I/O, no global state, deterministic.
pub fn scan(input: &ScanInput, strictness: Strictness) -> ScanVerdict {
    let mut findings = Vec::new();

    // Vault references are stripped FIRST so legitimate secret slots
    // never trip the downstream patterns.
    let body = strip_vault_refs(input.body_md);
    let desc = strip_vault_refs(input.description);
    let fm = strip_vault_refs(input.frontmatter_json);

    scan_text("body_md", &body, &mut findings);
    scan_text("description", &desc, &mut findings);
    scan_text("frontmatter_json", &fm, &mut findings);

    scan_frontmatter_keys(&fm, &mut findings);

    for (path, bytes) in input.resources {
        // Only scan text-y resources. Pure-binary resources (images,
        // archives) are kept verbatim and trusted to be opaque blobs;
        // a determined attacker could embed a key in a PNG palette but
        // that's well past this scanner's threat model.
        if let Ok(s) = std::str::from_utf8(bytes) {
            let stripped = strip_vault_refs(s);
            scan_text(&format!("resources/{}", path), &stripped, &mut findings);
        }
    }

    let blocks = match strictness {
        Strictness::Strict => !findings.is_empty(),
        Strictness::Warn => findings.iter().any(|f| f.severity == Severity::Block),
    };

    if blocks {
        ScanVerdict::Suspicious { findings }
    } else {
        // Warn mode with only Warn-severity findings: report Clean
        // but carry the warnings through so the caller can emit them
        // to the audit trail. Truly-clean scans yield Clean with an
        // empty warnings vec.
        ScanVerdict::Clean { warnings: findings }
    }
}

fn strip_vault_refs(s: &str) -> String {
    VAULT_REF.replace_all(s, "<<vault-ref>>").into_owned()
}

fn scan_text(field: &str, text: &str, out: &mut Vec<Finding>) {
    push_matches(field, text, &OPENAI_KEY, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &GITHUB_TOKEN, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &AWS_ACCESS_KEY, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &SLACK_TOKEN, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &GOOGLE_API_KEY, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &PEM_PRIVATE_KEY, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &JWT_TOKEN, FindingKind::KnownPattern, Severity::Block, out);
    push_matches(field, text, &URL_WITH_CREDS, FindingKind::InlineCredential, Severity::Block, out);

    scan_entropy(field, text, out);
}

fn push_matches(
    field: &str,
    text: &str,
    re: &Regex,
    kind: FindingKind,
    severity: Severity,
    out: &mut Vec<Finding>,
) {
    for m in re.find_iter(text) {
        out.push(Finding {
            kind,
            field: field.to_string(),
            span: (m.start(), m.end()),
            matched: redact(m.as_str()),
            severity,
        });
    }
}

fn scan_entropy(field: &str, text: &str, out: &mut Vec<Finding>) {
    let mut cursor = 0usize;
    for token_match in TOKEN_SPLIT.find_iter(text) {
        let token = &text[cursor..token_match.start()];
        if !token.is_empty() {
            check_token_entropy(field, text, cursor, token, out);
        }
        cursor = token_match.end();
    }
    // Trailing token after last separator.
    if cursor < text.len() {
        let token = &text[cursor..];
        if !token.is_empty() {
            check_token_entropy(field, text, cursor, token, out);
        }
    }
}

fn check_token_entropy(field: &str, _text: &str, start: usize, token: &str, out: &mut Vec<Finding>) {
    if token.len() < ENTROPY_MIN_LEN {
        return;
    }
    if SHA256_HEX.is_match(token) || UUID.is_match(token) {
        return;
    }
    // Skip the stripped vault placeholder.
    if token.contains("<<vault-ref>>") {
        return;
    }
    // Skip obvious markdown/URL noise: tokens dominated by `/`, `:`, or
    // backticks aren't credentials.
    let punct = token.chars().filter(|c| !c.is_alphanumeric()).count();
    if punct * 4 > token.len() {
        return;
    }
    let entropy = shannon_entropy(token);
    if entropy > ENTROPY_THRESHOLD {
        out.push(Finding {
            kind: FindingKind::HighEntropy,
            field: field.to_string(),
            span: (start, start + token.len()),
            matched: redact(token),
            severity: Severity::Warn,
        });
    }
}

fn scan_frontmatter_keys(json: &str, out: &mut Vec<Finding>) {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
        return; // Malformed frontmatter rejected elsewhere; nothing to scan.
    };
    walk_json("frontmatter_json", &parsed, out);
}

fn walk_json(path: &str, v: &serde_json::Value, out: &mut Vec<Finding>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                let p = format!("{path}.{k}");
                if SUSPICIOUS_KEY.is_match(k) {
                    if let serde_json::Value::String(s) = val {
                        if s.len() >= 4 && !s.starts_with("<<vault-ref>>") {
                            out.push(Finding {
                                kind: FindingKind::SuspiciousFrontmatterKey,
                                field: p.clone(),
                                span: (0, 0),
                                matched: redact(s),
                                severity: Severity::Warn,
                            });
                        }
                    }
                }
                walk_json(&p, val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                walk_json(&format!("{path}[{i}]"), val, out);
            }
        }
        _ => {}
    }
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

fn redact(s: &str) -> String {
    let n = s.chars().count();
    let prefix: String = s.chars().take(4).collect();
    format!("{prefix}...***...({n} chars)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(s: &str) -> ScanInput<'_> {
        ScanInput {
            body_md: s,
            description: "",
            frontmatter_json: "{}",
            resources: &[],
        }
    }

    // --- vault syntax ---

    #[test]
    fn vault_reference_is_invisible_to_other_patterns() {
        // A vault reference whose name LOOKS like a sensitive field
        // must not trigger any finding because it's stripped first.
        let v = scan(
            &body("Use {{vault:openai_api_key}} when calling the model."),
            Strictness::Strict,
        );
        assert!(v.is_clean(), "vault refs must be invisible: {v:?}");
    }

    #[test]
    fn multiple_vault_refs_all_stripped() {
        let v = scan(
            &body("{{vault:a}} {{vault:b/c}} {{vault:d.e}}"),
            Strictness::Strict,
        );
        assert!(v.is_clean());
    }

    // --- known patterns ---

    #[test]
    fn openai_style_key_is_blocked() {
        let v = scan(
            &body("api key: sk-proj-AbCdEfGhIjKlMnOpQrStUvWx"),
            Strictness::Strict,
        );
        assert!(matches!(v, ScanVerdict::Suspicious { .. }));
        assert!(v.findings().iter().any(|f| f.kind == FindingKind::KnownPattern));
    }

    #[test]
    fn anthropic_style_key_is_blocked() {
        let v = scan(
            &body("ANTHROPIC_API_KEY=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
    }

    #[test]
    fn github_pat_is_blocked() {
        let v = scan(
            &body("export GITHUB_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
    }

    #[test]
    fn aws_access_key_is_blocked() {
        let v = scan(&body("aws_access_key_id = AKIAIOSFODNN7EXAMPLE"), Strictness::Strict);
        assert!(!v.is_clean());
    }

    #[test]
    fn slack_bot_token_is_blocked() {
        let v = scan(
            &body("SLACK_BOT_TOKEN=xoxb-1234567890-abcdefghijklm"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
    }

    #[test]
    fn google_api_key_is_blocked() {
        // Real Google API keys are exactly 39 chars: "AIza" + 35 chars
        // from [A-Za-z0-9_-]. Use a fixture matching that shape.
        let v = scan(
            &body("AIzaSyAbCdEfGhIjKlMnOpQrStUvWxYz0123456"),
            Strictness::Strict,
        );
        assert!(!v.is_clean(), "google api key shape must trigger");
    }

    #[test]
    fn pem_private_key_header_is_blocked() {
        let v = scan(
            &body("-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
    }

    #[test]
    fn jwt_is_blocked() {
        let v = scan(
            &body("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
    }

    #[test]
    fn url_with_inline_creds_is_blocked() {
        let v = scan(
            &body("connect to postgres://admin:hunter2@db.internal:5432/app"),
            Strictness::Strict,
        );
        assert!(v.findings().iter().any(|f| f.kind == FindingKind::InlineCredential));
    }

    // --- entropy ---

    #[test]
    fn high_entropy_random_string_is_warn() {
        // 48 chars of random base64 — high entropy, no known pattern.
        let v = scan(
            &body("token = X9aB2cD4eF6gH8iJ0kL2mN4oP6qR8sT0uV2wX4yZ6"),
            Strictness::Strict,
        );
        assert!(!v.is_clean());
        assert!(v.findings().iter().any(|f| f.kind == FindingKind::HighEntropy));
    }

    #[test]
    fn sha256_hex_is_allowlisted_not_flagged() {
        let v = scan(
            &body("commit: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            Strictness::Strict,
        );
        assert!(v.is_clean(), "sha256 hex must not trip entropy: {v:?}");
    }

    #[test]
    fn uuid_is_allowlisted_not_flagged() {
        let v = scan(
            &body("session: 550e8400-e29b-41d4-a716-446655440000"),
            Strictness::Strict,
        );
        assert!(v.is_clean());
    }

    #[test]
    fn ordinary_prose_does_not_trip_entropy() {
        let v = scan(
            &body("This is a perfectly ordinary skill body describing how to scaffold a new Rust crate using cargo new and conventional layout choices the team has standardized on."),
            Strictness::Strict,
        );
        assert!(v.is_clean(), "prose must be clean: {v:?}");
    }

    // --- frontmatter heuristic ---

    #[test]
    fn suspicious_frontmatter_key_is_warn_not_block() {
        let input = ScanInput {
            body_md: "ok",
            description: "ok",
            frontmatter_json: r#"{"name": "x", "api_key": "abc12345"}"#,
            resources: &[],
        };
        let strict = scan(&input, Strictness::Strict);
        let warn = scan(&input, Strictness::Warn);
        assert!(!strict.is_clean(), "Strict mode must block on warn findings");
        assert!(warn.is_clean(), "Warn mode lets warn-only findings through");
    }

    #[test]
    fn vault_ref_value_in_frontmatter_does_not_trip() {
        let input = ScanInput {
            body_md: "ok",
            description: "ok",
            frontmatter_json: r#"{"api_key": "{{vault:my-key}}"}"#,
            resources: &[],
        };
        let v = scan(&input, Strictness::Strict);
        assert!(v.is_clean(), "vault-ref in frontmatter must be allowed: {v:?}");
    }

    // --- strictness modes ---

    #[test]
    fn strict_mode_blocks_warn_findings() {
        let input = ScanInput {
            body_md: "X9aB2cD4eF6gH8iJ0kL2mN4oP6qR8sT0uV2wX4yZ6",
            description: "ok",
            frontmatter_json: "{}",
            resources: &[],
        };
        assert!(!scan(&input, Strictness::Strict).is_clean());
    }

    #[test]
    fn warn_mode_preserves_warn_findings_in_clean_verdict() {
        // Audit fix: when Warn strictness lets warn-only findings
        // through, the verdict must still carry them so the caller
        // can log to the audit trail. This was a documented bug in
        // the first cut of the scanner.
        let v = scan(
            &body("X9aB2cD4eF6gH8iJ0kL2mN4oP6qR8sT0uV2wX4yZ6"),
            Strictness::Warn,
        );
        assert!(v.is_clean(), "Warn mode should allow warn-only findings");
        assert!(
            !v.findings().is_empty(),
            "but the warnings must be preserved on the Clean verdict for audit logging"
        );
        assert!(v.findings().iter().all(|f| f.severity == Severity::Warn));
    }

    #[test]
    fn truly_clean_input_yields_empty_warnings() {
        let v = scan(&body("ordinary skill body"), Strictness::Strict);
        match v {
            ScanVerdict::Clean { warnings } => assert!(warnings.is_empty()),
            ScanVerdict::Suspicious { .. } => panic!("expected Clean"),
        }
    }

    #[test]
    fn warn_mode_allows_warn_findings_but_blocks_known_patterns() {
        let warn_only = ScanInput {
            body_md: "X9aB2cD4eF6gH8iJ0kL2mN4oP6qR8sT0uV2wX4yZ6",
            description: "ok",
            frontmatter_json: "{}",
            resources: &[],
        };
        assert!(scan(&warn_only, Strictness::Warn).is_clean());

        let with_block = ScanInput {
            body_md: "sk-proj-AbCdEfGhIjKlMnOpQrStUvWx",
            description: "ok",
            frontmatter_json: "{}",
            resources: &[],
        };
        assert!(!scan(&with_block, Strictness::Warn).is_clean());
    }

    // --- redaction ---

    #[test]
    fn redacted_preview_does_not_leak_full_secret() {
        let v = scan(
            &body("ghp_AbCdEfGhIjKlMnOpQrStUvWxYz1234567890"),
            Strictness::Strict,
        );
        for f in v.findings() {
            assert!(
                !f.matched.contains("AbCdEfGhIjKlMnOpQrStUvWxYz"),
                "redacted preview leaked: {}",
                f.matched
            );
        }
    }

    // --- adversarial ---

    #[test]
    fn empty_input_is_clean() {
        let v = scan(
            &ScanInput {
                body_md: "",
                description: "",
                frontmatter_json: "{}",
                resources: &[],
            },
            Strictness::Strict,
        );
        assert!(v.is_clean());
    }

    #[test]
    fn malformed_frontmatter_is_skipped_not_panic() {
        let input = ScanInput {
            body_md: "ok",
            description: "ok",
            frontmatter_json: "not json at all {{{",
            resources: &[],
        };
        // Must not panic; must not accidentally pass a known-pattern body.
        let _ = scan(&input, Strictness::Strict);
    }

    #[test]
    fn binary_resource_is_not_scanned_as_text() {
        // A PNG header is not valid UTF-8; the scanner must skip it
        // rather than misinterpret bytes as text.
        let png_header: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let resources: Vec<(String, &[u8])> = vec![("logo.png".to_string(), png_header)];
        let input = ScanInput {
            body_md: "ok",
            description: "ok",
            frontmatter_json: "{}",
            resources: &resources,
        };
        let v = scan(&input, Strictness::Strict);
        assert!(v.is_clean());
    }

    #[test]
    fn resource_text_is_scanned() {
        let bytes: &[u8] = b"sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz";
        let resources: Vec<(String, &[u8])> = vec![("config.env".to_string(), bytes)];
        let input = ScanInput {
            body_md: "ok",
            description: "ok",
            frontmatter_json: "{}",
            resources: &resources,
        };
        let v = scan(&input, Strictness::Strict);
        assert!(!v.is_clean(), "credential in resource must trigger");
        assert!(v.findings().iter().any(|f| f.field.starts_with("resources/config.env")));
    }
}
