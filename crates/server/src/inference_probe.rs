//! `POST /api/admin/inference/probe` — direct inference diagnostic.
//!
//! Bypasses the entire agent loop (no event log writes, no tool
//! dispatch, no supervisor RPC, no history hydration) and sends a
//! controlled request straight to the resolved inference backend
//! using the same `InferenceClient` the runner uses. Returns the
//! same timing splits the runner emits at `agent::turn_timing`
//! (`open_stream_ms`, `first_chunk_ms`, `decode_ms`,
//! `chunks_per_sec`) plus a preview of the model's response.
//!
//! Use it to localize where the latency in a slow turn lives:
//!
//!   * Probe with a small prompt + no tools → if this is fast,
//!     vLLM and the network are healthy; the slowness is in the
//!     server's prompt assembly or the live tool catalog.
//!   * Probe with the same prompt + `include_tools=true` → if this
//!     is suddenly slow, the live tool catalog is the suspect
//!     (size, schema complexity).
//!   * Probe with tools + `guided_decoding_backend="outlines"` vs
//!     `null` → isolates whether outlines is stalling vLLM prefill
//!     on a complex schema (the chart.render `additionalProperties:
//!     false` case).
//!   * Probe with a large synthetic prompt (e.g. 50 KiB) → tells
//!     you how prefill scales with prompt size on your hardware.
//!
//! The endpoint is auth-gated (Controller-only). It runs the same
//! idle-watchdog pattern the runner uses, so a stalled probe
//! produces the same `runner::turn_loop` heartbeat logs.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use execlaw_core::backends::BackendPurpose;
use execlaw_inference_api::{
    ChatMessage, ChatRequest, FunctionDecl, ModelId, ToolDeclaration,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct InferenceProbeRequest {
    /// System prompt to send. Optional — when absent, a short
    /// fixed string is used so the experiment is deterministic.
    /// Use to test prompt-size sensitivity by sending a large
    /// padded string.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// User message to send. Optional — defaults to a short
    /// instruction asking for a one-sentence reply.
    #[serde(default)]
    pub user_text: Option<String>,
    /// `max_tokens` cap on the response. Defaults to 100 — small
    /// enough that a working backend completes in under a second,
    /// so a probe that takes longer points clearly at backend
    /// latency rather than just "lots to generate."
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Temperature override. Defaults to 0.0 for deterministic
    /// repeat runs.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// When `true`, attach the live agent-callable tool catalogue
    /// (built-ins ∪ plugin tools) to the request. Use to A/B-test
    /// whether the catalogue itself is slowing prefill.
    #[serde(default)]
    pub include_tools: bool,
    /// Override `guided_decoding_backend`. `Some("")` disables.
    /// `None` (default) leaves it unset (default: outlines when
    /// tools are present, otherwise nothing).
    #[serde(default)]
    pub guided_decoding_backend: Option<String>,
    /// Force `tool_choice: "auto"` even without tools. Mostly
    /// useful for testing vLLM's `--enable-auto-tool-choice` flag.
    #[serde(default)]
    pub force_tool_choice_auto: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InferenceProbeTimings {
    pub open_stream_ms: u64,
    pub first_chunk_ms: u64,
    pub decode_ms: u64,
    pub stream_total_ms: u64,
    pub chunks_per_sec: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InferenceProbeResponse {
    pub timings: InferenceProbeTimings,
    pub chunks_received: u64,
    pub text_chars: usize,
    pub text_preview: String,
    pub finish_reason: Option<String>,
    pub model: String,
    pub request_body_chars: usize,
    pub tool_count: usize,
    pub errored: bool,
    /// Anyhow-chain-walked error message when `errored = true`.
    /// Absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/admin/inference/probe",
    request_body = InferenceProbeRequest,
    responses(
        (status = 200, description = "Probe completed (success OR caught error in `errored`)", body = InferenceProbeResponse),
        (status = 503, description = "No inference backend configured"),
    ),
    security(("bearer_jwt" = [])),
    tag = "diagnostics"
)]
pub async fn inference_probe_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Json(req): Json<InferenceProbeRequest>,
) -> Result<Json<InferenceProbeResponse>, ApiError> {
    // 2026-05-16 — fix #7: enforce the Controller-only contract the
    // module doc claims. Pre-fix the handler bound `_user` and
    // discarded the role, so any authenticated user (including a
    // future KnownLimited admin / read-only viewer role) could
    // dispatch arbitrary prompts at the inference backend, see the
    // raw model output, and stress-test prompt-prefill latency
    // — none of which a non-Controller principal should be able to do.
    // Mirrors the same `UserStore::get_by_id`-based role check every
    // other admin route uses (`alerts`, `backends`, `mcp_admin`,
    // `factory_reset`, `personality`, `oauth_admin`,
    // `plugin_settings_admin`).
    require_controller(&state, &user)?;

    let resolved = state
        .inference
        .resolve(&state.db, BackendPurpose::Standard)
        .ok_or_else(|| ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "inference_unavailable",
            message: "no inference backend resolvable for `Standard` purpose".into(),
        })?;
    let client = resolved.client;
    let model_id = resolved.model_id;

    let system_prompt = req.system_prompt.unwrap_or_else(|| {
        "You are a diagnostic probe target. Respond concisely.".to_owned()
    });
    let user_text = req
        .user_text
        .unwrap_or_else(|| "Say hello in one short sentence.".to_owned());

    // Live tool catalog when requested. Built from the same
    // registry the runner sees so timing comparisons are faithful.
    let tools: Option<Vec<ToolDeclaration>> = if req.include_tools {
        let registry = state.plugin_host.registry();
        let mut decls: Vec<ToolDeclaration> = registry
            .all_builtins()
            .iter()
            .map(|t| {
                let d = t.descriptor();
                ToolDeclaration::function(d.name.clone(), d.description.clone(), d.schema.clone())
            })
            .collect();
        decls.extend(registry.agent_callable_tools().iter().map(|t| {
            let description = t.description.clone().unwrap_or_else(|| {
                format!(
                    "Plugin tool '{}' from '{}' (latency: {}).",
                    t.tool_name, t.plugin_id, t.latency,
                )
            });
            let schema = t
                .schema_json
                .clone()
                .unwrap_or_else(|| serde_json::json!({"type": "object"}));
            ToolDeclaration {
                kind: "function".into(),
                function: FunctionDecl {
                    name: t.tool_name.clone(),
                    description,
                    parameters: schema,
                },
            }
        }));
        Some(decls)
    } else {
        None
    };

    let tool_count = tools.as_ref().map(|v| v.len()).unwrap_or(0);

    // `guided_decoding_backend` override semantics:
    //   * `None` → no override; behave like the runner default
    //     (outlines if tools present, otherwise unset).
    //   * `Some("")` → explicit disable (operator wants to see if
    //     outlines is the stall cause).
    //   * `Some(name)` → use that backend.
    let guided_decoding_backend = match (&req.guided_decoding_backend, tools.is_some()) {
        (Some(v), _) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        (None, true) => Some("outlines".to_owned()),
        (None, false) => None,
    };

    let tool_choice = if tools.is_some() || req.force_tool_choice_auto {
        Some(serde_json::Value::String("auto".to_owned()))
    } else {
        None
    };

    let chat_req = ChatRequest {
        model: ModelId(model_id.clone()),
        messages: vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(user_text),
        ],
        tools,
        stream: true,
        temperature: req.temperature.or(Some(0.0)),
        max_tokens: req.max_tokens.or(Some(100)),
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
        tool_choice,
        guided_decoding_backend,
    };
    let request_body_chars = serde_json::to_string(&chat_req)
        .map(|s| s.chars().count())
        .unwrap_or(0);

    // Run the stream with the same idle-watchdog pattern the
    // runner uses so probe stalls produce the same heartbeat
    // logs at `runner::turn_loop`.
    let round_started_at = std::time::Instant::now();
    let stream_result = client.chat_completions_stream(&chat_req).await;
    let mut stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            return Ok(Json(InferenceProbeResponse {
                timings: InferenceProbeTimings {
                    open_stream_ms: round_started_at.elapsed().as_millis() as u64,
                    first_chunk_ms: 0,
                    decode_ms: 0,
                    stream_total_ms: round_started_at.elapsed().as_millis() as u64,
                    chunks_per_sec: 0,
                },
                chunks_received: 0,
                text_chars: 0,
                text_preview: String::new(),
                finish_reason: None,
                model: model_id,
                request_body_chars,
                tool_count,
                errored: true,
                error: Some(format!("stream-open failure: {e:#}")),
            }));
        }
    };
    let open_stream_ms = round_started_at.elapsed().as_millis() as u64;

    let mut text_acc = String::new();
    let mut chunks_received: u64 = 0;
    let mut finish_reason: Option<String> = None;
    let mut first_chunk_at: Option<std::time::Instant> = None;
    let mut last_chunk_at = std::time::Instant::now();
    let mut errored: Option<String> = None;

    let idle_warn_secs: u64 = std::env::var("EXECLAW_INFERENCE_IDLE_WARN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let mut idle_interval =
        tokio::time::interval(std::time::Duration::from_secs(idle_warn_secs));
    idle_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    idle_interval.tick().await;

    let probe_id = uuid::Uuid::new_v4().to_string();
    loop {
        tokio::select! {
            biased;
            maybe_chunk = stream.next() => {
                let Some(chunk) = maybe_chunk else { break; };
                match chunk {
                    Ok(c) => {
                        if first_chunk_at.is_none() {
                            first_chunk_at = Some(std::time::Instant::now());
                        }
                        last_chunk_at = std::time::Instant::now();
                        chunks_received = chunks_received.saturating_add(1);
                        for choice in &c.choices {
                            if let Some(t) = &choice.delta.content {
                                text_acc.push_str(t);
                            }
                            if let Some(fr) = &choice.finish_reason {
                                finish_reason = Some(fr.clone());
                            }
                        }
                    }
                    Err(e) => {
                        errored = Some(format!("mid-stream read failure: {e:#}"));
                        break;
                    }
                }
            }
            _ = idle_interval.tick() => {
                let idle_ms = last_chunk_at.elapsed().as_millis() as u64;
                tracing::warn!(
                    target: "runner::turn_loop",
                    probe_id = %probe_id,
                    idle_ms,
                    chunks_so_far = chunks_received,
                    text_chars_so_far = text_acc.chars().count(),
                    first_chunk_seen = first_chunk_at.is_some(),
                    stall_phase = if first_chunk_at.is_some() { "decode" } else { "prefill" },
                    "inference probe idle — no chunks arrived in the last interval"
                );
            }
        }
    }
    drop(stream);

    let stream_total_ms = round_started_at.elapsed().as_millis() as u64;
    let first_chunk_ms = first_chunk_at
        .map(|t| t.duration_since(round_started_at).as_millis() as u64)
        .unwrap_or(0);
    let decode_ms = if first_chunk_at.is_some() {
        stream_total_ms.saturating_sub(first_chunk_ms)
    } else {
        0
    };
    let chunks_per_sec = if decode_ms > 0 {
        (chunks_received as f64 * 1000.0 / decode_ms as f64) as u64
    } else {
        0
    };

    let text_preview: String = text_acc.chars().take(512).collect();
    let text_chars = text_acc.chars().count();

    Ok(Json(InferenceProbeResponse {
        timings: InferenceProbeTimings {
            open_stream_ms,
            first_chunk_ms,
            decode_ms,
            stream_total_ms,
            chunks_per_sec,
        },
        chunks_received,
        text_chars,
        text_preview,
        finish_reason,
        model: model_id,
        request_body_chars,
        tool_count,
        errored: errored.is_some(),
        error: errored,
    }))
}

/// 2026-05-16 — fix #7: Controller-only role check. Same shape as
/// `alerts::require_controller`, `backends::require_controller`,
/// etc. Duplicated here rather than lifted to a shared module so
/// fix #7's diff stays minimal; a follow-up can consolidate the
/// seven copies of this helper at once.
fn require_controller(state: &AppState, user: &AuthedUser) -> Result<(), ApiError> {
    use execlaw_core::users::{UserRole, UserStore};
    let row = UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?;
    match row.map(|u| u.role) {
        Some(UserRole::Controller) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "controller_only",
            message: "only a Controller can run the inference probe".into(),
        }),
    }
}

pub fn inference_probe_router() -> Router<AppState> {
    Router::new().route("/api/admin/inference/probe", post(inference_probe_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request, header};
    use execlaw_core::users::{UserRole, UserRow, UserStore};
    use tower::ServiceExt;

    /// Helper mirroring `skills_admin::tests::seed_user_and_token`.
    /// Inserts a user with the given role and mints an access token.
    async fn seed_user_and_token(role: UserRole) -> (axum::Router, String) {
        let state = test_app_state();
        UserStore::new(&state.db)
            .insert(&UserRow {
                user_id: "u-probe-test".into(),
                username: "probe-tester".into(),
                display_name: "Probe Tester".into(),
                email: None,
                password_hash: "argon2-placeholder".into(),
                role,
                created_at: 0,
                last_login_at: None,
            })
            .expect("insert user");
        let token = state
            .signer
            .issue_access_token("u-probe-test", "session-test", 600)
            .expect("issue token");
        (build_router(state), format!("Bearer {token}"))
    }

    /// 2026-05-16 — fix #7: the doc comment says Controller-only, the
    /// handler must actually enforce it. Pre-fix the handler bound
    /// `_user: AuthedUser` and discarded the role — any authenticated
    /// user (including the Operator/Viewer roles that the user-management
    /// flow can mint) could POST to /api/admin/inference/probe and
    /// run arbitrary prompts at the inference backend. With the fix,
    /// non-Controller callers get 403 from `require_controller`.
    #[tokio::test]
    async fn inference_probe_rejects_non_controller_caller() {
        let (app, bearer) = seed_user_and_token(UserRole::Operator).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/inference/probe")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .header(header::AUTHORIZATION, HeaderValue::from_str(&bearer).unwrap())
            .body(Body::from(b"{}".to_vec()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "Operator role must NOT be able to run the inference probe"
        );
    }

    /// Same coverage for Viewer — anyone below Controller is rejected.
    #[tokio::test]
    async fn inference_probe_rejects_viewer_caller() {
        let (app, bearer) = seed_user_and_token(UserRole::Viewer).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/inference/probe")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .header(header::AUTHORIZATION, HeaderValue::from_str(&bearer).unwrap())
            .body(Body::from(b"{}".to_vec()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Sanity: a Controller caller passes the role check. The probe
    /// itself will surface 503 (no inference backend configured in
    /// the test fixture), which proves the role check let the
    /// request through — not 403 from `require_controller`.
    #[tokio::test]
    async fn inference_probe_passes_role_check_for_controller() {
        let (app, bearer) = seed_user_and_token(UserRole::Controller).await;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/admin/inference/probe")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .header(header::AUTHORIZATION, HeaderValue::from_str(&bearer).unwrap())
            .body(Body::from(b"{}".to_vec()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "Controller passes the role check; test fixture has no inference backend so 503 is the next gate"
        );
    }
}
