-- 0002: event-log tamper evidence (§7.8, axiom #13 invariant).
--
-- Every `state_events` row gets an HMAC-SHA256 tag computed over its
-- canonical bytes (conversation_id || seq || kind || committed_at ||
-- actor || payload) and signed with a key stored in the vault.
--
-- `tag` is nullable in the column definition so existing rows from a
-- pre-0002 DB can coexist; new rows are ALWAYS populated by
-- `EventLog::append` / `EventLog::commit_turn`. A background verifier
-- (Phase 2 hardening) will back-fill tags for any NULL rows and then
-- flip the column to NOT NULL in migration 0003.
--
-- `key_id` is reserved for future key-rotation. Phase 1 ships a single
-- key (key_id = 0). Rotation is Phase 2+.
ALTER TABLE state_events ADD COLUMN tag    BLOB;
ALTER TABLE state_events ADD COLUMN key_id INTEGER NOT NULL DEFAULT 0;
