# Handoff — deep research subsystem (resume at C3)

**Audience:** the Claude agent picking up this work in a fresh
session. Read this end-to-end before touching any code.

**Date written:** 2026-04-29.

## What you're building

The deep research subsystem — a long-running, semi-autonomous
workflow where the agent decomposes a research question, runs
many web_search + web_fetch cycles in parallel, builds a notes
corpus, and produces a final markdown report. Operates over
minutes-to-hours, persists across server restarts, operator-visible
mid-flight.

It is **a job**, not a subagent. Subagents are used INSIDE the job's
gather phase as a parallelism + isolation primitive. This
distinction is important — see "Architecture (locked)" below.

## Architecture (locked — do not re-derive)

Three orthogonal primitives, each shipped in its own commit
already:

1. **Retention** — global Settings → General dropdown
   (`history_retention_days`: 30 / 60 / 90 / 120 / Infinite,
   default 30). Sweepers consume `RetentionPolicy::load(&db)` per
   tick. Research will plug into this for workspace + job-row
   cleanup in C6.

2. **Cards** — generic, event-sourced UI primitive for any
   long-running operator-visible task. Three event kinds
   (`CardOpened` / `CardProgressed` / `CardClosed`), projection
   logic mirrored Rust↔TS, `kind`-keyed renderer registry on the
   SPA, transport-side per-channel update policy
   (Silent / Milestones / Live). Research will use a `kind:
   "research"` card.

3. **Subagents** — synchronous child LLM calls, in-turn,
   context-isolated. `delegate_task` is the operator-facing tool;
   `SubagentApi` is the trait the gather phase will call directly
   in C4.

**The job-vs-subagent line:**

- **Job** = stateful, long-running, persistent, operator-visible,
  resumable across restarts. Has DB-backed state, runner actor,
  progress events, cancel/pause. Deep research IS a job.
- **Subagent** = bounded child LLM call (seconds to a minute),
  in-process, context-isolated, parent's turn pauses. Used INSIDE
  the gather phase for parallel sub-query workers.

If you find yourself reaching for a subagent for "research X" you
took a wrong turn — that's the job's job.

## What's already shipped (current branch: `foundation`)

Three new commits this session, plus the prior tools work. To see
the full landscape: `git log --oneline -10`.

| Commit (most recent first) | What it lands |
|---|---|
| `7fda28a` (C2) | `SubagentApi` + `delegate_task` + `subagent.*` events. `InferenceSubagentApi` impl in `crates/server/src/tool_apis_subagent.rs`. Dispatcher gains `with_inference(client, model)`. |
| (C1b) `cards: generic Card primitive` | `core::cards`, `server::cards`, `web/src/cards/`. `EventKind::Card{Opened,Progressed,Closed}` with projection mirrored Rust↔TS. Generic `LongRunningTaskCard` renderer registered. |
| (C1a) `retention: global history-retention setting` | Migration 0026, `core::retention::RetentionPolicy`, `core::event_retention` sweeper, Settings → General dropdown, sweeper rewiring. |

**Workspace tests: 959 passing, 0 failing.**

Files to skim before starting (in this order):

- `docs/handoffs/2026-04-29-research-subsystem.md` (this file).
- `crates/core/src/cards.rs` — card primitive shape + projection.
- `crates/server/src/cards.rs` — `open_card / progress_card /
  close_card` helpers.
- `crates/core/src/tool.rs` — `SubagentApi`, `ScheduleApi`,
  `WebSearchApi`, `WebFetchApi` — the capability traits the
  research tools will compose.
- `crates/server/src/tool_apis_subagent.rs` — production subagent
  impl. The gather phase in C4 calls `SubagentApi::delegate`
  directly (without going through `delegate_task`).
- `crates/server/src/tool_dispatch.rs` — `ChainedToolDispatch`
  with its `with_*` builder methods. C3 will add a runner that
  also constructs `InferenceSubagentApi` instances directly for
  the gather workers (the runner doesn't go through the
  dispatcher because it's not on the turn-loop path).
- `crates/core/src/routines.rs` — pattern for cron-style
  recurring tasks. The research-runner actor's lifecycle should
  feel similar (long-running tokio task, sweep cadence, reads
  policy live).

## What's left (C3 → C6)

Each is a single commit. Run perf/test loops between them. Audit
after each step before moving on.

### C3 — research job infrastructure + plan phase

**Scope:**
- Migration `0027_research_jobs.sql`: `state_research_jobs` table
  (id, conversation_id, status, query, plan_json, started_at,
  finished_at, error, etc. — see "Data model" below).
- `crates/server/src/research/` module:
  - `job_store.rs` — CRUD over `state_research_jobs`.
  - `runner.rs` — long-running tokio actor; spawned in `cmd_serve`
    next to the existing sweepers. **Plan phase only** in C3:
    intake → single LLM call → write `plan.json` to workspace +
    `plan_json` column → emit `CardOpened` for the research card
    + `CardProgressed` for "Plan complete." Status flips to
    `Planned`. Gather + Synthesize stay stubbed (`unimplemented!`
    or no-op return).
  - `runner_supervisor.rs` — picks up `Pending` rows on each tick
    and spawns runners for them.
- `config_research` extended on `config_general` (or a separate
  table — your call, but argue for which in the commit message).
- Tools: `research_start` (creates `Pending` row, returns
  `job_id`), `research_status` (reads job row), `research_list`
  (lists jobs visible to caller). Capability:
  `ResearchSpawn` for start, `ResearchRead` for the others.
  **Add these capabilities to `core::tool::Capability`.**
- Settings → Research page: dropdowns/inputs for
  `max_wall_clock_minutes` (default 30),
  `max_subqueries` (default 12), `parallel_workers` (default 3),
  `phase_gates` (default `plan_only`), `default_search_provider`
  (inherits from Settings → Search). UI reads/writes via a new
  `/api/admin/settings/research` endpoint pair.
- "Test research" button on the Settings page that fires a tiny
  job (canned query "what's the weather in San Francisco today")
  to exercise the whole pipe end-to-end.

**Tests:**
- Job-store CRUD round-trips.
- Runner picks up Pending → flips to Planning → flips to Planned.
- `research_start` validates query + creates row.
- Settings round-trip including `phase_gates` defaulting to
  `plan_only`.
- An adversarial test: a low-trust caller can't start a job (the
  `ResearchSpawn` capability gate denies before the runner sees
  anything).

**Card integration:** plan phase emits
`CardOpened {kind: Research, ...}` with `details_json` containing
`{plan: [...]}` once the plan lands. `CardProgressed` with
`phase: "Planning"` while the LLM call is in flight.

### C4 — gather phase + ResearchCard renderer + chat-pane card integration

**Scope:**
- Gather phase implementation in `runner.rs`. Bounded
  parallelism via a `Semaphore` (default 3). Each worker:
  1. Acquire semaphore permit.
  2. `WebSearchApi::search(sub_query)` → top N URLs.
  3. For each URL: `WebFetchApi::get(url)` → text body.
  4. `SubagentApi::delegate({task: "extract key facts about
     <sub_query> from these excerpts", context: <truncated bodies>})`.
  5. Write `notes/<n>.json`.
  6. Emit `CardProgressed` with progress bumped + the per-sub-
     query state in `details_json`.
- New `ResearchCard.tsx` renderer — replaces the generic
  `LongRunningTaskCard` for `kind: research`. Shows the plan
  tree with per-sub-query state (Pending / Running / Done /
  Failed), the current phase, total progress.
- **Chat-pane integration** (the C1b deferral): the chat
  message-stream component subscribes to `card.*` events,
  maintains a `Map<card_id, Card>` projection, and inline-renders
  cards alongside ordinary messages. Use `getCardRenderer(kind)`
  from `web/src/cards/CardRenderer.tsx`.
- Hard caps from `config_research`: max_pages_total enforced
  workspace-wide; max_total_tokens tallied across subagent calls.

**Tests:**
- Gather worker happy path (mock SearchApi + FetchApi + SubagentApi).
- Bounded parallelism — at most N workers in flight.
- Cancellation: cooperative; in-flight workers finish their
  current HTTP request and exit.
- Cap enforcement: max_pages_total kills further fetches.
- ResearchCard component renders plan tree + per-query state.

### C5 — synthesize + report attachment + transport delivery

**Scope:**
- Synthesize phase: single LLM call given all notes + original
  query → `report.md` written to workspace.
- Register `report.md` as an `AttachmentId` via the existing
  `AttachmentStore`.
- `CardClosed` event with `attachment_id` set + final
  `details_json` carrying `{report_url}`.
- Transport-side: each transport plugin's `send_file` invoked on
  `CardClosed` for `TextOnly` channels. Web's `Rich` channel just
  re-renders the card with the report inline (use the existing
  `react-markdown` pipeline).
- Tools: `research_get_report(job_id)` → returns markdown text.

**Tests:**
- Synthesize phase produces a non-empty report from N notes.
- Attachment row created + linked from the job row.
- TextOnly transport sees `send_file` invoked on close (mock).

### C6 — /research page + retention sweeper

**Scope:**
- `/research` route in the SPA: list active + recent jobs across
  conversations. Per-job drill-down showing the plan tree,
  every note JSON, the final report, the workspace contents,
  audit log of phase transitions.
- "Running jobs" badge above the composer in any conversation
  with active jobs.
- `crates/core/src/research_retention.rs` — sweeper that purges
  terminal jobs older than the global retention cutoff PLUS
  removes their workspace dirs. Reads
  `RetentionPolicy::load(&db)` like the other sweepers.
- Phase-gate approval flow (when `phase_gates: every_phase`):
  pause between plan/gather/synthesize, emit an approval request
  through the existing approvals plumbing, resume on approval.

**Tests:**
- /research page renders active + completed jobs.
- Retention sweeper deletes terminal jobs past cutoff and
  cleans the workspace dir (use a `tempfile::tempdir` in the
  test).
- Phase-gate flow: `every_phase` causes the runner to wait for
  approval between phases.

## Defaults (locked — argue against in the commit if you disagree)

| Knob | Default | Note |
|---|---|---|
| `max_wall_clock_minutes` | 30 | Hard kill switch |
| `max_total_tokens` | 100 000 | Cost ceiling |
| `max_subqueries` | 12 | Planner cap |
| `parallel_workers` | 3 | Concurrent gather subagents |
| `max_urls_per_subquery` | 5 | Per sub-query fetch cap |
| `max_pages_total` | 60 | Belt-and-braces |
| `default_search_provider` | inherits Settings → Search | DDG |
| `auto_cancel_after_idle_seconds` | 120 | Stuck-runner kill |
| `phase_gates` | `plan_only` | Operator confirms before gather |
| `workspace_retention_days` | inherits global retention | C6 |

**Workspace path:** `~/.execlaw/research/<job_id>/` containing
`plan.json`, `notes/<n>.json`, `report.md`. Filesystem (not
encrypted DB) so reports stay greppable / publishable. The DB
row has the index; the workspace holds bulky payloads.

**Per-conversation override:** `state_conversations.settings_json`
blob (faster to ship than a new table). Schema for the override
keys mirrors the global config.

**Per-job override:** args to `research_start`. Operator's defaults
cap the per-job override (you can shrink, never expand past the
global ceiling).

## Standards (carry over from this session)

- **Each commit independently shippable.** Tests pass before
  commit. Bench unaffected (or measured).
- **Per-commit message format:** present-tense headline + multi-
  paragraph body covering scope, tests added, and any defaults
  locked in. See the existing `git log` for the in-house style.
- **Audit after each step.** Workspace test count must move
  upward; nothing red.
- **No shortcuts on capability traits.** Tools never see a raw
  `Database`. If a sub-task needs new DB access, add a typed
  `*Api` capability + impl.
- **Reduced-motion / accessibility** for any new UI is a hard
  requirement, not a follow-up.

## Bootstrap

Open a new Claude session, navigate to the `execlaw` project, then:

> "Read `docs/handoffs/2026-04-29-research-subsystem.md` and start
> C3. Run perf/test loops, audit after each step, commit per
> step. Don't open PRs — just commits."

That's it. The handoff doc is self-contained; the new agent
should not need to re-derive any of the architecture decisions.

## What you should NOT do

- Re-debate the cards-vs-subagents-vs-jobs split. It's locked.
- Add a `delegate_task` for async/long work — that's the job
  path; subagents are synchronous in-turn only.
- Put a `db: Database` field on `ToolCtx`. Use the capability
  traits.
- Skip transport adaptation. Cards must work on
  Signal/WhatsApp/SMS/voice via the `summary` text fallback;
  default channel policy is `Silent`.
- Default `phase_gates` to `none` — `plan_only` is the safer
  default that gives operators a one-click confirm before the
  expensive gather phase fires.
