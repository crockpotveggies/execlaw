//! Signal-cli inbound consumer (Phase 4).
//!
//! Long-lived background task that subscribes to the supervised
//! `signal-cli-rest-api` sidecar's `/v1/receive/<self_number>`
//! WebSocket, decodes each envelope, and routes inbound messages
//! through the existing trust pipeline. The consumer's job ends at
//! "user message committed + agent turn dispatched"; the existing
//! `commit_turn` / cold-contact / approval machinery owns the rest.
//!
//! ## Lifecycle
//!
//! One consumer task per supervised Signal sidecar, spawned at boot
//! alongside `SidecarSupervisor::run`. Cooperatively cancelled via the
//! same shared `Notify` the supervisor uses, so a single
//! `stop.notify_waiters()` shuts down both cleanly.
//!
//! Reconnect strategy: exponential backoff capped at 60s. Every
//! successful frame resets the backoff. A "sidecar host port not
//! published yet" miss (supervisor still spawning, crash-loop park,
//! etc.) sleeps for the configured tick interval before the next
//! lookup so we don't busy-loop against the supervisor's mutex.
//!
//! ## Message shape
//!
//! `bbernhard/signal-cli-rest-api` emits one JSON object per inbound
//! signal-cli event. We care about exactly one shape — a `dataMessage`
//! envelope that carries text:
//!
//! ```json
//! {
//!   "envelope": {
//!     "source": "+15559998888",
//!     "sourceNumber": "+15559998888",
//!     "sourceUuid": "....",
//!     "sourceName": "Alice",
//!     "timestamp": 1700000000000,
//!     "dataMessage": {
//!       "timestamp": 1700000000000,
//!       "message": "hi from signal",
//!       "groupInfo": null
//!     }
//!   },
//!   "account": "+15551234567"
//! }
//! ```
//!
//! Receipts, typing indicators, sync messages — anything without a
//! `dataMessage.message` — are silently dropped at decode time; the
//! consumer never bothers the trust pipeline with non-content
//! events. Group `dataMessage`s (those with `groupInfo`) are
//! deferred to Phase 5; the decoder logs and skips them so the
//! agent doesn't act on partial data.
//!
//! ## Self-number sourcing
//!
//! Same env-var as the outbound transport: `EXECLAW_SIGNAL_CONTROLLER_NUMBER`.
//! Read once at consumer-spawn time. When unset, the consumer logs a
//! warning and exits cleanly (the sidecar is still useful for outbound
//! after the operator sets the var; restart picks it up).

use crate::signal_transport::SIGNAL_CHANNEL;
use crate::state::AppState;
use execlaw_core::ids::PrincipalId;
use execlaw_core::principal::{
    Identifier, Principal, PrincipalStore, TrustLevel as CoreTrustLevel,
};
use execlaw_core::principal_groups::{GroupKey, PrincipalGroupStore};
use execlaw_core::transport_bindings::TransportBindingStore;
use execlaw_core::transport_conversations::{ConversationResolver, ResolveInput};
use execlaw_policy::trust::TrustLevel;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Hard cap on per-attachment payload size for inbound Signal
/// attachments. Picked at 25 MiB to match Signal's own attachment
/// cap and bound the disk impact of a single hostile inbound. The
/// bridge advertises `size` in the envelope; we reject pre-fetch
/// when it exceeds the cap so we never even pull the bytes for an
/// oversize payload.
pub const MAX_INBOUND_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// Hard cap on reconnect backoff. Picked so a long sidecar outage
/// retries roughly once a minute — frequent enough that recovery
/// is fast once the supervisor brings the sidecar back, slow enough
/// that we don't pile up failed connect attempts in the log.
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// Floor for backoff. Reset on every successful frame so a
/// long-running connection that drops once doesn't pay the full
/// cap on the first retry.
const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// How long to wait between sidecar host-port polls when the
/// supervisor hasn't published a port yet. Matches the supervisor's
/// own tick interval so a brand-new install settles within one or
/// two ticks.
const SIDECAR_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Idle window for `ConversationResolver` — a Signal contact whose
/// last message was ≤24h ago continues the same thread; older →
/// rotation. Mirrors the per-transport recommendation in
/// MIGRATION_PLAN §2.6.
const SIGNAL_IDLE_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1_000;

/// Plugin id we stamp on transport-conversation rows. Matches the
/// `[plugin].id` in `plugins/signal/plugin.toml`.
const SIGNAL_PLUGIN_ID: &str = "signal";

// -----------------------------------------------------------------
// WS envelope shape
// -----------------------------------------------------------------

/// Top-level frame from `/v1/receive/<number>`. `account` is the
/// registered self-number; signal-cli echoes it back so a multi-
/// account deployment can disambiguate.
#[derive(Debug, Deserialize)]
struct WsFrame {
    envelope: SignalEnvelope,
    /// The registered Signal number that received the message — same
    /// value we passed in the URL. Validated against `self_number`
    /// at decode time so a misrouted frame doesn't accidentally get
    /// processed.
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SignalEnvelope {
    /// E.164 phone number of the sender. Stable across messages but
    /// can rebind to a different account (rare). Always present on
    /// inbound dataMessages.
    source: String,
    #[serde(rename = "sourceNumber")]
    #[serde(default)]
    source_number: Option<String>,
    /// `sourceUuid` is in the wire shape but we don't use it today —
    /// the `source` (E.164) is the routing key. Reserved for Phase 5
    /// when we tie incoming UUIDs to multi-device cross-correlation.
    #[serde(rename = "sourceUuid", default)]
    _source_uuid: Option<String>,
    #[serde(rename = "sourceName")]
    #[serde(default)]
    source_name: Option<String>,
    /// Sender's signal-cli timestamp in milliseconds. Doubles as the
    /// message id for read-receipt correlation later.
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(rename = "dataMessage")]
    #[serde(default)]
    data_message: Option<SignalDataMessage>,
}

#[derive(Debug, Deserialize)]
struct SignalDataMessage {
    /// Body text. `None` for attachment-only messages — Phase 6
    /// surfaces those as attachment-only inbound (text becomes the
    /// empty string, attachments carry the payload).
    #[serde(default)]
    message: Option<String>,
    /// Present when the message landed in a Signal group rather than
    /// a 1:1 DM. Phase 5 wired full group routing around this field.
    #[serde(rename = "groupInfo")]
    #[serde(default)]
    group_info: Option<SignalGroupInfo>,
    /// Phase 6 — inbound attachment metadata. Bridge-issued ids; the
    /// blob bytes come from a follow-up `GET /v1/attachments/{id}`
    /// against the sidecar.
    #[serde(default)]
    attachments: Vec<SignalInboundAttachment>,
}

#[derive(Debug, Deserialize)]
struct SignalInboundAttachment {
    /// signal-cli's attachment id — opaque base64-shaped string we
    /// pass back verbatim on the `/v1/attachments/{id}` GET. The
    /// transport's fetcher validates the shape before URL injection.
    id: String,
    /// IANA media type. signal-cli always populates this for
    /// attachments it accepted; defensive `Option` for older bridge
    /// versions.
    #[serde(rename = "contentType", default)]
    content_type: Option<String>,
    /// Sender-provided filename. Sanitised before disk write.
    #[serde(default)]
    filename: Option<String>,
    /// Byte length the bridge advertised. Used as a pre-fetch
    /// rejection threshold (don't even pull the bytes if they're
    /// over the cap) — a misbehaving sender can't lie larger and
    /// fill our disk before we notice.
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SignalGroupInfo {
    /// Base64-encoded group identifier. Stored as the binding's
    /// `foreign_id` for group bindings (Phase 5).
    #[serde(rename = "groupId")]
    #[serde(default)]
    group_id: Option<String>,
}

/// Decoded inbound message ready for routing. Built from
/// [`WsFrame`] + the configured self-number; carries only the
/// fields the routing logic needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundDataMessage {
    /// Sender's E.164 — the binding's `foreign_id`.
    pub source_number: String,
    /// Display name signal-cli resolved from the contact list, when
    /// available. Used as the principal's display label on
    /// first-contact intake.
    pub source_name: Option<String>,
    /// Message body. May be the empty string when the inbound is
    /// attachment-only (a contact sent just an image / voice note);
    /// Phase 6 keeps such frames so the attachment ingestion path
    /// can surface them as cards.
    pub text: String,
    /// `Some(group_id)` for group inbound; `None` for 1:1 DMs. Phase
    /// 4 routes `Some` to a deferred-skip path.
    pub group_id: Option<String>,
    /// Sender's signal-cli timestamp in milliseconds, when present.
    pub sender_timestamp_ms: Option<i64>,
    /// Phase 6 — attachment metadata extracted from the data
    /// message. Empty vec for text-only inbound. The routing path
    /// fetches the blob bytes via `transport.fetch_attachment(id)`
    /// and persists each as a `state_attachments` row + emits an
    /// `Attachment` card before the agent turn dispatches.
    pub attachments: Vec<InboundAttachmentMeta>,
}

/// Per-attachment metadata surfaced by [`decode_frame`].
/// Stripped-down view of the bridge's wire shape so callers don't
/// have to reach into Signal-specific deserialise types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundAttachmentMeta {
    /// Bridge-side attachment id — passed back verbatim to
    /// `transport.fetch_attachment` for the body fetch.
    pub bridge_id: String,
    /// IANA media type the bridge advertised (`image/jpeg`,
    /// `audio/aac`, ...). `None` when the bridge omitted it; the
    /// persistence layer falls back to `application/octet-stream`.
    pub content_type: Option<String>,
    /// Sender-supplied filename, when present. Pre-sanitisation —
    /// the persistence layer strips path components before disk
    /// write.
    pub filename: Option<String>,
    /// Bridge-advertised byte size, used as a pre-fetch oversize
    /// reject threshold so a lying contact can't trick us into
    /// downloading multi-GB blobs we'd then drop.
    pub size: Option<u64>,
}

/// Decode a raw WS frame body into a routable `InboundDataMessage`.
///
/// Returns `None` (silently) for every shape we don't act on —
/// receipts, typing indicators, sync messages, attachment-only
/// messages, account-mismatch frames. Non-`None` is always
/// safe-to-route content with non-empty text. Pure function so the
/// adversarial tests cover every drop path without spinning a
/// runtime.
pub fn decode_frame(body: &str, expected_account: Option<&str>) -> Option<InboundDataMessage> {
    let frame: WsFrame = match serde_json::from_str(body) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(target: "signal_inbound", error = %e, "skipping unparseable frame");
            return None;
        }
    };
    // Multi-account guard: drop frames addressed to a different
    // self-number than ours. Defensive — a single-account install
    // never sees these — but cheap.
    if let (Some(expected), Some(got)) = (expected_account, frame.account.as_deref())
        && expected != got
    {
        tracing::debug!(
            target: "signal_inbound",
            expected = %expected,
            got = %got,
            "dropping frame with mismatched account",
        );
        return None;
    }
    let env = frame.envelope;
    let dm = env.data_message?;
    let attachments: Vec<InboundAttachmentMeta> = dm
        .attachments
        .into_iter()
        .filter(|a| !a.id.is_empty())
        .map(|a| InboundAttachmentMeta {
            bridge_id: a.id,
            content_type: a.content_type.filter(|s| !s.is_empty()),
            filename: a.filename.filter(|s| !s.is_empty()),
            size: a.size,
        })
        .collect();
    // Phase 6: keep attachment-only frames (text becomes empty
    // string). Phase 4-and-prior dropped any frame without text;
    // now an inbound that's purely an image / voice note still
    // routes so the consumer can ingest the attachment.
    let text = dm.message.map(|s| s.trim().to_owned()).unwrap_or_default();
    if text.is_empty() && attachments.is_empty() {
        // Pure receipt / typing / sync — nothing to surface.
        return None;
    }
    let group_id = dm.group_info.and_then(|g| g.group_id);
    Some(InboundDataMessage {
        source_number: env.source_number.unwrap_or(env.source),
        source_name: env.source_name,
        text,
        group_id,
        sender_timestamp_ms: env.timestamp,
        attachments,
    })
}

// -----------------------------------------------------------------
// Routing
// -----------------------------------------------------------------

/// Outcome of [`route_inbound_message`]. Public so tests can pin
/// the decision tree without scraping logs / events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Sender's principal is `Blocked`; the message was committed
    /// only as an audit-log line and never reached the agent.
    /// In group context, only this sender's message is dropped —
    /// other group members keep flowing.
    Blocked { principal_id: String },
    /// Sender is a brand-new contact — the consumer minted a
    /// principal + binding + conversation, committed a
    /// `ColdContactArrived` event, fired the approval alert, and
    /// returned. The agent does NOT run a turn; the controller
    /// approves out-of-band first.
    ColdContact {
        principal_id: String,
        conversation_id: String,
    },
    /// Sender is a known contact and their trust class admits an
    /// agent turn. The consumer committed the user message and
    /// dispatched a turn through the existing pipeline.
    Dispatched {
        principal_id: String,
        conversation_id: String,
    },
}

/// Route one decoded inbound message through the trust pipeline.
///
/// The flow:
///
///   1. Skip group messages (Phase 4 deferral).
///   2. Lookup `(channel, source_number)` in
///      [`TransportBindingStore`]. Hit → reuse the bound principal
///      group + principal. Miss → mint both and seed an
///      `UnknownPending` principal so the cold-contact gate fires
///      below.
///   3. Resolve / mint a conversation via [`ConversationResolver`]
///      and bind it to the principal group.
///   4. Branch on the principal's trust class:
///        * `Blocked` — drop with an audit-log line.
///        * `UnknownPending` — call `chats::handle_cold_contact_for_inbound`.
///        * Anything else — call `chats::dispatch_external_turn`.
///
/// Returns the [`RouteOutcome`] for telemetry + tests. Errors are
/// surfaced as `Err(String)` so the consumer loop can log + skip
/// without taking the whole subscriber down.
pub async fn route_inbound_message(
    state: &AppState,
    msg: &InboundDataMessage,
) -> Result<RouteOutcome, String> {
    if let Some(group_id) = &msg.group_id {
        // Phase 5: group routing. The 1:1 path's identity model
        // doesn't fit groups — we want one conversation per group
        // (not per sender), and the binding is on the group_id, not
        // the sender's number. Delegated to keep the per-message
        // 1:1 path readable.
        return route_group_inbound(state, msg, group_id).await;
    }

    let now = chrono::Utc::now().timestamp();
    let binding_store = TransportBindingStore::new(&state.db);
    let pg_store = PrincipalGroupStore::new(&state.db);
    let principals = PrincipalStore::new(&state.db);

    // 1. Resolve or mint the binding + principal.
    let (principal, principal_group_id) = match binding_store
        .lookup_principal_group(SIGNAL_CHANNEL, &msg.source_number)
        .map_err(|e| format!("binding lookup: {e}"))?
    {
        Some(pg_id) => {
            // Existing binding. Read the canonical principal from
            // the store — its trust class is authoritative.
            //
            // Audit fix (Phase 4 audit finding #2): if the principal
            // row is MISSING (operator deleted it, migration
            // glitch, etc.) we used to re-mint as UnknownPending.
            // That silently downgrades a previously-Blocked principal
            // to a cold-contact and leaks spam. Drop the inbound
            // instead — better an operator-visible audit-log line
            // than a quietly-routed message.
            let pid = signal_principal_id(&msg.source_number);
            let principal = match principals
                .get(&pid)
                .map_err(|e| format!("principal get: {e}"))?
            {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        target: "signal_inbound",
                        principal_id = %pid.as_str(),
                        binding_pg_id = %pg_id,
                        "binding exists but principal row missing — dropping inbound rather than \
                         silently re-minting as UnknownPending (would downgrade a previously-\
                         Blocked principal). Operator must heal the binding manually.",
                    );
                    return Ok(RouteOutcome::Blocked {
                        principal_id: pid.as_str().to_owned(),
                    });
                }
            };
            // Bump last_seen so the dashboards reflect activity.
            let mut updated = principal.clone();
            updated.last_seen = Some(now);
            let _ = principals.upsert(&updated);
            (updated, pg_id)
        }
        None => {
            // First-contact path: route through the shared admit
            // helper before minting. This catches:
            //   * The controller's own "My identities" mappings —
            //     `signal:+16047005800` registered as a controller
            //     identifier resolves to the Controller principal,
            //     bypassing the cold-contact gate entirely.
            //   * Identity-provider plugin matches (Google Contacts,
            //     local address book) — when the operator's Trust
            //     Policy enables auto-trust and a plugin vouches at
            //     ≥ min_trust_hint, the sender is admitted as
            //     KnownLimited (or KnownTrusted, configurable) and
            //     the agent can reply immediately.
            //   * Otherwise: UnknownPending mint, same as before.
            let hint_pid = signal_principal_id(&msg.source_number);
            let (principal, _flat_trust) = crate::principal_admit::admit_external_principal(
                &state.db,
                &state.plugin_host,
                SIGNAL_CHANNEL,
                &msg.source_number,
                hint_pid.as_str(),
            )
            .await
            .map_err(|e| format!("admit principal: {e}"))?;

            // Group selection: if the resolved principal is the
            // Controller, route to the controller's principal_group
            // so the Signal thread joins the controller's existing
            // conversation rather than spinning up an
            // "external + signal" group. Otherwise key the group on
            // the sender.
            let is_controller = matches!(
                principal.trust_level,
                execlaw_core::principal::TrustLevel::Controller
            );
            let pg = pg_store
                .resolve(
                    &GroupKey {
                        channel: SIGNAL_CHANNEL,
                        native_group_id: None,
                        principals: &[principal.id.clone()],
                        includes_controller: is_controller,
                    },
                    now,
                )
                .map_err(|e| format!("principal_group mint: {e}"))?;
            let inserted = binding_store
                .insert_binding(
                    SIGNAL_CHANNEL,
                    &msg.source_number,
                    &pg.group_id,
                    false,
                    now,
                )
                .map_err(|e| format!("binding insert: {e}"))?;
            if !inserted {
                // Race: another consumer (or a retried frame) won
                // the binding. The other writer's principal is now
                // the canonical one — re-fetch instead of using our
                // stale `principal` (audit fix #1). Otherwise the
                // trust gate downstream would run against the
                // stale UnknownPending we just minted while the
                // canonical principal could be Blocked / known.
                tracing::debug!(
                    target: "signal_inbound",
                    "binding insert lost a race; refetching canonical principal",
                );
                let pg_id = binding_store
                    .lookup_principal_group(SIGNAL_CHANNEL, &msg.source_number)
                    .map_err(|e| format!("binding re-lookup: {e}"))?
                    .ok_or_else(|| "binding vanished after insert race".to_owned())?;
                let canonical = principals
                    .get(&principal.id)
                    .map_err(|e| format!("principal refetch: {e}"))?
                    .ok_or_else(|| {
                        "principal vanished after race-recovery refetch".to_owned()
                    })?;
                (canonical, pg_id)
            } else {
                (principal, pg.group_id)
            }
        }
    };

    // 2. Resolve / mint a conversation. The Signal transport's
    //    routing key is the source_number itself. `is_controller`
    //    flows through from the admitted principal so a controller's
    //    Signal message routes into a ControllerDM-shaped
    //    conversation rather than an external one.
    let is_controller_principal = matches!(
        principal.trust_level,
        execlaw_core::principal::TrustLevel::Controller
    );
    let resolver = ConversationResolver::new(&state.db);
    let outcome = resolver
        .resolve_or_mint(&ResolveInput {
            plugin_id: SIGNAL_PLUGIN_ID,
            transport_handle: &msg.source_number,
            principal_id: principal.id.as_str(),
            is_controller: is_controller_principal,
            idle_timeout_ms: SIGNAL_IDLE_TIMEOUT_MS,
            now,
        })
        .map_err(|e| format!("conversation resolve: {e}"))?;
    let cid = outcome.conversation_id().clone();

    // Make sure the conversation row exists (resolve_or_mint may have
    // returned a fresh id without inserting state_conversations).
    crate::chats::ensure_conversation_for(&state.db, &cid);
    pg_store
        .bind_conversation(cid.as_str(), &principal_group_id)
        .map_err(|e| format!("bind conversation: {e}"))?;

    // 3. Trust gate.
    let trust_tag = principal.trust_level.class_tag();
    let trust_flat = TrustLevel::parse(trust_tag).unwrap_or(TrustLevel::UnknownPending);

    if trust_flat == TrustLevel::Blocked {
        tracing::info!(
            target: "signal_inbound",
            principal_id = %principal.id.as_str(),
            "dropping inbound from Blocked principal",
        );
        return Ok(RouteOutcome::Blocked {
            principal_id: principal.id.as_str().to_owned(),
        });
    }

    if trust_flat == TrustLevel::UnknownPending {
        // Cold-contact path. The chats helper commits the
        // ColdContactArrived event, transitions to
        // AwaitingTrustDecision, and fires the approval alert. We
        // don't run a turn — the controller responds out-of-band.
        //
        // Audit fix (Phase 6): we deliberately do NOT ingest
        // attachments for UnknownPending senders. Otherwise a
        // brand-new contact could bomb N × 25 MiB worth of disk
        // before any human approved them. Once approved, future
        // inbound goes through the Dispatched path below and
        // attachments DO land. signal-cli holds attachments
        // server-side for a window after approval, so the
        // controller can still backfill the missed attachments
        // by asking the agent to fetch them.
        crate::chats::handle_cold_contact_for_inbound(state, &cid, &principal, &msg.text)
            .await
            .map_err(|e| format!("cold-contact handler: {e}"))?;
        return Ok(RouteOutcome::ColdContact {
            principal_id: principal.id.as_str().to_owned(),
            conversation_id: cid.as_str().to_owned(),
        });
    }

    // Phase 6 — attachment ingestion runs ONLY for trusted senders
    // past the cold-contact gate. When `state.sidecar_supervisor`
    // is unwired (test fixture / managed-mode without Docker), the
    // transport is None and ingestion silently skips — the routing
    // tree stays the same shape.
    if let Some(transport) = build_inbound_transport(state) {
        ingest_inbound_attachments(
            state,
            transport.as_ref(),
            msg,
            &cid,
            principal.id.as_str(),
            default_signal_blob_root().as_path(),
        )
        .await;
    }

    // 4. Known contact — dispatch a turn through the existing
    //    pipeline. The agent's response (if any) flows back out via
    //    `signal.send_message` / `signal.reply` (Phase 3), with
    //    `current_chat_id` populated by the dispatcher's per-turn
    //    binding lookup.
    crate::chats::dispatch_external_turn(state, &cid, &principal, trust_flat, &msg.text)
        .await
        .map_err(|e| format!("dispatch_external_turn: {e}"))?;
    // The user_msg event lands inside `commit_turn` (called from
    // run_real_turn / run_tool_capable_turn), so the SPA's chat
    // pane sees it on its next list_messages refresh. We don't
    // publish a live `ChatMessageInbound` here because the
    // dispatcher's existing path handles that broadcast — emitting
    // again would double-fire on every Signal inbound.
    Ok(RouteOutcome::Dispatched {
        principal_id: principal.id.as_str().to_owned(),
        conversation_id: cid.as_str().to_owned(),
    })
}

/// Deterministic principal id for a Signal contact. Mirrors the
/// `pri_<provider>_<id>` shape the existing identity-provider plugins
/// use (see `plugins/google-contacts/main.rhai`'s `pri_google_<hash>`).
/// Stable across consumer restarts so the binding's principal id
/// stays identifiable in the principal store after a process bounce.
fn signal_principal_id(source_number: &str) -> PrincipalId {
    PrincipalId::from(format!("pri_signal_{source_number}"))
}

/// Construct the per-route transport handle used for inbound
/// fetches (today: attachment blob downloads). Returns `None` when
/// the supervisor isn't wired (test fixtures, managed-mode without
/// Docker) — the routing path treats that as "skip attachment
/// ingest" since there's no signal-cli to fetch from anyway.
fn build_inbound_transport(state: &AppState) -> Option<Arc<dyn execlaw_core::tool::TransportApi>> {
    let supervisor = state.sidecar_supervisor.clone()?;
    let resolver: Arc<dyn crate::signal_transport::RpcEndpointResolver> = Arc::new(supervisor);
    let self_number = crate::signal_transport::SignalCliTransport::read_self_number_from_env();
    Some(Arc::new(crate::signal_transport::SignalCliTransport::new(
        resolver,
        state.db.clone(),
        self_number,
        None,
    )))
}

/// Default root for inbound-Signal attachment blobs. Mirrors the
/// research workspace's `<home>/.execlaw/research/` shape so all
/// large blobs cluster under one operator-discoverable prefix.
fn default_signal_blob_root() -> std::path::PathBuf {
    match directories::UserDirs::new() {
        Some(d) => d.home_dir().join(".execlaw").join("blobs").join("signal"),
        None => std::path::PathBuf::from(".execlaw")
            .join("blobs")
            .join("signal"),
    }
}

/// Sanitise a sender-supplied filename for safe inclusion in a
/// disk path. Strips path components, drops control bytes, caps
/// length so a hostile contact can't burn the path-name budget on
/// a single attachment. Always returns a non-empty string —
/// callers compose it with the attachment id, so the worst case
/// is the empty fallback `attachment` reaching disk as
/// `<id>_attachment`.
fn sanitise_attachment_filename(raw: &str) -> String {
    let basename = std::path::Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let cleaned: String = basename
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .take(96)
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "attachment".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Persist every inbound attachment into `state_attachments` + the
/// blob directory and emit one `Attachment` card per ingested file
/// so the SPA's chat pane renders a download chip alongside the
/// inbound message. Failures on any single attachment skip + log
/// rather than fail the whole route — a malformed attachment
/// shouldn't drop the accompanying text message.
///
/// The card is committed under the SENDER's principal id (not
/// "agent") so the card's authorship line in chat correctly
/// attributes the inbound file to the contact who sent it.
async fn ingest_inbound_attachments(
    state: &AppState,
    transport: &dyn execlaw_core::tool::TransportApi,
    msg: &InboundDataMessage,
    cid: &execlaw_core::ids::ConversationId,
    sender_principal_id: &str,
    blob_root: &std::path::Path,
) {
    use crate::cards::{close_card_and_broadcast, open_card_and_broadcast};
    use execlaw_core::attachments::{AttachmentRow, AttachmentStore};
    use execlaw_core::cards::{
        CardAction, CardClosedPayload, CardKind, CardOpenedPayload, CardState,
    };
    use execlaw_core::ids::AttachmentId;
    use sha2::{Digest, Sha256};

    if msg.attachments.is_empty() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(blob_root) {
        tracing::warn!(
            target: "signal_inbound",
            error = %e,
            root = %blob_root.display(),
            "failed to provision signal blob root; skipping attachment ingest",
        );
        return;
    }

    for meta in &msg.attachments {
        // Pre-fetch oversize reject — the bridge tells us the
        // size before we pull the body. Saves both bandwidth and
        // the expense of a half-written file.
        if let Some(size) = meta.size
            && size > MAX_INBOUND_ATTACHMENT_BYTES
        {
            tracing::warn!(
                target: "signal_inbound",
                attachment_id = %meta.bridge_id,
                size,
                "skipping oversize signal attachment",
            );
            continue;
        }

        let fetched = match transport
            .fetch_attachment(SIGNAL_CHANNEL, &meta.bridge_id)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    target: "signal_inbound",
                    attachment_id = %meta.bridge_id,
                    error = %e,
                    "failed to fetch signal attachment; skipping",
                );
                continue;
            }
        };
        // Defence in depth — even though pre-fetch reject covered
        // the advertised case, a misbehaving sidecar might return a
        // body larger than it promised. Reject post-fetch too.
        if (fetched.bytes.len() as u64) > MAX_INBOUND_ATTACHMENT_BYTES {
            tracing::warn!(
                target: "signal_inbound",
                attachment_id = %meta.bridge_id,
                size = fetched.bytes.len(),
                "post-fetch size exceeds cap; dropping",
            );
            continue;
        }

        let mime_type = meta
            .content_type
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if fetched.mime_type.is_empty() {
                    "application/octet-stream".to_owned()
                } else {
                    fetched.mime_type.clone()
                }
            });

        let mut hasher = Sha256::new();
        hasher.update(&fetched.bytes);
        let sha = format!("{:x}", hasher.finalize());

        let attachment_id = AttachmentId::new();
        let filename = sanitise_attachment_filename(
            meta.filename
                .as_deref()
                .or(fetched.filename.as_deref())
                .unwrap_or(""),
        );
        // Audit note: if the future you reaches for tracing the
        // filename in a log line, use `{filename:?}` (Debug)
        // rather than `{filename}` (Display) so unicode RTL
        // override / zero-width / homoglyph tricks render as
        // escaped sequences instead of visually spoofing the
        // operator's audit-log read. The SPA's chip rendering
        // already React-text-escapes; this is purely about log
        // readability.
        let disk_name = format!("{}_{}", attachment_id.as_str(), filename);
        let disk_path = blob_root.join(&disk_name);

        if let Err(e) = std::fs::write(&disk_path, &fetched.bytes) {
            tracing::warn!(
                target: "signal_inbound",
                error = %e,
                path = %disk_path.display(),
                "failed to write signal attachment to disk; skipping",
            );
            continue;
        }

        let row = AttachmentRow {
            id: attachment_id.clone(),
            conversation_id: cid.clone(),
            mime_type: mime_type.clone(),
            path: disk_path.to_string_lossy().into_owned(),
            sha256: sha,
            received_at: chrono::Utc::now().timestamp(),
        };
        if let Err(e) = AttachmentStore::new(&state.db).insert(&row) {
            tracing::warn!(
                target: "signal_inbound",
                attachment_id = %attachment_id.as_str(),
                error = %e,
                "failed to insert AttachmentRow; rolling back blob",
            );
            // Audit fix: no attachment retention sweeper exists
            // today, so leaving an orphaned blob on disk would
            // accumulate forever. Best-effort delete here so a
            // db hiccup doesn't bleed disk over time. If the
            // delete itself fails, the blob is genuinely orphaned;
            // log + accept (very rare path).
            if let Err(rm_err) = std::fs::remove_file(&disk_path) {
                tracing::warn!(
                    target: "signal_inbound",
                    path = %disk_path.display(),
                    error = %rm_err,
                    "failed to roll back orphaned blob; manual cleanup needed",
                );
            }
            continue;
        }

        // Emit the card pair so the SPA renders a download chip
        // inline with the inbound message. Authorship is the
        // sender's principal_id — inbound attachments are
        // attributed to the contact who sent them, not "agent".
        let download_url = format!("/api/attachments/{}", attachment_id.as_str());
        let card_id = format!("att-{}", attachment_id.as_str());
        let summary = format!(
            "{filename} ({mime_type}, {size} bytes)",
            size = fetched.bytes.len(),
        );
        let details = serde_json::json!({
            "attachment_id": attachment_id.as_str(),
            "filename": filename,
            "mime_type": mime_type,
            "byte_size": fetched.bytes.len() as i64,
            "download_url": download_url,
            "caption": serde_json::Value::Null,
        });
        if let Err(e) = open_card_and_broadcast(
            &state.db,
            &state.events,
            cid,
            sender_principal_id,
            &CardOpenedPayload {
                card_id: card_id.clone(),
                kind: CardKind::Attachment,
                title: filename.clone(),
                summary: summary.clone(),
                state: Some(CardState::Running),
                details: details.clone(),
                actions: vec![CardAction::OpenDetail { href: download_url }],
            },
        ) {
            tracing::warn!(
                target: "signal_inbound",
                attachment_id = %attachment_id.as_str(),
                error = %e,
                "failed to open Attachment card; row persisted but no chip rendered",
            );
            continue;
        }
        if let Err(e) = close_card_and_broadcast(
            &state.db,
            &state.events,
            cid,
            sender_principal_id,
            &CardClosedPayload {
                card_id,
                state: CardState::Completed,
                summary,
                details: Some(details),
                attachment_id: Some(attachment_id.as_str().to_owned()),
                error: None,
            },
        ) {
            // Audit fix: ERROR not WARN. The open card has already
            // landed and the SPA is rendering a "Running" chip; if
            // close fails, the chip will spin forever from the
            // user's perspective. Operators need to see this in the
            // log scan so they can manually re-close the card or
            // refresh the conversation.
            tracing::error!(
                target: "signal_inbound",
                attachment_id = %attachment_id.as_str(),
                error = %e,
                "card open succeeded but close failed; SPA will render chip stuck in Running state until conversation reload",
            );
        }
    }
}

/// Phase 5 — route a group-inbound `dataMessage` through the trust
/// pipeline. Identity model:
///
///   * The **group** is the binding subject. `(channel, group_id)`
///     in the binding store points at a `principal_group` whose
///     `native_group_id = group_id`; member principals reconcile
///     lazily as senders post.
///   * The **sender** of each message is a separate principal
///     (`pri_signal_<source_number>`) with its own trust class.
///     The trust gate runs against the SENDER's class — a group
///     full of KnownTrusted contacts plus one Blocked contact
///     drops only the Blocked one's messages.
///   * The **conversation** is per-group (one thread shared across
///     every member's posts), keyed on `(plugin_id, group_id,
///     group_id)` in the conversation resolver. Putting the group
///     id in the resolver's `principal_id` slot is a deliberate
///     re-purpose — the resolver only cares about uniqueness, and
///     the agent UI groups by conversation_id so this collapses
///     all group activity into one thread.
async fn route_group_inbound(
    state: &AppState,
    msg: &InboundDataMessage,
    group_id: &str,
) -> Result<RouteOutcome, String> {
    let now = chrono::Utc::now().timestamp();
    let binding_store = TransportBindingStore::new(&state.db);
    let pg_store = PrincipalGroupStore::new(&state.db);
    let principals = PrincipalStore::new(&state.db);

    // 1. Resolve or mint the GROUP's principal_group + binding.
    let group_pg_id = match binding_store
        .lookup_principal_group(SIGNAL_CHANNEL, group_id)
        .map_err(|e| format!("group binding lookup: {e}"))?
    {
        Some(pg_id) => pg_id,
        None => {
            // First time we've seen this group. Mint a
            // principal_group keyed by native_group_id and an
            // empty principal set; members reconcile as their
            // messages arrive (each individual sender is a
            // separate `pri_signal_<number>` principal). The
            // create_group outbound path takes the same shape.
            let pg = pg_store
                .resolve(
                    &GroupKey {
                        channel: SIGNAL_CHANNEL,
                        native_group_id: Some(group_id),
                        principals: &[],
                        includes_controller: true,
                    },
                    now,
                )
                .map_err(|e| format!("group principal_group mint: {e}"))?;
            let inserted = binding_store
                .insert_binding(SIGNAL_CHANNEL, group_id, &pg.group_id, true, now)
                .map_err(|e| format!("group binding insert: {e}"))?;
            if !inserted {
                // Race: another consumer beat us to the binding
                // (e.g. concurrent inbounds in the same group, or
                // a create_group outbound that just landed). Use
                // the canonical pg_id from the store.
                binding_store
                    .lookup_principal_group(SIGNAL_CHANNEL, group_id)
                    .map_err(|e| format!("group binding re-lookup: {e}"))?
                    .ok_or_else(|| "group binding vanished after insert race".to_owned())?
            } else {
                pg.group_id
            }
        }
    };

    // 2. Resolve or mint the SENDER's principal. The binding doesn't
    //    point here — we just need the principal row for trust
    //    evaluation + the user_msg event's sender field.
    let sender_pid = signal_principal_id(&msg.source_number);
    let sender = match principals
        .get(&sender_pid)
        .map_err(|e| format!("sender principal get: {e}"))?
    {
        Some(p) => {
            let mut updated = p.clone();
            updated.last_seen = Some(now);
            let _ = principals.upsert(&updated);
            updated
        }
        None => {
            let p = mint_unknown_principal(&sender_pid, msg, now);
            principals
                .upsert(&p)
                .map_err(|e| format!("sender principal mint: {e}"))?;
            p
        }
    };

    // 3. Resolve / mint the conversation. Keyed by GROUP, not
    //    sender, so all members' posts share one thread.
    let resolver = ConversationResolver::new(&state.db);
    let outcome = resolver
        .resolve_or_mint(&ResolveInput {
            plugin_id: SIGNAL_PLUGIN_ID,
            transport_handle: group_id,
            principal_id: group_id, // see fn doc-comment for the re-purpose rationale
            is_controller: false,
            idle_timeout_ms: SIGNAL_IDLE_TIMEOUT_MS,
            now,
        })
        .map_err(|e| format!("group conversation resolve: {e}"))?;
    let cid = outcome.conversation_id().clone();
    crate::chats::ensure_conversation_for(&state.db, &cid);
    pg_store
        .bind_conversation(cid.as_str(), &group_pg_id)
        .map_err(|e| format!("bind group conversation: {e}"))?;

    // 4. Trust gate runs against the SENDER's class. One Blocked
    //    member of an otherwise-trusted group only loses their own
    //    message; the rest of the group keeps flowing.
    let trust_tag = sender.trust_level.class_tag();
    let trust_flat = TrustLevel::parse(trust_tag).unwrap_or(TrustLevel::UnknownPending);

    if trust_flat == TrustLevel::Blocked {
        tracing::info!(
            target: "signal_inbound",
            sender = %sender.id.as_str(),
            group_id = %group_id,
            "dropping group inbound from Blocked sender",
        );
        return Ok(RouteOutcome::Blocked {
            principal_id: sender.id.as_str().to_owned(),
        });
    }

    if trust_flat == TrustLevel::UnknownPending {
        // Audit-fix-mirrored from the 1:1 path: cold-contact senders
        // do NOT ingest attachments. Bad-actor surface is even
        // larger in groups (one Blocked-eligible bot can reach
        // multiple controllers' disks via shared groups).
        crate::chats::handle_cold_contact_for_inbound(state, &cid, &sender, &msg.text)
            .await
            .map_err(|e| format!("cold-contact handler: {e}"))?;
        return Ok(RouteOutcome::ColdContact {
            principal_id: sender.id.as_str().to_owned(),
            conversation_id: cid.as_str().to_owned(),
        });
    }

    // Phase 6 — attachment ingest for trusted group senders only.
    if let Some(transport) = build_inbound_transport(state) {
        ingest_inbound_attachments(
            state,
            transport.as_ref(),
            msg,
            &cid,
            sender.id.as_str(),
            default_signal_blob_root().as_path(),
        )
        .await;
    }

    // 5. Known sender — dispatch the turn against the GROUP's
    //    conversation.
    crate::chats::dispatch_external_turn(state, &cid, &sender, trust_flat, &msg.text)
        .await
        .map_err(|e| format!("dispatch_external_turn: {e}"))?;
    Ok(RouteOutcome::Dispatched {
        principal_id: sender.id.as_str().to_owned(),
        conversation_id: cid.as_str().to_owned(),
    })
}

/// Mint a fresh `UnknownPending` principal for first-contact
/// intake. Trust class is the load-bearing field — it forces the
/// cold-contact gate to fire on the next routing pass even if the
/// caller forgets to check.
fn mint_unknown_principal(pid: &PrincipalId, msg: &InboundDataMessage, now: i64) -> Principal {
    Principal {
        id: pid.clone(),
        identifiers: vec![Identifier {
            transport: SIGNAL_CHANNEL.to_owned(),
            handle: msg.source_number.clone(),
        }],
        trust_level: CoreTrustLevel::UnknownPending {
            first_seen: now,
            notification_event_seq: None,
        },
        resolved_by: vec![],
        metadata: serde_json::json!({
            "signal_display_name": msg.source_name,
        }),
        first_seen: now,
        last_seen: Some(now),
        controller_notes: None,
    }
}

// -----------------------------------------------------------------
// Consumer task
// -----------------------------------------------------------------

/// Best-effort GET `/v1/accounts` against the supervised sidecar.
/// Returns the first registered E.164 number, or `None` for any
/// failure (sidecar not running, daemon not up yet, no account
/// paired). The consumer treats `None` as "not paired yet — sleep
/// and try again next tick" so a freshly-installed plugin sits
/// idle until the operator scans a QR rather than crash-looping.
async fn fetch_first_account(client: &reqwest::Client, port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/v1/accounts");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let arr: Vec<String> = resp.json().await.ok()?;
    arr.into_iter().next()
}

/// Spawn the inbound consumer on `state`'s tokio runtime, returning
/// the `JoinHandle` so the boot orchestrator can await on shutdown.
/// `stop` is the same `Notify` the sidecar supervisor watches —
/// `stop.notify_waiters()` shuts both down cleanly.
///
/// Returns `None` only when `state.sidecar_supervisor` is `None`
/// (test fixture / managed-mode install with no Docker reachable).
/// The self-number is resolved dynamically per connect attempt
/// against the sidecar's `/v1/accounts` endpoint — it doesn't
/// require any env var, and it picks up the operator's pairing
/// without a server restart.
pub fn spawn_signal_inbound_consumer(
    state: AppState,
    stop: Arc<Notify>,
) -> Option<tokio::task::JoinHandle<()>> {
    let supervisor = state.sidecar_supervisor.clone()?;
    Some(tokio::spawn(async move {
        run_consumer_loop(state, supervisor, None, stop).await;
    }))
}

/// Drive the connect-read-reconnect loop. Public for tests that
/// want to exercise the loop without going through `tokio::spawn`.
///
/// `self_number_override`: when `Some`, the loop uses it verbatim
/// (test paths, env-var override). When `None`, the loop fetches
/// `/v1/accounts` against the sidecar on each connect attempt and
/// uses the first registered number — auto-tracks pairing without
/// restart.
pub async fn run_consumer_loop(
    state: AppState,
    supervisor: crate::sidecar_supervisor::SidecarSupervisor,
    self_number_override: Option<String>,
    stop: Arc<Notify>,
) {
    use tokio_tungstenite::tungstenite::Message;

    tracing::info!(
        target: "signal_inbound",
        override_self_number = ?self_number_override,
        "signal inbound consumer starting",
    );
    let mut backoff = RECONNECT_MIN;
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    'outer: loop {
        tokio::select! {
            _ = stop.notified() => {
                tracing::info!(target: "signal_inbound", "stop received; exiting");
                return;
            }
            _ = async {} => {}
        }

        // 1. Resolve the sidecar's host port. Sleep + retry when
        //    unpublished so we don't hammer the supervisor's lock.
        let port = match supervisor
            .host_port_for(crate::signal_transport::SIGNAL_SIDECAR_NAME)
            .await
        {
            Some(p) => p,
            None => {
                tokio::select! {
                    _ = stop.notified() => return,
                    _ = tokio::time::sleep(SIDECAR_POLL_INTERVAL) => continue 'outer,
                }
            }
        };

        // 1b. Resolve self-number. Operator's env-var override wins
        //     (matches the existing outbound contract); otherwise
        //     dial /v1/accounts to discover the paired number. When
        //     no account is paired yet, sleep + retry — the consumer
        //     can sit idle here for hours during a fresh install
        //     waiting for the operator to scan a QR.
        let self_number = match self_number_override.clone() {
            Some(n) => n,
            None => match fetch_first_account(&http_client, port).await {
                Some(n) => n,
                None => {
                    tracing::debug!(
                        target: "signal_inbound",
                        "no Signal account paired yet; sleeping then retrying"
                    );
                    tokio::select! {
                        _ = stop.notified() => return,
                        _ = tokio::time::sleep(SIDECAR_POLL_INTERVAL) => continue 'outer,
                    }
                }
            },
        };
        let url = format!("ws://127.0.0.1:{port}/v1/receive/{self_number}");

        // 2. Connect.
        let connect = tokio_tungstenite::connect_async(&url);
        let socket = tokio::select! {
            _ = stop.notified() => return,
            res = connect => match res {
                Ok((sock, _)) => sock,
                Err(e) => {
                    tracing::warn!(target: "signal_inbound", error = %e, url = %url, "connect failed; backing off");
                    tokio::select! {
                        _ = stop.notified() => return,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue 'outer;
                }
            }
        };
        tracing::info!(target: "signal_inbound", url = %url, "connected");
        backoff = RECONNECT_MIN;
        // Track how many frames the connection actually delivered.
        // Audit fix #3: on a connect-then-immediate-disconnect cycle
        // (sidecar accepts then crashes), we used to skip the
        // outer-loop backoff because connect succeeded. That spun
        // the read loop ~3-5x/sec hammering logs + the supervisor's
        // mutex. Now: if the connection delivered ZERO frames
        // before failing, treat it like a connect failure and
        // double the backoff.
        let mut frames_received: u64 = 0;

        let (_writer, mut reader) = futures::StreamExt::split(socket);
        // 3. Read loop.
        loop {
            let frame = tokio::select! {
                _ = stop.notified() => return,
                f = futures::StreamExt::next(&mut reader) => f,
            };
            let msg = match frame {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    tracing::warn!(target: "signal_inbound", error = %e, "ws read error; reconnecting");
                    break;
                }
                None => {
                    tracing::info!(target: "signal_inbound", "ws closed by peer; reconnecting");
                    break;
                }
            };
            frames_received = frames_received.saturating_add(1);
            let body = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => {
                    tracing::info!(target: "signal_inbound", "ws close frame; reconnecting");
                    break;
                }
            };
            let Some(decoded) = decode_frame(&body, Some(&self_number)) else {
                continue;
            };
            // Route the message. Errors here are logged + dropped —
            // a single bad frame must not take down the whole
            // subscriber.
            match route_inbound_message(&state, &decoded).await {
                Ok(outcome) => {
                    tracing::info!(
                        target: "signal_inbound",
                        outcome = ?outcome,
                        source = %decoded.source_number,
                        "routed inbound signal message",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "signal_inbound",
                        error = %e,
                        source = %decoded.source_number,
                        "route_inbound_message failed",
                    );
                }
            }
        }

        // Read loop broke. If the connection delivered zero frames
        // before failing, double the backoff before the next
        // reconnect — otherwise an immediately-failing sidecar
        // would spin this loop several times per second.
        if frames_received == 0 {
            tracing::debug!(
                target: "signal_inbound",
                backoff_secs = backoff.as_secs(),
                "connection delivered no frames before disconnect; backing off",
            );
            tokio::select! {
                _ = stop.notified() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_text_frame(account: &str, source: &str, text: &str) -> String {
        serde_json::json!({
            "account": account,
            "envelope": {
                "source": source,
                "sourceNumber": source,
                "sourceName": "Alice",
                "timestamp": 1700000000000_i64,
                "dataMessage": {
                    "timestamp": 1700000000000_i64,
                    "message": text,
                    "groupInfo": null,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn decode_canonical_text_frame() {
        let body = make_text_frame("+15551234567", "+15559998888", "hello");
        let decoded = decode_frame(&body, Some("+15551234567")).expect("must decode");
        assert_eq!(decoded.source_number, "+15559998888");
        assert_eq!(decoded.text, "hello");
        assert_eq!(decoded.source_name.as_deref(), Some("Alice"));
        assert!(decoded.group_id.is_none());
    }

    #[test]
    fn decode_drops_account_mismatch() {
        let body = make_text_frame("+19999999999", "+15559998888", "hello");
        // Self number is +15551234567 but the frame is addressed to
        // +19999999999 — must drop.
        assert!(decode_frame(&body, Some("+15551234567")).is_none());
    }

    #[test]
    fn decode_drops_when_no_account_passed_through_unconditionally() {
        // When the consumer doesn't pass an expected_account (e.g.
        // a test fixture), all frames pass the multi-account guard.
        let body = make_text_frame("+15551234567", "+15559998888", "hello");
        assert!(decode_frame(&body, None).is_some());
    }

    #[test]
    fn decode_drops_typing_indicator() {
        // Typing indicators have no dataMessage at all.
        let body = serde_json::json!({
            "account": "+15551234567",
            "envelope": {
                "source": "+15559998888",
                "timestamp": 1700000000000_i64,
                "typingMessage": {
                    "action": "STARTED",
                    "timestamp": 1700000000000_i64,
                }
            }
        })
        .to_string();
        assert!(decode_frame(&body, Some("+15551234567")).is_none());
    }

    #[test]
    fn decode_drops_text_and_attachment_empty_message() {
        // dataMessage with NO text AND NO attachments — pure
        // receipt / typing / sync. Must drop. (Phase 6 keeps
        // attachment-only frames; the test below pins that.)
        let body = serde_json::json!({
            "account": "+15551234567",
            "envelope": {
                "source": "+15559998888",
                "sourceNumber": "+15559998888",
                "timestamp": 1700000000000_i64,
                "dataMessage": {
                    "timestamp": 1700000000000_i64,
                    "message": null,
                    "groupInfo": null,
                    "attachments": [],
                }
            }
        })
        .to_string();
        assert!(decode_frame(&body, Some("+15551234567")).is_none());
    }

    #[test]
    fn decode_keeps_attachment_only_message_with_empty_text() {
        // Phase 6: an inbound carrying only an image / voice note
        // (no text body) must still route — the consumer ingests
        // the attachment as a card. Text becomes the empty string
        // and `attachments` carries the metadata.
        let body = serde_json::json!({
            "account": "+15551234567",
            "envelope": {
                "source": "+15559998888",
                "sourceNumber": "+15559998888",
                "timestamp": 1700000000000_i64,
                "dataMessage": {
                    "timestamp": 1700000000000_i64,
                    "message": null,
                    "groupInfo": null,
                    "attachments": [
                        {
                            "id": "att-base64-id",
                            "contentType": "image/jpeg",
                            "filename": "selfie.jpg",
                            "size": 24_576,
                        }
                    ]
                }
            }
        })
        .to_string();
        let decoded = decode_frame(&body, Some("+15551234567")).expect("must decode");
        assert_eq!(decoded.text, "");
        assert_eq!(decoded.attachments.len(), 1);
        assert_eq!(decoded.attachments[0].bridge_id, "att-base64-id");
        assert_eq!(
            decoded.attachments[0].content_type.as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            decoded.attachments[0].filename.as_deref(),
            Some("selfie.jpg")
        );
        assert_eq!(decoded.attachments[0].size, Some(24_576));
    }

    #[test]
    fn decode_strips_attachments_with_empty_id() {
        // Defensive: a bridge that emits an attachment with no id
        // gives us nothing to fetch; drop it from the metadata so
        // the persistence layer doesn't try.
        let body = serde_json::json!({
            "account": "+15551234567",
            "envelope": {
                "source": "+15559998888",
                "sourceNumber": "+15559998888",
                "timestamp": 1700000000000_i64,
                "dataMessage": {
                    "timestamp": 1700000000000_i64,
                    "message": "with garbage attachment",
                    "groupInfo": null,
                    "attachments": [
                        { "id": "", "contentType": "image/jpeg" },
                        { "id": "good", "contentType": "image/png" }
                    ]
                }
            }
        })
        .to_string();
        let decoded = decode_frame(&body, Some("+15551234567")).expect("must decode");
        assert_eq!(decoded.attachments.len(), 1);
        assert_eq!(decoded.attachments[0].bridge_id, "good");
    }

    #[test]
    fn decode_drops_whitespace_only_message() {
        let body = make_text_frame("+15551234567", "+15559998888", "   \n\t  ");
        assert!(decode_frame(&body, Some("+15551234567")).is_none());
    }

    #[test]
    fn decode_picks_up_group_id_when_present() {
        let body = serde_json::json!({
            "account": "+15551234567",
            "envelope": {
                "source": "+15559998888",
                "sourceNumber": "+15559998888",
                "timestamp": 1700000000000_i64,
                "dataMessage": {
                    "timestamp": 1700000000000_i64,
                    "message": "to the group",
                    "groupInfo": {
                        "groupId": "group-base64-id",
                    }
                }
            }
        })
        .to_string();
        let decoded = decode_frame(&body, Some("+15551234567")).expect("must decode");
        assert_eq!(decoded.group_id.as_deref(), Some("group-base64-id"));
    }

    #[test]
    fn decode_returns_none_for_unparseable_garbage() {
        assert!(decode_frame("not json at all", None).is_none());
        assert!(decode_frame("{}", None).is_none());
        assert!(decode_frame("{\"envelope\":42}", None).is_none());
    }

    #[test]
    fn signal_principal_id_is_deterministic_and_distinct_per_number() {
        let a1 = signal_principal_id("+15551234567");
        let a2 = signal_principal_id("+15551234567");
        let b = signal_principal_id("+15559998888");
        assert_eq!(a1.as_str(), a2.as_str());
        assert_ne!(a1.as_str(), b.as_str());
        assert!(a1.as_str().starts_with("pri_signal_+"));
    }

    // -----------------------------------------------------------------
    // Routing tests (cover the trust-pipeline integration end-to-end
    // without spinning a sidecar).
    // -----------------------------------------------------------------

    use crate::routes::test_app_state;

    fn dm(source_number: &str, text: &str) -> InboundDataMessage {
        InboundDataMessage {
            source_number: source_number.to_owned(),
            source_name: Some("Alice".into()),
            text: text.to_owned(),
            group_id: None,
            sender_timestamp_ms: Some(1700000000000),
            attachments: vec![],
        }
    }

    #[tokio::test]
    async fn route_group_inbound_first_message_mints_group_binding_not_member_binding() {
        // Phase 5: group routing. The binding is on the group_id,
        // not the sender's number. The sender becomes a separate
        // principal (which lands in cold-contact since it's
        // first-contact).
        let state = test_app_state();
        let mut msg = dm("+15559998888", "first message in a group");
        msg.group_id = Some("g-base64-id".into());
        let outcome = route_inbound_message(&state, &msg).await.unwrap();
        // First-contact sender takes the cold-contact path.
        assert!(matches!(outcome, RouteOutcome::ColdContact { .. }));
        let store = TransportBindingStore::new(&state.db);
        // Group binding exists.
        let group_pg = store
            .lookup_principal_group(SIGNAL_CHANNEL, "g-base64-id")
            .unwrap()
            .expect("group binding must be minted on first group inbound");
        assert!(!group_pg.is_empty());
        // 1:1 binding for the sender does NOT exist — group inbound
        // must not pollute the contact-routing namespace.
        assert!(
            store
                .lookup_principal_group(SIGNAL_CHANNEL, "+15559998888")
                .unwrap()
                .is_none(),
            "group inbound must NOT mint a 1:1 binding for the sender",
        );
        // The sender's principal exists with UnknownPending trust.
        let sender_p = PrincipalStore::new(&state.db)
            .get(&signal_principal_id("+15559998888"))
            .unwrap()
            .expect("sender principal must be minted");
        assert_eq!(sender_p.trust_level.class_tag(), "UnknownPending");
    }

    #[tokio::test]
    async fn route_group_inbound_second_message_reuses_group_binding() {
        let state = test_app_state();
        let mut msg = dm("+15559998888", "first");
        msg.group_id = Some("g-id".into());
        let _ = route_inbound_message(&state, &msg).await.unwrap();
        let store = TransportBindingStore::new(&state.db);
        let pg_id_first = store
            .lookup_principal_group(SIGNAL_CHANNEL, "g-id")
            .unwrap()
            .unwrap();
        // Different sender, same group.
        let mut msg2 = dm("+15553334444", "second");
        msg2.group_id = Some("g-id".into());
        let _ = route_inbound_message(&state, &msg2).await.unwrap();
        let pg_id_second = store
            .lookup_principal_group(SIGNAL_CHANNEL, "g-id")
            .unwrap()
            .unwrap();
        assert_eq!(
            pg_id_first, pg_id_second,
            "group binding must be reused on subsequent inbound from any member",
        );
    }

    #[tokio::test]
    async fn route_group_inbound_drops_blocked_sender_only() {
        // A group containing a Blocked sender must drop only that
        // sender's messages — other members keep flowing.
        let state = test_app_state();
        let now = chrono::Utc::now().timestamp();
        let blocked_pid = signal_principal_id("+15551112222");
        PrincipalStore::new(&state.db)
            .upsert(&Principal {
                id: blocked_pid.clone(),
                identifiers: vec![Identifier {
                    transport: SIGNAL_CHANNEL.into(),
                    handle: "+15551112222".into(),
                }],
                trust_level: CoreTrustLevel::Blocked {
                    blocked_at: now,
                    blocked_by: PrincipalId::from("controller"),
                    reason: None,
                },
                resolved_by: vec![],
                metadata: serde_json::json!({}),
                first_seen: now,
                last_seen: Some(now),
                controller_notes: None,
            })
            .unwrap();
        let mut msg = dm("+15551112222", "spam");
        msg.group_id = Some("g-spam".into());
        let outcome = route_inbound_message(&state, &msg).await.unwrap();
        assert!(matches!(outcome, RouteOutcome::Blocked { .. }));
    }

    #[tokio::test]
    async fn route_first_contact_mints_binding_and_takes_cold_contact_path() {
        let state = test_app_state();
        let outcome = route_inbound_message(&state, &dm("+15559998888", "hello"))
            .await
            .unwrap();
        match outcome {
            RouteOutcome::ColdContact {
                principal_id,
                conversation_id,
            } => {
                assert_eq!(principal_id, "pri_signal_+15559998888");
                assert!(!conversation_id.is_empty());
            }
            other => panic!("expected ColdContact, got {other:?}"),
        }
        // Binding must now exist so the next inbound resolves directly.
        let pg_id = TransportBindingStore::new(&state.db)
            .lookup_principal_group(SIGNAL_CHANNEL, "+15559998888")
            .unwrap()
            .expect("binding must be minted on first contact");
        assert!(!pg_id.is_empty());
        // Principal stored as UnknownPending.
        let p = PrincipalStore::new(&state.db)
            .get(&signal_principal_id("+15559998888"))
            .unwrap()
            .expect("principal must be minted");
        assert_eq!(p.trust_level.class_tag(), "UnknownPending");
    }

    #[tokio::test]
    async fn route_second_message_from_same_contact_reuses_binding() {
        let state = test_app_state();
        // First message → first-contact path mints the binding.
        let _ = route_inbound_message(&state, &dm("+15559998888", "hello"))
            .await
            .unwrap();
        let bindings_after_first = TransportBindingStore::new(&state.db)
            .bindings_for_group(
                &TransportBindingStore::new(&state.db)
                    .lookup_principal_group(SIGNAL_CHANNEL, "+15559998888")
                    .unwrap()
                    .unwrap(),
                SIGNAL_CHANNEL,
            )
            .unwrap();
        assert_eq!(bindings_after_first.len(), 1);
        // Second message → cold-contact again (still UnknownPending),
        // but no new binding row.
        let outcome = route_inbound_message(&state, &dm("+15559998888", "still here?"))
            .await
            .unwrap();
        assert!(matches!(outcome, RouteOutcome::ColdContact { .. }));
        let bindings_after_second = TransportBindingStore::new(&state.db)
            .bindings_for_group(
                &TransportBindingStore::new(&state.db)
                    .lookup_principal_group(SIGNAL_CHANNEL, "+15559998888")
                    .unwrap()
                    .unwrap(),
                SIGNAL_CHANNEL,
            )
            .unwrap();
        assert_eq!(
            bindings_after_second.len(),
            1,
            "second inbound must reuse the binding, not mint a duplicate",
        );
    }

    #[tokio::test]
    async fn route_drops_blocked_principal_without_dispatching_turn() {
        let state = test_app_state();
        // Mint a Blocked principal + binding manually (simulates an
        // operator blocking a contact via Settings → Trust).
        let pid = signal_principal_id("+15551112222");
        let now = chrono::Utc::now().timestamp();
        PrincipalStore::new(&state.db)
            .upsert(&Principal {
                id: pid.clone(),
                identifiers: vec![Identifier {
                    transport: SIGNAL_CHANNEL.into(),
                    handle: "+15551112222".into(),
                }],
                trust_level: CoreTrustLevel::Blocked {
                    blocked_at: now,
                    blocked_by: PrincipalId::from("controller"),
                    reason: None,
                },
                resolved_by: vec![],
                metadata: serde_json::json!({}),
                first_seen: now,
                last_seen: Some(now),
                controller_notes: None,
            })
            .unwrap();
        let pg = PrincipalGroupStore::new(&state.db)
            .resolve(
                &GroupKey {
                    channel: SIGNAL_CHANNEL,
                    native_group_id: None,
                    principals: &[pid.clone()],
                    includes_controller: false,
                },
                now,
            )
            .unwrap();
        TransportBindingStore::new(&state.db)
            .insert_binding(SIGNAL_CHANNEL, "+15551112222", &pg.group_id, false, now)
            .unwrap();
        let outcome = route_inbound_message(&state, &dm("+15551112222", "spam"))
            .await
            .unwrap();
        match outcome {
            RouteOutcome::Blocked { principal_id } => {
                assert_eq!(principal_id, "pri_signal_+15551112222");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_drops_when_binding_exists_but_principal_row_is_missing() {
        // Audit fix #2 from the Phase 4 audit: a binding pointing at
        // a missing principal row used to silently re-mint as
        // UnknownPending, which downgraded a previously-Blocked
        // principal back to cold-contact. The fix drops the inbound
        // with an audit log line instead.
        let state = test_app_state();
        let pid = signal_principal_id("+15557776666");
        let now = chrono::Utc::now().timestamp();
        let pg = PrincipalGroupStore::new(&state.db)
            .resolve(
                &GroupKey {
                    channel: SIGNAL_CHANNEL,
                    native_group_id: None,
                    principals: &[pid.clone()],
                    includes_controller: false,
                },
                now,
            )
            .unwrap();
        TransportBindingStore::new(&state.db)
            .insert_binding(SIGNAL_CHANNEL, "+15557776666", &pg.group_id, false, now)
            .unwrap();
        // NOTE: deliberately do NOT insert a Principal row. This
        // simulates the dangling-binding state the audit flagged.
        let outcome = route_inbound_message(&state, &dm("+15557776666", "spam"))
            .await
            .unwrap();
        assert!(
            matches!(outcome, RouteOutcome::Blocked { .. }),
            "missing principal must NOT be silently re-minted as UnknownPending; got {outcome:?}"
        );
        // And the principal should still be missing — the route
        // path must NOT have minted a replacement.
        assert!(
            PrincipalStore::new(&state.db).get(&pid).unwrap().is_none(),
            "route path must not silently mint a replacement principal",
        );
    }

    #[tokio::test]
    async fn route_known_contact_dispatches_turn() {
        let state = test_app_state();
        // Mint a KnownTrusted principal + binding to bypass the
        // cold-contact gate.
        let pid = signal_principal_id("+15553334444");
        let now = chrono::Utc::now().timestamp();
        PrincipalStore::new(&state.db)
            .upsert(&Principal {
                id: pid.clone(),
                identifiers: vec![Identifier {
                    transport: SIGNAL_CHANNEL.into(),
                    handle: "+15553334444".into(),
                }],
                trust_level: CoreTrustLevel::KnownTrusted {
                    resolvers: vec![],
                    approved_by: PrincipalId::from("controller"),
                    approved_at: now,
                },
                resolved_by: vec![],
                metadata: serde_json::json!({}),
                first_seen: now,
                last_seen: Some(now),
                controller_notes: None,
            })
            .unwrap();
        let pg = PrincipalGroupStore::new(&state.db)
            .resolve(
                &GroupKey {
                    channel: SIGNAL_CHANNEL,
                    native_group_id: None,
                    principals: &[pid.clone()],
                    includes_controller: false,
                },
                now,
            )
            .unwrap();
        TransportBindingStore::new(&state.db)
            .insert_binding(SIGNAL_CHANNEL, "+15553334444", &pg.group_id, false, now)
            .unwrap();
        let outcome = route_inbound_message(&state, &dm("+15553334444", "hi"))
            .await
            .unwrap();
        match outcome {
            RouteOutcome::Dispatched {
                principal_id,
                conversation_id,
            } => {
                assert_eq!(principal_id, "pri_signal_+15553334444");
                assert!(!conversation_id.is_empty());
                // Conversation should now be bound to the principal_group.
                let bound = PrincipalGroupStore::new(&state.db)
                    .principal_group_id_for(&conversation_id)
                    .unwrap();
                assert_eq!(bound.as_deref(), Some(pg.group_id.as_str()));
            }
            other => panic!("expected Dispatched, got {other:?}"),
        }
    }

    // ---- Phase 6: attachment ingestion --------------------------

    #[test]
    fn sanitise_attachment_filename_strips_path_components() {
        assert_eq!(sanitise_attachment_filename("hello.jpg"), "hello.jpg");
        assert_eq!(sanitise_attachment_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_attachment_filename("/etc/passwd"), "passwd");
        // The backslash case differs by platform — Windows treats
        // `\` as a path separator so `Path::file_name` returns
        // `file.exe`; Unix keeps `evil\file.exe` and the char-set
        // filter strips the backslash to `evilfile.exe`. Both
        // outcomes are safe (no path traversal); we accept either.
        let backslash = sanitise_attachment_filename("evil\\file.exe");
        assert!(
            backslash == "evilfile.exe" || backslash == "file.exe",
            "got {backslash:?}",
        );
        // Forward-slash path traversal is a hard rule on every
        // platform — the backslash case above is just OS-dependent
        // bonus stripping.
        assert!(!sanitise_attachment_filename("a/b").contains('/'));
    }

    #[test]
    fn sanitise_attachment_filename_drops_control_bytes_and_caps_length() {
        let with_nul = "name\0\n.jpg";
        assert_eq!(sanitise_attachment_filename(with_nul), "name.jpg");
        let huge = "a".repeat(500) + ".jpg";
        let cleaned = sanitise_attachment_filename(&huge);
        assert!(cleaned.len() <= 96, "len = {}", cleaned.len());
    }

    #[test]
    fn sanitise_attachment_filename_falls_back_when_blank() {
        assert_eq!(sanitise_attachment_filename(""), "attachment");
        assert_eq!(sanitise_attachment_filename("..."), "attachment");
        assert_eq!(sanitise_attachment_filename("/"), "attachment");
    }

    /// In-process TransportApi that lets attachment-ingest tests
    /// drive `fetch_attachment` without a sidecar. Records every
    /// fetch + serves a static body the test pre-loaded.
    struct StubTransport {
        responses: std::sync::Mutex<
            std::collections::HashMap<String, execlaw_core::tool::FetchedAttachment>,
        >,
        fetch_log: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl execlaw_core::tool::TransportApi for StubTransport {
        async fn resolve_recipient(
            &self,
            _: &str,
            _: &str,
        ) -> Result<String, execlaw_core::tool::ApiError> {
            unreachable!("attachment ingest doesn't resolve recipients")
        }
        async fn send(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, execlaw_core::tool::ApiError> {
            unreachable!()
        }
        async fn current_chat_id(
            &self,
            _: &str,
        ) -> Result<Option<String>, execlaw_core::tool::ApiError> {
            Ok(None)
        }
        async fn fetch_attachment(
            &self,
            _: &str,
            attachment_id: &str,
        ) -> Result<execlaw_core::tool::FetchedAttachment, execlaw_core::tool::ApiError> {
            self.fetch_log
                .lock()
                .unwrap()
                .push(attachment_id.to_owned());
            self.responses
                .lock()
                .unwrap()
                .get(attachment_id)
                .cloned()
                .ok_or_else(|| {
                    execlaw_core::tool::ApiError::NotFound(format!(
                        "stub: no attachment {attachment_id}"
                    ))
                })
        }
    }

    #[tokio::test]
    async fn ingest_writes_blob_inserts_row_emits_card() {
        use execlaw_core::attachments::AttachmentStore;
        use execlaw_core::ids::ConversationId;
        let state = test_app_state();
        let cid = ConversationId::from_string("conv-att-test");
        crate::chats::ensure_conversation_for(&state.db, &cid);

        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "att-1".to_owned(),
            execlaw_core::tool::FetchedAttachment {
                bytes: vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F'],
                mime_type: "image/jpeg".into(),
                filename: None,
            },
        );
        let stub = StubTransport {
            responses: std::sync::Mutex::new(responses),
            fetch_log: std::sync::Mutex::new(vec![]),
        };

        let mut msg = dm("+15559998888", "look at this");
        msg.attachments = vec![InboundAttachmentMeta {
            bridge_id: "att-1".into(),
            content_type: Some("image/jpeg".into()),
            filename: Some("selfie.jpg".into()),
            size: Some(10),
        }];

        let blob_root = tempfile::tempdir().unwrap();
        ingest_inbound_attachments(
            &state,
            &stub,
            &msg,
            &cid,
            "pri_signal_+15559998888",
            blob_root.path(),
        )
        .await;

        // Fetch was issued exactly once with the bridge id.
        assert_eq!(*stub.fetch_log.lock().unwrap(), vec!["att-1".to_owned()]);

        // state_attachments has one row scoped to the conversation.
        let store = AttachmentStore::new(&state.db);
        let mut found_row = None;
        for entry in std::fs::read_dir(blob_root.path()).unwrap() {
            let entry = entry.unwrap();
            let id_str = entry
                .file_name()
                .to_string_lossy()
                .split_once('_')
                .map(|(id, _)| id.to_owned())
                .unwrap();
            let row = store
                .get(&execlaw_core::ids::AttachmentId::from(id_str.clone()))
                .unwrap()
                .expect("row must exist");
            assert_eq!(row.conversation_id.as_str(), "conv-att-test");
            assert_eq!(row.mime_type, "image/jpeg");
            // Disk file matches the bytes we stubbed.
            let disk = std::fs::read(&row.path).unwrap();
            assert_eq!(disk.len(), 10);
            found_row = Some(row);
            break;
        }
        assert!(found_row.is_some(), "no attachment row landed");
    }

    #[tokio::test]
    async fn ingest_skips_oversize_attachment_pre_fetch() {
        use execlaw_core::ids::ConversationId;
        let state = test_app_state();
        let cid = ConversationId::from_string("conv-oversize");
        crate::chats::ensure_conversation_for(&state.db, &cid);
        // The stub has NO response — if ingest tried to fetch
        // anyway, we'd see a NotFound error. Instead the size cap
        // must reject before fetch fires.
        let stub = StubTransport {
            responses: std::sync::Mutex::new(std::collections::HashMap::new()),
            fetch_log: std::sync::Mutex::new(vec![]),
        };
        let mut msg = dm("+15559998888", "huge file");
        msg.attachments = vec![InboundAttachmentMeta {
            bridge_id: "huge".into(),
            content_type: Some("application/octet-stream".into()),
            filename: Some("huge.bin".into()),
            size: Some(MAX_INBOUND_ATTACHMENT_BYTES + 1),
        }];
        let blob_root = tempfile::tempdir().unwrap();
        ingest_inbound_attachments(
            &state,
            &stub,
            &msg,
            &cid,
            "pri_signal_+15559998888",
            blob_root.path(),
        )
        .await;
        // No fetch should have happened.
        assert!(stub.fetch_log.lock().unwrap().is_empty());
        // No file written.
        let entries: Vec<_> = std::fs::read_dir(blob_root.path()).unwrap().collect();
        assert!(entries.is_empty(), "oversize blob must not land on disk");
    }

    #[tokio::test]
    async fn cold_contact_inbound_does_not_persist_attachments() {
        // Audit fix: a brand-new (UnknownPending) sender posting an
        // attachment-laden message must NOT land any blobs on disk
        // before the operator approves them — otherwise a bot
        // sending 100 × 25MiB before approval would burn 2.5GB
        // pre-cold-contact. Test pins the gate.
        let state = test_app_state();
        let mut msg = dm("+15559998888", "spam with attachment");
        msg.attachments = vec![InboundAttachmentMeta {
            bridge_id: "att-evil".into(),
            content_type: Some("image/jpeg".into()),
            filename: Some("payload.jpg".into()),
            size: Some(1024),
        }];
        let outcome = route_inbound_message(&state, &msg).await.unwrap();
        // First-contact gate fires.
        assert!(matches!(outcome, RouteOutcome::ColdContact { .. }));
        // No state_attachments row inserted — the ingest path was
        // skipped before the cold-contact handler. (Even if the
        // test's transport were live, the routing layer is gated
        // on trust class.)
        let conv_id = match outcome {
            RouteOutcome::ColdContact {
                ref conversation_id,
                ..
            } => conversation_id.clone(),
            _ => unreachable!(),
        };
        let count: i64 = state
            .db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM state_attachments WHERE conversation_id = ?1",
                    rusqlite::params![conv_id],
                    |r| r.get(0),
                )
                .unwrap_or(0))
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "cold-contact inbound must not insert attachment rows",
        );
    }

    #[tokio::test]
    async fn ingest_continues_past_one_failed_fetch() {
        use execlaw_core::ids::ConversationId;
        let state = test_app_state();
        let cid = ConversationId::from_string("conv-mixed");
        crate::chats::ensure_conversation_for(&state.db, &cid);
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "good".to_owned(),
            execlaw_core::tool::FetchedAttachment {
                bytes: vec![1, 2, 3, 4],
                mime_type: "text/plain".into(),
                filename: None,
            },
        );
        // "bad" intentionally absent from responses → fetch errors.
        let stub = StubTransport {
            responses: std::sync::Mutex::new(responses),
            fetch_log: std::sync::Mutex::new(vec![]),
        };
        let mut msg = dm("+15559998888", "two attachments");
        msg.attachments = vec![
            InboundAttachmentMeta {
                bridge_id: "bad".into(),
                content_type: Some("text/plain".into()),
                filename: None,
                size: Some(4),
            },
            InboundAttachmentMeta {
                bridge_id: "good".into(),
                content_type: Some("text/plain".into()),
                filename: None,
                size: Some(4),
            },
        ];
        let blob_root = tempfile::tempdir().unwrap();
        ingest_inbound_attachments(
            &state,
            &stub,
            &msg,
            &cid,
            "pri_signal_+15559998888",
            blob_root.path(),
        )
        .await;
        // Both fetches were attempted.
        assert_eq!(stub.fetch_log.lock().unwrap().len(), 2);
        // Exactly ONE blob landed (the "good" one).
        let entries: Vec<_> = std::fs::read_dir(blob_root.path())
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(entries.len(), 1);
    }
}
