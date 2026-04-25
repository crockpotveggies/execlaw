-- 0006: thread metadata + transport→conversation mapping (Phase 6 prep).
--
-- Adds the four metadata columns the SPA needs to render the thread
-- list, and the transport_conversations table that lets non-UI
-- transports (Signal, email, voice, SMS) deterministically resolve
-- inbound messages to a `ConversationId`. See MIGRATION_PLAN §2.6.
--
-- `display_name`           — LLM-generated 3-word title (or "Control thread"
--                            for the pinned controller DM, or transport-
--                            supplied group name for external groups).
-- `is_pinned`              — `1` for the Control thread; surfaces it at
--                            the top of the SPA sidebar.
-- `is_ephemeral`           — `1` for incognito threads. Events ARE
--                            persisted during the conversation (so crash
--                            recovery still works) but get DELETEd by
--                            `EphemeralSweeper` once `now > ephemeral_expires_at`.
-- `ephemeral_expires_at`   — unix seconds; NULL for non-ephemeral.

ALTER TABLE state_conversations ADD COLUMN display_name TEXT;
ALTER TABLE state_conversations ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE state_conversations ADD COLUMN is_ephemeral INTEGER NOT NULL DEFAULT 0;
ALTER TABLE state_conversations ADD COLUMN ephemeral_expires_at INTEGER;

-- Sweeper hot-path index: only ephemeral rows show up here, so the
-- WHERE clause keeps the index tiny even on a large conversation set.
CREATE INDEX IF NOT EXISTS idx_state_conversations_ephemeral
    ON state_conversations(is_ephemeral, ephemeral_expires_at)
    WHERE is_ephemeral = 1;

------------------------------------------------------------------------
-- transport_conversations: deterministic inbound-message routing.
--
-- `(plugin_id, transport_handle, principal_id)` is the lookup key the
-- transport tier hands `ConversationResolver::resolve_or_mint` on every
-- inbound message; `is_current = 1` selects the row to extend, with
-- old (rotated) rows preserved at `is_current = 0` so the UI can still
-- list "previous threads with X" without losing history.
------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS transport_conversations (
    plugin_id          TEXT    NOT NULL,
    transport_handle   TEXT    NOT NULL,
    principal_id       TEXT    NOT NULL,
    conversation_id    TEXT    NOT NULL,
    is_current         INTEGER NOT NULL DEFAULT 1,
    last_message_at    INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, transport_handle, principal_id, conversation_id)
);

-- Resolver hot path: lookup by triple + is_current.
CREATE INDEX IF NOT EXISTS idx_transport_conv_current
    ON transport_conversations(plugin_id, transport_handle, principal_id, is_current);

-- Reverse lookup for the "previous threads with <principal>" UI.
CREATE INDEX IF NOT EXISTS idx_transport_conv_principal
    ON transport_conversations(principal_id, last_message_at);
