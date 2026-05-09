-- Track WHO last set a conversation's display_name so transport-driven
-- renames (e.g. Signal group rename) can refresh the title without
-- clobbering an operator's hand-typed rename via PATCH /api/chats/{id}.
--
-- Source values:
--   'auto'   — set by a transport on inbound (Signal `groupName`,
--              future WhatsApp/email subject lines, etc.). Free to be
--              overwritten by the next inbound that supplies a
--              different name.
--   'manual' — set by the operator via the SPA's rename affordance.
--              Locked: no transport-driven rename will overwrite it.
--
-- Default 'auto' for fresh rows minted post-migration. Existing rows
-- with a name pre-migration are stamped 'manual' so the operator's
-- previously-typed names don't get clobbered the next time a Signal
-- group sender posts. Existing rows with a NULL name stay 'auto' so
-- the next transport inbound can populate them.
ALTER TABLE state_conversations
    ADD COLUMN display_name_source TEXT NOT NULL DEFAULT 'auto';

UPDATE state_conversations
   SET display_name_source = 'manual'
 WHERE display_name IS NOT NULL;
