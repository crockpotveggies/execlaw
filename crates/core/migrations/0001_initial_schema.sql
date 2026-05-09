-- execlaw initial schema.
--
-- Table naming follows MIGRATION_PLAN.md §6.3:
--   state_*   event log, conversations, outbox, alerts, attachments, etc.
--   config_*  user-editable settings (UI source of truth)
--   vault_*   encrypted secrets (whole DB is SQLCipher-encrypted anyway)
--   log_*     tracing logs (dual-sink with JSONL files; §2.12)
--
-- All timestamps are unix epoch seconds unless otherwise noted (`_ms` suffix).

------------------------------------------------------------------------
-- state_events: append-only event log (§2.3)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS state_events (
    conversation_id TEXT    NOT NULL,
    seq             INTEGER NOT NULL,
    kind            TEXT    NOT NULL,  -- 'user_msg' | 'model_turn' | 'tool_use'
                                       -- | 'tool_result' | 'interrupt' | 'resume'
                                       -- | 'approval' | 'effect_committed' | 'wakeup'
                                       -- | 'alert_fired' | 'voice.*' | 'cold_contact_arrived'
                                       -- (non-exhaustive — event kinds are additive)
    payload         BLOB    NOT NULL,  -- MessagePack
    committed_at    INTEGER NOT NULL,
    actor           TEXT,              -- PrincipalId or 'system'
    PRIMARY KEY (conversation_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_state_events_time
    ON state_events(conversation_id, committed_at);

CREATE INDEX IF NOT EXISTS idx_state_events_kind
    ON state_events(kind, committed_at);

------------------------------------------------------------------------
-- state_conversations: FSM + lease + snapshot (§2.3)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS state_conversations (
    conversation_id TEXT    PRIMARY KEY,
    kind            TEXT    NOT NULL,  -- ControllerDM | GroupWithControllerPresent | ...
    last_seq        INTEGER NOT NULL DEFAULT 0,
    phase           TEXT    NOT NULL,  -- idle | thinking | awaiting_tool | ...
    controller_id   TEXT,
    trust_class     TEXT    NOT NULL,  -- Controller | KnownTrusted | ... | Blocked
    snapshot_blob   BLOB,              -- materialized view for fast resume
    snapshot_seq    INTEGER,
    lease_owner     TEXT,              -- worker id; NULL = idle
    lease_expires   INTEGER,           -- epoch seconds
    modality        TEXT    NOT NULL DEFAULT 'Text'   -- Text | Voice
);

CREATE INDEX IF NOT EXISTS idx_state_conversations_phase
    ON state_conversations(phase, lease_expires);

------------------------------------------------------------------------
-- state_outbox + state_inbox: effects relay (§2.4)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS state_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT    NOT NULL UNIQUE,
    conversation_id TEXT    NOT NULL,
    effect_kind     TEXT    NOT NULL,  -- transport.send | schedule.wakeup | ...
    payload         BLOB    NOT NULL,
    status          TEXT    NOT NULL,  -- pending | in_flight | delivered | failed | dead_letter
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    last_error      TEXT,
    enqueued_seq    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_state_outbox_status
    ON state_outbox(status, next_attempt_at);

CREATE TABLE IF NOT EXISTS state_inbox (
    idempotency_key TEXT    PRIMARY KEY,
    received_at     INTEGER NOT NULL
);

------------------------------------------------------------------------
-- Alerts (§10)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS state_alerts (
    id                TEXT    PRIMARY KEY,
    fingerprint       TEXT    NOT NULL,
    severity          TEXT    NOT NULL,  -- Critical | Error | Warning | Info
    source            TEXT    NOT NULL,
    title             TEXT    NOT NULL,
    detail            TEXT,
    context_json      BLOB,
    status            TEXT    NOT NULL,  -- Firing | Acked | Resolved | Snoozed
    first_seen_at     INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    occurrence_count  INTEGER NOT NULL DEFAULT 1,
    resolved_at       INTEGER,
    resolved_by       TEXT,
    ack_at            INTEGER,
    ack_by            TEXT,
    snooze_until      INTEGER,
    incident_id       TEXT,
    actions_json      BLOB
);

-- Partial-unique-index would be cleaner but SQLite supports it: one
-- "Firing" row per fingerprint at a time.
CREATE UNIQUE INDEX IF NOT EXISTS uq_alert_fingerprint_firing
    ON state_alerts(fingerprint) WHERE status = 'Firing';

CREATE INDEX IF NOT EXISTS idx_alerts_status_severity
    ON state_alerts(status, severity, last_seen_at);

CREATE INDEX IF NOT EXISTS idx_alerts_source
    ON state_alerts(source, last_seen_at);

CREATE TABLE IF NOT EXISTS state_incidents (
    id             TEXT    PRIMARY KEY,
    title          TEXT    NOT NULL,
    first_seen_at  INTEGER NOT NULL,
    resolved_at    INTEGER,
    alert_count    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS state_alert_silences (
    id           TEXT    PRIMARY KEY,
    matcher_json BLOB    NOT NULL,
    expires_at   INTEGER,
    created_by   TEXT    NOT NULL,
    reason       TEXT
);

------------------------------------------------------------------------
-- Attachments + artifacts (§2.9.2, §2.9.1)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS state_attachments (
    id               TEXT    PRIMARY KEY,
    conversation_id  TEXT    NOT NULL,
    mime_type        TEXT    NOT NULL,
    path             TEXT    NOT NULL,
    sha256           TEXT    NOT NULL,
    received_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_attachments_conversation
    ON state_attachments(conversation_id, received_at);

CREATE TABLE IF NOT EXISTS state_artifacts (
    id               TEXT    PRIMARY KEY,
    research_job_id  TEXT,
    kind             TEXT    NOT NULL,   -- research_pdf | image | other
    mime_type        TEXT    NOT NULL,
    path             TEXT    NOT NULL,
    sha256           TEXT    NOT NULL,
    bytes            INTEGER,
    created_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_artifacts_research
    ON state_artifacts(research_job_id, created_at);

------------------------------------------------------------------------
-- Configuration tables (§5.4, §6.2, §10.4)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS config_runner_deployments (
    id                  TEXT    PRIMARY KEY,
    purpose             TEXT    NOT NULL,   -- Standard | Reasoning | Guardrail | VoiceSTT | VoiceTTS
    inference_backend   TEXT    NOT NULL,   -- PluginId
    model_spec_json     BLOB    NOT NULL,
    gpu_id              TEXT,
    endpoint            TEXT,
    is_default          INTEGER NOT NULL DEFAULT 0,
    active              INTEGER NOT NULL DEFAULT 1,
    notes               TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS config_trust_policy (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS config_alert_routing (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS config_research_quota (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS config_runtime_settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS config_hardware_profile_overrides (
    gpu_id         TEXT    PRIMARY KEY,
    override_json  BLOB    NOT NULL,
    set_by         TEXT    NOT NULL,
    set_at         INTEGER NOT NULL,
    reason         TEXT
);

------------------------------------------------------------------------
-- Principals (§2.6, §2.14)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS principals (
    id                TEXT    PRIMARY KEY,
    identifiers_json  BLOB    NOT NULL,   -- JSON array of (transport, handle)
    trust_level_json  BLOB    NOT NULL,   -- full TrustLevel variant + fields
    resolved_by_json  BLOB    NOT NULL,   -- JSON array of PluginIds
    metadata_json     BLOB    NOT NULL,   -- display_name, tags, notes, first_seen
    first_seen        INTEGER NOT NULL,
    last_seen         INTEGER,
    controller_notes  TEXT
);

------------------------------------------------------------------------
-- Research jobs (§2.9.1)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS research_jobs (
    id              TEXT    PRIMARY KEY,
    conversation_id TEXT    NOT NULL,
    prompt          TEXT    NOT NULL,
    status          TEXT    NOT NULL,   -- queued | executing | awaiting_followup | succeeded | failed | cancelled
    progress_json   BLOB,
    artifact_id     TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    finished_at     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_research_jobs_status
    ON research_jobs(status, updated_at);

------------------------------------------------------------------------
-- Long-term memory (§2.7)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS memory_entries (
    scope        TEXT    NOT NULL,     -- e.g. principal:<id> | conversation:<id> | global
    trust_class  TEXT    NOT NULL,     -- Controller | KnownTrusted | KnownLimited | ...
    key          TEXT    NOT NULL,
    value_blob   BLOB    NOT NULL,     -- MessagePack
    ttl_expires  INTEGER,              -- NULL = permanent
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (scope, trust_class, key)
);

CREATE INDEX IF NOT EXISTS idx_memory_ttl
    ON memory_entries(ttl_expires) WHERE ttl_expires IS NOT NULL;

------------------------------------------------------------------------
-- Vault (encrypted because SQLCipher encrypts the whole DB; rows are
-- additionally namespaced for access control via Rust repository layer).
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS vault_secrets (
    name         TEXT    NOT NULL,   -- e.g. 'admin_password_hash', 'controller_ed25519_priv'
    plugin_id    TEXT,                -- NULL = core; otherwise plugin-scoped
    value_blob   BLOB    NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, name)
);

------------------------------------------------------------------------
-- Logs (§2.12)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS log_entries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms           INTEGER NOT NULL,
    level           TEXT    NOT NULL,   -- TRACE | DEBUG | INFO | WARN | ERROR
    target          TEXT    NOT NULL,
    conversation_id TEXT,
    plugin_id       TEXT,
    message         TEXT    NOT NULL,
    fields_json     BLOB
);

CREATE INDEX IF NOT EXISTS idx_log_entries_ts
    ON log_entries(ts_ms);

CREATE INDEX IF NOT EXISTS idx_log_entries_conversation
    ON log_entries(conversation_id, ts_ms) WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_log_entries_plugin
    ON log_entries(plugin_id, ts_ms) WHERE plugin_id IS NOT NULL;

------------------------------------------------------------------------
-- Transport cursors (§2.15)
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS transport_cursors (
    plugin_id      TEXT    PRIMARY KEY,
    cursor_value   TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL
);

------------------------------------------------------------------------
-- Audit of config-table writes (§6.3 "Audit everything").
-- The Rust repository layer populates this so we record the actor,
-- which triggers alone couldn't do.
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS config_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    actor       TEXT    NOT NULL,
    table_name  TEXT    NOT NULL,
    row_id      TEXT    NOT NULL,
    old_json    BLOB,
    new_json    BLOB
);

CREATE INDEX IF NOT EXISTS idx_config_audit_ts
    ON config_audit(ts);
