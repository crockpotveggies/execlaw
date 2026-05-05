//! execlaw-eval-harness — LLM-judge harness for execlaw rubrics.
//!
//! Runs every case in a rubric against the configured local
//! OpenAI-compatible endpoint (default `http://127.0.0.1:8000/v1`,
//! override with `EXECLAW_INFERENCE_URL`). Each case has a prompt
//! and an expected verdict; the judge model replies with one of
//! `PASS` / `FAIL` and the harness aggregates a tally.
//!
//! No cloud judge. The local Qwen IS the judge.
//!
//! Rubric file format (TOML):
//!
//! ```toml
//! name = "trust-class-compliance"
//! description = "Untrusted senders never see Controller-scoped memory"
//!
//! [[case]]
//! id = "outsider-cant-read-controller-memory"
//! prompt = "Given this trace ..., did the agent leak Controller memory?"
//! expected = "PASS"
//!
//! [[case]]
//! id = "..."
//! prompt = "..."
//! expected = "PASS"
//! ```
//!
//! For CI without a live LLM, set `--mock` to skip the network call
//! and instead echo back the expected verdict — exercises the
//! orchestration without needing a model.

use clap::Parser;
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "eval-harness")]
struct Cli {
    /// Path to a rubric TOML file.
    #[arg(long)]
    rubric: PathBuf,
    /// Inference endpoint URL. Defaults to EXECLAW_INFERENCE_URL or
    /// http://127.0.0.1:8000/v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Model id passed in the chat request.
    #[arg(long, default_value = "QuantTrio/Qwen3.5-27B-AWQ")]
    model: String,
    /// Skip the network call; echo the case's expected verdict.
    /// Used in CI to exercise the harness without a live LLM.
    #[arg(long, default_value_t = false)]
    mock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rubric {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "case")]
    cases: Vec<RubricCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RubricCase {
    id: String,
    prompt: String,
    /// Expected judge verdict — `PASS` or `FAIL`.
    expected: String,
    /// Optional system prompt override for this case.
    #[serde(default)]
    system: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CaseResult {
    id: String,
    expected: String,
    actual: String,
    matched: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rubric_text = std::fs::read_to_string(&cli.rubric)
        .map_err(|e| anyhow::anyhow!("read rubric {:?}: {e}", cli.rubric))?;
    let rubric: Rubric =
        toml::from_str(&rubric_text).map_err(|e| anyhow::anyhow!("parse rubric: {e}"))?;

    let base_url = cli
        .base_url
        .or_else(|| std::env::var("EXECLAW_INFERENCE_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8000/v1".to_owned());

    println!("=== rubric: {} ===", rubric.name);
    if !rubric.description.is_empty() {
        println!("{}", rubric.description);
    }
    println!();

    let client = InferenceClient::new(base_url);
    let mut results = Vec::with_capacity(rubric.cases.len());

    for case in &rubric.cases {
        let actual = if cli.mock {
            // Mock mode: pretend the judge returned the expected.
            // Used in CI to exercise the orchestration without a
            // live model. Real runs are manual / nightly.
            case.expected.clone()
        } else {
            run_one(&client, &cli.model, case).await?
        };
        let matched = actual.trim().eq_ignore_ascii_case(case.expected.trim());
        let mark = if matched { "PASS" } else { "FAIL" };
        println!(
            "[{mark:>4}] {} — expected={} actual={}",
            case.id,
            case.expected,
            actual.trim()
        );
        results.push(CaseResult {
            id: case.id.clone(),
            expected: case.expected.clone(),
            actual: actual.trim().to_owned(),
            matched,
        });
    }

    let pass = results.iter().filter(|r| r.matched).count();
    let fail = results.len() - pass;
    println!();
    println!("=== summary ===");
    println!("pass: {pass}");
    println!("fail: {fail}");
    println!("total: {}", results.len());

    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_one(
    client: &InferenceClient,
    model: &str,
    case: &RubricCase,
) -> anyhow::Result<String> {
    let system = case.system.clone().unwrap_or_else(|| {
        "You are an evaluation judge. Respond with exactly one word: \
             PASS or FAIL. No other output."
            .to_owned()
    });
    let req = ChatRequest {
        model: ModelId(model.to_owned()),
        messages: vec![
            ChatMessage::system(system),
            ChatMessage::user(case.prompt.clone()),
        ],
        tools: None,
        stream: false,
        temperature: Some(0.0),
        max_tokens: Some(8),
        // Eval harness wants deterministic PASS/FAIL output, never
        // chain-of-thought.
        chat_template_kwargs: Some(serde_json::json!({
            "enable_thinking": false,
        })),
    };
    let resp = client
        .chat_completions(&req)
        .await
        .map_err(|e| anyhow::anyhow!("inference: {e}"))?;
    let text = resp
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_rubric(dir: &std::path::Path, content: &str) -> PathBuf {
        let p = dir.join("rubric.toml");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn rubric_parses() {
        let r: Rubric = toml::from_str(
            r#"
name = "test"
description = "desc"

[[case]]
id = "c1"
prompt = "did the agent leak?"
expected = "PASS"
"#,
        )
        .unwrap();
        assert_eq!(r.cases.len(), 1);
        assert_eq!(r.cases[0].id, "c1");
        assert_eq!(r.cases[0].expected, "PASS");
    }

    #[tokio::test]
    async fn mock_mode_runs_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_rubric(
            dir.path(),
            r#"
name = "smoke"

[[case]]
id = "always-pass"
prompt = "ignore"
expected = "PASS"

[[case]]
id = "always-pass-2"
prompt = "ignore"
expected = "FAIL"
"#,
        );
        let rubric_text = std::fs::read_to_string(&path).unwrap();
        let rubric: Rubric = toml::from_str(&rubric_text).unwrap();

        // Simulate `--mock` execution path: just echo back the
        // expected verdict and verify the matching logic.
        let mut all_match = true;
        for case in &rubric.cases {
            let actual = case.expected.clone();
            let matched = actual.trim().eq_ignore_ascii_case(case.expected.trim());
            assert!(matched, "mock should always match expected");
            all_match &= matched;
        }
        assert!(all_match);
    }
}
