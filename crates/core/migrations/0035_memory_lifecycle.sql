-- Memory tiering, usage counters, promotion proposals, and reflection log.
--
-- Background — addresses the gap we identified after surveying the
-- "self-improving / proactive agent" patterns: §2.7's 4-layer model
-- describes the *kinds* of memory and *who* may see them (trust-class
-- scoping), but it does not describe *recency* or *frequency* — there
-- was no notion of a "hot working set", no promotion when a fact is
-- repeatedly useful, no demotion when it goes stale. This migration
-- adds the lifecycle.
--
-- Locked-decision compatibility:
--   * No new top-level tables for general state — only the two
--     additions strictly required (proposals + reflections).
--   * Memory remains exposed as `read_memory` / `write_memory` /
--     `list_memory` OpenAI-style tools (not Anthropic's
--     memory_20250818). The new columns are read by the existing
--     tool shim — the runner never sees them directly.
--   * Trust-class scoping unchanged; lifecycle columns are orthogonal
--     to the (scope, trust_class, key) primary key.
--   * HOT-tier promotions go through an approval event (proposals
--     table) — agents cannot self-promote into the always-loaded
--     working set without controller (or trust-policy-rule) approval,
--     consistent with the Rule-of-Two posture in §2.5.
--
-- Added columns on `memory_entries`:
--   * `tier`         — 'hot' | 'warm' | 'cold'.
--                      hot  = always injected into the system prompt
--                             (bounded slot; controller-approved).
--                      warm = readable on demand via `read_memory`.
--                      cold = excluded from default reads; explicit
--                             scope+key lookup still works (audit /
--                             never-truly-forget posture).
--                      Default 'warm' matches every existing row's
--                      semantics — pre-migration writes had no tier
--                      concept, so warm is the safe carry-over.
--   * `hits`         — incremented every time `read_memory` returns
--                      this row (or it's listed by `list_memory`).
--                      Drives promotion proposals.
--   * `last_used_at` — unix seconds of the most-recent successful
--                      read. NULL until first read post-migration.
--                      Drives demotion proposals.
--   * `created_at`   — unix seconds of first insert. Backfilled from
--                      `updated_at` for pre-migration rows so the
--                      reflection-log audit trail can correlate
--                      memory writes with the events that produced
--                      them.
--
-- Indexes:
--   * `idx_memory_tier_used` — supports the HOT working-set query
--     ("ORDER BY last_used_at DESC LIMIT N WHERE tier='hot'").
--   * `idx_memory_hits`      — supports promotion-candidate scans
--     ("WHERE tier='warm' AND hits >= 3 AND last_used_at >= ...").
ALTER TABLE memory_entries ADD COLUMN tier         TEXT    NOT NULL DEFAULT 'warm';
ALTER TABLE memory_entries ADD COLUMN hits         INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory_entries ADD COLUMN last_used_at INTEGER;
ALTER TABLE memory_entries ADD COLUMN created_at   INTEGER NOT NULL DEFAULT 0;

-- Backfill `created_at` from `updated_at` for rows minted pre-migration.
-- Rows written post-migration set both at insert time, so this is a
-- one-shot repair.
UPDATE memory_entries SET created_at = updated_at WHERE created_at = 0;

-- Hot working-set lookup: per (scope, trust_class), all rows at the
-- HOT tier ordered by recency. Used to assemble the bounded slot the
-- runner injects on every turn.
CREATE INDEX IF NOT EXISTS idx_memory_tier_used
    ON memory_entries(scope, trust_class, tier, last_used_at);

-- Promotion-candidate scan: warm rows with enough hits to be worth
-- proposing for HOT promotion.
CREATE INDEX IF NOT EXISTS idx_memory_hits
    ON memory_entries(tier, hits, last_used_at);

-------------------------------------------------------------------------
-- memory_promotions — proposed tier transitions awaiting approval.
--
-- The agent never writes the `tier` column directly. When the
-- promotion sweeper (or a planner-role reflection pass) decides a
-- row deserves a different tier, it inserts a row here. Approval
-- (controller via UI dropdown, or a trust-policy auto-approve rule)
-- flips the linked `memory_entries` row's tier and stamps
-- `decided_at`. Rejection just stamps `decided_at` and a reason.
--
-- This is the same approval-event shape used elsewhere in execlaw
-- (skill proposals, OAuth tokens), so the SPA's existing approval
-- dropdown can render these without a new surface.
-------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS memory_promotions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    scope           TEXT    NOT NULL,
    trust_class     TEXT    NOT NULL,
    key             TEXT    NOT NULL,
    from_tier       TEXT    NOT NULL,         -- snapshot of current tier at propose time
    to_tier         TEXT    NOT NULL,         -- 'hot' | 'warm' | 'cold'
    reason          TEXT    NOT NULL,         -- 'frequency' | 'recency' | 'reflection' | 'manual'
    proposed_by     TEXT    NOT NULL,         -- 'sweeper' | 'planner' | 'controller'
    proposed_at     INTEGER NOT NULL,
    decided_at      INTEGER,                  -- NULL while pending
    decision        TEXT,                     -- 'approved' | 'rejected'
    decision_note   TEXT,
    -- Loose FK; rows in memory_entries don't have a numeric id, so
    -- we keep the natural composite key here. Cascading is handled
    -- at the application layer (entry deletion is rare anyway).
    FOREIGN KEY (scope, trust_class, key)
        REFERENCES memory_entries(scope, trust_class, key)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_promotions_pending
    ON memory_promotions(decided_at) WHERE decided_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_memory_promotions_target
    ON memory_promotions(scope, trust_class, key);

-------------------------------------------------------------------------
-- memory_reflections — CONTEXT / REFLECTION / LESSON entries.
--
-- These are the structured outputs of the post-turn reflection pass
-- (see docs/agent-model.md §"Reflection"). The reflection pass is a
-- planner-role inference call gated by heuristics (corrections
-- detected, novel proper nouns, etc.) that emits zero or more
-- candidate lessons; each becomes a row here, and may also propose a
-- write to `memory_entries` (HOT tier proposal threading through
-- `memory_promotions`).
--
-- Reflections are append-only. Stale entries are pruned by the
-- existing retention sweeper (see `crates/core/src/retention.rs`
-- and the `config_general.history_retention_days` knob from
-- migration 0026) — *not* the agent.
-------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS memory_reflections (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id   TEXT    NOT NULL,
    -- Anchor each reflection to the event that ended the turn it's
    -- reflecting on, so the audit trail can link "this lesson came
    -- from that model_turn". The HMAC chain in state_events makes
    -- the link tamper-evident.
    anchor_event_seq  INTEGER NOT NULL,
    context_text      TEXT    NOT NULL,        -- "what task was happening"
    reflection_text   TEXT    NOT NULL,        -- "what I observed"
    lesson_text       TEXT    NOT NULL,        -- "actionable change for next time"
    -- If the reflection produced a memory write, link the resulting
    -- promotion proposal (or NULL if it was a no-op observation).
    promotion_id      INTEGER,
    created_at        INTEGER NOT NULL,
    FOREIGN KEY (promotion_id) REFERENCES memory_promotions(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_reflections_conv
    ON memory_reflections(conversation_id, created_at);

CREATE INDEX IF NOT EXISTS idx_memory_reflections_recent
    ON memory_reflections(created_at);
