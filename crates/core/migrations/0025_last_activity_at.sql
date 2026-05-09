-- 0025 — `state_conversations.last_activity_at`.
--
-- Sidebar thread ordering needs a recency proxy. The original schema
-- order was `ORDER BY is_pinned DESC, last_seq DESC` and the comment
-- on `list_thread_summaries` documented `last_seq` as a "coarse
-- stand-in until we wire a per-row last_activity_at column" — that
-- stand-in proved wrong: `last_seq` is the EVENT COUNT, not recency,
-- so a chatty old conversation (last_seq=20) sorts above a freshly
-- minted one (last_seq=2) in the sidebar even though the new chat is
-- where the operator's attention is.
--
-- This migration:
--   1. Adds `last_activity_at INTEGER NOT NULL DEFAULT 0` to
--      state_conversations. Unix-seconds wall-clock; updated by the
--      chat handler after every committed turn.
--   2. Backfills existing rows from MAX(committed_at) of state_events
--      grouped by conversation_id. Conversations with no events
--      (cold-contact rows that never received a message) keep 0; they
--      sort last under the new ORDER BY, which is the right desk-out
--      behaviour.
--   3. Adds an index on (is_pinned, last_activity_at) so the sidebar
--      query stays cheap as the table grows.
--
-- The list_thread_summaries SELECT switches to ORDER BY last_activity_at
-- DESC in the same change-set (Rust side); this migration is a pure
-- schema/data evolution.

ALTER TABLE state_conversations
    ADD COLUMN last_activity_at INTEGER NOT NULL DEFAULT 0;

UPDATE state_conversations
   SET last_activity_at = COALESCE(
       (SELECT MAX(committed_at)
          FROM state_events
         WHERE state_events.conversation_id = state_conversations.conversation_id),
       0
   );

CREATE INDEX IF NOT EXISTS idx_state_conversations_pinned_activity
    ON state_conversations(is_pinned, last_activity_at);
