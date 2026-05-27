-- 2026-05-26 — add `has_attachments` flag to state_conversations.
--
-- Phase E perf audit (build_attached_files_block) flagged that every
-- chat turn calls AttachmentStore::list_for_conversation just to ask
-- "are there any non-image attachments on this conversation?". The
-- common case (the vast majority of turns) is "no" — a wasted scan +
-- decode of zero rows for nothing. This column lets the per-turn
-- prose builder short-circuit on the no-attachments fast path.
--
-- Semantics: `1` iff at least one NON-image attachment exists for the
-- conversation. Tracks build_attached_files_block's actual gating —
-- image-only attachments land as vision content parts and don't need
-- a prose mention, so an image-only conversation stays at `0`.
--
-- Invariant safety (chosen over a HashSet<ConversationId> in
-- AppState): survives restarts without a cache hydration pass, and is
-- written in the same transaction as the corresponding
-- state_attachments INSERT (see AttachmentStore::insert) so the flag
-- can never lag the row.
--
-- Backfill: any existing conversation with at least one non-image
-- attachment is flipped to 1 in this migration. The image MIME list
-- mirrors `crate::attachments::IMAGE_MIMES` (and
-- `crates/server/src/chats/attachments.rs::is_image_mime`) — keep
-- them in sync if the allowed image set ever grows.

ALTER TABLE state_conversations
    ADD COLUMN has_attachments INTEGER NOT NULL DEFAULT 0;

UPDATE state_conversations
   SET has_attachments = 1
 WHERE conversation_id IN (
     SELECT DISTINCT conversation_id FROM state_attachments
      WHERE mime_type NOT IN (
          'image/png',
          'image/jpeg',
          'image/webp',
          'image/gif'
      )
 );
