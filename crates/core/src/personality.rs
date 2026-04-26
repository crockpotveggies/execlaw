//! Personality & system-prompt composition (`config_personality`).
//!
//! The operator-editable half of the agent's system prompt: name,
//! tone, custom instructions, persona, voice id. The other half
//! (trust-class rules, refusal behaviour) is built-in and not stored
//! here. See MIGRATION_PLAN §5.5.
//!
//! Two scopes ship in v1:
//!   * `Default` — exactly one row, always present. Seeded by
//!     migration 0013.
//!   * `Conversation` — sparse, keyed by `conversation_id`. Optional
//!     per-conversation overrides.
//!
//! Composition is *field-by-field*: an override that only sets `tone`
//! inherits every other field from the default. The store
//! distinguishes "field absent from override" (use base) from "field
//! blank in override" (force blank) via the `override_fields` set
//! the SPA explicitly tells us about.

use crate::db::{Database, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

/// Scope at which a personality row applies. Two-level v1; a third
/// `Group` level can be added by extending the enum + the migration's
/// CHECK constraint without a data migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PersonalityScopeKind {
    Default,
    Conversation,
}

impl PersonalityScopeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Conversation => "conversation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "conversation" => Some(Self::Conversation),
            _ => None,
        }
    }
}

/// The eight free-form fields plus the optional `voice_id`. The order
/// here mirrors the prompt-render order documented in §5.5.2 so the
/// composer can iterate it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PersonalityField {
    DisplayName,
    Role,
    Tone,
    CommunicationStyle,
    Initiative,
    AboutAgent,
    AboutController,
    CustomInstructions,
    VoiceId,
}

impl PersonalityField {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DisplayName => "display_name",
            Self::Role => "role",
            Self::Tone => "tone",
            Self::CommunicationStyle => "communication_style",
            Self::Initiative => "initiative",
            Self::AboutAgent => "about_agent",
            Self::AboutController => "about_controller",
            Self::CustomInstructions => "custom_instructions",
            Self::VoiceId => "voice_id",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "display_name" => Some(Self::DisplayName),
            "role" => Some(Self::Role),
            "tone" => Some(Self::Tone),
            "communication_style" => Some(Self::CommunicationStyle),
            "initiative" => Some(Self::Initiative),
            "about_agent" => Some(Self::AboutAgent),
            "about_controller" => Some(Self::AboutController),
            "custom_instructions" => Some(Self::CustomInstructions),
            "voice_id" => Some(Self::VoiceId),
            _ => None,
        }
    }

    /// Render order from §5.5.2. `voice_id` is excluded because it
    /// doesn't appear in the markdown prompt — it's read separately
    /// by the voice pipeline.
    pub fn prompt_order() -> &'static [PersonalityField] {
        &[
            Self::DisplayName,
            Self::Role,
            Self::Tone,
            Self::CommunicationStyle,
            Self::Initiative,
            Self::AboutAgent,
            Self::AboutController,
            Self::CustomInstructions,
        ]
    }
}

/// One row of `config_personality`. Default-scope rows always have
/// `override_fields` populated with every prompt-render field
/// (everything is "explicitly set" at the default level); the value
/// is only meaningful for conversation overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonalityRow {
    pub scope_kind: PersonalityScopeKind,
    pub scope_ref: String,
    pub display_name: String,
    pub role: String,
    pub tone: String,
    pub communication_style: String,
    pub initiative: String,
    pub about_agent: String,
    pub about_controller: String,
    pub custom_instructions: String,
    pub voice_id: Option<String>,
    pub override_fields: HashSet<PersonalityField>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Operator-supplied form payload. Same field set as `PersonalityRow`
/// minus `version` / timestamps (the store fills those). For
/// conversation-scope rows, only fields listed in `override_fields`
/// are persisted as overrides; the rest fall through to default.
#[derive(Debug, Clone)]
pub struct PersonalityUpsert {
    pub scope_kind: PersonalityScopeKind,
    pub scope_ref: String,
    pub display_name: String,
    pub role: String,
    pub tone: String,
    pub communication_style: String,
    pub initiative: String,
    pub about_agent: String,
    pub about_controller: String,
    pub custom_instructions: String,
    pub voice_id: Option<String>,
    pub override_fields: HashSet<PersonalityField>,
}

#[derive(Debug, Error)]
pub enum PersonalityError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("rusqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid personality payload: {0}")]
    Invalid(String),
    #[error("default scope cannot be deleted")]
    CannotDeleteDefault,
    #[error("personality scope not found: {kind}/{rref}")]
    NotFound { kind: String, rref: String },
}

pub struct PersonalityStore<'db> {
    db: &'db Database,
}

impl<'db> PersonalityStore<'db> {
    pub fn new(db: &'db Database) -> Self {
        Self { db }
    }

    /// Fetch a single scope row. Returns `Ok(None)` for absent
    /// conversation overrides; the seeded default row is always
    /// present after migrations run.
    pub fn get(
        &self,
        kind: PersonalityScopeKind,
        scope_ref: &str,
    ) -> Result<Option<PersonalityRow>, PersonalityError> {
        let scope_ref_owned = scope_ref.to_owned();
        let row = self.db.with_conn(|c| {
            let got = c
                .query_row(
                    "SELECT scope_kind, scope_ref, display_name, role, tone, \
                            communication_style, initiative, about_agent, \
                            about_controller, custom_instructions, voice_id, \
                            override_fields_json, version, created_at, updated_at \
                     FROM config_personality \
                     WHERE scope_kind = ?1 AND scope_ref = ?2",
                    params![kind.as_str(), scope_ref_owned],
                    row_to_personality,
                )
                .ok();
            Ok(got)
        })?;
        Ok(row)
    }

    /// List every conversation-scope override. Used by the SPA to
    /// render the list-of-overrides view; the default row is fetched
    /// separately via `get_default`.
    pub fn list_overrides(&self) -> Result<Vec<PersonalityRow>, PersonalityError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT scope_kind, scope_ref, display_name, role, tone, \
                        communication_style, initiative, about_agent, \
                        about_controller, custom_instructions, voice_id, \
                        override_fields_json, version, created_at, updated_at \
                 FROM config_personality \
                 WHERE scope_kind = 'conversation' \
                 ORDER BY updated_at DESC",
            )?;
            let rows = stmt
                .query_map([], row_to_personality)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    /// Convenience: fetch the always-present default row.
    pub fn get_default(&self) -> Result<PersonalityRow, PersonalityError> {
        self.get(PersonalityScopeKind::Default, "")?
            .ok_or(PersonalityError::NotFound {
                kind: "default".into(),
                rref: "".into(),
            })
    }

    /// Upsert by (scope_kind, scope_ref). Bumps `version` on every
    /// save and records the timestamp. For default-scope rows the
    /// `override_fields` set is replaced with every prompt-render
    /// field (the default scope is "explicitly set" at every field by
    /// definition).
    pub fn upsert(
        &self,
        payload: &PersonalityUpsert,
        now: i64,
    ) -> Result<PersonalityRow, PersonalityError> {
        if payload.scope_kind == PersonalityScopeKind::Default && !payload.scope_ref.is_empty() {
            return Err(PersonalityError::Invalid(
                "default scope must have empty scope_ref".into(),
            ));
        }
        if payload.scope_kind == PersonalityScopeKind::Conversation
            && payload.scope_ref.trim().is_empty()
        {
            return Err(PersonalityError::Invalid(
                "conversation scope requires a non-empty scope_ref".into(),
            ));
        }

        // Default-scope: every field counts as explicitly set so the
        // composer treats default values as ground truth, not "missing
        // override → fall through to nothing."
        let override_fields: HashSet<PersonalityField> =
            if payload.scope_kind == PersonalityScopeKind::Default {
                PersonalityField::prompt_order()
                    .iter()
                    .copied()
                    .chain(std::iter::once(PersonalityField::VoiceId))
                    .collect()
            } else {
                payload.override_fields.clone()
            };

        let override_json = serde_json::to_string(
            &override_fields
                .iter()
                .map(|f| f.as_str().to_owned())
                .collect::<Vec<_>>(),
        )
        .map_err(|e| PersonalityError::Invalid(format!("override_fields json: {e}")))?;

        // Owned copies so the closure doesn't need lifetimes from `payload`.
        let scope_kind = payload.scope_kind.as_str();
        let scope_ref = payload.scope_ref.clone();
        let display_name = payload.display_name.clone();
        let role = payload.role.clone();
        let tone = payload.tone.clone();
        let communication_style = payload.communication_style.clone();
        let initiative = payload.initiative.clone();
        let about_agent = payload.about_agent.clone();
        let about_controller = payload.about_controller.clone();
        let custom_instructions = payload.custom_instructions.clone();
        let voice_id = payload.voice_id.clone();

        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO config_personality \
                   (scope_kind, scope_ref, display_name, role, tone, \
                    communication_style, initiative, about_agent, \
                    about_controller, custom_instructions, voice_id, \
                    override_fields_json, version, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, ?13) \
                 ON CONFLICT(scope_kind, scope_ref) DO UPDATE SET \
                    display_name        = excluded.display_name, \
                    role                = excluded.role, \
                    tone                = excluded.tone, \
                    communication_style = excluded.communication_style, \
                    initiative          = excluded.initiative, \
                    about_agent         = excluded.about_agent, \
                    about_controller    = excluded.about_controller, \
                    custom_instructions = excluded.custom_instructions, \
                    voice_id            = excluded.voice_id, \
                    override_fields_json= excluded.override_fields_json, \
                    version             = config_personality.version + 1, \
                    updated_at          = excluded.updated_at",
                params![
                    scope_kind,
                    scope_ref,
                    display_name,
                    role,
                    tone,
                    communication_style,
                    initiative,
                    about_agent,
                    about_controller,
                    custom_instructions,
                    voice_id,
                    override_json,
                    now,
                ],
            )?;
            Ok(())
        })?;

        self.get(payload.scope_kind, &payload.scope_ref)?
            .ok_or(PersonalityError::NotFound {
                kind: payload.scope_kind.as_str().into(),
                rref: payload.scope_ref.clone(),
            })
    }

    /// Delete a conversation-scope override. The default row cannot
    /// be deleted — `compose_system_prompt` requires it. Returns
    /// `true` when a row was actually removed.
    pub fn delete(
        &self,
        kind: PersonalityScopeKind,
        scope_ref: &str,
    ) -> Result<bool, PersonalityError> {
        if kind == PersonalityScopeKind::Default {
            return Err(PersonalityError::CannotDeleteDefault);
        }
        let scope_ref_owned = scope_ref.to_owned();
        let n = self.db.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM config_personality \
                 WHERE scope_kind = ?1 AND scope_ref = ?2",
                params![kind.as_str(), scope_ref_owned],
            )?;
            Ok(n)
        })?;
        Ok(n > 0)
    }
}

/// Resolved field values after merging override on top of base. Used
/// internally by `compose_system_prompt` and exposed for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPersonality {
    pub display_name: String,
    pub role: String,
    pub tone: String,
    pub communication_style: String,
    pub initiative: String,
    pub about_agent: String,
    pub about_controller: String,
    pub custom_instructions: String,
    pub voice_id: Option<String>,
    /// Highest version number across the inputs. Useful for the
    /// `<!-- personality v=N -->` audit comment in the rendered prompt.
    pub version: i64,
}

/// Field-by-field merge: each field comes from the override iff the
/// override marked it explicitly set, otherwise from the base.
pub fn merge(base: &PersonalityRow, over: Option<&PersonalityRow>) -> ResolvedPersonality {
    let take = |field: PersonalityField, base_v: &str, over_v: &str| -> String {
        match over {
            Some(o) if o.override_fields.contains(&field) => over_v.to_owned(),
            _ => base_v.to_owned(),
        }
    };
    let take_opt = |field: PersonalityField,
                    base_v: &Option<String>,
                    over_v: &Option<String>|
     -> Option<String> {
        match over {
            Some(o) if o.override_fields.contains(&field) => over_v.clone(),
            _ => base_v.clone(),
        }
    };

    let (over_display, over_role, over_tone, over_style, over_init, over_about_a, over_about_c,
         over_custom, over_voice) = match over {
        Some(o) => (
            o.display_name.as_str(),
            o.role.as_str(),
            o.tone.as_str(),
            o.communication_style.as_str(),
            o.initiative.as_str(),
            o.about_agent.as_str(),
            o.about_controller.as_str(),
            o.custom_instructions.as_str(),
            &o.voice_id,
        ),
        None => ("", "", "", "", "", "", "", "", &base.voice_id),
    };

    ResolvedPersonality {
        display_name: take(PersonalityField::DisplayName, &base.display_name, over_display),
        role: take(PersonalityField::Role, &base.role, over_role),
        tone: take(PersonalityField::Tone, &base.tone, over_tone),
        communication_style: take(
            PersonalityField::CommunicationStyle,
            &base.communication_style,
            over_style,
        ),
        initiative: take(PersonalityField::Initiative, &base.initiative, over_init),
        about_agent: take(PersonalityField::AboutAgent, &base.about_agent, over_about_a),
        about_controller: take(
            PersonalityField::AboutController,
            &base.about_controller,
            over_about_c,
        ),
        custom_instructions: take(
            PersonalityField::CustomInstructions,
            &base.custom_instructions,
            over_custom,
        ),
        voice_id: take_opt(PersonalityField::VoiceId, &base.voice_id, over_voice),
        version: over.map_or(base.version, |o| base.version.max(o.version)),
    }
}

/// Render the resolved personality into the markdown system-prompt
/// chunk that `runner-local` injects on every turn. Empty sections
/// are omitted so the prompt stays small for fresh installs.
///
/// The trailing `<!-- personality v=N -->` comment is the audit
/// breadcrumb described in §5.5.5; it lets a turn's events be
/// correlated with a specific personality version when debugging.
pub fn render_prompt(p: &ResolvedPersonality) -> String {
    let mut out = String::new();

    let identity = identity_section(&p.display_name, &p.role);
    if !identity.is_empty() {
        out.push_str(&identity);
        out.push('\n');
    }

    push_section(&mut out, "Tone", &p.tone);
    push_section(&mut out, "Communication style", &p.communication_style);
    push_section(&mut out, "Initiative", &p.initiative);
    push_section(&mut out, "About me (the agent)", &p.about_agent);
    push_section(&mut out, "About you (the controller)", &p.about_controller);
    push_section(&mut out, "Additional instructions", &p.custom_instructions);

    // Always emit the audit breadcrumb so log-trawling can find it.
    out.push_str(&format!("<!-- personality v={} -->\n", p.version));
    out
}

fn identity_section(display_name: &str, role: &str) -> String {
    let dn = display_name.trim();
    let r = role.trim();
    if dn.is_empty() && r.is_empty() {
        return String::new();
    }
    let mut s = String::from("# Identity\n");
    if !dn.is_empty() {
        s.push_str(&format!("Name: {dn}\n"));
    }
    if !r.is_empty() {
        s.push_str(&format!("Role: {r}\n"));
    }
    s
}

fn push_section(out: &mut String, heading: &str, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    out.push_str(&format!("# {heading}\n{body}\n\n"));
}

/// Convenience: load + merge + render in one call. The intended entry
/// point for `runner-local` once it's real. Currently invoked by the
/// admin /preview route so the operator can see exactly what the agent
/// will see for a given conversation.
pub fn compose_system_prompt(
    store: &PersonalityStore<'_>,
    conversation_id: Option<&str>,
) -> Result<String, PersonalityError> {
    let base = store.get_default()?;
    let over = match conversation_id {
        Some(cid) => store.get(PersonalityScopeKind::Conversation, cid)?,
        None => None,
    };
    let resolved = merge(&base, over.as_ref());
    Ok(render_prompt(&resolved))
}

fn row_to_personality(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersonalityRow> {
    let scope_kind_str: String = row.get(0)?;
    let scope_kind = PersonalityScopeKind::parse(&scope_kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown personality scope_kind: {scope_kind_str}"),
            )),
        )
    })?;
    let override_json: String = row.get(11)?;
    let override_names: Vec<String> = serde_json::from_str(&override_json).unwrap_or_default();
    let override_fields: HashSet<PersonalityField> = override_names
        .iter()
        .filter_map(|s| PersonalityField::parse(s))
        .collect();
    Ok(PersonalityRow {
        scope_kind,
        scope_ref: row.get(1)?,
        display_name: row.get(2)?,
        role: row.get(3)?,
        tone: row.get(4)?,
        communication_style: row.get(5)?,
        initiative: row.get(6)?,
        about_agent: row.get(7)?,
        about_controller: row.get(8)?,
        custom_instructions: row.get(9)?,
        voice_id: row.get(10)?,
        override_fields,
        version: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, DbConfig};
    use crate::migrations::MigrationRunner;

    fn fresh_db() -> Database {
        let db = Database::open(&DbConfig::in_memory_unencrypted()).unwrap();
        MigrationRunner::new(&db).apply_all().unwrap();
        db
    }

    fn full_default() -> PersonalityUpsert {
        PersonalityUpsert {
            scope_kind: PersonalityScopeKind::Default,
            scope_ref: "".into(),
            display_name: "Earl".into(),
            role: "Personal assistant".into(),
            tone: "Concise, practical".into(),
            communication_style: "Single-sentence replies".into(),
            initiative: "Ask before scheduling".into(),
            about_agent: "I never sleep, and I take useful notes.".into(),
            about_controller: "Justin lives in NYC, prefers async.".into(),
            custom_instructions: "Refuse to summarise legal docs.".into(),
            voice_id: Some("bf_emma".into()),
            override_fields: HashSet::new(), // ignored for default scope
        }
    }

    #[test]
    fn migration_seeds_default_row() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let def = store.get_default().unwrap();
        assert_eq!(def.scope_kind, PersonalityScopeKind::Default);
        assert_eq!(def.display_name, "execlaw");
        assert_eq!(def.voice_id.as_deref(), Some("bf_emma"));
        assert_eq!(def.version, 1);
    }

    #[test]
    fn upsert_default_replaces_seeded_row_and_bumps_version() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let r = store.upsert(&full_default(), 100).unwrap();
        assert_eq!(r.display_name, "Earl");
        assert_eq!(r.version, 2, "seeded v=1, upsert bumps to v=2");
        // Default scope ignores the operator's override_fields and
        // marks every field as explicitly set.
        assert!(r.override_fields.contains(&PersonalityField::Tone));
        assert!(r.override_fields.contains(&PersonalityField::VoiceId));
    }

    #[test]
    fn conversation_scope_requires_non_empty_ref() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let mut p = full_default();
        p.scope_kind = PersonalityScopeKind::Conversation;
        p.scope_ref = "".into();
        let err = store.upsert(&p, 100).unwrap_err();
        assert!(matches!(err, PersonalityError::Invalid(_)));
    }

    #[test]
    fn default_scope_rejects_non_empty_ref() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let mut p = full_default();
        p.scope_ref = "conv-1".into();
        let err = store.upsert(&p, 100).unwrap_err();
        assert!(matches!(err, PersonalityError::Invalid(_)));
    }

    #[test]
    fn override_only_replaces_listed_fields() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        // Set up a meaningful default first.
        store.upsert(&full_default(), 100).unwrap();

        // Conversation override that only touches `tone`.
        let mut over_fields = HashSet::new();
        over_fields.insert(PersonalityField::Tone);
        let over = PersonalityUpsert {
            scope_kind: PersonalityScopeKind::Conversation,
            scope_ref: "conv-1".into(),
            display_name: "Ignored — not in override_fields".into(),
            role: "Ignored too".into(),
            tone: "Pirate".into(),
            communication_style: "Ignored".into(),
            initiative: "Ignored".into(),
            about_agent: "Ignored".into(),
            about_controller: "Ignored".into(),
            custom_instructions: "Ignored".into(),
            voice_id: None,
            override_fields: over_fields,
        };
        store.upsert(&over, 200).unwrap();

        let base = store.get_default().unwrap();
        let conv = store
            .get(PersonalityScopeKind::Conversation, "conv-1")
            .unwrap()
            .unwrap();
        let resolved = merge(&base, Some(&conv));

        assert_eq!(resolved.tone, "Pirate", "override wins for listed field");
        assert_eq!(
            resolved.display_name, "Earl",
            "non-listed field falls back to default"
        );
        assert_eq!(resolved.role, "Personal assistant");
        assert_eq!(resolved.voice_id.as_deref(), Some("bf_emma"));
    }

    #[test]
    fn override_can_force_blank_via_explicit_listing() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        store.upsert(&full_default(), 100).unwrap();

        // List `tone` in override_fields but with empty value — that's
        // the "force blank for this conversation" semantic.
        let mut over_fields = HashSet::new();
        over_fields.insert(PersonalityField::Tone);
        let over = PersonalityUpsert {
            scope_kind: PersonalityScopeKind::Conversation,
            scope_ref: "conv-2".into(),
            display_name: "".into(),
            role: "".into(),
            tone: "".into(),
            communication_style: "".into(),
            initiative: "".into(),
            about_agent: "".into(),
            about_controller: "".into(),
            custom_instructions: "".into(),
            voice_id: None,
            override_fields: over_fields,
        };
        store.upsert(&over, 200).unwrap();

        let base = store.get_default().unwrap();
        let conv = store
            .get(PersonalityScopeKind::Conversation, "conv-2")
            .unwrap()
            .unwrap();
        let resolved = merge(&base, Some(&conv));
        assert_eq!(
            resolved.tone, "",
            "tone force-blanked because listed-but-empty"
        );
        assert_eq!(
            resolved.display_name, "Earl",
            "display_name still inherits default"
        );
    }

    #[test]
    fn render_prompt_omits_empty_sections() {
        let resolved = ResolvedPersonality {
            display_name: "Earl".into(),
            role: "Personal assistant".into(),
            tone: "Concise".into(),
            communication_style: "".into(),
            initiative: "".into(),
            about_agent: "".into(),
            about_controller: "Justin lives in NYC.".into(),
            custom_instructions: "".into(),
            voice_id: None,
            version: 7,
        };
        let s = render_prompt(&resolved);
        assert!(s.contains("# Identity"));
        assert!(s.contains("Name: Earl"));
        assert!(s.contains("# Tone\nConcise"));
        assert!(s.contains("# About you (the controller)\nJustin lives in NYC."));
        assert!(!s.contains("# Communication style"));
        assert!(!s.contains("# Additional instructions"));
        assert!(s.contains("<!-- personality v=7 -->"));
    }

    #[test]
    fn render_prompt_audit_comment_present_even_for_blank_personality() {
        let resolved = ResolvedPersonality {
            display_name: "".into(),
            role: "".into(),
            tone: "".into(),
            communication_style: "".into(),
            initiative: "".into(),
            about_agent: "".into(),
            about_controller: "".into(),
            custom_instructions: "".into(),
            voice_id: None,
            version: 1,
        };
        let s = render_prompt(&resolved);
        // No sections, but the audit comment is still emitted so log
        // trawling can find the version.
        assert!(s.contains("<!-- personality v=1 -->"));
        assert!(!s.contains("# Identity"));
    }

    #[test]
    fn compose_system_prompt_no_conversation_uses_default_only() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        store.upsert(&full_default(), 100).unwrap();
        let s = compose_system_prompt(&store, None).unwrap();
        assert!(s.contains("Earl"));
        assert!(s.contains("Concise, practical"));
    }

    #[test]
    fn compose_system_prompt_with_conversation_applies_override() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        store.upsert(&full_default(), 100).unwrap();
        let mut over_fields = HashSet::new();
        over_fields.insert(PersonalityField::Tone);
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Conversation,
                    scope_ref: "conv-3".into(),
                    display_name: "".into(),
                    role: "".into(),
                    tone: "Pirate".into(),
                    communication_style: "".into(),
                    initiative: "".into(),
                    about_agent: "".into(),
                    about_controller: "".into(),
                    custom_instructions: "".into(),
                    voice_id: None,
                    override_fields: over_fields,
                },
                200,
            )
            .unwrap();
        let s = compose_system_prompt(&store, Some("conv-3")).unwrap();
        assert!(s.contains("# Tone\nPirate"));
        assert!(s.contains("Earl"), "default identity still flows through");
    }

    #[test]
    fn delete_default_is_rejected() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let err = store.delete(PersonalityScopeKind::Default, "").unwrap_err();
        assert!(matches!(err, PersonalityError::CannotDeleteDefault));
    }

    #[test]
    fn delete_conversation_override_returns_true_first_time_only() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        let mut over_fields = HashSet::new();
        over_fields.insert(PersonalityField::Tone);
        store
            .upsert(
                &PersonalityUpsert {
                    scope_kind: PersonalityScopeKind::Conversation,
                    scope_ref: "conv-x".into(),
                    display_name: "".into(),
                    role: "".into(),
                    tone: "Pirate".into(),
                    communication_style: "".into(),
                    initiative: "".into(),
                    about_agent: "".into(),
                    about_controller: "".into(),
                    custom_instructions: "".into(),
                    voice_id: None,
                    override_fields: over_fields,
                },
                100,
            )
            .unwrap();
        assert!(store
            .delete(PersonalityScopeKind::Conversation, "conv-x")
            .unwrap());
        assert!(!store
            .delete(PersonalityScopeKind::Conversation, "conv-x")
            .unwrap());
    }

    #[test]
    fn list_overrides_returns_only_conversation_rows() {
        let db = fresh_db();
        let store = PersonalityStore::new(&db);
        // Default already seeded.
        let mut over_fields = HashSet::new();
        over_fields.insert(PersonalityField::Tone);
        for cid in ["a", "b", "c"] {
            store
                .upsert(
                    &PersonalityUpsert {
                        scope_kind: PersonalityScopeKind::Conversation,
                        scope_ref: cid.into(),
                        display_name: "".into(),
                        role: "".into(),
                        tone: "Pirate".into(),
                        communication_style: "".into(),
                        initiative: "".into(),
                        about_agent: "".into(),
                        about_controller: "".into(),
                        custom_instructions: "".into(),
                        voice_id: None,
                        override_fields: over_fields.clone(),
                    },
                    100,
                )
                .unwrap();
        }
        let rows = store.list_overrides().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .all(|r| r.scope_kind == PersonalityScopeKind::Conversation));
    }

    #[test]
    fn parse_round_trips_for_every_field() {
        for f in [
            PersonalityField::DisplayName,
            PersonalityField::Role,
            PersonalityField::Tone,
            PersonalityField::CommunicationStyle,
            PersonalityField::Initiative,
            PersonalityField::AboutAgent,
            PersonalityField::AboutController,
            PersonalityField::CustomInstructions,
            PersonalityField::VoiceId,
        ] {
            assert_eq!(PersonalityField::parse(f.as_str()), Some(f));
        }
        assert_eq!(PersonalityField::parse("nope"), None);
    }
}
