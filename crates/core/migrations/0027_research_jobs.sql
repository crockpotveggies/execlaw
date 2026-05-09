-- 2026-04-29 — deep-research subsystem (C3).
--
-- Two new tables, plus a hard reset of the legacy Phase-1 stub.
--
--   * `state_research_jobs` — durable per-job row. Lifecycle:
--       Pending  → runner picks up → Planning  → LLM call  → Planned
--                                               (gather + synthesize
--                                                stay stubbed in C3)
--       Pending  → runner picks up → ... → Gathering / Synthesizing
--                                            / Complete / Failed /
--                                            Cancelled (C4-C5)
--     The runner is a long-running tokio actor that reads its
--     workspace state from `plan_json` / `notes_json` / `report_url`
--     columns and writes back atomically. Workspaces themselves
--     (`~/.execlaw/research/<job_id>/`) live on disk; the row carries
--     the index. A retention sweeper purges terminal rows + their
--     workspace dirs in C6 (reads `RetentionPolicy::load`).
--
--   * `config_research` — singleton-row store for the operator's
--     global research caps (max_wall_clock_minutes, max_subqueries,
--     parallel_workers, phase_gates, …). Per-conversation overrides
--     land on `state_conversations.settings_json` in a follow-up.
--     We chose a separate table over extending `config_general`
--     because every research knob is research-specific; co-locating
--     them with bind_address / start_on_boot would balloon the
--     general-settings surface. The existing `config_research_quota`
--     k/v table from migration 0001 is intentionally NOT reused —
--     it's an unused stub from selfhosted-claw's design and the
--     k/v shape is awkward for typed config.
--
-- The `research_jobs` table from migration 0001 is dropped: it was
-- only ever exercised by its own unit test, the surrounding store
-- has zero call sites, and its column shape (queued/executing
-- vocabulary) doesn't match the C3+ lifecycle. A clean replacement
-- avoids a half-migrated row format the runner would otherwise have
-- to defend against.

------------------------------------------------------------------------
-- Drop the inert Phase-1 stub.
------------------------------------------------------------------------

DROP INDEX IF EXISTS idx_research_jobs_status;
DROP TABLE IF EXISTS research_jobs;

------------------------------------------------------------------------
-- state_research_jobs — durable per-job row.
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS state_research_jobs (
    -- Stable id; the runner picks rows up by it; tools return it to
    -- the model so subsequent `research_status(job_id)` calls work.
    id                  TEXT    PRIMARY KEY,
    -- Conversation that originated the job. The Card lifecycle
    -- events (CardOpened/Progressed/Closed) commit to this
    -- conversation's event log so the chat-pane renderer picks
    -- them up naturally.
    conversation_id     TEXT    NOT NULL,
    -- The original research question (operator-facing or model-
    -- generated). Bounded by the tool layer (8000 chars).
    query               TEXT    NOT NULL,
    -- Lifecycle vocabulary. Matches `core::research::JobStatus`:
    --   pending     → row inserted, runner not yet picked up
    --   planning    → runner is making the plan-phase LLM call
    --   planned     → plan landed; awaiting gather (C4)
    --   gathering   → fan-out workers running (C4)
    --   synthesizing → final report being composed (C5)
    --   complete    → terminal: report.md written + attachment_id set
    --   failed      → terminal: `error` populated
    --   cancelled   → terminal: operator cancelled cooperatively
    status              TEXT    NOT NULL DEFAULT 'pending',
    -- Caller's trust class at job creation. Capability-gated tools
    -- check the row's caller_trust before allowing reads. Stored
    -- because the runner needs to honour the original caller's
    -- scope when the operator's class might change between
    -- enqueue and read.
    caller_trust        TEXT    NOT NULL,
    -- Stable id of the operator-visible Card driving the chat-
    -- pane render. Set on `Pending → Planning` so the SPA can
    -- subscribe; `NULL` until then.
    card_id             TEXT,
    -- Plan as MessagePack-encoded `ResearchPlan`. Populated when
    -- the planner LLM call returns; stays NULL during pending /
    -- planning. C4 reads it to drive the gather workers.
    plan_json           BLOB,
    -- Per-sub-query notes (gathering output). MessagePack-encoded
    -- `Vec<ResearchNote>`. C4 writes here.
    notes_json          BLOB,
    -- Path under the workspace root (see `crate::research::workspace`)
    -- where bulky payloads live. NULL until the runner provisions
    -- the dir; absolute on disk.
    workspace_path      TEXT,
    -- Final attachment id (the synthesized report.md) once
    -- synthesize lands in C5.
    attachment_id       TEXT,
    -- Operator-safe failure message when status='failed'.
    error               TEXT,
    -- Operator-supplied per-job overrides on the global config_research
    -- defaults. JSON blob: presence of a key means the override applies.
    overrides_json      BLOB,
    -- Per-row clocks. `started_at` flips on `Pending → Planning`;
    -- `finished_at` flips on the terminal transition. `created_at`
    -- and `updated_at` are bookkeeping for the operator UI.
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    started_at          INTEGER,
    finished_at         INTEGER
);

-- Supervisor query: "Pending rows oldest-first" so a runner that
-- crashed mid-tick doesn't starve later jobs.
CREATE INDEX IF NOT EXISTS idx_state_research_jobs_pending
    ON state_research_jobs(status, created_at);

-- Per-conversation list query for the chat-pane "running jobs"
-- badge + the future /research page.
CREATE INDEX IF NOT EXISTS idx_state_research_jobs_conv
    ON state_research_jobs(conversation_id, created_at);

-- Retention sweeper (C6) keys on terminal-state rows past the
-- global retention cutoff.
CREATE INDEX IF NOT EXISTS idx_state_research_jobs_terminal
    ON state_research_jobs(status, finished_at);

------------------------------------------------------------------------
-- config_research — singleton operator-editable defaults.
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS config_research (
    -- Singleton constraint: only id=1 is ever inserted; the row
    -- itself is seeded just below so `GET /api/admin/settings/research`
    -- on a fresh DB doesn't 500.
    id                          INTEGER PRIMARY KEY CHECK (id = 1),
    -- Hard kill-switch: a runner past this wall-clock cap is
    -- terminated regardless of phase. Default 30 min.
    max_wall_clock_minutes      INTEGER NOT NULL DEFAULT 30,
    -- Total subagent token ceiling — sum across plan + gather +
    -- synthesize. Default 100000 tokens.
    max_total_tokens            INTEGER NOT NULL DEFAULT 100000,
    -- Planner cap on sub-query count. Default 12.
    max_subqueries              INTEGER NOT NULL DEFAULT 12,
    -- Gather-phase parallelism. Default 3 (Semaphore size in C4).
    parallel_workers            INTEGER NOT NULL DEFAULT 3,
    -- Per-sub-query URL fetch cap. Default 5.
    max_urls_per_subquery       INTEGER NOT NULL DEFAULT 5,
    -- Belt-and-braces total page cap (kills further fetches once
    -- exceeded, even if individual sub-queries haven't maxed out).
    -- Default 60.
    max_pages_total             INTEGER NOT NULL DEFAULT 60,
    -- Idle-runner kill: if no progress event in this many seconds,
    -- the supervisor terminates and marks failed. Default 120s.
    auto_cancel_after_idle_secs INTEGER NOT NULL DEFAULT 120,
    -- Phase-gate vocabulary:
    --   none         — runner advances straight through plan→
    --                  gather→synthesize, no operator confirms
    --   plan_only    — pause after plan, await operator confirm
    --   every_phase  — pause between every phase (C6)
    -- Default `plan_only`: gives the operator a one-click confirm
    -- before the expensive gather phase fires.
    phase_gates                 TEXT    NOT NULL DEFAULT 'plan_only',
    -- Search provider id ("duckduckgo", "brave", …). NULL means
    -- "inherit from Settings → Search" (forthcoming).
    default_search_provider     TEXT,
    updated_at                  INTEGER NOT NULL DEFAULT 0
);

-- Seed the singleton row so reads on a fresh DB don't return None.
INSERT INTO config_research (id) VALUES (1)
    ON CONFLICT(id) DO NOTHING;
