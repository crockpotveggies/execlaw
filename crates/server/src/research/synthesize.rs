//! Synthesize phase — composes the final report from gather notes.
//!
//! One LLM call: system prompt + (original query + per-sub-query
//! excerpt + source list, all joined as markdown) → report.md. The
//! report is written to the workspace, registered as an
//! `AttachmentRow` in `state_attachments`, and the row's
//! `attachment_id` column is set so the SPA can render the report
//! inline (web/Rich channel) and transport plugins can `send_file`
//! it on TextOnly channels.
//!
//! Failures are isolated: an LLM error or empty notes corpus causes
//! the runner to mark the row Failed via the existing `mark_failed`
//! path. We don't try to fall back to a "best-effort summary" of
//! gather notes — surfacing the real failure to the operator beats
//! quietly producing a low-quality report.
//!
//! 2026-04-29.

use crate::cards::CardEmitError;
use crate::research::workspace::{ResearchWorkspace, WorkspaceError};
use execlaw_core::Database;
use execlaw_core::attachments::{AttachmentRow, AttachmentStore};
use execlaw_core::ids::{AttachmentId, ConversationId, ResearchJobId};
use execlaw_core::research::{ResearchError, ResearchNote, ResearchPlan, SubQueryState};
use execlaw_inference_api::{ChatMessage, ChatRequest, InferenceClient, ModelId};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SynthesizeError {
    #[error(transparent)]
    Store(#[from] ResearchError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    CardEmit(#[from] CardEmitError),
    #[error("inference: {0}")]
    Inference(String),
    #[error("no notes — gather produced zero usable rows")]
    NoNotes,
    #[error("attachment store: {0}")]
    Attachment(String),
}

/// Inputs to `run_synthesize`. The runner constructs this after
/// gather completes.
pub struct SynthesizeCtx {
    pub db: Database,
    pub job_id: ResearchJobId,
    pub conversation_id: ConversationId,
    pub workspace: ResearchWorkspace,
    pub query: String,
    pub plan: ResearchPlan,
    pub notes: Vec<ResearchNote>,
    pub inference: Arc<InferenceClient>,
    pub model: String,
}

/// Successful return: the rendered report markdown + the attachment
/// id the runner should store on the row.
#[derive(Debug)]
pub struct SynthesizeOutcome {
    pub report_markdown: String,
    pub attachment_id: AttachmentId,
    pub attachment_path: String,
}

const SYNTHESIZE_SYSTEM_PROMPT: &str = "You are the synthesise stage of a deep-research job. You receive the \
original research question, the planner's thesis, and a numbered list of sub-question excerpts (each from a \
parallel gather worker). Compose a clear, well-structured markdown report that answers the original question. \
Include a one-paragraph summary at the top, then thematic sections drawing on the per-sub-question material, \
and a short Sources section at the bottom listing the URLs you cited. No preamble (\"Sure!\", \"As an AI...\"). \
Reply with markdown only.";

const REPORT_MAX_TOKENS: u32 = 2048;

/// Run synthesize. Returns the rendered markdown + a
/// fresh `AttachmentId`. The runner persists the attachment id on
/// the row + emits `CardClosed{Completed}` with it; this function
/// stays focused on the LLM + workspace + attachments handoff.
pub async fn run_synthesize(ctx: SynthesizeCtx) -> Result<SynthesizeOutcome, SynthesizeError> {
    let SynthesizeCtx {
        db,
        job_id,
        conversation_id,
        workspace,
        query,
        plan,
        notes,
        inference,
        model,
    } = ctx;

    let usable: Vec<&ResearchNote> = notes
        .iter()
        .filter(|n| matches!(n.state, SubQueryState::Done))
        .collect();
    if usable.is_empty() {
        return Err(SynthesizeError::NoNotes);
    }

    let prompt_user = build_synthesize_prompt(&query, &plan, &usable);

    let chat_req = ChatRequest {
        model: ModelId(model),
        messages: vec![
            ChatMessage::system(SYNTHESIZE_SYSTEM_PROMPT),
            ChatMessage::user(prompt_user),
        ],
        max_tokens: Some(REPORT_MAX_TOKENS),
        temperature: Some(0.2),
        stream: false,
        tools: None,
        chat_template_kwargs: None,
    };
    let resp = inference
        .chat_completions(&chat_req)
        .await
        .map_err(|e| SynthesizeError::Inference(e.to_string()))?;
    let report_markdown = resp
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    if report_markdown.trim().is_empty() {
        return Err(SynthesizeError::Inference(
            "synthesize LLM returned empty markdown".into(),
        ));
    }

    finalize_report(&db, &workspace, &job_id, &conversation_id, report_markdown).await
}

/// Test seam: compose the prompt + finalize without going through
/// the LLM. Tests substitute a canned report markdown to verify the
/// attachment + workspace wiring without needing a mock InferenceClient.
pub async fn finalize_report(
    db: &Database,
    workspace: &ResearchWorkspace,
    job_id: &ResearchJobId,
    conversation_id: &ConversationId,
    report_markdown: String,
) -> Result<SynthesizeOutcome, SynthesizeError> {
    // Workspace write — the durable artifact. Any later failure
    // should not lose the report.
    let path = {
        let ws = workspace.clone();
        let id = job_id.clone();
        let body = report_markdown.clone();
        tokio::task::spawn_blocking(move || ws.write_report(&id, &body))
            .await
            .map_err(|e| SynthesizeError::Inference(format!("join: {e}")))??
    };
    let path_str = path.to_string_lossy().into_owned();

    // Attachment row — the cross-cutting handle transport plugins +
    // the SPA both consume. sha256 lets the SPA / transports
    // de-duplicate if the operator re-runs the same job.
    let mut hasher = Sha256::new();
    hasher.update(report_markdown.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let att_id = AttachmentId::new();
    let row = AttachmentRow {
        id: att_id.clone(),
        conversation_id: conversation_id.clone(),
        mime_type: "text/markdown".into(),
        path: path_str.clone(),
        sha256: sha,
        received_at: chrono::Utc::now().timestamp(),
    };
    let db_for_task = db.clone();
    tokio::task::spawn_blocking(move || AttachmentStore::new(&db_for_task).insert(&row))
        .await
        .map_err(|e| SynthesizeError::Attachment(format!("join: {e}")))?
        .map_err(|e| SynthesizeError::Attachment(e.to_string()))?;

    Ok(SynthesizeOutcome {
        report_markdown,
        attachment_id: att_id,
        attachment_path: path_str,
    })
}

fn build_synthesize_prompt(query: &str, plan: &ResearchPlan, notes: &[&ResearchNote]) -> String {
    let mut buf = String::new();
    buf.push_str("Original research question:\n");
    buf.push_str(query);
    buf.push_str("\n\nPlanner's thesis:\n");
    buf.push_str(&plan.thesis);
    buf.push_str("\n\nGather-phase findings:\n");
    for note in notes {
        buf.push_str(&format!(
            "\n## Sub-question {}: {}\n",
            note.index + 1,
            note.sub_query
        ));
        if !note.excerpt.trim().is_empty() {
            buf.push_str(&note.excerpt);
            buf.push_str("\n");
        }
        let ok_sources: Vec<&_> = note.sources.iter().filter(|s| s.fetched_ok).collect();
        if !ok_sources.is_empty() {
            buf.push_str("\nSources:\n");
            for src in ok_sources {
                let title = src.title.clone().unwrap_or_else(|| src.url.clone());
                buf.push_str(&format!("- [{title}]({})\n", src.url));
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::db::DbConfig;
    use execlaw_core::ids::EventSeq;
    use execlaw_core::migrations::MigrationRunner;
    use execlaw_core::research::{PlanStep, ResearchPlan, ResearchSource};

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn seed_conv(db: &Database, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(db)
            .upsert(&ConversationRow {
                conversation_id: cid.clone(),
                kind: ConversationKind::ControllerDM,
                last_seq: EventSeq(0),
                phase: Phase::Idle,
                controller_id: None,
                trust_class: "Controller".into(),
                snapshot_blob: None,
                snapshot_seq: None,
                lease_owner: None,
                lease_expires: None,
                modality: Modality::Text,
                display_name: None,
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    fn fixture_note(index: u32, query: &str, state: SubQueryState) -> ResearchNote {
        ResearchNote {
            index,
            sub_query: query.into(),
            state,
            excerpt: format!("Excerpt for {query}"),
            sources: vec![ResearchSource {
                url: format!("https://example.com/{query}"),
                title: Some(query.into()),
                fetched_ok: true,
                error: None,
            }],
            tokens_used: Some(50),
            error: None,
        }
    }

    #[test]
    fn build_synthesize_prompt_includes_query_thesis_and_done_notes() {
        let plan = ResearchPlan {
            thesis: "thesis-text".into(),
            steps: vec![PlanStep {
                query: "q1".into(),
                rationale: None,
            }],
        };
        let notes = vec![
            fixture_note(0, "q1", SubQueryState::Done),
            fixture_note(1, "q2", SubQueryState::Failed),
        ];
        let usable: Vec<&_> = notes
            .iter()
            .filter(|n| matches!(n.state, SubQueryState::Done))
            .collect();
        let prompt = build_synthesize_prompt("the question", &plan, &usable);
        assert!(prompt.contains("the question"));
        assert!(prompt.contains("thesis-text"));
        assert!(prompt.contains("Sub-question 1: q1"));
        assert!(prompt.contains("Excerpt for q1"));
        // Failed sub-questions are filtered before this function
        // sees them, so q2 should NOT appear.
        assert!(!prompt.contains("Sub-question 2: q2"));
        // Source list rendered.
        assert!(prompt.contains("https://example.com/q1"));
    }

    #[tokio::test]
    async fn finalize_report_writes_workspace_and_inserts_attachment() {
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-syn");
        let job_id = ResearchJobId::new();
        let tmp = tempfile::tempdir().unwrap().keep();
        let workspace = ResearchWorkspace::new(tmp.clone());
        let outcome = finalize_report(
            &db,
            &workspace,
            &job_id,
            &cid,
            "# Final report\n\nBody.".into(),
        )
        .await
        .unwrap();
        // Workspace file lands at <tmp>/<job_id>/report.md.
        let on_disk = std::fs::read_to_string(tmp.join(job_id.as_str()).join("report.md")).unwrap();
        assert!(on_disk.starts_with("# Final report"));
        assert!(!outcome.attachment_id.as_str().is_empty());
        assert!(outcome.attachment_path.contains("report.md"));
        // Attachment row inserted — round-trip query.
        let count: i64 = db
            .with_conn(|c| {
                let n: i64 = c
                    .query_row(
                        "SELECT COUNT(*) FROM state_attachments WHERE id = ?1",
                        rusqlite::params![outcome.attachment_id.as_str()],
                        |r| r.get(0),
                    )
                    .unwrap();
                Ok(n)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn run_synthesize_errors_on_zero_done_notes() {
        // No mock InferenceClient needed — the no-notes guard fires
        // before the LLM call.
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-no-notes");
        let job_id = ResearchJobId::new();
        let tmp = tempfile::tempdir().unwrap().keep();
        let workspace = ResearchWorkspace::new(tmp);
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q".into(),
                rationale: None,
            }],
        };
        let notes = vec![fixture_note(0, "q", SubQueryState::Failed)];
        let ctx = SynthesizeCtx {
            db,
            job_id,
            conversation_id: cid,
            workspace,
            query: "what?".into(),
            plan,
            notes,
            inference: Arc::new(InferenceClient::new("http://127.0.0.1:0/v1")),
            model: "m".into(),
        };
        let err = run_synthesize(ctx).await.unwrap_err();
        assert!(matches!(err, SynthesizeError::NoNotes));
    }

    #[tokio::test]
    async fn run_synthesize_round_trips_against_mock_inference_backend() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "id": "syn-1",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "# Report\n\nFindings…"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-roundtrip");
        let job_id = ResearchJobId::new();
        let tmp = tempfile::tempdir().unwrap().keep();
        let workspace = ResearchWorkspace::new(tmp);
        let plan = ResearchPlan {
            thesis: "t".into(),
            steps: vec![PlanStep {
                query: "q".into(),
                rationale: None,
            }],
        };
        let notes = vec![fixture_note(0, "q", SubQueryState::Done)];
        let ctx = SynthesizeCtx {
            db,
            job_id,
            conversation_id: cid,
            workspace,
            query: "what?".into(),
            plan,
            notes,
            inference: Arc::new(InferenceClient::new(format!("http://{addr}/v1"))),
            model: "test-model".into(),
        };
        let outcome = run_synthesize(ctx).await.unwrap();
        assert!(outcome.report_markdown.starts_with("# Report"));
        assert!(!outcome.attachment_id.as_str().is_empty());
    }

    #[tokio::test]
    async fn run_synthesize_errors_when_llm_returns_empty_text() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "id": "syn-empty",
                "object": "chat.completion",
                "created": 1_700_000_000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "   "},
                    "finish_reason": "stop",
                }],
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let db = fresh_db();
        let cid = seed_conv(&db, "conv-empty");
        let job_id = ResearchJobId::new();
        let tmp = tempfile::tempdir().unwrap().keep();
        let ctx = SynthesizeCtx {
            db,
            job_id,
            conversation_id: cid,
            workspace: ResearchWorkspace::new(tmp),
            query: "q".into(),
            plan: ResearchPlan {
                thesis: "t".into(),
                steps: vec![PlanStep {
                    query: "q".into(),
                    rationale: None,
                }],
            },
            notes: vec![fixture_note(0, "q", SubQueryState::Done)],
            inference: Arc::new(InferenceClient::new(format!("http://{addr}/v1"))),
            model: "test-model".into(),
        };
        let err = run_synthesize(ctx).await.unwrap_err();
        assert!(matches!(err, SynthesizeError::Inference(_)));
    }
}
