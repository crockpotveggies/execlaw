//! Conversation FSM rows (`state_conversations`, §2.3).

use crate::db::{Database, DbError};
use crate::ids::{ConversationId, EventSeq};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Conversation kind (§2.6). Derived from participant composition; stored
/// in the DB only as a shorthand / UI affordance.
///
/// Derivation logic ([`ConversationKind::derive`]) takes a slice of
/// participant trust-class tags and returns the matching kind:
///
/// | Participants | Kind |
/// |---|---|
/// | exactly Controller, ≤1 participant | `ControllerDM` |
/// | Controller present + 1+ KnownTrusted/Delegated | `GroupWithControllerPresent` |
/// | no Controller, all KnownTrusted/Delegated | `GroupWithControllerAbsent` |
/// | exactly KnownLimited (or UnknownPending) participants | `ExternalWithOutsider` |
/// | mix of trusted (Controller / KnownTrusted / Delegated) and untrusted (KnownLimited / UnknownPending) | `MixedTrust` |
/// | empty participant list | `ControllerDM` (default for fresh conversations) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationKind {
    ControllerDM,
    GroupWithControllerPresent,
    GroupWithControllerAbsent,
    ExternalWithOutsider,
    MixedTrust,
}

impl ConversationKind {
    /// Derive the conversation kind from a slice of participant
    /// trust-class tags (the strings produced by
    /// `crate::principal::TrustLevel::class_tag()`).
    ///
    /// Pure function — no DB access — so tests can exhaustively
    /// cover the participant-composition matrix.
    pub fn derive(participant_classes: &[&str]) -> Self {
        if participant_classes.is_empty() {
            return ConversationKind::ControllerDM;
        }
        let has_controller = participant_classes.contains(&"Controller");
        // "trusted" = Controller / Delegated / KnownTrusted (rank ≥ KnownTrusted).
        let trusted = participant_classes
            .iter()
            .filter(|c| matches!(**c, "Controller" | "Delegated" | "KnownTrusted"))
            .count();
        // "untrusted" = KnownLimited / UnknownPending (excluding Blocked,
        // which never reaches the conversation since drop_turn fires).
        let untrusted = participant_classes
            .iter()
            .filter(|c| matches!(**c, "KnownLimited" | "UnknownPending"))
            .count();

        // Mixed: at least one of each side.
        if trusted > 0 && untrusted > 0 {
            return ConversationKind::MixedTrust;
        }

        // Outsider-only: every participant is at-or-below KnownLimited.
        if trusted == 0 && untrusted > 0 {
            return ConversationKind::ExternalWithOutsider;
        }

        // From here, every participant is trusted (no untrusted entries).
        if has_controller {
            // Controller alone or in a group with one or more trusted others.
            if participant_classes.len() == 1 {
                ConversationKind::ControllerDM
            } else {
                ConversationKind::GroupWithControllerPresent
            }
        } else {
            // All trusted but no Controller — a delegated/KnownTrusted-only group.
            ConversationKind::GroupWithControllerAbsent
        }
    }
}

impl ConversationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationKind::ControllerDM => "ControllerDM",
            ConversationKind::GroupWithControllerPresent => "GroupWithControllerPresent",
            ConversationKind::GroupWithControllerAbsent => "GroupWithControllerAbsent",
            ConversationKind::ExternalWithOutsider => "ExternalWithOutsider",
            ConversationKind::MixedTrust => "MixedTrust",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ControllerDM" => Some(Self::ControllerDM),
            "GroupWithControllerPresent" => Some(Self::GroupWithControllerPresent),
            "GroupWithControllerAbsent" => Some(Self::GroupWithControllerAbsent),
            "ExternalWithOutsider" => Some(Self::ExternalWithOutsider),
            "MixedTrust" => Some(Self::MixedTrust),
            _ => None,
        }
    }
}

/// Conversation phase (state-machine state, §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Thinking,
    AwaitingTool,
    AwaitingApproval,
    AwaitingWakeup,
    AwaitingReconnect,
    AwaitingTrustDecision,
    TrustRevoked,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Thinking => "thinking",
            Phase::AwaitingTool => "awaiting_tool",
            Phase::AwaitingApproval => "awaiting_approval",
            Phase::AwaitingWakeup => "awaiting_wakeup",
            Phase::AwaitingReconnect => "awaiting_reconnect",
            Phase::AwaitingTrustDecision => "awaiting_trust_decision",
            Phase::TrustRevoked => "trust_revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "thinking" => Some(Self::Thinking),
            "awaiting_tool" => Some(Self::AwaitingTool),
            "awaiting_approval" => Some(Self::AwaitingApproval),
            "awaiting_wakeup" => Some(Self::AwaitingWakeup),
            "awaiting_reconnect" => Some(Self::AwaitingReconnect),
            "awaiting_trust_decision" => Some(Self::AwaitingTrustDecision),
            "trust_revoked" => Some(Self::TrustRevoked),
            _ => None,
        }
    }

    /// True iff the agent is hot-path-busy on this conversation —
    /// i.e. the recipient should see a "typing" / "processing"
    /// indicator. The set is the union of phases where the server
    /// is actively producing toward a reply: `Thinking` (LLM running)
    /// and `AwaitingTool` (tool call in progress, still on the hot
    /// path between inbound message and outbound reply).
    ///
    /// Phases that wait on a human (`AwaitingApproval`,
    /// `AwaitingTrustDecision`) or a clock (`AwaitingWakeup`,
    /// `AwaitingReconnect`) explicitly do NOT count as processing —
    /// the recipient shouldn't see typing dots while the controller
    /// is pondering. Terminal `TrustRevoked` and the idle baseline
    /// also return false. See MIGRATION_PLAN §5.6 / agent-processing
    /// awareness notes.
    pub fn is_processing(&self) -> bool {
        matches!(self, Phase::Thinking | Phase::AwaitingTool)
    }
}

/// Modality (§2.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality {
    Text,
    Voice,
}

impl Modality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Modality::Text => "Text",
            Modality::Voice => "Voice",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Text" => Some(Self::Text),
            "Voice" => Some(Self::Voice),
            _ => None,
        }
    }
}

/// A `state_conversations` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRow {
    pub conversation_id: ConversationId,
    pub kind: ConversationKind,
    pub last_seq: EventSeq,
    pub phase: Phase,
    pub controller_id: Option<String>,
    pub trust_class: String, // free text; crate::principal::TrustLevel is the typed shape
    pub snapshot_blob: Option<Vec<u8>>,
    pub snapshot_seq: Option<EventSeq>,
    pub lease_owner: Option<String>,
    pub lease_expires: Option<i64>,
    pub modality: Modality,

    // ---- Thread metadata (migration 0006). Set by dedicated mutators
    // (`set_display_name`, `set_pinned`, `mark_ephemeral`) so the FSM
    // upsert path doesn't clobber UX state on every turn.
    pub display_name: Option<String>,
    pub is_pinned: bool,
    pub is_ephemeral: bool,
    pub ephemeral_expires_at: Option<i64>,
}

/// Trimmed projection of a `state_conversations` row used by the SPA
/// sidebar listing. Mirrors the shape the React thread list actually
/// reads — no snapshot blob, no lease internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub conversation_id: ConversationId,
    pub kind: ConversationKind,
    pub phase: Phase,
    pub trust_class: String,
    pub modality: Modality,
    pub display_name: Option<String>,
    pub is_pinned: bool,
    pub is_ephemeral: bool,
    pub ephemeral_expires_at: Option<i64>,
    pub last_seq: EventSeq,
}

/// Simple repository for `state_conversations`.
pub struct ConversationStore<'db> {
    db: &'db Database,
}

impl<'db> ConversationStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Insert-or-update a conversation row.
    ///
    /// **Important** — the metadata columns (`display_name`, `is_pinned`,
    /// `is_ephemeral`, `ephemeral_expires_at`) are written ONLY on first
    /// insert. Subsequent upserts of the same `conversation_id` from the
    /// FSM hot path leave them alone; mutate them via [`set_display_name`],
    /// [`set_pinned`], or [`mark_ephemeral`].
    pub fn upsert(&self, row: &ConversationRow) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO state_conversations \
                 (conversation_id, kind, last_seq, phase, controller_id, trust_class, \
                  snapshot_blob, snapshot_seq, lease_owner, lease_expires, modality, \
                  display_name, is_pinned, is_ephemeral, ephemeral_expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
                 ON CONFLICT(conversation_id) DO UPDATE SET \
                    kind = excluded.kind, \
                    last_seq = excluded.last_seq, \
                    phase = excluded.phase, \
                    controller_id = excluded.controller_id, \
                    trust_class = excluded.trust_class, \
                    snapshot_blob = excluded.snapshot_blob, \
                    snapshot_seq = excluded.snapshot_seq, \
                    lease_owner = excluded.lease_owner, \
                    lease_expires = excluded.lease_expires, \
                    modality = excluded.modality",
                params![
                    row.conversation_id.as_str(),
                    row.kind.as_str(),
                    row.last_seq.0,
                    row.phase.as_str(),
                    row.controller_id,
                    row.trust_class,
                    row.snapshot_blob,
                    row.snapshot_seq.map(|s| s.0),
                    row.lease_owner,
                    row.lease_expires,
                    row.modality.as_str(),
                    row.display_name,
                    row.is_pinned as i64,
                    row.is_ephemeral as i64,
                    row.ephemeral_expires_at,
                ],
            )?;
            Ok(())
        })
    }

    /// Update the user-facing thread title (the LLM-generated 3-word
    /// name, or the transport-supplied group name, or a hard-coded value
    /// like "Control thread").
    pub fn set_display_name(
        &self,
        conversation_id: &ConversationId,
        name: Option<&str>,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_conversations SET display_name = ?1 WHERE conversation_id = ?2",
                params![name, conversation_id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Set or clear the pinned flag for the SPA sidebar.
    pub fn set_pinned(
        &self,
        conversation_id: &ConversationId,
        pinned: bool,
    ) -> Result<(), DbError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_conversations SET is_pinned = ?1 WHERE conversation_id = ?2",
                params![pinned as i64, conversation_id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Mark a conversation as incognito with the supplied expiry (unix
    /// seconds). Pass `None` to clear the flag.
    pub fn mark_ephemeral(
        &self,
        conversation_id: &ConversationId,
        expires_at: Option<i64>,
    ) -> Result<(), DbError> {
        let is_ephemeral = expires_at.is_some() as i64;
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE state_conversations \
                 SET is_ephemeral = ?1, ephemeral_expires_at = ?2 \
                 WHERE conversation_id = ?3",
                params![is_ephemeral, expires_at, conversation_id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Lightweight summary of every conversation row — enough to render
    /// the SPA sidebar without a roundtrip per thread. Pinned threads
    /// come first, then everything else by most-recent activity (we use
    /// `last_seq` as a coarse stand-in until we wire a per-row
    /// `last_activity_at` column).
    pub fn list_thread_summaries(&self) -> Result<Vec<ThreadSummary>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT conversation_id, kind, phase, trust_class, modality, \
                        display_name, is_pinned, is_ephemeral, ephemeral_expires_at, \
                        last_seq \
                 FROM state_conversations \
                 ORDER BY is_pinned DESC, last_seq DESC, conversation_id ASC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    let kind_str: String = r.get(1)?;
                    let phase_str: String = r.get(2)?;
                    let modality_str: String = r.get(4)?;
                    let is_pinned: i64 = r.get(6)?;
                    let is_ephemeral: i64 = r.get(7)?;
                    Ok(ThreadSummary {
                        conversation_id: ConversationId::from(
                            r.get::<_, String>(0)?,
                        ),
                        kind: ConversationKind::parse(&kind_str)
                            .unwrap_or(ConversationKind::ControllerDM),
                        phase: Phase::parse(&phase_str).unwrap_or(Phase::Idle),
                        trust_class: r.get(3)?,
                        modality: Modality::parse(&modality_str)
                            .unwrap_or(Modality::Text),
                        display_name: r.get(5)?,
                        is_pinned: is_pinned != 0,
                        is_ephemeral: is_ephemeral != 0,
                        ephemeral_expires_at: r.get(8)?,
                        last_seq: EventSeq(r.get::<_, i64>(9)?),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// IDs of every ephemeral conversation whose `ephemeral_expires_at <= now`.
    /// Used by `EphemeralSweeper` to discover what to purge.
    pub fn list_expired_ephemeral(
        &self,
        now: i64,
    ) -> Result<Vec<ConversationId>, DbError> {
        self.db.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT conversation_id FROM state_conversations \
                 WHERE is_ephemeral = 1 \
                   AND ephemeral_expires_at IS NOT NULL \
                   AND ephemeral_expires_at <= ?1",
            )?;
            let rows = stmt
                .query_map(params![now], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().map(ConversationId::from).collect())
        })
    }

    pub fn get(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<ConversationRow>, DbError> {
        self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT kind, last_seq, phase, controller_id, trust_class, \
                            snapshot_blob, snapshot_seq, lease_owner, lease_expires, modality, \
                            display_name, is_pinned, is_ephemeral, ephemeral_expires_at \
                     FROM state_conversations WHERE conversation_id = ?1",
                    params![conversation_id.as_str()],
                    row_to_conversation,
                )
                .ok();
            Ok(got.map(|mut r| {
                r.conversation_id = conversation_id.clone();
                r
            }))
        })
    }
}

fn row_to_conversation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationRow> {
    let kind_str: String = row.get(0)?;
    let last_seq: i64 = row.get(1)?;
    let phase_str: String = row.get(2)?;
    let controller_id: Option<String> = row.get(3)?;
    let trust_class: String = row.get(4)?;
    let snapshot_blob: Option<Vec<u8>> = row.get(5)?;
    let snapshot_seq: Option<i64> = row.get(6)?;
    let lease_owner: Option<String> = row.get(7)?;
    let lease_expires: Option<i64> = row.get(8)?;
    let modality_str: String = row.get(9)?;
    let display_name: Option<String> = row.get(10)?;
    let is_pinned: i64 = row.get(11)?;
    let is_ephemeral: i64 = row.get(12)?;
    let ephemeral_expires_at: Option<i64> = row.get(13)?;
    Ok(ConversationRow {
        // Filled in by the caller — we don't have the typed id here.
        conversation_id: ConversationId::from(""),
        kind: ConversationKind::parse(&kind_str).unwrap_or(ConversationKind::ControllerDM),
        last_seq: EventSeq(last_seq),
        phase: Phase::parse(&phase_str).unwrap_or(Phase::Idle),
        controller_id,
        trust_class,
        snapshot_blob,
        snapshot_seq: snapshot_seq.map(EventSeq),
        lease_owner,
        lease_expires,
        modality: Modality::parse(&modality_str).unwrap_or(Modality::Text),
        display_name,
        is_pinned: is_pinned != 0,
        is_ephemeral: is_ephemeral != 0,
        ephemeral_expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    #[test]
    fn is_processing_covers_thinking_and_awaiting_tool_only() {
        // The set is intentionally narrow: human-wait and clock-wait
        // phases do NOT count as processing — the recipient shouldn't
        // see typing dots while the controller is pondering.
        for p in [Phase::Thinking, Phase::AwaitingTool] {
            assert!(p.is_processing(), "{} must count as processing", p.as_str());
        }
        for p in [
            Phase::Idle,
            Phase::AwaitingApproval,
            Phase::AwaitingWakeup,
            Phase::AwaitingReconnect,
            Phase::AwaitingTrustDecision,
            Phase::TrustRevoked,
        ] {
            assert!(
                !p.is_processing(),
                "{} must NOT count as processing",
                p.as_str()
            );
        }
    }

    #[test]
    fn phase_round_trips_through_serialized_form() {
        for p in [
            Phase::Idle,
            Phase::Thinking,
            Phase::AwaitingTool,
            Phase::AwaitingApproval,
            Phase::AwaitingWakeup,
            Phase::AwaitingReconnect,
            Phase::AwaitingTrustDecision,
            Phase::TrustRevoked,
        ] {
            assert_eq!(Phase::parse(p.as_str()), Some(p));
        }
        assert_eq!(Phase::parse("nope"), None);
    }

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn fresh_row(id: &str) -> ConversationRow {
        ConversationRow {
            conversation_id: ConversationId::from(id),
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
        }
    }

    #[test]
    fn upsert_and_get_roundtrip() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let mut row = fresh_row("c1");
        row.controller_id = Some("p1".into());
        store.upsert(&row).unwrap();
        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert_eq!(got.kind, ConversationKind::ControllerDM);
        assert_eq!(got.phase, Phase::Idle);
        assert_eq!(got.modality, Modality::Text);
        assert!(!got.is_pinned);
        assert!(!got.is_ephemeral);
    }

    // ---- ConversationKind::derive tests -------------------------

    #[test]
    fn derive_empty_is_controller_dm() {
        assert_eq!(
            ConversationKind::derive(&[]),
            ConversationKind::ControllerDM
        );
    }

    #[test]
    fn derive_solo_controller_is_controller_dm() {
        assert_eq!(
            ConversationKind::derive(&["Controller"]),
            ConversationKind::ControllerDM
        );
    }

    #[test]
    fn derive_controller_with_known_trusted_is_group_with_controller_present() {
        assert_eq!(
            ConversationKind::derive(&["Controller", "KnownTrusted"]),
            ConversationKind::GroupWithControllerPresent
        );
    }

    #[test]
    fn derive_only_known_trusted_is_group_without_controller() {
        assert_eq!(
            ConversationKind::derive(&["KnownTrusted", "Delegated"]),
            ConversationKind::GroupWithControllerAbsent
        );
    }

    #[test]
    fn derive_only_outsiders_is_external_with_outsider() {
        assert_eq!(
            ConversationKind::derive(&["KnownLimited", "UnknownPending"]),
            ConversationKind::ExternalWithOutsider
        );
    }

    #[test]
    fn derive_controller_plus_outsider_is_mixed_trust() {
        assert_eq!(
            ConversationKind::derive(&["Controller", "KnownLimited"]),
            ConversationKind::MixedTrust
        );
    }

    #[test]
    fn derive_known_trusted_plus_outsider_is_mixed_trust() {
        assert_eq!(
            ConversationKind::derive(&["KnownTrusted", "UnknownPending"]),
            ConversationKind::MixedTrust
        );
    }

    /// Adversarial: a Blocked participant alone shouldn't influence
    /// the kind — Blocked senders never reach the conversation
    /// (`drop_turn`), but the derivation should treat them as "no
    /// participant".
    #[test]
    fn derive_blocked_only_falls_back_to_controller_dm() {
        // Blocked counts as neither trusted nor untrusted in the
        // derivation; with no other participants, the empty-set
        // default fires.
        assert_eq!(
            ConversationKind::derive(&["Blocked"]),
            ConversationKind::GroupWithControllerAbsent,
            "Blocked alone is treated as a degenerate trusted-empty / untrusted-empty / no-controller case → GroupWithControllerAbsent"
        );
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let mut row = fresh_row("c2");
        store.upsert(&row).unwrap();
        row.phase = Phase::AwaitingApproval;
        row.last_seq = EventSeq(17);
        store.upsert(&row).unwrap();
        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert_eq!(got.phase, Phase::AwaitingApproval);
        assert_eq!(got.last_seq, EventSeq(17));
    }

    /// Once metadata is set via the dedicated mutators, a follow-up
    /// FSM upsert (carrying default metadata) MUST NOT clobber it.
    #[test]
    fn upsert_preserves_metadata_set_via_mutators() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let row = fresh_row("c-meta");
        store.upsert(&row).unwrap();

        store
            .set_display_name(&row.conversation_id, Some("Control thread"))
            .unwrap();
        store.set_pinned(&row.conversation_id, true).unwrap();
        store
            .mark_ephemeral(&row.conversation_id, Some(9_999))
            .unwrap();

        // FSM upsert with default metadata fields — should leave the
        // out-of-band-set metadata untouched.
        let mut second = fresh_row("c-meta");
        second.last_seq = EventSeq(42);
        second.phase = Phase::Thinking;
        store.upsert(&second).unwrap();

        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert_eq!(got.last_seq, EventSeq(42));
        assert_eq!(got.phase, Phase::Thinking);
        assert_eq!(got.display_name.as_deref(), Some("Control thread"));
        assert!(got.is_pinned);
        assert!(got.is_ephemeral);
        assert_eq!(got.ephemeral_expires_at, Some(9_999));
    }

    #[test]
    fn mark_ephemeral_clears_when_passed_none() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);
        let row = fresh_row("c-eph");
        store.upsert(&row).unwrap();
        store
            .mark_ephemeral(&row.conversation_id, Some(1_000))
            .unwrap();
        store.mark_ephemeral(&row.conversation_id, None).unwrap();
        let got = store.get(&row.conversation_id).unwrap().unwrap();
        assert!(!got.is_ephemeral);
        assert_eq!(got.ephemeral_expires_at, None);
    }

    #[test]
    fn list_thread_summaries_orders_pinned_first_then_recent() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);

        // Three conversations: one pinned, two regular with different last_seq.
        let mut a = fresh_row("conv-a"); // pinned
        a.last_seq = EventSeq(5);
        let mut b = fresh_row("conv-b"); // newest non-pinned
        b.last_seq = EventSeq(20);
        let mut c = fresh_row("conv-c"); // older non-pinned
        c.last_seq = EventSeq(10);
        store.upsert(&a).unwrap();
        store.upsert(&b).unwrap();
        store.upsert(&c).unwrap();
        store.set_pinned(&a.conversation_id, true).unwrap();
        store
            .set_display_name(&a.conversation_id, Some("Control thread"))
            .unwrap();

        let summaries = store.list_thread_summaries().unwrap();
        assert_eq!(summaries.len(), 3);
        // Pinned first.
        assert_eq!(summaries[0].conversation_id.as_str(), "conv-a");
        assert!(summaries[0].is_pinned);
        assert_eq!(summaries[0].display_name.as_deref(), Some("Control thread"));
        // Then by last_seq DESC.
        assert_eq!(summaries[1].conversation_id.as_str(), "conv-b");
        assert_eq!(summaries[2].conversation_id.as_str(), "conv-c");
    }

    #[test]
    fn list_thread_summaries_returns_empty_when_no_rows() {
        let db = fresh_db();
        let summaries = ConversationStore::new(&db).list_thread_summaries().unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn list_expired_ephemeral_returns_only_due_rows() {
        let db = fresh_db();
        let store = ConversationStore::new(&db);

        // not ephemeral
        store.upsert(&fresh_row("plain")).unwrap();

        // ephemeral, future expiry
        store.upsert(&fresh_row("future")).unwrap();
        store
            .mark_ephemeral(&ConversationId::from("future"), Some(10_000))
            .unwrap();

        // ephemeral, already expired
        store.upsert(&fresh_row("past")).unwrap();
        store
            .mark_ephemeral(&ConversationId::from("past"), Some(50))
            .unwrap();

        // ephemeral, expiry exactly == now (boundary: <= so it lists)
        store.upsert(&fresh_row("edge")).unwrap();
        store
            .mark_ephemeral(&ConversationId::from("edge"), Some(100))
            .unwrap();

        let expired = store.list_expired_ephemeral(100).unwrap();
        let mut ids: Vec<String> =
            expired.into_iter().map(|c| c.as_str().to_owned()).collect();
        ids.sort();
        assert_eq!(ids, vec!["edge", "past"]);
    }
}
