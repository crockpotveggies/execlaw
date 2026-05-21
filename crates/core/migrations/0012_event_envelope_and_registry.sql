-- Migration 0012 — event envelope + registered event-kind/reply-handler tables.
--
-- Part of the M6 event-driven architecture migration:
--   * `state_bus_events` gains `envelope_json` (rich event envelope per
--     `crates/core/src/event_envelope.rs`). Existing rows default to
--     `system_internal()` via NULL → app-side fallback.
--   * `state_registered_event_kinds` — what kinds plugins (and core) can
--     publish, plus payload schema + `expects_reply` validator gate.
--   * `state_registered_reply_handlers` — capability flags consulted by
--     the ReplyRouter degrade matrix.
--   * `state_automations` gains source-tracking columns so default flows
--     shipped by plugins can be tracked + diff'd on upgrade.
--   * `state_conversations` gains `kind` column so OperatorHome ("Inbox")
--     threads sort + render distinctly.

ALTER TABLE state_bus_events
    ADD COLUMN envelope_json TEXT;

CREATE TABLE state_registered_event_kinds (
    kind                  TEXT    PRIMARY KEY,
    source                TEXT    NOT NULL, -- 'core' | 'plugin:<id>'
    description           TEXT    NOT NULL DEFAULT '',
    payload_schema_json   TEXT,             -- nullable JSON Schema
    expects_reply         INTEGER NOT NULL DEFAULT 0,
    default_origin_kind   TEXT    NOT NULL DEFAULT 'none'
);
CREATE INDEX idx_registered_event_kinds_source
    ON state_registered_event_kinds (source);

CREATE TABLE state_registered_reply_handlers (
    name                            TEXT    PRIMARY KEY,
    plugin_id                       TEXT    NOT NULL, -- 'core' for built-ins
    description                     TEXT    NOT NULL DEFAULT '',
    supports_streaming              INTEGER NOT NULL DEFAULT 0,
    supports_attachments            INTEGER NOT NULL DEFAULT 0,
    supports_inline_chart           INTEGER NOT NULL DEFAULT 0,
    supports_table                  INTEGER NOT NULL DEFAULT 0,
    supports_card                   INTEGER NOT NULL DEFAULT 0,
    supports_markdown               INTEGER NOT NULL DEFAULT 0,
    max_attachment_size_bytes       INTEGER,
    max_attachments_per_message     INTEGER,
    max_text_length                 INTEGER,
    allowed_mime_prefixes_json      TEXT              -- nullable JSON array
);
CREATE INDEX idx_registered_reply_handlers_plugin
    ON state_registered_reply_handlers (plugin_id);

-- Automation source-tracking. Default flows shipped by plugins get
-- `source = 'plugin:<id>'` + `source_version = <plugin_version>`;
-- operator-authored flows get `source = 'operator'`. Editing a
-- plugin-shipped flow flips `operator_modified = 1` so the upgrade
-- path can surface a diff card instead of silently overwriting.
ALTER TABLE state_automations
    ADD COLUMN source TEXT NOT NULL DEFAULT 'operator';
ALTER TABLE state_automations
    ADD COLUMN source_version TEXT;
ALTER TABLE state_automations
    ADD COLUMN operator_modified INTEGER NOT NULL DEFAULT 0;

-- Note: state_conversations.kind already exists (baseline migration);
-- we reuse it for the `OperatorHome` variant ("Inbox" thread). No
-- schema change needed — values stay open-string on the wire, the
-- ConversationKind enum in code is the source of truth.
