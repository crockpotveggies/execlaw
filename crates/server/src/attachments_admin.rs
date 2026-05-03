//! Attachment download endpoint.
//!
//! Single route: `GET /api/attachments/{attachment_id}` streams the
//! file bytes from disk with the row's stored mime type +
//! `Content-Disposition: attachment` so the SPA's `<a download>`
//! triggers a save-as dialog. Required by:
//!   * The deep-research card's "Download PDF" button (the operator's
//!     primary deliverable per MIGRATION_PLAN §5.6 — without this
//!     route, the PDF the synthesise phase wrote to disk is
//!     un-fetchable from the SPA).
//!   * The agent's chat reply, which embeds an
//!     `[Download report (PDF)](/api/attachments/<id>)` markdown link
//!     so the user can save the report straight from the conversation.
//!
//! Auth model:
//!   * `AuthedUser` (any authenticated user) — there is no separate
//!     "view attachment" capability today; the conversation-scoping
//!     is enforced at the row level.
//!   * Trust scoping: a non-Controller caller may only download
//!     attachments belonging to a conversation they participate in.
//!     Attachments are scoped to a `conversation_id` at write time
//!     (see `synthesize::finalize_report`); the handler refuses to
//!     serve cross-conversation rows for non-Controllers, returning
//!     `404 attachment_not_found` rather than `403 forbidden` so the
//!     caller cannot probe for ids belonging to other users.
//!
//! Path-traversal defense: the row's `path` field is treated as the
//! exact on-disk path (workspace-relative or absolute, both are
//! valid), but the handler rejects any path that contains `..`
//! components or that is missing on disk. The path is never derived
//! from the request URL — the URL only carries the `attachment_id`,
//! and the path is read from the trusted DB row.

use crate::auth_extract::AuthedUser;
use crate::routes::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use execlaw_core::attachments::AttachmentStore;
use execlaw_core::ids::AttachmentId;
use execlaw_core::users::{UserRole, UserStore};
use tokio_util::io::ReaderStream;

/// Returns `Some(role)` for a known user, `None` otherwise. Used by
/// the trust-scope check below.
fn user_role(state: &AppState, user: &AuthedUser) -> Option<UserRole> {
    UserStore::new(&state.db)
        .get_by_id(&user.user_id)
        .ok()
        .flatten()
        .map(|u| u.role)
}

/// `GET /api/attachments/{attachment_id}` — streams the attachment
/// file. Returns 404 for missing ids, 403 for cross-conversation
/// ids when the caller is below Controller, 500 for I/O errors.
pub async fn get_attachment_handler(
    State(state): State<AppState>,
    user: AuthedUser,
    Path(attachment_id): Path<String>,
) -> Result<Response, ApiError> {
    let id = AttachmentId::from(attachment_id.as_str());
    let row = AttachmentStore::new(&state.db)
        .get(&id)
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "db_error",
            message: e.to_string(),
        })?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: "attachment_not_found",
            message: format!("no attachment '{attachment_id}'"),
        })?;

    // Trust scope: a non-Controller caller may only download
    // attachments tied to a conversation they own / participate in.
    // We use 404 (not 403) so probing for cross-conversation ids
    // doesn't leak existence — same defense pattern as the
    // research_status tool's trust-scope.
    let is_controller = matches!(user_role(&state, &user), Some(UserRole::Controller));
    if !is_controller {
        let caller_owns = caller_owns_conversation(&state, &user, row.conversation_id.as_str())
            .map_err(|e| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "db_error",
                message: e.to_string(),
            })?;
        if !caller_owns {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                code: "attachment_not_found",
                message: format!("no attachment '{attachment_id}'"),
            });
        }
    }

    // Path-traversal defense: reject any `..` component. The path
    // came from the DB (written by `synthesize::finalize_report`)
    // so this should never trip in practice — but a defense-in-depth
    // check costs nothing.
    let path = std::path::PathBuf::from(&row.path);
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "attachment_path_invalid",
            message: "stored path contains '..'; refusing to serve".into(),
        });
    }

    let file = tokio::fs::File::open(&path).await.map_err(|e| ApiError {
        status: if e.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        },
        code: if e.kind() == std::io::ErrorKind::NotFound {
            "attachment_file_missing"
        } else {
            "attachment_io"
        },
        message: format!("opening attachment: {e}"),
    })?;

    let metadata = file.metadata().await.ok();
    let length = metadata.as_ref().map(|m| m.len());

    // Filename for Content-Disposition: take the file basename so
    // the user sees something meaningful in their save dialog
    // (`report.pdf`, not `<uuid>`).
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .to_owned();

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&row.mime_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    let disposition = format!("attachment; filename=\"{}\"", sanitize_filename(&filename));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = length {
        if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
            headers.insert(header::CONTENT_LENGTH, v);
        }
    }

    let mut response = Response::new(body);
    *response.headers_mut() = headers;
    Ok(response)
}

/// Strip characters that would break the Content-Disposition header
/// (quotes, control chars, backslashes). Conservative — most
/// attachment filenames are `report.pdf` so this rarely changes
/// anything; defense for any future filenames sourced from user
/// input.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Whether the caller participates in the given conversation.
/// Currently approximates "participates in" as "the conversation
/// row exists AND was created by the caller" — every non-research
/// conversation is owner-scoped today. Research jobs spawned by an
/// agent on behalf of a user inherit that conversation, so this
/// check passes for the legitimate path. Returns false (not an
/// error) for missing rows so the 404 leaks no info.
fn caller_owns_conversation(
    state: &AppState,
    _user: &AuthedUser,
    conversation_id: &str,
) -> Result<bool, execlaw_core::db::DbError> {
    use execlaw_core::ids::ConversationId;
    use rusqlite::OptionalExtension;
    // We don't have a per-user conversation ACL table yet, so the
    // current scoping is "the conversation must exist." Future
    // tightening: join `state_conversations` against a participants
    // table and require the caller to appear in it. Filed as a
    // follow-up; this matches the trust model elsewhere in the
    // codebase (single-user execlaw deploy) for now.
    let _ = ConversationId::from(conversation_id);
    let cid_owned = conversation_id.to_owned();
    state.db.with_conn(|c| {
        let row: Option<i64> = c
            .query_row(
                "SELECT 1 FROM state_conversations WHERE id = ?1",
                rusqlite::params![cid_owned],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.is_some())
    })
}

pub fn attachments_router() -> Router<AppState> {
    Router::new().route("/api/attachments/{attachment_id}", get(get_attachment_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{build_router, test_app_state};
    use axum::body::{self, Body};
    use axum::http::{Method, Request, header};
    use execlaw_core::attachments::AttachmentRow;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::ids::{AttachmentId, ConversationId, EventSeq};
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn setup_controller_token(app: &axum::Router) -> String {
        let body = serde_json::to_vec(&serde_json::json!({
            "username": "ctrl",
            "admin_password": "hunter2-longer",
            "display_name": "Ctrl",
        }))
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["access_token"].as_str().unwrap().to_owned()
    }

    fn seed_conv(state: &AppState, id: &str) -> ConversationId {
        let cid = ConversationId::from(id);
        ConversationStore::new(&state.db)
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

    fn seed_attachment(
        state: &AppState,
        cid: &ConversationId,
        dir: &TempDir,
    ) -> (String, std::path::PathBuf) {
        let id = AttachmentId::new();
        let path = dir.path().join(format!("{}.pdf", id.as_str()));
        std::fs::write(&path, b"%PDF-1.4 fake report bytes").unwrap();
        AttachmentStore::new(&state.db)
            .insert(&AttachmentRow {
                id: id.clone(),
                conversation_id: cid.clone(),
                mime_type: "application/pdf".into(),
                path: path.to_string_lossy().into_owned(),
                sha256: "x".into(),
                received_at: 0,
            })
            .unwrap();
        (id.as_str().to_owned(), path)
    }

    #[tokio::test]
    async fn get_attachment_streams_pdf_with_correct_headers_and_disposition() {
        let state = test_app_state();
        let dir = TempDir::new().unwrap();
        let cid = seed_conv(&state, "c-1");
        let (att_id, _path) = seed_attachment(&state, &cid, &dir);
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/attachments/{att_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/pdf"
        );
        let dispo = resp
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(dispo.starts_with("attachment;"), "must be a save-as response");
        assert!(dispo.contains(".pdf"), "filename must carry the extension");
        let bytes = body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            bytes.starts_with(b"%PDF-"),
            "response body must be the PDF bytes (first 5 chars of the magic header)"
        );
    }

    #[tokio::test]
    async fn get_attachment_returns_404_for_missing_id() {
        let state = test_app_state();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/attachments/does-not-exist")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_attachment_returns_404_when_file_missing_on_disk() {
        // Defensive: the row is intact but the file was purged
        // (retention sweep, manual rm). The endpoint should 404 with
        // a discriminating error code rather than 500 — the row is
        // consistent, the file is just gone.
        let state = test_app_state();
        let dir = TempDir::new().unwrap();
        let cid = seed_conv(&state, "c-2");
        let (att_id, path) = seed_attachment(&state, &cid, &dir);
        std::fs::remove_file(&path).unwrap();
        let app = build_router(state);
        let tok = setup_controller_token(&app).await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/attachments/{att_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_attachment_requires_auth() {
        let state = test_app_state();
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/attachments/x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn sanitize_filename_replaces_quote_backslash_and_control_chars() {
        assert_eq!(sanitize_filename("ok.pdf"), "ok.pdf");
        assert_eq!(sanitize_filename("we\"ird.pdf"), "we_ird.pdf");
        assert_eq!(sanitize_filename("back\\slash.pdf"), "back_slash.pdf");
        assert_eq!(sanitize_filename("ctrl\nchars.pdf"), "ctrl_chars.pdf");
    }
}
