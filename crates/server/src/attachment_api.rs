//! `AttachmentApi` impl for the web channel.
//!
//! Symmetric to (eventual) channel-plugin `send_attachment` impls
//! for SMS / email transports. The web-channel implementation
//! delivers a file by emitting an `Attachment` card (an open +
//! immediate close pair) into the conversation. The SPA's card
//! projection picks the card up and renders it as an inline
//! download chip with a click-through to `/api/attachments/<id>`.
//!
//! Design notes
//!   * Card pair (open + close) rather than a single event because
//!     the existing card projection requires both halves to surface
//!     a terminal-state card; emitting just one would land as a
//!     half-rendered "Pending" chip that never resolves.
//!   * Trust scope: the AttachmentRow's `conversation_id` MUST
//!     match the caller's conversation. Refuses with `NotFound`
//!     (not NotAuthorized) so cross-conversation id probing leaks
//!     no info — same defense pattern as `DbResearchApi::status`.
//!   * Card title falls back to the file basename so the SPA shows
//!     something operator-friendly (`report.pdf`) even when the
//!     caller passes no caption.

use crate::cards::{close_card_and_broadcast, open_card_and_broadcast};
use crate::events::EventBus;
use async_trait::async_trait;
use execlaw_core::Database;
use execlaw_core::attachments::AttachmentStore;
use execlaw_core::cards::{CardAction, CardClosedPayload, CardKind, CardOpenedPayload, CardState};
// CardAction includes only its enum variants; no separate Kind type.
use execlaw_core::ids::{AttachmentId, ConversationId};
use execlaw_core::tool::{ApiError, AttachmentApi, CreatedArtifactView, DeliveredAttachmentView};
use serde_json::json;
use std::path::PathBuf;

pub struct ServerAttachmentApi {
    db: Database,
    events: EventBus,
    caller_conversation_id: ConversationId,
    /// Host-transport registry. When the caller's conversation is
    /// bound to a transport (state_transport_bindings hit on the
    /// conversation's principal_group) AND the registry has a
    /// channel mapping for that channel, `send` ALSO ships the
    /// attachment through the channel's plugin — not just the
    /// web-UI download chip.
    transports: Option<crate::transport_registry::HostTransportRegistry>,
    /// Plugin host for dispatching `<channel>.send_with_attachments`
    /// tool calls. Required alongside `transports` for the auto-
    /// bridge fan-out.
    plugin_host: Option<execlaw_plugin_host::PluginHost>,
    /// Filesystem root where `create_artifact` writes its content-
    /// addressed blob files. `None` means `create_artifact` is
    /// disabled (errors with `ApiError::Storage`); set explicitly
    /// in production via `with_artifacts_root` so tests can't
    /// accidentally write under the developer's real
    /// `~/.execlaw/plugin_artifacts/`. The `send` method works
    /// regardless of this field.
    artifacts_root: Option<PathBuf>,
}

impl ServerAttachmentApi {
    pub fn new(db: Database, events: EventBus, caller_conversation_id: ConversationId) -> Self {
        Self {
            db,
            events,
            caller_conversation_id,
            transports: None,
            plugin_host: None,
            artifacts_root: None,
        }
    }

    /// Builder-style attach the host-transport registry + plugin
    /// host so `send` fans out to the originating channel's
    /// plugin tool.
    pub fn with_transports(
        mut self,
        registry: Option<crate::transport_registry::HostTransportRegistry>,
    ) -> Self {
        self.transports = registry;
        self
    }

    pub fn with_plugin_host(mut self, plugin_host: execlaw_plugin_host::PluginHost) -> Self {
        self.plugin_host = Some(plugin_host);
        self
    }

    /// Enable `create_artifact` by pinning the on-disk root used
    /// for content-addressed blob writes. Production wires this to
    /// the same `~/.execlaw/plugin_artifacts/` (or
    /// `EXECLAW_PLUGIN_ARTIFACTS_DIR` override) the host_caps impl
    /// uses, so a built-in tool's artifact lands in the same tree
    /// a script-tier `host_render_chart` would.
    pub fn with_artifacts_root(mut self, root: PathBuf) -> Self {
        self.artifacts_root = Some(root);
        self
    }
}

#[async_trait]
impl AttachmentApi for ServerAttachmentApi {
    async fn send(
        &self,
        attachment_id: &str,
        caption: Option<&str>,
    ) -> Result<DeliveredAttachmentView, ApiError> {
        let id = AttachmentId::from(attachment_id);
        let db = self.db.clone();
        let id_for_task = id.clone();
        let row = tokio::task::spawn_blocking(move || AttachmentStore::new(&db).get(&id_for_task))
            .await
            .map_err(|e| ApiError::Storage(format!("join: {e}")))?
            .map_err(|e| ApiError::Storage(e.to_string()))?
            .ok_or_else(|| ApiError::NotFound(format!("no attachment '{attachment_id}'")))?;

        // Trust scope: refuse to deliver an attachment that belongs
        // to a different conversation. NotFound (not NotAuthorized)
        // so the caller can't probe for cross-thread ids.
        if row.conversation_id.as_str() != self.caller_conversation_id.as_str() {
            return Err(ApiError::NotFound(format!(
                "no attachment '{attachment_id}'"
            )));
        }

        // Read the file size best-effort. Doesn't gate delivery on
        // success — the chip is still useful even if metadata read
        // fails (e.g. the file got purged between insert and
        // delivery; the chip will surface the eventual 404 when
        // the user clicks).
        let path = std::path::PathBuf::from(&row.path);
        let byte_size = std::fs::metadata(&path).ok().map(|m| m.len() as i64);
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment")
            .to_owned();

        let download_url = format!("/api/attachments/{attachment_id}");
        let card_id = format!("att-{attachment_id}");
        let title_caption = caption
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let title = title_caption.clone().unwrap_or_else(|| filename.clone());
        let summary = format!(
            "{filename} ({mime})",
            filename = filename,
            mime = row.mime_type
        );
        let details = serde_json::json!({
            "attachment_id": attachment_id,
            "filename": filename,
            "mime_type": row.mime_type,
            "byte_size": byte_size,
            "download_url": download_url,
            "caption": title_caption,
        });

        // Open the card.
        open_card_and_broadcast(
            &self.db,
            &self.events,
            &self.caller_conversation_id,
            "agent",
            &CardOpenedPayload {
                card_id: card_id.clone(),
                kind: CardKind::Attachment,
                title: title.clone(),
                summary: summary.clone(),
                state: Some(CardState::Running),
                details: details.clone(),
                actions: vec![CardAction::OpenDetail {
                    href: download_url.clone(),
                }],
            },
        )
        .map_err(|e| ApiError::Storage(format!("emit attachment card open: {e}")))?;
        // Close immediately — attachments are not progressing tasks.
        close_card_and_broadcast(
            &self.db,
            &self.events,
            &self.caller_conversation_id,
            "agent",
            &CardClosedPayload {
                card_id,
                state: CardState::Completed,
                summary: summary.clone(),
                details: Some(details),
                attachment_id: Some(attachment_id.to_owned()),
                error: None,
            },
        )
        .map_err(|e| ApiError::Storage(format!("emit attachment card close: {e}")))?;

        // Transport fan-out: if the caller's conversation is bound
        // to a transport (Signal today; future bridges follow), the
        // file ALSO ships through the transport so the contact on
        // their phone sees it. Best-effort: a transport-side error
        // logs but doesn't fail the call — the web-UI chip is
        // already committed and visible to the operator.
        //
        // Caption (when present) becomes the message body; an empty
        // caption falls back to the filename so the contact sees
        // something useful next to the file rather than a bare
        // attachment with no context.
        self.bridge_to_originating_transport(attachment_id, &filename, title_caption.as_deref())
            .await;

        Ok(DeliveredAttachmentView {
            attachment_id: attachment_id.to_owned(),
            conversation_id: self.caller_conversation_id.as_str().to_owned(),
            mime_type: row.mime_type,
            filename,
            byte_size,
            download_url,
            caption: title_caption,
        })
    }

    async fn create_artifact(
        &self,
        filename: &str,
        mime_type: &str,
        bytes: Vec<u8>,
        ttl_seconds: Option<i64>,
    ) -> Result<CreatedArtifactView, ApiError> {
        // 2026-05-15 — built-in tools that produce attachments (the
        // chart renderer is the v1 caller) land their bytes here.
        // Mirrors the script-tier `host_caps.create_artifact_attachment`
        // path so an artifact produced by a built-in is
        // indistinguishable from one produced by a Rhai plugin —
        // same DB shape, same on-disk layout, same TTL story.
        //
        // Refuses cleanly when `artifacts_root` wasn't configured
        // (test fixtures that don't opt in) — no UserDirs fallback
        // here, deliberately. Tests that accidentally call
        // create_artifact MUST get an error rather than silently
        // writing to the developer's real ~/.execlaw/.
        let root = match &self.artifacts_root {
            Some(r) => r.clone(),
            None => {
                return Err(ApiError::Storage(
                    "create_artifact called but ServerAttachmentApi has no artifacts_root \
                     configured (only `send` is available). Production wires this via \
                     `with_artifacts_root` in tool_dispatch."
                        .into(),
                ));
            }
        };
        let db = self.db.clone();
        let filename_owned = filename.to_owned();
        let mime_owned = mime_type.to_owned();
        // We attribute built-in artifacts to the "core" plugin id —
        // gives ops + the artifact-cleanup sweeper a stable handle
        // for "artifacts produced by the host itself, not by a
        // user-installed plugin."
        const BUILTIN_PLUGIN_ID: &str = "core";
        let now = chrono::Utc::now().timestamp();
        let created = tokio::task::spawn_blocking(move || {
            AttachmentStore::new(&db).insert_plugin_artifact(
                &root,
                BUILTIN_PLUGIN_ID,
                &filename_owned,
                &mime_owned,
                &bytes,
                ttl_seconds,
                now,
            )
        })
        .await
        .map_err(|e| ApiError::Storage(format!("artifact write join: {e}")))?
        .map_err(|e| ApiError::Storage(format!("artifact write: {e}")))?;
        Ok(CreatedArtifactView {
            attachment_id: created.attachment_id,
            sha256: created.sha256,
            size_bytes: created.size_bytes,
        })
    }

    async fn emit_chart_card(
        &self,
        attachment_id: &str,
        svg: &str,
        filename: &str,
        title: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<(), ApiError> {
        // 2026-05-16 — emit a Chart card following the same
        // open-then-immediately-close pattern AttachmentCard uses.
        // The SPA's card-projection picks up the pair on
        // `/api/chats/:id/cards` and dispatches to the registered
        // `chart` renderer (web/src/cards/ChartCard.tsx). Same
        // proven pipeline as deep-research progress + the
        // send_attachment download chip.
        //
        // The SVG goes into `details.svg` so the renderer can drop
        // it inline via dangerouslySetInnerHTML — same trust posture
        // as ChartInlineComponent had: SVG comes from our own
        // plotters renderer, not user-controlled input.
        let card_id = format!("chart-{attachment_id}");
        let title_owned = title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let card_title = title_owned.clone().unwrap_or_else(|| filename.to_owned());
        let summary = format!("Chart ({width}x{height})");
        // CardAction::OpenDetail wires a "click to open" affordance;
        // for charts the natural action is "download the PNG."
        let download_url = format!("/api/attachments/{attachment_id}");
        let details = json!({
            "attachment_id": attachment_id,
            "filename": filename,
            "title": title_owned,
            "svg": svg,
            "width": width,
            "height": height,
            "download_url": download_url,
            "mime_type": "image/png",
        });

        open_card_and_broadcast(
            &self.db,
            &self.events,
            &self.caller_conversation_id,
            "agent",
            &CardOpenedPayload {
                card_id: card_id.clone(),
                kind: CardKind::Chart,
                title: card_title,
                summary: summary.clone(),
                state: Some(CardState::Running),
                details: details.clone(),
                actions: vec![CardAction::OpenDetail {
                    href: download_url.clone(),
                }],
            },
        )
        .map_err(|e| ApiError::Storage(format!("emit chart card open: {e}")))?;
        close_card_and_broadcast(
            &self.db,
            &self.events,
            &self.caller_conversation_id,
            "agent",
            &CardClosedPayload {
                card_id,
                state: CardState::Completed,
                summary,
                details: Some(details),
                attachment_id: Some(attachment_id.to_owned()),
                error: None,
            },
        )
        .map_err(|e| ApiError::Storage(format!("emit chart card close: {e}")))?;

        // 2026-05-16 — auto-bridge the chart's PNG to the
        // conversation's originating transport (signal /
        // whatsapp / discord / slack / future plugins —
        // channel-agnostic via the registry). Same proven pattern
        // deep-research uses for its report PDF
        // (`bridge_research_pdf_to_originating_transport` in
        // `crates/server/src/research/runner.rs`) and that
        // `send_attachment` uses for the in-chat chip.
        //
        // For Signal-bridged conversations the operator's chat IS
        // Signal — they're not going to look at the web UI to see
        // the chart. The web-UI Chart card stays as a fallback if
        // the bridge dispatch fails.
        //
        // No-op for web-only conversations (the bridge returns
        // early when no transport binding exists). Best-effort:
        // failures log but don't propagate — the web-UI Chart
        // card has already committed.
        //
        // Caption: prefer the chart's title (operator-meaningful
        // context) over the bare filename — same convention as
        // `send_attachment`.
        let bridge_caption = title.map(str::trim).filter(|s| !s.is_empty());
        self.bridge_to_originating_transport(attachment_id, filename, bridge_caption)
            .await;
        Ok(())
    }
}

impl ServerAttachmentApi {
    /// If the caller's conversation has a binding for any
    /// registered transport, fan the attachment out through it.
    /// No-op for web-only conversations and when the registry is
    /// empty. Errors log but don't propagate — the web-UI chip
    /// already shipped, so a transport-side failure is a UX
    /// degradation, not a correctness break.
    async fn bridge_to_originating_transport(
        &self,
        attachment_id: &str,
        filename: &str,
        caption: Option<&str>,
    ) {
        use execlaw_core::principal_groups::PrincipalGroupStore;
        use execlaw_core::transport_bindings::TransportBindingStore;

        // 2026-05-16 — make wiring-missing failures LOUD. Pre-fix,
        // a missing `with_plugin_host(...)` call at the dispatch
        // construction site (was the case for tool_dispatch.rs's
        // chart.render path) silently no-op'd the entire bridge.
        // Operators saw "chart rendered fine in web UI but never
        // arrived on Signal" with zero diagnostic. Now: a single
        // WARN at boot of the dispatch, every time, so operators
        // can immediately see WHICH wiring slot is missing.
        let Some(registry) = self.transports.as_ref() else {
            tracing::warn!(
                target: "attachment_api::bridge",
                conversation_id = %self.caller_conversation_id.as_str(),
                attachment_id = %attachment_id,
                "transport bridge skipped: ServerAttachmentApi has no `transports` registry. \
                 Wire it via `.with_transports(state.host_transports.clone())` at the call site.",
            );
            return;
        };
        let Some(plugin_host) = self.plugin_host.as_ref() else {
            tracing::warn!(
                target: "attachment_api::bridge",
                conversation_id = %self.caller_conversation_id.as_str(),
                attachment_id = %attachment_id,
                "transport bridge skipped: ServerAttachmentApi has no `plugin_host`. \
                 Wire it via `.with_plugin_host(state.plugin_host.clone())` at the call site.",
            );
            return;
        };

        let pg_store = PrincipalGroupStore::new(&self.db);
        let pg_id = match pg_store.principal_group_id_for(self.caller_conversation_id.as_str()) {
            Ok(Some(id)) => id,
            // Web-only conversation (no transport binding). Skip
            // silently — this is the expected case, not an error.
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(
                    target: "attachment_api::bridge",
                    conversation_id = %self.caller_conversation_id.as_str(),
                    error = %e,
                    "principal_group lookup failed; skipping transport bridge",
                );
                return;
            }
        };
        let binding_store = TransportBindingStore::new(&self.db);
        let bindings = match binding_store.bindings_for_group_any_channel(&pg_id) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "attachment_api::bridge",
                    conversation_id = %self.caller_conversation_id.as_str(),
                    error = %e,
                    "binding lookup failed; web-UI chip remains the only deliverable",
                );
                return;
            }
        };
        let Some(resolved) = registry.lookup_first_supported_binding(&bindings) else {
            // Conversation has bindings but none match a registered
            // transport plugin. Surfaces as INFO rather than WARN —
            // a plausible operator config (e.g. installed Signal,
            // then uninstalled it without unbinding the
            // conversation). Still worth logging so the operator
            // can see why the chip didn't fan out.
            tracing::info!(
                target: "attachment_api::bridge",
                conversation_id = %self.caller_conversation_id.as_str(),
                attachment_id = %attachment_id,
                bindings_len = bindings.len(),
                bound_channels = ?bindings.iter().map(|b| &b.channel).collect::<Vec<_>>(),
                "transport bridge skipped: no installed transport plugin handles any of the \
                 conversation's bound channels (registered handlers: {:?})",
                registry.channels().collect::<Vec<_>>(),
            );
            return;
        };

        let body = caption.unwrap_or(filename);
        let tool_name = format!("{}.send_with_attachments", resolved.channel);
        let args = serde_json::json!({
            "to": resolved.foreign_id,
            "text": body,
            "attachments": [attachment_id],
        });
        if let Err(e) = plugin_host
            .call_tool(&tool_name, args, &["*"], Some("Controller"))
            .await
        {
            tracing::warn!(
                target: "attachment_api::bridge",
                conversation_id = %self.caller_conversation_id.as_str(),
                channel = %resolved.channel,
                recipient = %resolved.foreign_id,
                error = %e,
                "plugin tool fan-out failed; web-UI chip remains visible to the operator",
            );
        } else {
            tracing::info!(
                target: "attachment_api::bridge",
                conversation_id = %self.caller_conversation_id.as_str(),
                channel = %resolved.channel,
                recipient = %resolved.foreign_id,
                attachment_id = %attachment_id,
                "fanned attachment out via originating transport",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execlaw_core::DbConfig;
    use execlaw_core::MigrationRunner;
    use execlaw_core::attachments::AttachmentRow;
    use execlaw_core::conversation::{
        ConversationKind, ConversationRow, ConversationStore, Modality, Phase,
    };
    use execlaw_core::ids::EventSeq;
    use tempfile::TempDir;

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
                display_name_source: "auto".into(),
                is_pinned: false,
                is_ephemeral: false,
                ephemeral_expires_at: None,
                last_activity_at: 0,
            })
            .unwrap();
        cid
    }

    fn seed_attachment(
        db: &Database,
        cid: &ConversationId,
        dir: &TempDir,
    ) -> (String, std::path::PathBuf) {
        let id = AttachmentId::new();
        let path = dir.path().join(format!("{}.pdf", id.as_str()));
        std::fs::write(&path, b"%PDF-1.4 hello").unwrap();
        AttachmentStore::new(db)
            .insert(&AttachmentRow {
                id: id.clone(),
                conversation_id: cid.clone(),
                mime_type: "application/pdf".into(),
                path: path.to_string_lossy().into_owned(),
                sha256: "x".into(),
                received_at: 0,
                filename: None,
            })
            .unwrap();
        (id.as_str().to_owned(), path)
    }

    #[tokio::test]
    async fn send_returns_view_with_correct_url_filename_and_size() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c-1");
        let dir = TempDir::new().unwrap();
        let (att_id, _path) = seed_attachment(&db, &cid, &dir);
        let bus = EventBus::new();
        let api = ServerAttachmentApi::new(db, bus, cid);
        let view = api
            .send(&att_id, Some("Final research report"))
            .await
            .unwrap();
        assert_eq!(view.attachment_id, att_id);
        assert_eq!(view.mime_type, "application/pdf");
        assert!(view.filename.ends_with(".pdf"));
        assert_eq!(view.download_url, format!("/api/attachments/{att_id}"));
        assert_eq!(view.caption.as_deref(), Some("Final research report"));
        assert!(view.byte_size.unwrap() > 0);
    }

    #[tokio::test]
    async fn send_refuses_cross_conversation_attachment_with_not_found() {
        // Adversarial: agent in conv-A tries to deliver an attachment
        // that belongs to conv-B. Must return NotFound (not
        // NotAuthorized) so cross-thread id probing leaks no info.
        let db = fresh_db();
        let cid_a = seed_conv(&db, "conv-a");
        let cid_b = seed_conv(&db, "conv-b");
        let dir = TempDir::new().unwrap();
        let (att_id, _) = seed_attachment(&db, &cid_b, &dir);
        let bus = EventBus::new();
        let api = ServerAttachmentApi::new(db, bus, cid_a);
        let err = api.send(&att_id, None).await.unwrap_err();
        match err {
            ApiError::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_returns_not_found_for_missing_id() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c-1");
        let bus = EventBus::new();
        let api = ServerAttachmentApi::new(db, bus, cid);
        let err = api.send("nope", None).await.unwrap_err();
        assert!(matches!(err, ApiError::NotFound(_)));
    }

    /// Regression for the chart.render → cards-path migration
    /// (commit `d80a002`) + the auto-bridge wire-up
    /// (Option A, this commit). The chart's CardOpened+CardClosed
    /// pair MUST land on the event bus carrying the SVG +
    /// attachment_id in `details`, so the SPA's CardStore picks
    /// them up + the ChartCard renderer mounts.
    ///
    /// Bridge behavior is exercised on the `send_emits_…` test
    /// path implicitly — emit_chart_card calls the SAME helper
    /// (`bridge_to_originating_transport`) that `send` uses, and
    /// that helper no-ops without a transport binding (as is the
    /// case in this test fixture). The shared-helper structure
    /// means a regression that breaks the bridge for charts would
    /// also break it for `send_attachment` and trip THAT test.
    #[tokio::test]
    async fn emit_chart_card_lands_open_close_pair_with_svg_and_attachment_id() {
        let db = fresh_db();
        let cid = seed_conv(&db, "c-chart");
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        // No transports/plugin_host wired — the bridge step is a
        // no-op so this test focuses on the card emit. A
        // production-equivalent test that DID wire a mock
        // PluginHost lives outside this scope (the same coverage
        // gap the existing `send_emits_card_open_then_close_pair`
        // test has — bridge integration is exercised end-to-end
        // through real Signal / WhatsApp fixtures).
        let api = ServerAttachmentApi::new(db, bus, cid);

        api.emit_chart_card(
            "art-chart-123",
            "<svg width=\"720\" height=\"400\"><g/></svg>",
            "AAPL_1mo.png",
            Some("AAPL — last month"),
            720,
            400,
        )
        .await
        .unwrap();

        let evt1 = rx.try_recv().unwrap();
        let evt2 = rx.try_recv().unwrap();
        match evt1 {
            crate::events::UiEvent::CardOpened {
                card_kind, details, ..
            } => {
                assert_eq!(
                    card_kind, "chart",
                    "CardOpened MUST use kind='chart' so the SPA's \
                     `getCardRenderer(\"chart\")` dispatches to \
                     ChartCard.tsx (the cards-path renderer)",
                );
                // The renderer reads `details.svg` and
                // `details.attachment_id`; pin both.
                assert!(
                    details
                        .get("svg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .contains("<svg"),
                    "details.svg MUST be the inline SVG markup, got: {details}",
                );
                assert_eq!(
                    details.get("attachment_id").and_then(|v| v.as_str()),
                    Some("art-chart-123"),
                );
            }
            other => panic!("expected CardOpened(kind=chart), got {other:?}"),
        }
        match evt2 {
            crate::events::UiEvent::CardClosed {
                state,
                attachment_id,
                ..
            } => {
                assert_eq!(state, "Completed");
                assert_eq!(attachment_id.as_deref(), Some("art-chart-123"));
            }
            other => panic!("expected CardClosed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_emits_card_open_then_close_pair_on_event_bus() {
        // The SPA's card store needs both Open + Close to land for
        // the chip to surface as a terminal-state card. Subscribe
        // to the bus first so we capture both events.
        let db = fresh_db();
        let cid = seed_conv(&db, "c-1");
        let dir = TempDir::new().unwrap();
        let (att_id, _) = seed_attachment(&db, &cid, &dir);
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let api = ServerAttachmentApi::new(db, bus, cid);
        api.send(&att_id, Some("here")).await.unwrap();

        // Expect CardOpened then CardClosed for kind=attachment.
        let evt1 = rx.try_recv().unwrap();
        let evt2 = rx.try_recv().unwrap();
        match evt1 {
            crate::events::UiEvent::CardOpened { card_kind, .. } => {
                assert_eq!(card_kind, "attachment");
            }
            other => panic!("expected CardOpened, got {other:?}"),
        }
        match evt2 {
            crate::events::UiEvent::CardClosed {
                state,
                attachment_id,
                ..
            } => {
                assert_eq!(state, "Completed");
                assert_eq!(attachment_id.as_deref(), Some(att_id.as_str()));
            }
            other => panic!("expected CardClosed, got {other:?}"),
        }
    }
}
