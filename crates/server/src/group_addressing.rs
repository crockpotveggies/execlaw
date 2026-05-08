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
//! It encapsulates four layers of decision, in order of cost:
//!
//! 1. **Transport-supplied verdict** — when the plugin set
//!    `mention_of_self = Some(true)` on the inbound (Slack today;
//!    any future transport with a structured `<@bot>` mention),
//!    we trust it and skip every downstream check. Saves the
//!    classifier round-trip on the messages most likely to be
//!    addressed.
//!
//! 2. **Eligibility gate** — does this conversation actually need
//!    classification? A 1:1 conversation (controller ↔ agent only)
//!    or any conversation where the only participant is the
//!    Controller falls through to "always dispatch." We only spend
//!    inference budget on classification when there's at least one
//!    other human in the room.
//!
//! 3. **Cheap name-mention shortcut** — if the agent's configured
//!    `display_name` appears in the message text (case-insensitive
//!    substring), skip the LLM. Catches the obvious "Lena, can you
//!    ..." case for ~free.
//!
//! 4. **LLM classifier** — for conversations that pass every cheap
//!    gate, consult a small/fast inference backend to decide if the
//!    inbound text is directed at the agent. The classifier knows
//!    the agent's configured display name + role from the
//!    personality store and returns a strict
//!    `{"directed": bool}` JSON verdict.
//!
//! ### Fall-open semantics
//!
//! Every uncertain path returns dispatch=true (= run the turn,
//! preserve "agent answers" behaviour). The ONLY return-skip path
//! is a clean `{"directed": false}` verdict on a group-with-
//! external-users conversation. A misbehaving classifier degrades
//! to "agent answers everything" rather than "agent silent on
//! everything" — the safer of the two failure modes.
//!
//! ### Verdict surfacing
//!
//! The return shape is rich enough that the caller can thread the
//! reason into the agent's per-turn system prompt (see
//! [`crate::chats::GroupTurnContext`]). Telling the model *why* it
//! was woken up — "you were named in the text," "the classifier
//! decided this was directed at you," "the classifier was
//! unavailable" — gives it the context it needs to be more
//! reserved when the upstream signal is weaker.

use crate::state::AppState;
use execlaw_core::ids::ConversationId;
use execlaw_core::principal::PrincipalStore;
use execlaw_core::principal_groups::PrincipalGroupStore;

/// Why the router decided a group inbound should reach the agent.
///
/// Threaded into [`crate::chats::GroupTurnContext::addressed_reason`]
/// so the agent's system prompt can describe the upstream signal in
/// plain English. The agent's response posture changes with the
/// strength of the signal: an explicit `<@agent>` mention is a clear
/// invitation to engage; a fall-open dispatch on a flapping classifier
/// is a "you might not have been addressed — be careful" hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressedReason {
    /// The transport's wire format flagged this message as a mention
    /// of the agent (Slack `<@bot-user-id>`). Strongest possible
    /// signal — by definition the user named the agent in a way the
    /// transport's UI surfaces as a mention.
    TransportMention,

    /// The agent's configured `display_name` appeared in the message
    /// text as a case-insensitive substring. Cheap host-side check
    /// that catches "Lena, can you...".
    NameInText,

    /// The LLM classifier returned `{"directed": true}`. Used when
    /// neither transport-mention nor name-in-text fired — the
    /// classifier inferred the message was directed from semantics
    /// (a question only the agent could answer, a clear reply to
    /// something the agent just said, etc.).
    ClassifierDirected,

    /// The conversation bypassed classification (1:1, single-member
    /// group, all-Controller group). The agent should answer
    /// normally; there's no other human in the room to confuse it
    /// with.
    EligibilityBypass,

    /// The classifier couldn't be consulted (no inference backend
    /// resolved, missing display_name, principal lookup failed). We
    /// dispatched anyway because silencing the agent on a config
    /// problem is worse than answering. Tell the agent so it can be
    /// more conservative.
    FallOpenClassifierUnavailable,

    /// The classifier was reachable but failed (HTTP error, timeout,
    /// unparseable JSON). Same fall-open semantics as above —
    /// dispatched, but the agent should know the upstream signal
    /// was unreliable.
    FallOpenClassifierError,
}

impl AddressedReason {
    /// Short human-readable phrase suitable for the agent's system
    /// prompt. Stable strings — change with care, the model has
    /// been primed on this wording.
    pub fn description(&self) -> &'static str {
        match self {
            AddressedReason::TransportMention => {
                "the message contains an explicit @-mention of you in the transport's wire format"
            }
            AddressedReason::NameInText => "your name appears in the message text",
            AddressedReason::ClassifierDirected => {
                "a fast classifier judged the message to be directed at you (your name was NOT in the text — the verdict came from semantics, so be careful)"
            }
            AddressedReason::EligibilityBypass => {
                "this conversation has no other non-Controller humans, so the addressing question doesn't apply"
            }
            AddressedReason::FallOpenClassifierUnavailable => {
                "the addressing classifier was unavailable, so the router defaulted to dispatching the turn — you may NOT have been addressed; default to staying brief or asking who the message was for"
            }
            AddressedReason::FallOpenClassifierError => {
                "the addressing classifier errored, so the router defaulted to dispatching the turn — you may NOT have been addressed; default to staying brief or asking who the message was for"
            }
        }
    }
}

/// Outcome of [`should_dispatch_to_agent`]. The caller threads the
/// `reason` into [`crate::chats::GroupTurnContext::addressed_reason`]
/// when it dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDecision {
    /// Run the turn. `reason` describes the upstream signal so the
    /// agent's prompt can render it.
    Dispatch(AddressedReason),
    /// Skip dispatch. Caller should still persist the inbound for
    /// chat-history context (see
    /// [`crate::chats::commit_inbound_user_msg_silently`]).
    Skip,
}

impl DispatchDecision {
    /// True for every variant that should run a turn.
    pub fn should_dispatch(&self) -> bool {
        matches!(self, DispatchDecision::Dispatch(_))
    }

    /// `Some(reason)` when dispatching, `None` for `Skip`. Convenience
    /// for callers that destructure into the
    /// [`crate::chats::GroupTurnContext`] builder.
    pub fn reason(self) -> Option<AddressedReason> {
        match self {
            DispatchDecision::Dispatch(r) => Some(r),
            DispatchDecision::Skip => None,
        }
    }
}

/// Decide whether the agent should answer this group inbound, and
/// surface the reason for the verdict.
///
/// `mention_of_self` carries any structured-mention hint the
/// transport plugin attached to the inbound (Slack today). When
/// `Some(true)`, the function short-circuits to
/// `Dispatch(TransportMention)` without touching the DB or
/// classifier.
///
/// Always returns `Dispatch(EligibilityBypass)` for:
///   * Conversations that don't have a principal_group binding
///     (single-actor / web chat).
///   * Single-member groups (just the Controller).
///   * Groups whose only members are Controllers (rare:
///     multi-controller deployments — every participant is the
///     operator's identity).
///
/// Returns `Dispatch(FallOpenClassifierUnavailable)` for:
///   * Personality-store lookup failures or dangerously-short
///     `display_name` (< 2 chars).
///   * No inference backend resolved.
///
/// Returns `Dispatch(FallOpenClassifierError)` for:
///   * Inference HTTP error / timeout / unparseable JSON.
///
/// Returns `Skip` only when a clean `{"directed": false}` verdict
/// lands — the agent stays silent and the inbound is committed for
/// context only.
pub async fn should_dispatch_to_agent(
    state: &AppState,
    conversation_id: &ConversationId,
    message_text: &str,
    mention_of_self: Option<bool>,
) -> DispatchDecision {
    // ---- Layer 1: transport-supplied verdict ------------------
    // Slack already gates `<@bot-user-id>` in the plugin and only
    // forwards mentioning channel-messages. When we see that hint,
    // we trust it and avoid spending classifier budget on messages
    // that are obviously addressed.
    if matches!(mention_of_self, Some(true)) {
        return DispatchDecision::Dispatch(AddressedReason::TransportMention);
    }

    // ---- Layer 2: eligibility gate ----------------------------
    let pg_store = PrincipalGroupStore::new(&state.db);
    let pg_id = match pg_store.principal_group_id_for(conversation_id.as_str()) {
        Ok(Some(id)) => id,
        _ => {
            // Web-only / unbridged / lookup error → not subject
            // to classification. Dispatch.
            return DispatchDecision::Dispatch(AddressedReason::EligibilityBypass);
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
            return DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierUnavailable);
        }
    };
    if members.len() < 2 {
        // Single-member group — no other humans to confuse the
        // agent with. Dispatch.
        return DispatchDecision::Dispatch(AddressedReason::EligibilityBypass);
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
        return DispatchDecision::Dispatch(AddressedReason::EligibilityBypass);
    }

    // ---- Layer 3: cheap name-in-text shortcut -----------------
    let (agent_name, agent_role) = match read_agent_identity(state) {
        Some(pair) => pair,
        None => {
            // Personality unavailable / display_name too short →
            // fall open (skip-dispatch is destructive UX).
            return DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierUnavailable);
        }
    };
    if name_in_text(&agent_name, message_text) {
        return DispatchDecision::Dispatch(AddressedReason::NameInText);
    }

    // ---- Layer 4: LLM classifier ------------------------------
    classify_via_llm(state, &agent_name, &agent_role, message_text).await
}

/// Case-insensitive substring match for `name` in `text`, with a
/// dignified handful of guards:
///
/// * Bails out (returns `false`) for names shorter than 2 chars —
///   single letters create false positives in any English message.
///   The eligibility gate already rejects display_name < 2 chars
///   upstream, but this is the second line of defence in case the
///   personality store ever loosens that rule.
/// * Lower-cases both sides via `to_lowercase` (locale-independent
///   for the ASCII case we care about; non-ASCII names work too,
///   they just compare folded).
/// * Substring, not word-boundary. We accept "Lena's" matching
///   "lena" — false positives here cost a turn that wouldn't have
///   run anyway, false negatives skip a clear address.
pub fn name_in_text(name: &str, text: &str) -> bool {
    let name = name.trim();
    if name.chars().count() < 2 {
        return false;
    }
    let name_l = name.to_lowercase();
    let text_l = text.to_lowercase();
    text_l.contains(&name_l)
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

/// Run the LLM classifier. Returns a `Dispatch(reason)` on every
/// uncertain path (no inference, timeout, HTTP error, unparseable
/// JSON). Only a clean `{"directed": false}` becomes `Skip`.
async fn classify_via_llm(
    state: &AppState,
    agent_name: &str,
    agent_role: &str,
    message_text: &str,
) -> DispatchDecision {
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
            return DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierUnavailable);
        }
    };

    let role_phrase = if agent_role.trim().is_empty() {
        "assistant"
    } else {
        agent_role
    };
    // The classifier prompt is biased toward `false` (silence) on
    // ambiguous group chatter. Past tuning was permissive — anything
    // the agent *could* answer was treated as directed, which made
    // the agent barge into general group conversation. The new bias:
    //
    //   * "directed" requires a STRONG signal (named, mentioned,
    //     replying to the agent, or a request only the agent can
    //     fulfil because it references agent-specific tools).
    //   * Generic questions, casual banter, scheduling,
    //     opinion-asking with no addressee — all "false". The
    //     correct posture for the agent in a group is to listen
    //     unless invited.
    //
    // Cost of a false positive (= barging in): high — operators
    // consistently report this as the most annoying agent failure
    // mode.
    // Cost of a false negative (= missed address): low — the operator
    // can name the agent or @-mention to retry; barging happens at
    // every silent group event, missed addresses happen once.
    let system_prompt = format!(
        "You are a binary classifier for a chat-bot routing layer. \
         The agent is named \"{agent_name}\" and acts as the operator's {role_phrase}. \
         A new message arrived in a GROUP conversation that includes the agent, the operator, \
         and other humans. Decide if THIS specific message is directed at the agent.\n\n\
         IMPORTANT BIAS: in a group, the DEFAULT is that messages are between humans and the \
         agent should stay silent. Only return `true` when the message is clearly aimed at \
         the agent. When unsure, return `false`. Barging into casual group conversation is \
         worse than missing one address — the operator can re-prompt by naming the agent.\n\n\
         Return `true` ONLY when at least one of these holds:\n\
         - The agent is named or @-mentioned in this message (e.g. \"{agent_name}, ...\", \
           \"@{agent_name}\", \"hey {agent_name}\")\n\
         - The message is a direct reply or follow-up to something the agent ITSELF just said \
           (continuing an exchange the agent started)\n\
         - The message asks for output that ONLY the agent can produce (a draft the agent was \
           writing, a tool call only it has, a research task already in progress)\n\n\
         Return `false` for everything else, including:\n\
         - General questions to the room (\"anyone know a good restaurant?\", \"what time \
           tomorrow?\") with no clear addressee — even if the agent COULD answer\n\
         - Casual chatter, banter, reactions, emoji, one-word replies\n\
         - Messages addressing another human by name (\"Alice, did you ...\")\n\
         - Scheduling, planning, or coordination between other humans\n\
         - Opinions or commentary not directed at anyone in particular\n\
         - Forwarded content, links shared without a question to the agent\n\n\
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
            return DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierError);
        }
        Err(_) => {
            tracing::warn!(
                target: "group_addressing",
                "address classifier timed out after 5s; falling open"
            );
            return DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierError);
        }
    };

    let text = resp
        .choices
        .first()
        .and_then(|c| c.message.content.as_deref())
        .unwrap_or("");
    match parse_directed_verdict(text) {
        Some(true) => DispatchDecision::Dispatch(AddressedReason::ClassifierDirected),
        Some(false) => DispatchDecision::Skip,
        None => {
            tracing::debug!(
                target: "group_addressing",
                response_preview = %&text.chars().take(120).collect::<String>(),
                "couldn't parse JSON `directed` from classifier response; falling open"
            );
            DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierError)
        }
    }
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
        // `bind_conversation` is an UPDATE — the row in
        // `state_conversations` must exist or the bind silently
        // no-ops. Materialize it via the standard helper so the
        // foreign-key column actually populates and the classifier's
        // `principal_group_id_for` can find this group.
        crate::chats::ensure_conversation_for(&state.db, cid);
        pg_store
            .bind_conversation(cid.as_str(), &pg.group_id)
            .unwrap();
        pg.group_id
    }

    fn set_personality_display_name(state: &crate::state::AppState, name: &str) {
        use execlaw_core::personality::{
            PersonalityScopeKind, PersonalityStore, PersonalityUpsert,
        };
        let store = PersonalityStore::new(&state.db);
        let default_row = store.get_default().unwrap();
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Default,
                    scope_ref: "".into(),
                    display_name: name.into(),
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
            parse_directed_verdict("Here is the verdict:\n{\"directed\": true}\nThanks!"),
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

    #[test]
    fn name_in_text_matches_case_insensitive() {
        assert!(name_in_text("Lena", "Lena, can you check the calendar?"));
        assert!(name_in_text("Lena", "hey lena what time is it"));
        assert!(name_in_text("Lena", "LENA!!!"));
        // Substring is fine — possessive / punctuation attached.
        assert!(name_in_text("Lena", "lena's calendar"));
        // Multi-byte name folds correctly.
        assert!(name_in_text("Élodie", "yo élodie when's lunch"));
    }

    #[test]
    fn name_in_text_misses_when_name_absent() {
        assert!(!name_in_text("Lena", "anyone know a good Thai place?"));
        assert!(!name_in_text("Lena", "Alice did you reply to John?"));
        assert!(!name_in_text("Lena", ""));
    }

    #[test]
    fn name_in_text_rejects_short_name_to_avoid_false_positives() {
        // A single-letter display_name would match almost any
        // English sentence. The eligibility gate also rejects
        // names < 2 chars upstream, but this is a second line of
        // defence.
        assert!(!name_in_text("L", "this is a long sentence"));
        assert!(!name_in_text("", "anything"));
    }

    #[test]
    fn addressed_reason_descriptions_are_present_and_distinct() {
        // The agent sees these strings — they need to be present,
        // non-empty, and recognisably different per variant so the
        // model can distinguish "transport mention" from "fall-open
        // unavailable" in the prompt.
        let variants = [
            AddressedReason::TransportMention,
            AddressedReason::NameInText,
            AddressedReason::ClassifierDirected,
            AddressedReason::EligibilityBypass,
            AddressedReason::FallOpenClassifierUnavailable,
            AddressedReason::FallOpenClassifierError,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            let d = v.description();
            assert!(
                !d.trim().is_empty(),
                "{:?} description must be non-empty",
                v
            );
            assert!(
                seen.insert(d.to_owned()),
                "descriptions must be distinct: {:?} duplicates an earlier variant",
                v
            );
        }
    }

    #[test]
    fn dispatch_decision_helpers_round_trip() {
        let d = DispatchDecision::Dispatch(AddressedReason::TransportMention);
        assert!(d.should_dispatch());
        assert_eq!(d.reason(), Some(AddressedReason::TransportMention));

        let s = DispatchDecision::Skip;
        assert!(!s.should_dispatch());
        assert_eq!(s.reason(), None);
    }

    #[tokio::test]
    async fn transport_mention_short_circuits_with_transport_reason() {
        // Slack's plugin sets `mention_of_self: Some(true)` for
        // forwarded channel messages (DMs leave it as None). The
        // router must trust that hint and avoid the classifier
        // path entirely — both for performance (no inference
        // round-trip) and so the agent's prompt can see the
        // strongest possible signal.
        let state = test_app_state();
        let cid = ConversationId::from("conv-slack-mention");
        let decision = should_dispatch_to_agent(&state, &cid, "any text at all", Some(true)).await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::TransportMention)
        );
    }

    #[tokio::test]
    async fn dispatches_when_conversation_has_no_principal_group() {
        // Web-only / unbridged conversation: no principal_group
        // binding → not subject to classification. Dispatch with
        // EligibilityBypass so the prompt knows there's no group
        // to be careful about.
        let state = test_app_state();
        let cid = ConversationId::from("conv-web-only");
        let decision = should_dispatch_to_agent(&state, &cid, "anything", None).await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::EligibilityBypass)
        );
    }

    #[tokio::test]
    async fn dispatches_when_group_has_only_controller() {
        // Edge case: a "group" with only the Controller in the
        // member list. No other humans → no chance of misaddress
        // → dispatch with EligibilityBypass.
        let state = test_app_state();
        let cid = ConversationId::from("conv-solo-controller");
        upsert_principal(&state, "ctrl-1", TrustLevel::Controller);
        upsert_principal(&state, "ctrl-2", TrustLevel::Controller);
        mint_group_with_members(&state, &cid, &["ctrl-1", "ctrl-2"]);
        let decision = should_dispatch_to_agent(&state, &cid, "Elyssa, did you ...?", None).await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::EligibilityBypass)
        );
    }

    #[tokio::test]
    async fn name_in_text_short_circuits_classifier() {
        // Mixed group + agent's display name appears in the text
        // → cheap shortcut fires, classifier never runs. The
        // verdict's reason is NameInText so the prompt can show a
        // strong-signal message.
        let state = test_app_state();
        let cid = ConversationId::from("conv-named");
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
        set_personality_display_name(&state, "Lena");
        let decision =
            should_dispatch_to_agent(&state, &cid, "hey Lena, can you check the calendar?", None)
                .await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::NameInText)
        );
    }

    #[tokio::test]
    async fn falls_open_when_no_inference_backend() {
        // Multi-participant group with at least one non-Controller,
        // no name match, no transport hint → classifier path. But
        // test_app_state() doesn't wire an inference backend, so
        // we get FallOpenClassifierUnavailable. Pin the safe
        // failure mode: misconfigured / unavailable classifier
        // must NOT silence the agent.
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
        set_personality_display_name(&state, "Lena");
        // Note: text deliberately omits "Lena" so we don't fire
        // the name-in-text shortcut and do exercise the classifier
        // path (which then falls open on no inference backend).
        let decision = should_dispatch_to_agent(&state, &cid, "Elyssa, did you ...?", None).await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierUnavailable)
        );
    }

    #[tokio::test]
    async fn falls_open_when_display_name_is_dangerously_short() {
        // Short / missing display_name → personality-read returns
        // None → FallOpenClassifierUnavailable. Caller dispatches.
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
        set_personality_display_name(&state, "A");
        let decision = should_dispatch_to_agent(&state, &cid, "anything", None).await;
        assert_eq!(
            decision,
            DispatchDecision::Dispatch(AddressedReason::FallOpenClassifierUnavailable)
        );
    }
}
