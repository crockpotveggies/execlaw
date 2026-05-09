//! Phase C — server-side runtime for the skill auto-capture worker.
//!
//! Wires `execlaw_skills::AutoCaptureWorker` into:
//!   1. The inference resolver (so the summarizer can talk to a real
//!      LLM via `BackendPurpose::Small`).
//!   2. `AppState` (so the chat handler can `enqueue` at turn end).
//!   3. The CLI bootstrap (which constructs the worker + spawns its
//!      tokio task before the HTTP server starts taking traffic).
//!
//! The trait + worker live in `execlaw_skills`; everything inference-
//! specific is here so the skills crate stays dep-free of the
//! inference client.

use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::backends::BackendPurpose;
use execlaw_inference_api::{ChatMessage, ChatRequest, ModelId};
use execlaw_skills::{
    AutoCaptureSink, AutoCaptureWorker, SkillStore, SkillSummarizer, SummarizerOutput,
    SummarizerPrompt, build_prompt, parse_response,
};
use std::sync::Arc;

/// Production summarizer. Holds an `InferenceResolver` reference and
/// a per-call `Database` borrow so it can pick the right backend at
/// each turn (operator may have swapped the Small backend at runtime).
pub struct InferenceSummarizer {
    inference: Arc<crate::inference_resolver::InferenceResolver>,
    db: Database,
    /// Model id to send in the request. The Small backend's vLLM
    /// instance still requires a `model` field on the request; we
    /// pass through a stable string and let the backend echo it.
    /// Defaults to the standard model id; can be overridden per
    /// deployment when the operator runs a smaller model on the
    /// Small backend.
    model_id: ModelId,
}

impl InferenceSummarizer {
    pub fn new(
        inference: Arc<crate::inference_resolver::InferenceResolver>,
        db: Database,
        model_id: ModelId,
    ) -> Self {
        Self {
            inference,
            db,
            model_id,
        }
    }
}

#[async_trait]
impl SkillSummarizer for InferenceSummarizer {
    async fn summarize(&self, prompt: SummarizerPrompt) -> Result<SummarizerOutput, String> {
        let client = self
            .inference
            .resolve(&self.db, BackendPurpose::Small)
            .or_else(|| self.inference.resolve(&self.db, BackendPurpose::Standard))
            .ok_or_else(|| "no inference backend available for summarization".to_string())?;
        let max_tokens = prompt.max_tokens;
        let (system, user) = build_prompt(&prompt);
        let req = ChatRequest {
            model: self.model_id.clone(),
            messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
            tools: None,
            stream: false,
            temperature: Some(0.2),
            max_tokens: Some(max_tokens),
            // Adapter applies per-family kwargs.
            chat_template_kwargs: None,
        };
        let adapter = execlaw_model_adapter::adapter_for(
            execlaw_model_adapter::ModelFamily::detect(self.model_id.as_str()),
        );
        // Summarizer reply is the structured `NAME: ... ---BODY---`
        // protocol parsed by `parse_response`. Use Plain hint so
        // the adapter strips fences but doesn't try to extract
        // JSON-balanced braces (the body is markdown, not JSON).
        let adapted = adapter
            .chat(&client, req, execlaw_model_adapter::OutputHint::Plain)
            .await
            .map_err(|e| format!("inference call failed: {e}"))?;
        Ok(parse_response(&adapted.content))
    }
}

/// Build + spawn the auto-capture worker. Returns the
/// [`AutoCaptureSink`] the chat handler holds + the spawned task's
/// `JoinHandle` (the caller may `abort()` on shutdown).
///
/// Caller plumbs the returned sink through `AppState.skill_capture`.
pub fn spawn_capture_worker(
    db: Database,
    skill_store: Arc<SkillStore>,
    inference: Arc<crate::inference_resolver::InferenceResolver>,
    model_id: ModelId,
) -> (AutoCaptureSink, tokio::task::JoinHandle<()>) {
    let summarizer: Arc<dyn SkillSummarizer> =
        Arc::new(InferenceSummarizer::new(inference, db.clone(), model_id));
    let worker = Arc::new(AutoCaptureWorker::new(db, skill_store, summarizer));
    worker.spawn()
}

/// Phase D.3 — build + spawn the reuse-update worker. Same shape as
/// [`spawn_capture_worker`]. Shares the `InferenceSummarizer` impl
/// because both workers issue `BackendPurpose::Small` chat calls;
/// the inputs (system+user prompt) differ, but the trait surface is
/// identical.
pub fn spawn_reuse_update_worker(
    db: Database,
    skill_store: Arc<SkillStore>,
    inference: Arc<crate::inference_resolver::InferenceResolver>,
    model_id: ModelId,
) -> (execlaw_skills::ReuseUpdateSink, tokio::task::JoinHandle<()>) {
    let summarizer: Arc<dyn SkillSummarizer> =
        Arc::new(InferenceSummarizer::new(inference, db.clone(), model_id));
    let worker = Arc::new(execlaw_skills::ReuseUpdateWorker::new(
        db,
        skill_store,
        summarizer,
    ));
    worker.spawn()
}
