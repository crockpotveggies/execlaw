//! Phase C — summarizer interface and prompt construction.
//!
//! The auto-capture worker doesn't know how to talk to an LLM
//! itself — that lives in `crates/server` where the
//! `InferenceResolver` is wired. This module defines the
//! [`SkillSummarizer`] trait the worker calls into, plus the
//! deterministic prompt builder + output parser. The server
//! provides one production impl backed by `BackendPurpose::Small`;
//! tests provide a mock impl that returns a canned response.
//!
//! The output of summarization is a [`DraftSkillProposal`] —
//! exactly the fields the worker needs to call
//! `SkillStore::create`.

use crate::sanitizer::SanitizedStep;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Stable, fenced-block protocol the LLM is asked to emit.
///
/// We deliberately use a brittle structured format rather than free
/// markdown so the parser is unambiguous. The LLM is told to refuse
/// (return `SKIP`) when the trajectory doesn't generalize; we honor
/// that and produce no draft.
pub const SKIP_SENTINEL: &str = "SKIP";

/// The bundle of inputs handed to a summarizer impl.
#[derive(Debug, Clone)]
pub struct SummarizerPrompt {
    /// The trajectory to summarize. Already sanitized.
    pub steps: Vec<SanitizedStep>,
    /// The user message that kicked off the turn (sanitized).
    /// Used as additional context for what the trajectory was
    /// trying to accomplish.
    pub user_intent: Option<String>,
    /// Soft cap on the model's reply tokens. The impl translates
    /// to whatever its backend uses.
    pub max_tokens: u32,
}

/// What the summarizer returns. `Skip` means the trajectory wasn't
/// summarizable (model judged it too narrow / too noisy / not a
/// procedure). `Draft` carries the parsed proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizerOutput {
    Skip { reason: String },
    Draft(DraftSkillProposal),
}

/// Parsed proposal — exactly what `SkillStore::create` needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSkillProposal {
    /// Namespaced name in the canonical `<namespace>/<segment>`
    /// shape. Validated by `model::validate_skill_name` before
    /// the worker writes it.
    pub name: String,
    pub description: String,
    pub body_md: String,
    /// Optional structured tags for admin-UI filtering.
    pub tags: Vec<String>,
}

/// The trait the worker calls into. Async because production impls
/// hit the inference backend over HTTP. Returning `Result<_,
/// String>` keeps the trait dep-free of any specific error type.
#[async_trait]
pub trait SkillSummarizer: Send + Sync {
    async fn summarize(&self, prompt: SummarizerPrompt) -> Result<SummarizerOutput, String>;
}

/// Build the system + user prompt the summarizer sends to the LLM.
/// Pure: deterministic, no I/O. The output is stable across calls
/// for the same input so the prompt cache stays warm.
///
/// Format the LLM is asked to emit:
///
/// ```text
/// SKIP: <reason>
/// ```
///
/// or
///
/// ```text
/// NAME: <namespace>/<segment>
/// DESCRIPTION: <one-line, <= 200 chars>
/// TAGS: <comma-separated list, optional>
/// ---BODY---
/// <markdown body>
/// ---END---
/// ```
pub fn build_prompt(prompt: &SummarizerPrompt) -> (String, String) {
    let system = SYSTEM_PROMPT.to_string();
    let mut user = String::new();
    if let Some(intent) = &prompt.user_intent {
        user.push_str("USER INTENT:\n");
        user.push_str(intent.trim());
        user.push_str("\n\n");
    }
    user.push_str("TRAJECTORY (");
    user.push_str(&prompt.steps.len().to_string());
    user.push_str(" tool call(s)):\n\n");
    for step in &prompt.steps {
        user.push_str(&format!("[{}] tool: {}\n", step.ordinal, step.tool_name));
        user.push_str("  args: ");
        user.push_str(&compact_json(&step.args_json));
        user.push('\n');
        user.push_str(if step.outcome_ok {
            "  ok:   "
        } else {
            "  err:  "
        });
        user.push_str(&compact_json(&step.result_json));
        user.push_str("\n\n");
    }
    user.push_str("Respond now in the format described in the system prompt.");
    (system, user)
}

const SYSTEM_PROMPT: &str = r#"You are a skill summarizer for the execlaw agent system.

You are given a TRAJECTORY: an agent's sequence of tool calls and results from a single turn that completed successfully. Your job: decide whether the trajectory documents a reusable PROCEDURE worth saving as a skill, and if so, write that skill.

A trajectory is worth saving when ALL of these hold:
  1. The same problem is plausibly going to recur for this operator.
  2. The procedure is non-obvious — there's a sequence, ordering, or
     decision point a future agent could get wrong without guidance.
  3. The result reflects a successful end-to-end completion, not
     accidental success or partial progress.

If ANY of those fail (one-off task, trivial procedure, unclear success), respond with exactly:

SKIP: <one short sentence saying why>

Otherwise, respond in EXACTLY this format with no extra prose:

NAME: <namespace>/<segment>
DESCRIPTION: <one or two sentences telling a future agent when to use this skill, max 200 chars>
TAGS: <comma-separated optional tags, or omit the line>
---BODY---
<markdown procedure: when-to-use, step-by-step, gotchas, verification>
---END---

Naming rules:
- `<namespace>` is a single lowercase word matching the tool family
  (e.g. `calendar`, `research`, `git`, `email`).
- `<segment>` is a short verb-object phrase, lowercase + hyphens.
- Both segments are `[a-z0-9][a-z0-9-]*` and combined length 3..=128.

Body rules:
- Do NOT include any literal credentials, tokens, or operator-private
  data from the trajectory. Use placeholders like `<user-name>` or
  `{{vault:NAME}}`.
- Do NOT mention specific values that won't generalize (specific user
  ids, dates, conversation ids).
- DO name the tools used. Future agents will look at `skills.list`
  descriptions to decide whether to load the body.

Be terse. The body is for an agent reading it before acting, not a
human reading documentation. Aim for 5-15 lines."#;

/// Phase D.3 — improvement-evaluation prompt builder.
///
/// Used by the reuse-update worker. The model is shown an EXISTING
/// skill's body plus a fresh trajectory that activated it, and asked
/// whether the trajectory reveals an improvement worth saving as a
/// new version (fork). Returns `Skip` if the trajectory doesn't add
/// anything; otherwise returns a `Draft` whose `name` matches the
/// existing skill name.
pub fn build_improvement_prompt(
    skill_name: &str,
    current_body_md: &str,
    prompt: &SummarizerPrompt,
) -> (String, String) {
    let system = IMPROVEMENT_SYSTEM_PROMPT.to_string();
    let mut user = String::new();
    user.push_str("EXISTING SKILL: ");
    user.push_str(skill_name);
    user.push_str("\n\nCURRENT BODY:\n");
    user.push_str(current_body_md.trim());
    user.push_str("\n\n");
    if let Some(intent) = &prompt.user_intent {
        user.push_str("USER INTENT (this turn):\n");
        user.push_str(intent.trim());
        user.push_str("\n\n");
    }
    user.push_str("TRAJECTORY (");
    user.push_str(&prompt.steps.len().to_string());
    user.push_str(" tool call(s)):\n\n");
    for step in &prompt.steps {
        user.push_str(&format!("[{}] tool: {}\n", step.ordinal, step.tool_name));
        user.push_str("  args: ");
        user.push_str(&compact_json(&step.args_json));
        user.push('\n');
        user.push_str(if step.outcome_ok {
            "  ok:   "
        } else {
            "  err:  "
        });
        user.push_str(&compact_json(&step.result_json));
        user.push_str("\n\n");
    }
    user.push_str("Respond now in the format described in the system prompt. The skill name MUST be exactly \"");
    user.push_str(skill_name);
    user.push_str("\".");
    (system, user)
}

const IMPROVEMENT_SYSTEM_PROMPT: &str = r#"You are a skill improvement evaluator for the execlaw agent system.

You are given an EXISTING skill body plus a fresh TRAJECTORY that activated it. Decide whether the trajectory revealed something worth adding to the skill: a missing edge case, a verification step that prevented an error, a clearer ordering, or a gotcha the current body doesn't mention.

If the trajectory does NOT reveal an improvement (current body already covers it, the procedure was followed exactly, or the trajectory is too narrow to generalize), respond with exactly:

SKIP: <one short sentence saying why>

Otherwise, respond in EXACTLY this format with no extra prose:

NAME: <existing skill name verbatim — do NOT rename>
DESCRIPTION: <updated one or two sentences, max 200 chars>
TAGS: <comma-separated optional tags, or omit the line>
---BODY---
<full updated markdown body — keep what works, refine what didn't>
---END---

Rules:
- Do NOT include any literal credentials or operator-private data
  from the trajectory. Use placeholders or `{{vault:NAME}}`.
- The name MUST be the existing one — you are forking, not renaming.
- The body must STAND ALONE — replace the current body, don't diff
  against it. Operators see your full body as the next version.
- Be conservative: if you're not sure the change is an improvement,
  return SKIP. False positives spam the operator's review queue.
- Aim for 5-15 lines."#;

fn compact_json(v: &serde_json::Value) -> String {
    // Compact-print so large objects don't blow up the prompt.
    // Capped by sanitizer's per-field truncation.
    serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".into())
}

/// Parse the LLM's reply into a structured outcome. Lenient about
/// surrounding whitespace, strict about the field names + delimiters.
pub fn parse_response(reply: &str) -> SummarizerOutput {
    let reply = reply.trim();

    // SKIP path.
    if let Some(rest) = reply.strip_prefix(SKIP_SENTINEL) {
        let reason = rest.trim_start_matches(':').trim().to_string();
        return SummarizerOutput::Skip {
            reason: if reason.is_empty() {
                "model returned SKIP without a reason".into()
            } else {
                reason
            },
        };
    }

    // Structured proposal path.
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut body: Option<String> = None;

    // Split header from body.
    let (header, body_section) = match reply.split_once("---BODY---") {
        Some((h, b)) => (h, Some(b)),
        None => (reply, None),
    };

    for line in header.lines() {
        let line = line.trim();
        if let Some(rest) = strip_field(line, "NAME:") {
            name = Some(rest.to_string());
        } else if let Some(rest) = strip_field(line, "DESCRIPTION:") {
            description = Some(rest.to_string());
        } else if let Some(rest) = strip_field(line, "TAGS:") {
            tags = rest
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
    }

    if let Some(b) = body_section {
        let trimmed = b.trim().trim_end_matches("---END---").trim().to_string();
        if !trimmed.is_empty() {
            body = Some(trimmed);
        }
    }

    match (name, description, body) {
        (Some(name), Some(description), Some(body_md)) => {
            SummarizerOutput::Draft(DraftSkillProposal {
                name,
                description,
                body_md,
                tags,
            })
        }
        _ => SummarizerOutput::Skip {
            reason: "model reply did not match the structured format".into(),
        },
    }
}

fn strip_field<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let lc = line.to_ascii_uppercase();
    if lc.starts_with(prefix) {
        Some(line[prefix.len()..].trim())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitizer::SanitizedStep;
    use serde_json::json;

    fn step(ordinal: u32, tool: &str, args: serde_json::Value, ok: bool) -> SanitizedStep {
        SanitizedStep {
            ordinal,
            tool_name: tool.into(),
            args_json: args,
            result_json: json!({"ok": ok}),
            outcome_ok: ok,
        }
    }

    // --- prompt builder ---

    #[test]
    fn build_prompt_includes_user_intent_when_present() {
        let p = SummarizerPrompt {
            steps: vec![step(1, "search", json!({"q": "rust"}), true)],
            user_intent: Some("find a rust crate".into()),
            max_tokens: 512,
        };
        let (sys, user) = build_prompt(&p);
        assert!(sys.contains("skill summarizer"));
        assert!(user.contains("USER INTENT:"));
        assert!(user.contains("find a rust crate"));
    }

    #[test]
    fn build_prompt_renders_each_step_with_ok_or_err_marker() {
        let p = SummarizerPrompt {
            steps: vec![
                step(1, "tool_a", json!({"x": 1}), true),
                step(2, "tool_b", json!({"y": 2}), false),
            ],
            user_intent: None,
            max_tokens: 256,
        };
        let (_, user) = build_prompt(&p);
        assert!(user.contains("[1] tool: tool_a"));
        assert!(user.contains("ok:"));
        assert!(user.contains("[2] tool: tool_b"));
        assert!(user.contains("err:"));
    }

    #[test]
    fn build_prompt_is_deterministic_for_same_input() {
        let p = SummarizerPrompt {
            steps: vec![step(1, "t", json!({"k": "v"}), true)],
            user_intent: Some("intent".into()),
            max_tokens: 100,
        };
        let a = build_prompt(&p);
        let b = build_prompt(&p);
        assert_eq!(a, b);
    }

    // --- parser: SKIP ---

    #[test]
    fn parser_recognizes_skip_with_reason() {
        let out = parse_response("SKIP: trajectory too narrow to generalize");
        match out {
            SummarizerOutput::Skip { reason } => {
                assert!(reason.contains("too narrow"));
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn parser_recognizes_bare_skip_without_reason() {
        let out = parse_response("SKIP");
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }

    #[test]
    fn parser_tolerates_leading_trailing_whitespace_around_skip() {
        let out = parse_response("  \nSKIP: x\n  ");
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }

    // --- parser: structured draft ---

    #[test]
    fn parser_extracts_name_description_body() {
        let reply = "NAME: research/gather-sources
DESCRIPTION: Use when the operator wants web research on a topic.
---BODY---
1. Call search.
2. Fetch top 3.
3. Summarize.
---END---";
        let out = parse_response(reply);
        match out {
            SummarizerOutput::Draft(d) => {
                assert_eq!(d.name, "research/gather-sources");
                assert!(d.description.starts_with("Use when"));
                assert!(d.body_md.contains("Call search"));
                assert!(d.body_md.contains("Summarize"));
                assert!(d.tags.is_empty());
            }
            other => panic!("expected Draft, got {other:?}"),
        }
    }

    #[test]
    fn parser_extracts_tags_when_present() {
        let reply = "NAME: x/y
DESCRIPTION: d
TAGS: alpha, beta, gamma
---BODY---
body
---END---";
        let out = parse_response(reply);
        match out {
            SummarizerOutput::Draft(d) => {
                assert_eq!(d.tags, vec!["alpha", "beta", "gamma"]);
            }
            other => panic!("expected Draft, got {other:?}"),
        }
    }

    #[test]
    fn parser_returns_skip_when_required_field_missing() {
        // Missing DESCRIPTION.
        let reply = "NAME: x/y
---BODY---
body
---END---";
        let out = parse_response(reply);
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }

    #[test]
    fn parser_returns_skip_when_body_missing() {
        let reply = "NAME: x/y
DESCRIPTION: d";
        let out = parse_response(reply);
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }

    #[test]
    fn parser_tolerates_end_marker_or_no_end_marker() {
        // No ---END--- — body is everything after ---BODY---.
        let reply = "NAME: x/y
DESCRIPTION: d
---BODY---
body without end marker";
        let out = parse_response(reply);
        match out {
            SummarizerOutput::Draft(d) => assert_eq!(d.body_md, "body without end marker"),
            other => panic!("expected Draft, got {other:?}"),
        }
    }

    // --- adversarial ---

    #[test]
    fn parser_returns_skip_for_freeform_prose() {
        let out =
            parse_response("Sure! Based on the trajectory I think this would be a great skill...");
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }

    #[test]
    fn parser_returns_skip_for_empty_reply() {
        let out = parse_response("");
        assert!(matches!(out, SummarizerOutput::Skip { .. }));
    }
}
