//! Generic card primitive — operator-visible "this long-running task
//! is happening" UI affordance, event-sourced over the existing
//! `state_events` log.
//!
//! ## Why a primitive
//!
//! Long-running tools (deep research, shell sessions, file pipelines,
//! batch jobs, agent fan-outs) all want roughly the same UX: a
//! progress card in the conversation that updates live, has operator
//! controls (cancel / pause / open detail), and degrades to plain
//! text on channels that can't render structured UI (Signal,
//! WhatsApp, SMS, voice). Standardising lets the platform invest
//! once in event schema, projection, transport adaptation, and
//! per-channel update policy — then any new long-running tool plugs
//! in by emitting a `kind` and reading from the typed `details_json`
//! it owns.
//!
//! ## Event flow
//!
//! Three event kinds (`EventKind::CardOpened`, `CardProgressed`,
//! `CardClosed`) commit to the same `state_events` log every other
//! conversation event uses. Event-sourced: the live `Card` row is the
//! projection of the sequence; immutable history is preserved.
//!
//! ## Channel adaptation
//!
//! Each [`CardOpened`] / [`CardProgressed`] / [`CardClosed`] event
//! carries a `summary` string — the natural-language sentence the
//! card primitive contributes to channels that don't render rich UI.
//! Transport plugins consult their own [`CardCapability`]
//! (None / TextOnly / Rich / Native) + the operator's per-channel
//! update policy (Silent / Milestones / Live) to decide which events
//! to project as text messages on the wire.
//!
//! 2026-04-29.

use serde::{Deserialize, Serialize};

/// Card type discriminator. The web SPA's renderer registry keys on
/// this. Plugins can register additional kinds via the SDK's
/// `register_card_kind` call (forthcoming alongside the SDK's
/// programmatic-tool builder); the host treats unknown kinds as
/// `LongRunningTask` to keep the catalog stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardKind {
    /// Generic catch-all. Use this when no more specific kind
    /// applies — the SPA renders a basic progress card with title,
    /// summary, and progress bar.
    LongRunningTask,
    /// Deep-research job (lands in C3+). The SPA's renderer for
    /// this kind reads the plan tree + per-sub-query progress from
    /// `details_json`.
    Research,
    /// Future. Reserved here so plugin authors and the SPA renderer
    /// know the namespace. Not active until the corresponding tool
    /// suite lands.
    ShellSession,
    /// Future — file-pipeline tools (e.g. batch upload + transform).
    FilePipeline,
    /// A file the agent is delivering to the conversation. Opens
    /// (and immediately closes) with `details = { attachment_id,
    /// filename, mime_type, byte_size?, caption? }`. The SPA
    /// renders it as an inline download chip rather than a
    /// progress card. Symmetric to channel plugins'
    /// `send_attachment` — gives the web channel the same
    /// "agent attaches a file to its reply" affordance that
    /// TextOnly transports get via `send_file`.
    Attachment,
}

impl CardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LongRunningTask => "long_running_task",
            Self::Research => "research",
            Self::ShellSession => "shell_session",
            Self::FilePipeline => "file_pipeline",
            Self::Attachment => "attachment",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "long_running_task" => Self::LongRunningTask,
            "research" => Self::Research,
            "shell_session" => Self::ShellSession,
            "file_pipeline" => Self::FilePipeline,
            "attachment" => Self::Attachment,
            // Unknown kind from the wire — render as the generic
            // catch-all so the SPA still shows progress without the
            // operator seeing a broken-render placeholder.
            _ => Self::LongRunningTask,
        }
    }
}

/// Lifecycle state of a card. Composed by the projection from the
/// sequence of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardState {
    /// CardOpened seen but no progress events yet.
    Pending,
    /// Actively running; CardProgressed events are flowing.
    Running,
    /// Operator paused (or the runner self-paused awaiting input).
    Paused,
    /// CardClosed terminal — successful completion.
    Completed,
    /// CardClosed terminal — runner reported a failure. `Card.error`
    /// carries the message.
    Failed,
    /// CardClosed terminal — operator cancelled cooperatively.
    Cancelled,
}

impl CardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Pending" => Some(Self::Pending),
            "Running" => Some(Self::Running),
            "Paused" => Some(Self::Paused),
            "Completed" => Some(Self::Completed),
            "Failed" => Some(Self::Failed),
            "Cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Whether the card has reached a terminal state and won't emit
    /// further events. Web renderers use this to flip the card's
    /// inline rendering from "live progress" to "summary + actions".
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Operator-visible action the card advertises. The web SPA renders
/// these as buttons; transport plugins ignore them (cancel / pause
/// over Signal etc. land in a follow-up that reuses the existing
/// approvals plumbing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardAction {
    Cancel,
    Pause,
    Resume,
    /// Generic "open detail" affordance — for cards backed by a
    /// dedicated page (e.g. `/research/<job_id>`), this carries the
    /// path the SPA navigates to.
    OpenDetail { href: String },
}

impl CardAction {
    pub fn id(&self) -> &str {
        match self {
            Self::Cancel => "cancel",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::OpenDetail { .. } => "open_detail",
        }
    }
}

// -----------------------------------------------------------------
// Event payloads
// -----------------------------------------------------------------

/// Payload of an `EventKind::CardOpened` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardOpenedPayload {
    /// Stable id; subsequent events reference this. UUID by
    /// convention but the type is opaque.
    pub card_id: String,
    pub kind: CardKind,
    pub title: String,
    /// Plain-language one-liner — the text-fallback channels send.
    /// Required even for Rich-channel cards because the projection
    /// uses it as the "what is this card" summary.
    pub summary: String,
    /// Optional initial state; defaults to `Pending` at the
    /// projection layer if omitted.
    #[serde(default)]
    pub state: Option<CardState>,
    /// Initial details — kind-specific JSON. The renderer for
    /// `kind` knows how to read it.
    #[serde(default)]
    pub details: serde_json::Value,
    /// Initial action affordances.
    #[serde(default)]
    pub actions: Vec<CardAction>,
}

/// Payload of an `EventKind::CardProgressed` event. Every field is
/// optional; the projection merges populated fields onto the live
/// `Card` row, leaving omitted fields untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardProgressedPayload {
    pub card_id: String,
    /// Optional state transition. `None` keeps the prior state.
    #[serde(default)]
    pub state: Option<CardState>,
    /// 0.0–1.0. `None` keeps the prior progress.
    #[serde(default)]
    pub progress: Option<f32>,
    /// Free-form phase string; UI uses it as a sub-label
    /// ("Gathering · 7/12 sub-queries").
    #[serde(default)]
    pub phase: Option<String>,
    /// Replacement details JSON. When present, OVERRIDES the
    /// previous details entirely (the kind-specific renderer
    /// interprets the new payload). Use a sparse-merge convention
    /// inside details if you want partial-update semantics.
    #[serde(default)]
    pub details: Option<serde_json::Value>,
    /// Replacement actions. `None` keeps the prior list.
    #[serde(default)]
    pub actions: Option<Vec<CardAction>>,
    /// Channel-fallback text for this update. Transports with the
    /// `Milestones` or `Live` policy emit this as a status message;
    /// `Silent` transports ignore. `None` means no useful text-side
    /// update for this tick (e.g. a pure progress bar bump).
    #[serde(default)]
    pub summary: Option<String>,
}

/// Payload of an `EventKind::CardClosed` event. The card's terminal
/// state and the final summary always belong here; details may be
/// updated one last time (e.g. final report URL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardClosedPayload {
    pub card_id: String,
    /// Terminal state. Must be one of
    /// `Completed` / `Failed` / `Cancelled`.
    pub state: CardState,
    /// Channel-fallback text for the close ("Research complete.
    /// Sending the report.").
    pub summary: String,
    /// Final details replacement. Same OVERRIDE semantic as
    /// `CardProgressedPayload::details`.
    #[serde(default)]
    pub details: Option<serde_json::Value>,
    /// Optional reference to an attachment created by the card's
    /// owner (e.g. the research report). Transport plugins on rich
    /// channels render this as a file pill; on `TextOnly` channels
    /// the transport's own `send_file` is invoked alongside the
    /// summary text.
    #[serde(default)]
    pub attachment_id: Option<String>,
    /// Populated when `state == Failed`. Error message safe to show
    /// the operator (no sensitive details).
    #[serde(default)]
    pub error: Option<String>,
}

// -----------------------------------------------------------------
// Projection
// -----------------------------------------------------------------

/// Live shape of a card, composed by replaying its event sequence.
/// This is what the web SPA renders and what `/api/admin/cards/...`
/// hands out (forthcoming).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Card {
    pub card_id: String,
    pub conversation_id: String,
    pub kind: CardKind,
    pub state: CardState,
    pub title: String,
    pub summary: String,
    pub progress: Option<f32>,
    pub phase: Option<String>,
    pub details: serde_json::Value,
    pub actions: Vec<CardAction>,
    pub attachment_id: Option<String>,
    pub error: Option<String>,
    /// Unix-seconds when the `CardOpened` event committed.
    pub opened_at: i64,
    /// Unix-seconds of the most recent event committed.
    pub updated_at: i64,
    /// Monotonic event-log seq of the `CardOpened` event. Drives
    /// stable inline ordering in the SPA's chat-pane timeline:
    /// `opened_at` is Unix-seconds, so two cards opened in the
    /// same tick (the runner often emits CardOpened back-to-back
    /// during a single phase) tied on the timestamp and HashMap
    /// iteration in the projection produced non-deterministic
    /// ordering on every page refresh. The seq is monotonic and
    /// unique per row, so sorting on it gives a deterministic +
    /// log-order match.
    ///
    /// `Option<i64>` so the SPA's bootstrap that doesn't have the
    /// seq (incognito mock cards, fixture data) keeps round-
    /// tripping; `None` falls back to opened_at-based ordering.
    #[serde(default)]
    pub event_seq: Option<i64>,
}

/// One step in a projection — exposed so callers (the SPA's WS
/// handler) can incrementally apply incoming events to a cached
/// `Card` rather than re-replaying every time.
#[derive(Debug)]
pub enum CardEvent {
    Opened(CardOpenedPayload, i64),
    Progressed(CardProgressedPayload, i64),
    Closed(CardClosedPayload, i64),
}

impl Card {
    /// Apply a single event to this card. Returns `false` and
    /// leaves the card untouched if the event references a
    /// different `card_id` (defensive against the projection layer
    /// crossing wires).
    pub fn apply(&mut self, ev: &CardEvent) -> bool {
        let target_id = match ev {
            CardEvent::Opened(p, _) => &p.card_id,
            CardEvent::Progressed(p, _) => &p.card_id,
            CardEvent::Closed(p, _) => &p.card_id,
        };
        if target_id != &self.card_id {
            return false;
        }
        match ev {
            CardEvent::Opened(p, _ts) => {
                // CardOpened on an already-open card — treat as a
                // re-key (the runner restarted with the same id).
                // Replace title/summary/state/details/actions; keep
                // updated_at.
                self.title = p.title.clone();
                self.summary = p.summary.clone();
                self.kind = p.kind;
                if let Some(s) = p.state {
                    self.state = s;
                }
                self.details = p.details.clone();
                self.actions = p.actions.clone();
            }
            CardEvent::Progressed(p, ts) => {
                if let Some(s) = p.state {
                    self.state = s;
                }
                if let Some(pr) = p.progress {
                    self.progress = Some(pr.clamp(0.0, 1.0));
                }
                if let Some(ph) = &p.phase {
                    self.phase = Some(ph.clone());
                }
                if let Some(d) = &p.details {
                    self.details = d.clone();
                }
                if let Some(a) = &p.actions {
                    self.actions = a.clone();
                }
                if let Some(sum) = &p.summary {
                    self.summary = sum.clone();
                }
                self.updated_at = *ts;
            }
            CardEvent::Closed(p, ts) => {
                self.state = p.state;
                self.summary = p.summary.clone();
                if let Some(d) = &p.details {
                    self.details = d.clone();
                }
                if p.attachment_id.is_some() {
                    self.attachment_id = p.attachment_id.clone();
                }
                if p.error.is_some() {
                    self.error = p.error.clone();
                }
                // 2026-05-04 — derive post-close phase from
                // details.phase (the runner stamps "Complete" on
                // the deep-research path), else clear. Without
                // this the projection's `phase` field froze at
                // whatever the last CardProgressed had set
                // ("Synthesizing", "Gathering"), so the SPA
                // showed a stale phase label below the
                // "Completed" badge after refresh.
                let phase_from_details = self
                    .details
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                self.phase = phase_from_details;
                self.updated_at = *ts;
            }
        }
        true
    }

    /// Build a fresh card from its `CardOpened` event. Returns
    /// `None` if the event isn't an Opened (the projection layer
    /// should never call this with a Progressed/Closed before an
    /// Opened, but the defensive shape avoids a panic).
    pub fn from_opened(
        conversation_id: impl Into<String>,
        ev: &CardEvent,
    ) -> Option<Self> {
        match ev {
            CardEvent::Opened(p, ts) => Some(Self {
                card_id: p.card_id.clone(),
                conversation_id: conversation_id.into(),
                kind: p.kind,
                state: p.state.unwrap_or(CardState::Pending),
                title: p.title.clone(),
                summary: p.summary.clone(),
                progress: None,
                phase: None,
                details: p.details.clone(),
                actions: p.actions.clone(),
                attachment_id: None,
                error: None,
                opened_at: *ts,
                updated_at: *ts,
                // Caller (the projection helper) overwrites this
                // after construction with the open-event seq.
                event_seq: None,
            }),
            _ => None,
        }
    }
}

// -----------------------------------------------------------------
// Channel adaptation
// -----------------------------------------------------------------

/// What a transport plugin can do with cards. Drives the per-
/// channel rendering decision in the transport-host glue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardCapability {
    /// No card concept — Signal, SMS, basic Slack DM. Transport
    /// emits only `summary` text on `CardOpened` / `CardClosed`.
    TextOnly,
    /// Rich web SPA. Transport delivers the full card event stream;
    /// the renderer composes from the projection.
    Rich,
    /// Future: WhatsApp business templates, Slack Block Kit, etc.
    /// Transport translates structured cards to the channel's
    /// native rich-message format.
    Native,
}

/// Per-channel update policy. Operator-configurable in
/// `Settings → Channels` (alongside the existing per-channel
/// settings) once the multi-channel surface lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CardUpdatePolicy {
    /// Default for SMS / WhatsApp. Only `CardOpened` ack +
    /// `CardClosed` summary; no mid-job chatter.
    #[default]
    Silent,
    /// Default for Signal / Slack-DM. `CardOpened` + each phase
    /// boundary + `CardClosed`.
    Milestones,
    /// Default for Web (Rich). Every `CardProgressed`. Operator
    /// can opt other transports into this for power-user channels.
    Live,
}

impl CardUpdatePolicy {
    /// Whether a transport with this policy should emit a text
    /// message for the given event. For `Rich` channels (web SPA)
    /// the renderer reads the projection directly, so
    /// `should_emit_text` returns `false` for Live too — Rich
    /// channels NEVER emit text fallbacks; they render the card.
    /// `TextOnly` / `Native` consume this to decide which
    /// `CardEvent` ticks become wire messages.
    pub fn should_emit_text(self, ev: &CardEvent) -> bool {
        match (self, ev) {
            (Self::Silent, CardEvent::Opened(..) | CardEvent::Closed(..)) => true,
            (Self::Silent, CardEvent::Progressed(..)) => false,
            (Self::Milestones, CardEvent::Opened(..) | CardEvent::Closed(..)) => true,
            (Self::Milestones, CardEvent::Progressed(p, _)) => {
                // Phase change is a milestone; pure progress isn't.
                p.phase.is_some()
            }
            (Self::Live, _) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(card_id: &str, kind: CardKind) -> CardEvent {
        CardEvent::Opened(
            CardOpenedPayload {
                card_id: card_id.into(),
                kind,
                title: "T".into(),
                summary: "S".into(),
                state: None,
                details: serde_json::json!({}),
                actions: vec![],
            },
            100,
        )
    }

    fn progress(card_id: &str, p: f32, phase: Option<&str>, ts: i64) -> CardEvent {
        CardEvent::Progressed(
            CardProgressedPayload {
                card_id: card_id.into(),
                state: None,
                progress: Some(p),
                phase: phase.map(|s| s.to_owned()),
                details: None,
                actions: None,
                summary: None,
            },
            ts,
        )
    }

    fn closed(card_id: &str, state: CardState, ts: i64) -> CardEvent {
        CardEvent::Closed(
            CardClosedPayload {
                card_id: card_id.into(),
                state,
                summary: "done".into(),
                details: None,
                attachment_id: None,
                error: None,
            },
            ts,
        )
    }

    // --- Enum round-trips ---------------------------------------------

    #[test]
    fn card_kind_parse_roundtrips_known_values() {
        for k in [
            CardKind::LongRunningTask,
            CardKind::Research,
            CardKind::ShellSession,
            CardKind::FilePipeline,
        ] {
            assert_eq!(CardKind::parse(k.as_str()), k);
        }
    }

    #[test]
    fn card_kind_unknown_falls_back_to_long_running_task() {
        // A future plugin emitting an unrecognised kind must NOT
        // crash the SPA / projection — fall back to the generic
        // renderer.
        assert_eq!(CardKind::parse("ai_pipelining_with_jazz"), CardKind::LongRunningTask);
        assert_eq!(CardKind::parse(""), CardKind::LongRunningTask);
    }

    #[test]
    fn card_state_parse_roundtrips_every_variant() {
        for s in [
            CardState::Pending,
            CardState::Running,
            CardState::Paused,
            CardState::Completed,
            CardState::Failed,
            CardState::Cancelled,
        ] {
            assert_eq!(CardState::parse(s.as_str()), Some(s));
        }
        assert_eq!(CardState::parse("Goblin"), None);
    }

    #[test]
    fn card_state_terminal_marks_only_close_states() {
        assert!(CardState::Completed.is_terminal());
        assert!(CardState::Failed.is_terminal());
        assert!(CardState::Cancelled.is_terminal());
        assert!(!CardState::Pending.is_terminal());
        assert!(!CardState::Running.is_terminal());
        assert!(!CardState::Paused.is_terminal());
    }

    // --- Projection ---------------------------------------------------

    #[test]
    fn from_opened_seeds_pending_state_when_unspecified() {
        let card = Card::from_opened("c1", &opened("card-a", CardKind::LongRunningTask)).unwrap();
        assert_eq!(card.card_id, "card-a");
        assert_eq!(card.conversation_id, "c1");
        assert_eq!(card.kind, CardKind::LongRunningTask);
        assert_eq!(card.state, CardState::Pending);
        assert_eq!(card.opened_at, 100);
        assert_eq!(card.updated_at, 100);
    }

    #[test]
    fn apply_progressed_merges_state_progress_and_phase() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::LongRunningTask)).unwrap();
        let ev = CardEvent::Progressed(
            CardProgressedPayload {
                card_id: "a".into(),
                state: Some(CardState::Running),
                progress: Some(0.5),
                phase: Some("Gathering".into()),
                details: None,
                actions: None,
                summary: Some("halfway".into()),
            },
            150,
        );
        assert!(card.apply(&ev));
        assert_eq!(card.state, CardState::Running);
        assert_eq!(card.progress, Some(0.5));
        assert_eq!(card.phase.as_deref(), Some("Gathering"));
        assert_eq!(card.summary, "halfway");
        assert_eq!(card.updated_at, 150);
    }

    #[test]
    fn apply_progress_clamps_to_zero_one() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::LongRunningTask)).unwrap();
        card.apply(&progress("a", 1.5, None, 200));
        assert_eq!(card.progress, Some(1.0));
        card.apply(&progress("a", -0.3, None, 300));
        assert_eq!(card.progress, Some(0.0));
    }

    #[test]
    fn apply_with_wrong_card_id_is_a_no_op() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::LongRunningTask)).unwrap();
        let original = card.clone();
        let ev = progress("DIFFERENT", 0.9, None, 999);
        assert!(!card.apply(&ev));
        assert_eq!(card, original);
    }

    #[test]
    fn apply_closed_writes_terminal_state_and_summary() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::Research)).unwrap();
        card.apply(&progress("a", 0.4, Some("Gathering"), 200));
        let ev = CardEvent::Closed(
            CardClosedPayload {
                card_id: "a".into(),
                state: CardState::Completed,
                summary: "report ready".into(),
                details: Some(serde_json::json!({"report_url": "/r/abc"})),
                attachment_id: Some("att-1".into()),
                error: None,
            },
            500,
        );
        assert!(card.apply(&ev));
        assert_eq!(card.state, CardState::Completed);
        assert_eq!(card.summary, "report ready");
        assert_eq!(card.details["report_url"], "/r/abc");
        assert_eq!(card.attachment_id.as_deref(), Some("att-1"));
        assert_eq!(card.updated_at, 500);
    }

    #[test]
    fn apply_failed_carries_error_message() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::LongRunningTask)).unwrap();
        let ev = CardEvent::Closed(
            CardClosedPayload {
                card_id: "a".into(),
                state: CardState::Failed,
                summary: "the runner died".into(),
                details: None,
                attachment_id: None,
                error: Some("OOM at gather worker 3".into()),
            },
            400,
        );
        card.apply(&ev);
        assert_eq!(card.state, CardState::Failed);
        assert_eq!(card.error.as_deref(), Some("OOM at gather worker 3"));
    }

    #[test]
    fn apply_full_sequence_replays_to_final_state() {
        let mut card =
            Card::from_opened("c1", &opened("a", CardKind::Research)).unwrap();
        for ev in [
            progress("a", 0.1, Some("Planning"), 110),
            progress("a", 0.4, Some("Gathering"), 200),
            progress("a", 0.9, Some("Synthesizing"), 300),
            closed("a", CardState::Completed, 400),
        ] {
            card.apply(&ev);
        }
        assert_eq!(card.state, CardState::Completed);
        assert_eq!(card.progress, Some(0.9));
        assert_eq!(card.phase.as_deref(), Some("Synthesizing"));
        assert_eq!(card.updated_at, 400);
    }

    // --- Channel adaptation -------------------------------------------

    #[test]
    fn silent_policy_emits_text_only_on_open_and_close() {
        let policy = CardUpdatePolicy::Silent;
        assert!(policy.should_emit_text(&opened("a", CardKind::LongRunningTask)));
        assert!(!policy.should_emit_text(&progress("a", 0.5, Some("Phase"), 100)));
        assert!(policy.should_emit_text(&closed("a", CardState::Completed, 200)));
    }

    #[test]
    fn milestones_policy_emits_on_phase_change_only() {
        let policy = CardUpdatePolicy::Milestones;
        // Open / close — yes.
        assert!(policy.should_emit_text(&opened("a", CardKind::LongRunningTask)));
        assert!(policy.should_emit_text(&closed("a", CardState::Completed, 200)));
        // Pure progress (no phase): silent.
        assert!(!policy.should_emit_text(&progress("a", 0.7, None, 150)));
        // Phase change: milestone.
        assert!(policy.should_emit_text(&progress("a", 0.7, Some("Synthesizing"), 150)));
    }

    #[test]
    fn live_policy_emits_for_every_event() {
        let policy = CardUpdatePolicy::Live;
        assert!(policy.should_emit_text(&opened("a", CardKind::LongRunningTask)));
        assert!(policy.should_emit_text(&progress("a", 0.5, None, 100)));
        assert!(policy.should_emit_text(&progress("a", 0.5, Some("X"), 100)));
        assert!(policy.should_emit_text(&closed("a", CardState::Completed, 200)));
    }

    #[test]
    fn default_policy_is_silent() {
        // Default for SMS / WhatsApp / voice.
        assert_eq!(CardUpdatePolicy::default(), CardUpdatePolicy::Silent);
    }
}
