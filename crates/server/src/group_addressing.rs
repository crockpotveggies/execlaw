//! Channel-agnostic group inbound routing decisions.
//!
//! Every transport that delivers messages into multi-participant
//! conversations (Signal groups today; future WhatsApp groups,
//! email threads, SMS group MMS, Discord channels...) faces the
//! same question: *should the agent answer this message?* Bots in
//! groups that always answer are universally annoying — operators
//! consistently report "the bot keeps replying to messages clearly
//! aimed at someone else."
//!
//! This module is the answer: a single entry point
//! [`should_dispatch_to_agent`] that any transport's group-routing
//! path calls before invoking [`crate::chats::dispatch_external_turn`].
//! It encapsulates two layers of decision:
//!
//! 1. **Eligibility gate** — does this conversation actually need
//!    classification? A 1:1 conversation (controller ↔ agent only)
//!    or any conversation where the only participant is the
//!    Controller falls through to "always dispatch." We only spend
//!    inference budget on classification when there's at least one
//!    other human in the room.
//!
//! 2. **LLM classifier** — for conversations that pass the gate,
//!    consult a small/fast inference backend to decide if the
//!    inbound text is directed at the agent. The classifier knows
//!    the agent's configured display name + role from the
//!    personality store and returns a strict
//!    `{"directed": bool}` JSON verdict.
//!
//! ### Fall-open semantics
//!
//! Every uncertain path returns `true` (= dispatch the turn,
//! preserve "agent answers" behaviour). The ONLY return-`false`
//! path is a clean `{"directed": false}` verdict on a
//! group-with-external-users conversation. So a misbehaving
//! classifier degrades to "agent answers everything" rather than
//! "agent silent on everything" — the safer of the two failure
//! modes.
//!
//! ### Why a generic module instead of per-transport implementations
//!
//! - The eligibility gate (group + at least one non-Controller)
//!   is computed from `state_principal_group_members`, which is
//!   transport-agnostic. Every channel writes there.
//! - The classifier prompt + JSON-parse glue is identical across
//!   channels — duplicating it per transport invites drift.
//! - New transports get the right behaviour for free by calling
//!   `should_dispatch_to_agent` from their group-routing path.

use crate::state::AppState;
use execlaw_core::ids::ConversationId;
use execlaw_core::principal::PrincipalStore;
use execlaw_core::principal_groups::PrincipalGroupStore;

/// Decide whether the agent should answer this group inbound.
///
/// `true` → dispatch the turn through the normal pipeline.
/// `false` → skip dispatch (the caller should still persist the
/// inbound for context — see `crate::chats::commit_inbound_user_msg_silently`).
///
/// Always returns `true` for:
///   * Conversations that don't have a principal_group binding
///     (single-actor / web chat).
///   * Single-member groups (just the Controller).
///   * Groups whose only members are Controllers (rare:
///     multi-controller deployments — every participant is the
///     operator's identity).
///   * Personality-store lookup failures or dangerously-short
///     `display_name` (< 2 chars) — falls open rather than
///     silencing the agent over a config issue.
///   * Any inference / classifier error — same fall-open
///     principle.
pub async fn should_dispatch_to_agent(
    state: &AppState,
    conversation_id: &ConversationId,
    message_text: &str,
) -> bool {
    // ---- Eligibility gate -------------------------------------
    let pg_store = PrincipalGroupStore::new(&state.db);
    let pg_id = match pg_store.principal_group_id_for(conversation_id.as_str()) {
        Ok(Some(id)) => id,
        _ => {
            // Web-only / unbridged / lookup error → not subject
            // to classification. Dispatch.
            return true;
        }
    };
    let members = match pg_store.members(&pg_id) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(
                target: "group_addressing",
                error = %e,
                "principal_group members lookup failed; falling open"
            );
            return true;
        }
    };
    if members.len() < 2 {
        // Single-member group — no other humans to confuse the
        // agent with. Dispatch.
        return true;
    }
    // Are there any non-Controller participants? If every member
    // is a Controller, the message can't be misaddressed (every
    // sender is "the operator"). This catches multi-controller
    // deployments + the edge case of a "group" with just the
    // controller in it.
    let principals = PrincipalStore::new(&state.db);
    let has_non_controller = members.iter().any(|pid| {
        match principals.get(pid) {
            Ok(Some(p)) => !matches!(
                p.trust_level,
                execlaw_core::principal::TrustLevel::Controller
            ),
            // Lookup failure — assume non-controller (the safer
            // assumption: classify rather than silently
            // dispatch).
            _ => true,
        }
    });
    if !has_non_controller {
        return true;
    }

    // ---- Classifier --------------------------------------------
    let (agent_name, agent_role) = match read_agent_identity(state) {
        Some(pair) => pair,
        None => {
            // Personality unavailable / display_name too short →
            // fall open (skip-dispatch is destructive UX).
            return true;
        }
    };

    classify_via_llm(state, &agent_name, &agent_role, message_text).await
}

/// Read the agent's `display_name` + `role` from the personality
/// store's default scope. Returns `None` when the row is missing,
/// the lookup fails, or the display_name is shorter than 2 chars
/// after trim — every "I'm not sure who the agent is" path falls
/// open at the caller.
fn read_agent_identity(state: &AppState) -> Option<(String, String)> {
    use execlaw_core::personality::PersonalityStore;
    let row = PersonalityStore::new(&state.db).get_default().ok()?;
    let name = row.display_name.trim().to_owned();
    if name.chars().count() < 2 {
        return None;
    }
    Some((name, row.role))
}

/// Run the LLM classifier. Returns `true` on every uncertain path
/// (no inference, timeout, HTTP error, unparseable JSON).
async fn classify_via_llm(
    state: &AppState,
    agent_name: &str,
    agent_role: &str,
    message_text: &str,
) -> bool {
    use execlaw_core::backends::BackendPurpose;
    use execlaw_inference_api::{ChatMessage, ChatRequest, ModelId};

    // Prefer the Small backend (Haiku-class, sub-second turnaround
    // for a 32-token classification). Fall back to Standard if
    // Small isn't wired — better to spend the standard model's
    // latency budget here than to silently dispatch every group
    // message.
    let inference = state
        .inference
        .resolve(&state.db, BackendPurpose::Small)
        .or_else(|| state.inference.resolve(&state.db, BackendPurpose::Standard));
    let inference = match inference {
        Some(c) => c,
        None => {
            tracing::debug!(
                target: "group_addressing",
                "no inference backend resolved; falling open (will dispatch)"
            );
            return true;
        }
    };

    let role_phrase = if agent_role.trim().is_empty() {
        "assistant"
    } else {
        agent_role
    };
    let system_prompt = format!(
        "You are a binary classifier for a chat-bot routing layer. \
         The agent is named \"{agent_name}\" and acts as the operator's {role_phrase}. \
         A new message arrived in a group conversation with the agent and other humans. \
         Decide if THIS message is directed at the agent.\n\n\
         DIRECTED (true):\n\
         - Names the agent (\"{agent_name}, ...\", \"@{agent_name}\", \"hey {agent_name}\")\n\
         - A question or instruction only the agent could reasonably answer\n\
         - A clear reply or follow-up to something the agent just said\n\
         - Mid-conversation clarifications inside an exchange the agent started\n\n\
         NOT DIRECTED (false):\n\
         - Addresses a different person by name (\"Alice, did you...\")\n\
         - General group chatter, banter, or scheduling between other humans\n\
         - Reactions or one-word replies tied to a non-agent participant's last turn\n\
         - Messages where the agent has no obvious involvement\n\n\
         Output STRICT JSON, nothing else: {{\"directed\": true}} or {{\"directed\": false}}."
    );

    let req = ChatRequest {
        model: ModelId(state.config.model_id.clone()),
        messages: vec![
            ChatMessage::system(system_prompt),
            ChatMessage::user(format!(
                "Message: \"{}\"",
                message_text.replace('"', "\\\"")
            )),
        ],
        tools: None,
        stream: false,
        temperature: Some(0.0),
        max_tokens: Some(32),
        // Suppress Qwen's `<think>` block — the classifier needs
        // a JSON token, not a chain-of-thought monologue. Other
        // models silently ignore the field.
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
    };

    let resp = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        inference.chat_completions(&req),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::warn!(
                target: "group_addressing",
                error = %e,
                "inference error during address classification; falling open"
            );
            return true;
        }
        Err(_) => {
            tracing::warn!(
                target: "group_addressing",
                "address classifier timed out after 5s; falling open"
            );
            return true;
        }
    };

    let text = resp
        .choices
        .first()
        .and_then(|c| c.message.content.as_deref())
        .unwrap_or("");
    parse_directed_verdict(text).unwrap_or_else(|| {
        tracing::debug!(
            target: "group_addressing",
            response_preview = %&text.chars().take(120).collect::<String>(),
            "couldn't parse JSON `directed` from classifier response; falling open"
        );
        true
    })
}

/// Parse `{"directed": true|false}` (possibly wrapped in a code
/// fence or trailing prose) from a model response. Returns the
/// boolean if found, `None` otherwise.
///
/// Greedy: scans for the first `{` to the last `}` and tries to
/// parse that span as JSON. Works for most "I'm not sure but
/// here's JSON" responses. A response that contains no braces or
/// no `directed` key returns `None` — the caller falls open.
pub(crate) fn parse_directed_verdict(text: &str) -> Option<bool> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{')?;
    let end = bytes.iter().rposition(|b| *b == b'}')?;
    if end <= start {
        return None;
    }
    let span = &text[start..=end];
    let v: serde_json::Value = serde_json::from_str(span).ok()?;
    v.get("directed")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_app_state;
    use execlaw_core::ids::PrincipalId;
    use execlaw_core::principal::{Identifier, Principal, PrincipalStore, TrustLevel};
    use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};

    fn upsert_principal(state: &crate::state::AppState, id: &str, trust: TrustLevel) {
        let now = chrono::Utc::now().timestamp();
        PrincipalStore::new(&state.db)
            .upsert(&Principal {
                id: PrincipalId::from(id.to_owned()),
                identifiers: vec![Identifier {
                    transport: "test".into(),
                    handle: id.to_owned(),
                }],
                trust_level: trust,
                resolved_by: vec![],
                metadata: serde_json::json!({}),
                first_seen: now,
                last_seen: Some(now),
                controller_notes: None,
            })
            .unwrap();
    }

    fn mint_group_with_members(
        state: &crate::state::AppState,
        cid: &ConversationId,
        member_ids: &[&str],
    ) -> String {
        let now = chrono::Utc::now().timestamp();
        let pg_store = PrincipalGroupStore::new(&state.db);
        let pid_owned: Vec<PrincipalId> = member_ids
            .iter()
            .map(|s| PrincipalId::from(s.to_owned()))
            .collect();
        let pg = pg_store
            .resolve(
                &GroupKey {
                    channel: "test",
                    native_group_id: Some(cid.as_str()),
                    principals: &pid_owned,
                    includes_controller: true,
                },
                now,
            )
            .unwrap();
        // bind_conversation links cid → pg.group_id so
        // `principal_group_id_for(cid)` returns it.
        pg_store
            .bind_conversation(cid.as_str(), &pg.group_id)
            .unwrap();
        pg.group_id
    }

    #[test]
    fn parse_directed_verdict_handles_clean_json() {
        assert_eq!(parse_directed_verdict("{\"directed\": true}"), Some(true));
        assert_eq!(parse_directed_verdict("{\"directed\": false}"), Some(false));
    }

    #[test]
    fn parse_directed_verdict_strips_code_fences_and_prose() {
        // Models often wrap JSON in code fences or trailing prose
        // even when told not to. The greedy `{...}` extractor
        // rescues the verdict.
        assert_eq!(
            parse_directed_verdict("```json\n{\"directed\": false}\n```"),
            Some(false)
        );
        assert_eq!(
            parse_directed_verdict(
                "Here is the verdict:\n{\"directed\": true}\nThanks!"
            ),
            Some(true)
        );
    }

    #[test]
    fn parse_directed_verdict_returns_none_for_garbage() {
        assert!(parse_directed_verdict("").is_none());
        assert!(parse_directed_verdict("yes").is_none());
        assert!(parse_directed_verdict("not json at all").is_none());
        assert!(parse_directed_verdict("{\"unrelated\": 1}").is_none());
        // Numeric `directed` doesn't satisfy `as_bool`.
        assert!(parse_directed_verdict("{\"directed\": 1}").is_none());
    }

    #[test]
    fn parse_directed_verdict_picks_outermost_braces() {
        assert_eq!(
            parse_directed_verdict("{\"directed\": true, \"meta\": {\"x\": 1}}"),
            Some(true)
        );
    }

    #[tokio::test]
    async fn dispatches_when_conversation_has_no_principal_group() {
        // Web-only / unbridged conversation: no principal_group
        // binding → no group-membership context → not subject to
        // classification. Must dispatch.
        let state = test_app_state();
        let cid = ConversationId::from("conv-web-only");
        assert!(
            should_dispatch_to_agent(&state, &cid, "anything").await,
            "web-only conversations must always dispatch (no group context)"
        );
    }

    #[tokio::test]
    async fn dispatches_when_group_has_only_controller() {
        // Edge case: a "group" with only the Controller in the
        // member list. No other humans → no chance of misaddress
        // → dispatch. Catches multi-controller setups + degenerate
        // single-member groups.
        let state = test_app_state();
        let cid = ConversationId::from("conv-solo-controller");
        upsert_principal(&state, "ctrl-1", TrustLevel::Controller);
        upsert_principal(&state, "ctrl-2", TrustLevel::Controller);
        mint_group_with_members(&state, &cid, &["ctrl-1", "ctrl-2"]);
        assert!(
            should_dispatch_to_agent(&state, &cid, "Elyssa, did you ...?").await,
            "all-Controller groups must dispatch (no external humans to confuse)"
        );
    }

    #[tokio::test]
    async fn falls_open_when_no_inference_backend() {
        // Multi-participant group with at least one non-Controller
        // → classifier path. But test_app_state() doesn't wire an
        // inference backend, so the LLM call short-circuits and
        // returns "directed = true". Pin the safe failure mode:
        // misconfigured / unavailable classifier must NOT silence
        // the agent.
        use execlaw_core::personality::{
            PersonalityScopeKind, PersonalityStore, PersonalityUpsert,
        };
        let state = test_app_state();
        let cid = ConversationId::from("conv-mixed-group");
        upsert_principal(&state, "ctrl", TrustLevel::Controller);
        upsert_principal(
            &state,
            "friend",
            TrustLevel::KnownTrusted {
                resolvers: vec![],
                approved_at: 100,
                approved_by: PrincipalId::from("ctrl"),
            },
        );
        mint_group_with_members(&state, &cid, &["ctrl", "friend"]);
        // Set a sensible display_name so the eligibility gate
        // moves on to the classifier path.
        let store = PersonalityStore::new(&state.db);
        let default_row = store.get_default().unwrap();
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Default,
                    scope_ref: "".into(),
                    display_name: "Lena".into(),
                    role: default_row.role,
                    tone: default_row.tone,
                    communication_style: default_row.communication_style,
                    initiative: default_row.initiative,
                    about_agent: default_row.about_agent,
                    about_controller: default_row.about_controller,
                    custom_instructions: default_row.custom_instructions,
                    voice_id: default_row.voice_id,
                    override_fields: std::collections::HashSet::new(),
                },
                100,
            )
            .unwrap();
        assert!(
            should_dispatch_to_agent(&state, &cid, "Elyssa, did you ...?").await,
            "no-inference fall-open must dispatch"
        );
    }

    #[tokio::test]
    async fn falls_open_when_display_name_is_dangerously_short() {
        // Short / missing display_name → personality-read returns
        // None → fall open. Same safe failure mode as no-inference.
        use execlaw_core::personality::{
            PersonalityScopeKind, PersonalityStore, PersonalityUpsert,
        };
        let state = test_app_state();
        let cid = ConversationId::from("conv-shortname");
        upsert_principal(&state, "ctrl", TrustLevel::Controller);
        upsert_principal(
            &state,
            "friend",
            TrustLevel::KnownTrusted {
                resolvers: vec![],
                approved_at: 100,
                approved_by: PrincipalId::from("ctrl"),
            },
        );
        mint_group_with_members(&state, &cid, &["ctrl", "friend"]);
        let store = PersonalityStore::new(&state.db);
        let default_row = store.get_default().unwrap();
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Default,
                    scope_ref: "".into(),
                    display_name: "A".into(),
                    role: default_row.role,
                    tone: default_row.tone,
                    communication_style: default_row.communication_style,
                    initiative: default_row.initiative,
                    about_agent: default_row.about_agent,
                    about_controller: default_row.about_controller,
                    custom_instructions: default_row.custom_instructions,
                    voice_id: default_row.voice_id,
                    override_fields: std::collections::HashSet::new(),
                },
                100,
            )
            .unwrap();
        assert!(
            should_dispatch_to_agent(&state, &cid, "anything").await,
            "short-name fall-open must dispatch"
        );
    }
}
