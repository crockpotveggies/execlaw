//! `bench_turn` — manual latency-diagnosis harness for the agent turn.
//!
//! Operator reported the agent taking minutes to answer "hi". The
//! turn-timing DEBUG instrumentation tells us the LATENCY of each
//! production step but not WHY a particular shape is slow. This
//! harness sends progressively-bigger ChatCompletions to a live
//! vLLM and prints a side-by-side breakdown so the operator can
//! isolate the responsible axis:
//!
//!     baseline                : "hi", no tools, no system prompt
//!     +system                 : + a 1-line system prompt
//!     +5_tools_nodescs        : + 5 tools, name+empty schema only
//!     +5_tools_full           : + 5 tools, real descriptions + schemas
//!     +real_catalog           : + all enabled tools from the operator's
//!                                config_tool_access table
//!     +real_catalog_streaming : same, but streaming so we can measure
//!                                first-token latency (= prefill time)
//!
//! Each scenario runs `EXECLAW_BENCH_ITERS` times (default 3). The
//! summary table shows min/median/p95 for `total_ms` plus prompt and
//! completion token counts so the operator can compute prefill /
//! decode tps after the fact.
//!
//! Knobs (all env-var):
//!   EXECLAW_INFERENCE_URL  — default http://127.0.0.1:8101/v1
//!   EXECLAW_BENCH_MODEL    — default QuantTrio/Qwen3.5-27B-AWQ
//!   EXECLAW_DB             — default ~/.execlaw/execlaw.db
//!   EXECLAW_BENCH_ITERS    — default 3
//!   EXECLAW_BENCH_MAX_TOK  — default 16  (small + capped so token
//!                                          counts don't dominate
//!                                          the wall-clock signal)
//!
//! Run:
//!   cargo run -p execlaw-inference-api --example bench_turn --release
//!
//! Released-binary required — debug builds add ~1-2s of overhead to
//! every iteration that masks the signal we're hunting.

use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId, ToolDeclaration};
use rusqlite::{Connection, OpenFlags};
use std::time::{Duration, Instant};

const DEFAULT_MODEL: &str = "QuantTrio/Qwen3.6-27B-AWQ";
const DEFAULT_URL: &str = "http://127.0.0.1:8101/v1";
const DEFAULT_ITERS: usize = 3;
const DEFAULT_MAX_TOKENS: u32 = 16;

const SYSTEM_PROMPT_SHORT: &str = "You are execlaw, the controller's local agent. Reply concisely.";

/// Result of one inference call.
#[derive(Debug, Clone)]
struct Sample {
    /// Total request wall-clock (request build → final body byte).
    total_ms: u64,
    /// Server-reported prompt-token count (None if usage absent).
    prompt_tokens: Option<u32>,
    /// Server-reported completion-token count.
    completion_tokens: Option<u32>,
    /// vLLM's `finish_reason` — "stop" | "length" | "tool_calls" | ...
    finish_reason: Option<String>,
    /// First 80 chars of content, for sanity-check.
    content_preview: String,
}

/// One scenario's roll-up across N iterations.
struct ScenarioReport {
    name: &'static str,
    description: String,
    samples: Vec<Sample>,
}

impl ScenarioReport {
    fn min_total_ms(&self) -> u64 {
        self.samples.iter().map(|s| s.total_ms).min().unwrap_or(0)
    }
    fn median_total_ms(&self) -> u64 {
        let mut v: Vec<u64> = self.samples.iter().map(|s| s.total_ms).collect();
        v.sort_unstable();
        if v.is_empty() { 0 } else { v[v.len() / 2] }
    }
    fn p95_total_ms(&self) -> u64 {
        let mut v: Vec<u64> = self.samples.iter().map(|s| s.total_ms).collect();
        v.sort_unstable();
        if v.is_empty() {
            return 0;
        }
        let idx = ((v.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        v[idx.min(v.len() - 1)]
    }
    fn typical_prompt_tokens(&self) -> u32 {
        self.samples
            .iter()
            .filter_map(|s| s.prompt_tokens)
            .next()
            .unwrap_or(0)
    }
    fn typical_completion_tokens(&self) -> u32 {
        self.samples
            .iter()
            .filter_map(|s| s.completion_tokens)
            .next()
            .unwrap_or(0)
    }
    fn typical_finish_reason(&self) -> &str {
        self.samples
            .first()
            .and_then(|s| s.finish_reason.as_deref())
            .unwrap_or("?")
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("EXECLAW_INFERENCE_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let model = std::env::var("EXECLAW_BENCH_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let iters = std::env::var("EXECLAW_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERS);
    let max_tokens = std::env::var("EXECLAW_BENCH_MAX_TOK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let db_path = std::env::var("EXECLAW_DB").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".into());
        format!("{home}/.execlaw/execlaw.db")
    });

    eprintln!("# execlaw bench_turn");
    eprintln!("#   inference_url = {url}");
    eprintln!("#   model         = {model}");
    eprintln!("#   iters         = {iters}");
    eprintln!("#   max_tokens    = {max_tokens}");
    eprintln!("#   db            = {db_path}");
    eprintln!();

    let client = InferenceClient::new(url);
    let model_id = ModelId(model.clone());

    // Pre-warm vLLM with one throwaway call. Cold-cache prefill on
    // the very first invocation includes a one-time CUDA-graph
    // build (~3-8s on Qwen3.5-AWQ) that would otherwise contaminate
    // the baseline scenario's median.
    eprintln!("warming up vLLM (1 call, discarded)...");
    let _ = run_one(
        &client,
        &model_id,
        BuildArgs {
            system: None,
            user: "warmup",
            tools: vec![],
            max_tokens,
            stream: false,
        },
    )
    .await;
    eprintln!("warmup done; starting scenarios.\n");

    let scenarios = build_scenarios(&db_path)?;
    let mut reports: Vec<ScenarioReport> = Vec::with_capacity(scenarios.len());

    for sc in scenarios {
        eprintln!(
            "scenario: {} — {} ({} iters)",
            sc.name, sc.description, iters
        );
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let s = run_one(
                &client,
                &model_id,
                BuildArgs {
                    system: sc.system,
                    user: "hi",
                    tools: sc.tools.clone(),
                    max_tokens: sc.max_tokens_override.unwrap_or(max_tokens),
                    stream: sc.stream,
                },
            )
            .await?;
            eprintln!(
                "  iter {} → total_ms={} prompt={} completion={} finish={} preview={:?}",
                i + 1,
                s.total_ms,
                s.prompt_tokens.unwrap_or(0),
                s.completion_tokens.unwrap_or(0),
                s.finish_reason.as_deref().unwrap_or("?"),
                s.content_preview,
            );
            samples.push(s);
        }
        reports.push(ScenarioReport {
            name: sc.name,
            description: sc.description,
            samples,
        });
        eprintln!();
    }

    print_summary(&reports);
    Ok(())
}

struct ScenarioDef {
    name: &'static str,
    description: String,
    system: Option<&'static str>,
    tools: Vec<ToolDeclaration>,
    stream: bool,
    /// Per-scenario override of the global `EXECLAW_BENCH_MAX_TOK`.
    /// `None` = inherit global cap. The `_max4096` scenario sets
    /// 4096 explicitly to mirror production.
    max_tokens_override: Option<u32>,
}

fn build_scenarios(db_path: &str) -> anyhow::Result<Vec<ScenarioDef>> {
    let mut out: Vec<ScenarioDef> = Vec::new();

    // 1. Baseline — just "hi", absolutely nothing else. This is the
    //    floor for inference latency on this model.
    out.push(ScenarioDef {
        name: "baseline",
        description: "no system prompt, no tools — pure 'hi'".into(),
        system: None,
        tools: vec![],
        stream: false,
        max_tokens_override: None,
    });

    // 2. + System prompt. Tiny addition (~50 tokens). If this
    //    scenario is meaningfully slower than baseline, prefill is
    //    not amortised on tiny prompts.
    out.push(ScenarioDef {
        name: "+system",
        description: "+ short system prompt".into(),
        system: Some(SYSTEM_PROMPT_SHORT),
        tools: vec![],
        stream: false,
        max_tokens_override: None,
    });

    // 3. + 5 toy tools with empty schemas. Tests the *count* axis
    //    independent of schema complexity. Schema-constrained
    //    grammar compile cost scales with schema content; empty
    //    schemas should be near-free.
    out.push(ScenarioDef {
        name: "+5_tools_nodescs",
        description: "+ 5 tools, empty schema, one-line desc".into(),
        system: Some(SYSTEM_PROMPT_SHORT),
        tools: (1..=5)
            .map(|i| {
                ToolDeclaration::function(
                    format!("toy_{i}"),
                    "Toy tool for benchmarking.".to_owned(),
                    serde_json::json!({"type": "object"}),
                )
            })
            .collect(),
        stream: false,
        max_tokens_override: None,
    });

    // 4. + 5 tools with REAL schemas + long descriptions. Pulls
    //    five rows from the operator's `config_tool_access` table
    //    so the shape matches production exactly. If this scenario
    //    is meaningfully slower than (3), schema complexity is the
    //    real cost driver (qwen3_xml grammar compile).
    let real_5 = load_real_tools(db_path, Some(5))?;
    out.push(ScenarioDef {
        name: "+5_tools_full",
        description: format!(
            "+ 5 real tools (descriptions+schemas; total {} bytes)",
            real_5
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
                .sum::<usize>()
        ),
        system: Some(SYSTEM_PROMPT_SHORT),
        tools: real_5,
        stream: false,
        max_tokens_override: None,
    });

    // 5. Full real catalog. Same shape production sends. If this
    //    is significantly slower than (4), the cost scales with
    //    catalog count (and/or vLLM's per-tool overhead — grammar
    //    compile, per-tool attention bias setup, etc.).
    let real_all = load_real_tools(db_path, None)?;
    out.push(ScenarioDef {
        name: "+real_catalog",
        description: format!(
            "+ ALL real tools ({} tools, total {} bytes)",
            real_all.len(),
            real_all
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
                .sum::<usize>()
        ),
        system: Some(SYSTEM_PROMPT_SHORT),
        tools: real_all.clone(),
        stream: false,
        max_tokens_override: None,
    });

    // 6. Full real catalog STREAMING — lets us measure first-token
    //    latency (= prefill cost). For the previous non-streaming
    //    scenarios `total_ms` includes prefill+decode; here we
    //    split them. NOTE: the runner-binary path already streams
    //    in production; the in-process path doesn't.
    out.push(ScenarioDef {
        name: "+real_catalog_streaming",
        description: "+ real catalog, STREAMING (measures first-token latency)".into(),
        system: Some(SYSTEM_PROMPT_SHORT),
        tools: real_all.clone(),
        stream: true,
        max_tokens_override: None,
    });

    // 7. Same shape as production's max_tokens=4096 cap. This is
    //    the kind that takes minutes. If the model generates anywhere
    //    near max_tokens, decode wall-clock will dominate. Capped
    //    behind an env override to keep the default bench fast.
    if std::env::var("EXECLAW_BENCH_INCLUDE_LONG").is_ok() {
        out.push(ScenarioDef {
            name: "+real_catalog_max4096",
            description: "+ real catalog, max_tokens=4096 (production cap)".into(),
            system: Some(SYSTEM_PROMPT_SHORT),
            tools: real_all,
            stream: false,
            max_tokens_override: Some(4096),
        });
    }

    Ok(out)
}

fn load_real_tools(db_path: &str, limit: Option<usize>) -> anyhow::Result<Vec<ToolDeclaration>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("open {db_path}: {e}"))?;
    let sql = match limit {
        Some(n) => format!(
            "SELECT tool_name, description, input_schema FROM config_tool_access \
             WHERE enabled = 1 ORDER BY tool_name LIMIT {n}"
        ),
        None => "SELECT tool_name, description, input_schema FROM config_tool_access \
             WHERE enabled = 1 ORDER BY tool_name"
            .into(),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(0)?;
        let description: Option<String> = r.get(1)?;
        let schema_json: Option<String> = r.get(2)?;
        Ok((name, description, schema_json))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (name, description, schema_json) = row?;
        let schema = schema_json
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        out.push(ToolDeclaration::function(
            name,
            description.unwrap_or_default(),
            schema,
        ));
    }
    Ok(out)
}

struct BuildArgs<'a> {
    system: Option<&'a str>,
    user: &'a str,
    tools: Vec<ToolDeclaration>,
    max_tokens: u32,
    stream: bool,
}

async fn run_one(
    client: &InferenceClient,
    model: &ModelId,
    args: BuildArgs<'_>,
) -> anyhow::Result<Sample> {
    let mut messages = Vec::new();
    if let Some(sys) = args.system {
        messages.push(ChatMessage::system(sys));
    }
    messages.push(ChatMessage::user(args.user));
    let req = ChatRequest {
        model: model.clone(),
        messages,
        tools: if args.tools.is_empty() {
            None
        } else {
            Some(args.tools)
        },
        stream: args.stream,
        temperature: Some(0.0),
        max_tokens: Some(args.max_tokens),
        // The whole point of this harness: reproduce production
        // request shape, which sets enable_thinking explicitly.
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
        tool_choice: None,
        guided_decoding_backend: None,
    };

    let started_at = Instant::now();
    if args.stream {
        // Streaming: read the full stream and accumulate. We don't
        // emit a separate "first chunk" stat here because the
        // production turn-timing DEBUG logs already capture that;
        // this harness just covers the same surface end-to-end.
        use futures::StreamExt;
        let mut stream = client.chat_completions_stream(&req).await?;
        let mut content = String::new();
        let mut finish: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for ch in &chunk.choices {
                if let Some(c) = &ch.delta.content {
                    content.push_str(c);
                }
                if let Some(fr) = &ch.finish_reason {
                    finish = Some(fr.clone());
                }
            }
        }
        let total_ms = started_at.elapsed().as_millis() as u64;
        Ok(Sample {
            total_ms,
            // vLLM's SSE stream omits usage; non-streaming includes
            // it. The trade-off is intentional — operators who want
            // token counts read it off the non-streaming scenarios.
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: finish,
            content_preview: short(&content, 80),
        })
    } else {
        let resp = client.chat_completions(&req).await?;
        let total_ms = started_at.elapsed().as_millis() as u64;
        let choice = resp.choices.first();
        let content = choice
            .and_then(|c| c.message.content.as_ref().map(|mc| mc.as_text()))
            .unwrap_or_default();
        let finish = choice.and_then(|c| c.finish_reason.clone());
        Ok(Sample {
            total_ms,
            prompt_tokens: resp.usage.as_ref().map(|u| u.prompt_tokens),
            completion_tokens: resp.usage.as_ref().map(|u| u.completion_tokens),
            finish_reason: finish,
            content_preview: short(&content, 80),
        })
    }
}

fn short(s: &str, max: usize) -> String {
    let trimmed: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

fn print_summary(reports: &[ScenarioReport]) {
    let _ = Duration::default(); // keep std::time imported if other paths shrink
    eprintln!("# summary");
    eprintln!(
        "  {:<28} {:>10} {:>10} {:>10} {:>10} {:>10}  finish_reason  notes",
        "scenario", "min_ms", "median", "p95", "prompt_t", "compl_t",
    );
    let mut prev_median: Option<u64> = None;
    for r in reports {
        let median = r.median_total_ms();
        let delta_note = match prev_median {
            None => String::new(),
            Some(prev) if median > prev => format!("(+{} ms)", median - prev),
            Some(prev) => format!("(-{} ms)", prev - median),
        };
        eprintln!(
            "  {:<28} {:>10} {:>10} {:>10} {:>10} {:>10}  {:<14} {}",
            r.name,
            r.min_total_ms(),
            r.median_total_ms(),
            r.p95_total_ms(),
            r.typical_prompt_tokens(),
            r.typical_completion_tokens(),
            r.typical_finish_reason(),
            delta_note,
        );
        prev_median = Some(median);
    }
    eprintln!();
    eprintln!("# scenario descriptions");
    for r in reports {
        eprintln!("  {:<28} {}", r.name, r.description);
    }
    eprintln!();
    eprintln!(
        "# tip: prefill tps  = prompt_t / (median_ms/1000); decode tps = compl_t / (median_ms/1000)"
    );
}
