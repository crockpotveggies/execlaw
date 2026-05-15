//! Channel-agnostic inbound routing entry point.
//!
//! Every transport plugin's frame decoder hands a decoded
//! [`InboundMessage`] here. The host owns:
//!
//!   1. Trust admit (mint principal if new, refresh if existing).
//!   2. Group binding lookup / mint.
//!   3. Conversation resolve / mint.
//!   4. Auto-rename via `apply_auto_display_name`.
//!   5. Cold-contact gate (UnknownPending senders → approval flow).
//!   6. Group-address classifier (skip dispatch for unaddressed
//!      group messages, persist them anyway).
//!   7. Turn dispatch via `dispatch_external_turn`.
//!
//! This is the channel-agnostic equivalent of
//! `signal_inbound::route_group_inbound` /
//! `route_inbound_message`. Phase B (signal plugin migration)
//! will fold those into this single router; for now both paths
//! coexist behind the new Rhai binding.

use crate::state::AppState;
use execlaw_core::ids::{ConversationId, PrincipalId};
use execlaw_core::principal::{Identifier, PrincipalStore, TrustLevel as CoreTrustLevel};
use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};
use execlaw_core::transport_bindings::TransportBindingStore;
use execlaw_core::transport_conversations::{ConversationResolver, ResolveInput};
use execlaw_policy::trust::TrustLevel;
use execlaw_script::{HostCapError, InboundMessage, RouteOutcome};

/// Generic inbound routing — no Signal-specific code.
pub async fn route_inbound(
    state: &AppState,
    msg: InboundMessage,
) -> Result<RouteOutcome, HostCapError> {
    let now = chrono::Utc::now().timestamp();
    let channel = msg.channel.as_str();
    let plugin_id = format!("plugin-{channel}"); // ConversationResolver routing key

    // 1. Resolve / mint the sender's principal via the shared
    //    admit helper. Same shape `signal_inbound` uses today.
    let hint_pid = PrincipalId::from(format!("pri_{channel}_{native}", native = msg.native_id));
    let (sender, _flat_trust) = crate::principal_admit::admit_external_principal(
        &state.db,
        &state.plugin_host,
        channel,
        &msg.native_id,
        hint_pid.as_str(),
    )
    .await
    .map_err(|e| HostCapError::new(format!("admit principal: {e}")))?;

    // Refresh last_seen on the principal row regardless of whether
    // admit minted it or returned an existing one.
    {
        let mut updated = sender.clone();
        updated.last_seen = Some(now);
        let _ = PrincipalStore::new(&state.db).upsert(&updated);
    }

    // 2. Branch on group vs DM. The two shapes are similar enough
    //    that one function handles both.
    let (cid, principal_group_id) = if let Some(gid) = msg.group_id.as_deref() {
        resolve_group(state, channel, gid, &plugin_id, now).await?
    } else {
        resolve_dm(state, channel, &msg.native_id, &sender, &plugin_id, now).await?
    };

    // 3. Conversation row + binding.
    crate::chats::ensure_conversation_for(&state.db, &cid);
    let display_name_for_seed = if msg.group_id.is_some() {
        msg.group_name.as_deref()
    } else {
        msg.display_name.as_deref()
    };
    crate::chats::apply_auto_display_name(&state.db, &cid, display_name_for_seed);
    let pg_store = PrincipalGroupStore::new(&state.db);
    pg_store
        .bind_conversation(cid.as_str(), &principal_group_id)
        .map_err(|e| HostCapError::new(format!("bind conversation: {e}")))?;

    // 3b. For groups, opportunistically grow the principal_group's
    // member list as senders appear. `resolve_group` mints with
    // empty `principals: &[]` because the transport plugins
    // (Signal / WhatsApp / Slack) only know the *current* sender
    // + the group JID on each inbound — none of them include the
    // full roster in their per-message wire format. Without this
    // step the member table stays empty forever, and
    // `should_dispatch_to_agent`'s eligibility gate
    // (`members.len() < 2`) returns `EligibilityBypass` on every
    // turn — the classifier never runs. That's the root cause of
    // "the agent keeps barging into group conversations."
    //
    // We seed two principal_ids per group inbound:
    //   * the observed sender (so subsequent messages from the
    //     same person reuse the row and we accumulate distinct
    //     senders over time)
    //   * the controller (always implicitly in the group — the
    //     operator owns the bridged WhatsApp/Signal/Slack identity
    //     — but never sends inbound there, so otherwise wouldn't
    //     get added)
    //
    // Together these bring `members.len()` to ≥2 as soon as the
    // first non-Controller speaks, which is exactly when the
    // addressing classifier becomes useful.
    if msg.group_id.is_some() {
        if let Err(e) = pg_store.add_member(&principal_group_id, &sender.id, now) {
            tracing::warn!(
                target: "generic_inbound",
                error = %e,
                group_id = %principal_group_id,
                principal_id = %sender.id.as_str(),
                "could not add sender to group membership; addressing classifier may bypass on this turn",
            );
        }
        match crate::routes::controller_principal_id(&state.db) {
            Ok(controller_pid) => {
                if let Err(e) = pg_store.add_member(&principal_group_id, &controller_pid, now) {
                    tracing::warn!(
                        target: "generic_inbound",
                        error = %e,
                        group_id = %principal_group_id,
                        "could not add controller to group membership",
                    );
                }
            }
            Err(e) => {
                // Fresh install before bootstrap finishes can land
                // here. Not fatal — the next inbound after
                // bootstrap will succeed.
                tracing::debug!(
                    target: "generic_inbound",
                    error = ?e,
                    "controller_principal_id unavailable; skipping controller-membership seed",
                );
            }
        }
    }

    // 4. Trust gate.
    let trust_tag = sender.trust_level.class_tag();
    let trust_flat = TrustLevel::parse(trust_tag).unwrap_or(TrustLevel::UnknownPending);

    if trust_flat == TrustLevel::Blocked {
        return Ok(RouteOutcome::Blocked);
    }

    if trust_flat == TrustLevel::UnknownPending {
        crate::chats::handle_cold_contact_for_inbound(state, &cid, &sender, &msg.text)
            .await
            .map_err(|e| HostCapError::new(format!("cold-contact handler: {e}")))?;
        return Ok(RouteOutcome::ColdContact);
    }

    // 5. Inbound image attachments. Fetch + persist BEFORE the
    // group-addressing check so:
    //   (a) silent-commit paths (unaddressed group messages) still
    //       carry the attachment refs through to the SPA's bubble —
    //       operator sees the photo even though no turn ran;
    //   (b) the agent dispatch path has the same `attachment_ids`
    //       handle the web composer's `+` flow produces.
    //
    // Non-image attachments (PDFs, audio, video, etc.) are skipped
    // by `persist_inbound_attachments`; vision models can't see
    // them and a follow-up PR will add per-kind preprocessors
    // (whisper for audio, text extraction for PDFs).
    //
    // Failure to fetch any single attachment doesn't fail the
    // turn — the helper logs at WARN and continues.
    let attachment_ids: Vec<String> =
        crate::chats::persist_inbound_attachments(state, &cid, channel, &msg.attachments).await;

    // 6. Group address filter + group-context resolution. For DMs
    // we leave `group_context = None` and dispatch directly. For
    // groups we consult the addressing classifier; on Skip we
    // silent-commit; on Dispatch we build the per-turn group
    // context (name + member count + addressed reason) so the
    // agent's system prompt knows it's in a group AND why this
    // message reached it.
    //
    // 2026-05-15 — image attachments shortcut the classifier: a
    // group member sending the agent a photo is almost always
    // intentionally addressing the agent (text-only banter
    // doesn't normally include media), and image-only messages
    // have empty text the classifier would otherwise filter out
    // every time.
    let has_image_attachment = !attachment_ids.is_empty();
    let group_context: Option<crate::chats::GroupTurnContext> = if msg.group_id.is_some() {
        if has_image_attachment {
            // Skip the classifier; treat as addressed via the
            // attachment signal.
            crate::chats::resolve_group_turn_context(
                state,
                &cid,
                crate::group_addressing::AddressedReason::AttachmentDirected,
            )
        } else {
            let decision = crate::group_addressing::should_dispatch_to_agent(
                state,
                &cid,
                &msg.text,
                msg.mention_of_self,
            )
            .await;
            match decision {
                crate::group_addressing::DispatchDecision::Skip => {
                    // Persist for context; skip dispatch.
                    if let Err(e) = crate::chats::commit_inbound_user_msg_silently(
                        state,
                        &cid,
                        sender.id.as_str(),
                        &msg.text,
                        channel,
                        attachment_ids.clone(),
                    )
                    .await
                    {
                        tracing::warn!(
                            target: "generic_inbound",
                            error = %e,
                            conversation_id = %cid.as_str(),
                            "silent commit of unaddressed group message failed",
                        );
                    }
                    return Ok(RouteOutcome::GroupNotAddressed);
                }
                crate::group_addressing::DispatchDecision::Dispatch(reason) => {
                    // The classifier already ran the eligibility +
                    // members lookups internally. The resolver here
                    // re-runs the cheap members/name lookups to build
                    // the per-turn context — duplicate cost is small
                    // (two SQLite reads on indexed tables) and keeping
                    // the resolver self-contained simplifies the
                    // approval-replay paths that don't have a verdict
                    // to thread.
                    crate::chats::resolve_group_turn_context(state, &cid, reason)
                }
            }
        }
    } else {
        None
    };

    // 7. Dispatch the turn through the standard pipeline.
    crate::chats::dispatch_external_turn(
        state,
        &cid,
        &sender,
        trust_flat,
        &msg.text,
        Some(channel),
        group_context,
        attachment_ids,
    )
    .await
    .map_err(|e| HostCapError::new(format!("dispatch_external_turn: {e}")))?;
    Ok(RouteOutcome::Dispatched)
}

async fn resolve_group(
    state: &AppState,
    channel: &str,
    group_id: &str,
    plugin_id: &str,
    now: i64,
) -> Result<(ConversationId, String), HostCapError> {
    let binding_store = TransportBindingStore::new(&state.db);
    let pg_store = PrincipalGroupStore::new(&state.db);
    let group_pg_id = match binding_store
        .lookup_principal_group(channel, group_id)
        .map_err(|e| HostCapError::new(format!("group binding lookup: {e}")))?
    {
        Some(pg_id) => pg_id,
        None => {
            let pg = pg_store
                .resolve(
                    &GroupKey {
                        channel,
                        native_group_id: Some(group_id),
                        principals: &[],
                        includes_controller: true,
                    },
                    now,
                )
                .map_err(|e| HostCapError::new(format!("group principal_group mint: {e}")))?;
            let inserted = binding_store
                .insert_binding(channel, group_id, &pg.group_id, true, now)
                .map_err(|e| HostCapError::new(format!("group binding insert: {e}")))?;
            if !inserted {
                binding_store
                    .lookup_principal_group(channel, group_id)
                    .map_err(|e| HostCapError::new(format!("group binding re-lookup: {e}")))?
                    .ok_or_else(|| HostCapError::new("group binding vanished after insert race"))?
            } else {
                pg.group_id
            }
        }
    };
    let resolver = ConversationResolver::new(&state.db);
    let outcome = resolver
        .resolve_or_mint(&ResolveInput {
            plugin_id,
            transport_handle: group_id,
            principal_id: group_id,
            is_controller: false,
            idle_timeout_ms: 30 * 60 * 1000,
            now,
        })
        .map_err(|e| HostCapError::new(format!("group conversation resolve: {e}")))?;
    Ok((outcome.conversation_id().clone(), group_pg_id))
}

async fn resolve_dm(
    state: &AppState,
    channel: &str,
    native_id: &str,
    sender: &execlaw_core::principal::Principal,
    plugin_id: &str,
    now: i64,
) -> Result<(ConversationId, String), HostCapError> {
    let binding_store = TransportBindingStore::new(&state.db);
    let pg_store = PrincipalGroupStore::new(&state.db);
    let principal_group_id = match binding_store
        .lookup_principal_group(channel, native_id)
        .map_err(|e| HostCapError::new(format!("binding lookup: {e}")))?
    {
        Some(pg_id) => pg_id,
        None => {
            let pid_array = [sender.id.clone()];
            let pg = pg_store
                .resolve(
                    &GroupKey {
                        channel,
                        native_group_id: None,
                        principals: &pid_array,
                        includes_controller: matches!(
                            sender.trust_level,
                            CoreTrustLevel::Controller
                        ),
                    },
                    now,
                )
                .map_err(|e| HostCapError::new(format!("principal_group mint: {e}")))?;
            let _ = binding_store
                .insert_binding(channel, native_id, &pg.group_id, false, now)
                .map_err(|e| HostCapError::new(format!("binding insert: {e}")))?;
            // Identifier upsert so future lookups by handle resolve.
            let mut updated = sender.clone();
            let ident = Identifier {
                transport: channel.to_owned(),
                handle: native_id.to_owned(),
            };
            if !updated
                .identifiers
                .iter()
                .any(|i| i.transport == ident.transport && i.handle == ident.handle)
            {
                updated.identifiers.push(ident);
                let _ = PrincipalStore::new(&state.db).upsert(&updated);
            }
            pg.group_id
        }
    };
    let is_controller = matches!(sender.trust_level, CoreTrustLevel::Controller);
    let resolver = ConversationResolver::new(&state.db);
    let outcome = resolver
        .resolve_or_mint(&ResolveInput {
            plugin_id,
            transport_handle: native_id,
            principal_id: sender.id.as_str(),
            is_controller,
            idle_timeout_ms: 30 * 60 * 1000,
            now,
        })
        .map_err(|e| HostCapError::new(format!("conversation resolve: {e}")))?;
    Ok((outcome.conversation_id().clone(), principal_group_id))
}
