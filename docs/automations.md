# Automations

Event-triggered flows. Webhooks, routines, plugin emits, and socket
messages land on a durable bus; matching automations run a typed graph
that can filter, transform, branch, or hand off to the agent. Every
run produces an auditable per-step trace.

Use cases the design serves:

- **Ring → animal watch.** Webhook delivers an image, an `AskAgent`
  node decides "animal present?" against a vision model, the flow
  routes to a notification or drops silently.
- **Slack triage.** Mention webhook → agent classifies → appends a
  card to a per-team chat conversation.
- **Routine reactions.** A scheduled routine fires its prompt; an
  automation listening for `routine.fired` reads the outcome and
  branches on success/failure.
- **Discovery.** Events with no matching automation surface on the
  `/automations` landing page as suggestions; one click seeds an
  editor pre-filled with the trigger.


## Architecture

```
   webhooks ──┐
   sockets ───┤
   plugins ───┼──► EventBus.publish() ──► state_bus_events
   routines ──┘            (durable PK-dedup)        │
                                                     ▼
                                    tokio::mpsc(256) ──► dispatcher
                                                              │
                                                              ▼
                                                Semaphore(16) worker pool
                                                              │
                                                              ▼
                                         AutomationStore::list_enabled_for_kind
                                                              │
                                                              ▼
                                             execute_graph(def, state, pool, trace_sink)
                                                  │
                                                  ├── Filter   (Rhai bool)
                                                  ├── Transform (Rhai value)
                                                  ├── Branch    (routing junction)
                                                  ├── Terminal  (explicit end)
                                                  └── AskAgent  ──► AutomationsAgentPool
                                                                       │
                                                                       ▼
                                                         InferenceMetrics.observe
                                                                       │
                                                                       ▼
                                                          InferenceAgentInvoker
                                                                       │
                                                            ┌──────────┴──────────┐
                                                       attachments?           none
                                                            │                    │
                                                            ▼                    ▼
                                                  BackendPurpose::Vision   BackendPurpose::Standard
                                                  (fail-fast if none)
```

Reference architecture: **LangGraph**. We borrowed the conditional-
edge + state-graph model but the runtime is pure Rust — LangGraph is
studied, not embedded. The code-view in the editor renders our internal
`AutomationDef` JSON, not Python.


## Building blocks

### Event bus (M1)

Owned by `crates/core/src/automation_bus.rs` (data layer) and
`crates/server/src/automation_bus.rs` (dispatch + poller).

| Concept | Where | Notes |
| --- | --- | --- |
| `Event` envelope | `core::automation_bus::Event` | `id` (PK / dedup), `kind`, `source`, `received_at`, `payload`. Producers pick `id` — content hash for dedup, ULID otherwise. |
| `BusEventKind` enum | `core::automation_bus::BusEventKind` | `WebhookReceived` / `SocketMessage` / `PluginEmit` / `RoutineFired` / `Other`. Serialized as the wire string (`"webhook.received"` etc.). |
| `state_bus_events` table | migration 0007 | PK on `id`. Two indexes: `(kind, received_at)` for the matcher, and a partial `(internal, received_at) WHERE dispatched_at IS NULL` for the crash-recovery scan. |
| `AutomationBus::publish` | `server::automation_bus` | Persists first (durability), then sends id over mpsc (backpressure). Internal producers (in-process emits) skip the channel; a 100 ms poller picks them up. |
| Race guard | `BusEventStore::mark_dispatched` returns `bool` | Claim-before-handle: only the caller whose UPDATE flipped `dispatched_at` runs the handler. Guarantees at-most-once handler invocation even when the recovery scan races a live mpsc delivery. |
| Retention | `core::bus_event_retention::BusEventRetentionSweeper` | Daily tick; uses the global `history_retention_days` setting. Only dispatched rows eligible — pending rows survive a stuck dispatcher. |


### Graph runtime (M2)

Owned by `crates/core/src/automations.rs` (data + validator) and
`crates/server/src/automation_runtime.rs` (executor).

**`AutomationDef`** is the typed graph stored as JSON in
`state_automations.definition`:

```jsonc
{
  "trigger": { "kind": "webhook.received", "when": "event.source == \"ring\"" },
  "nodes": [
    { "id": "filter1",  "kind": "Filter",   "config": { "expr": "event.payload.confidence > 0.5" } },
    { "id": "decide",   "kind": "AskAgent", "config": { /* see AskAgentConfig */ } },
    { "id": "notify",   "kind": "Terminal", "config": {} },
    { "id": "ignored",  "kind": "Terminal", "config": {} }
  ],
  "edges": [
    { "from": "trigger",  "to": "filter1" },
    { "from": "filter1",  "to": "decide" },
    { "from": "decide",   "to": "notify",  "when": "decide.tool == \"notify\"" },
    { "from": "decide",   "to": "ignored" }
  ]
}
```

**Implemented node kinds (M2 + M3):**

| Kind | Config | Behavior |
| --- | --- | --- |
| `Filter` | `{ expr: "<rhai bool>" }` | False → run drops to `skipped`. |
| `Transform` | `{ expr: "<rhai value>" }` | Value lands in `state[node_id]`, available to downstream nodes/edges. |
| `Branch` | `{}` | No-op routing junction. Outgoing edges' `when` clauses do the actual routing. |
| `Terminal` | `{}` | Explicit end; flow status becomes `success`. (Implicit end happens when no edge matches.) |
| `AskAgent` | `AskAgentConfig` | Hands off to the LLM; see below. |

**Reserved kinds** (validator rejects with `NotYetImplemented` —
schema slots reserved for future migrations): `CallPlugin`,
`AppendToChat`, `HttpFetch`, `Notify`, `AwaitApproval`,
`CallAutomation`, `Parallel`, `Join`.

**Edge routing.** Each edge has an optional `when` Rhai bool. The
executor picks the first edge whose `when` is truthy (or has none).
No matching edge = implicit end.

**Sandbox.** Rhai engine constructed fresh per call with hard caps:
100k ops, depth 32, 64 KB strings, 10k array/map size. No host
capabilities, no I/O. Tested via a runaway-loop test.

**Trace sink.** Same executor body powers live runs (DB checkpoint
via `AutomationRunStore::append_trace`) and dry runs (in-memory
`Vec<StepTrace>`). The seam is a `&mut dyn FnMut(StepTrace)` closure.


### AskAgent (M3)

Owned by `crates/server/src/automation_agent.rs`.

```rust
#[derive(Serialize, Deserialize, ToSchema)]
pub struct AskAgentConfig {
    pub prompt: String,
    pub attachments: Vec<String>,     // data: URLs or https:// URLs
    pub reasoning_tools: Vec<String>, // reserved; single-turn M3a
    pub exit_tools: Vec<ExitToolDef>, // agent MUST call exactly one
    pub max_turns: Option<u32>,       // default 3; M3a caps at 1
}
```

The agent picks one exit tool. The chosen tool's name becomes
`<node_id>.tool` in state (routes the outgoing edge via `when:
"ask.tool == \"notify\""`); the args become `<node_id>.args.*`.

**Pool & concurrency.** All AskAgent calls funnel through
`AutomationsAgentPool` — a tokio `Semaphore` (default 1). Production
constructs the pool around `InferenceAgentInvoker`; tests use
`StubAgentInvoker`. Per-flow `reasoning_tools` must be a subset of
the `KnownLimited` trust profile's allowed list (enforced at invoke
time).

**Vision routing (M5).** When `attachments` is non-empty, the
invoker resolves `BackendPurpose::Vision` first:

1. Vision row present + has endpoint → use it. Operator's intent is
   the source of truth; no heuristic.
2. Vision row absent → resolve `Standard` and apply the M3a
   model-id heuristic. Text-only-by-name → fail fast with
   `VisionRequiredButTextOnlyModel` and a message that points the
   operator at Settings → Backends.
3. Vision-from-bootstrap is rejected — the resolver's bootstrap
   fallback is the Standard URL, not a vision backend.

**Templating.** Prompt + attachments pass through `render_template`
before reaching the invoker, so authors can write
`{{event.payload.image_url}}` in the JSON and have it resolved at
execute time against the run's state map.

**Single-turn in M3a.** `max_turns` ≥ 1 is treated as 1. The
multi-turn loop with intermediate reasoning-tool execution is a
follow-up; the framework is sized for it (the `effective_max_turns`
field rides through the request).


### Discovery & suggestions (M4a + M5)

Owned by `crates/core/src/automation_suggestions.rs` (sweep +
store) and `crates/server/src/automation_suggestions_sweeper.rs`
(actor).

The daily sweep walks the last 7 days of `state_bus_events`, groups
by `(kind, source)`, and surfaces patterns that:

1. Have ≥ 10 events in the window;
2. Have no enabled automation for that kind;
3. Aren't in `state_automation_muted_patterns`.

Each survivor lands in `state_automation_suggestions` (upsert is
idempotent — re-sweep refreshes counts on the existing row). The
landing page reads `list_pending()`; the "Review and create" click
opens the editor with `?suggestion=<id>` and pre-fills the trigger
kind.

**Dismiss-mutes-pattern.** Dismissing a suggestion flips its status
AND inserts the `(kind, source)` into the muted-patterns table. The
next sweep skips it. Operators can later un-mute (no UI yet — direct
DB or the future settings sub-page).

**Agent-drafted seed (M5 scaffold).** Migration 0010 adds a nullable
`draft_definition` column. `SuggestionStore::set_draft_definition`
is the seam an agent-drafting path writes through; the SPA editor
seeds its JSON from `draft_definition` when present (falls back to
`emptyAutomationDef(kind)`). The actual LLM call that produces drafts
is a follow-up — the scaffolding lets that change land without
touching the schema or the frontend.


### Test-run (M4c)

Owned by `crates/server/src/automation_runtime.rs::dry_run` +
`crates/server/src/automations_admin.rs::test_run`.

The editor's "Test run" button POSTs to
`/api/admin/automations/{id}/test-run` with either an `event_id`
(picked from `/recent-events`) or a synthesized `sample_event`. The
endpoint runs the executor against the chosen event with an in-memory
trace sink — **no persistence**. The result is the trace + outcome
JSON, surfaced inline in the canvas drawer.

The sample-payload picker is fed by
`/api/admin/automations/recent-events?kind=<kind>&limit=N` —
indexed on `(kind, received_at)`, returns the last N events in that
slice.


### Inference observability (M5)

Owned by `crates/server/src/inference_metrics.rs` (counters) and
`crates/server/src/inference_admin.rs` (HTTP).

`InferenceMetrics::observe(consumer, fut)` wraps async LLM calls,
recording four counters per `InferenceConsumer`:

| Counter | Use |
| --- | --- |
| `in_flight` | Current outstanding calls. |
| `total_calls` | Lifetime monotonic. |
| `total_failures` | Subset that returned `Err(_)`. |
| `last_durations_ms` | 256-deep ring buffer; snapshot computes p50 + p95. |

Panic-safe via `InflightGuard` (RAII) — a panic propagating through
the wrapped future decrements `in_flight` during unwind; total /
failure / duration counters skip on panic (a panicked call isn't
"completed").

The Automations consumer slice is wired through the agent invoker.
Chat / Routines / Research are wired incrementally — wrapping their
`chat_completions` call with `state.inference_metrics.observe(consumer, …)`.
Until they're wired, the `/admin/inference` page just shows the
consumers that ARE wired.

UI lives at **Settings → Inference**. Auto-refresh on a 5 s tick;
operator can pause via the switch.


## API surface

Full machine-readable spec at:

- **`GET /api/openapi.json`** — generated from `utoipa` annotations.
- **`GET /api/docs`** — Swagger UI bundle.

### Automation CRUD

| Method | Path | Body | 200 |
| --- | --- | --- | --- |
| GET | `/api/admin/automations` | — | `AutomationDto[]` |
| POST | `/api/admin/automations` | `CreateAutomationRequest` | 201 `AutomationDto` |
| GET | `/api/admin/automations/{id}` | — | `AutomationDto` |
| PUT | `/api/admin/automations/{id}` | `UpdateAutomationRequest` | `AutomationDto` |
| DELETE | `/api/admin/automations/{id}` | — | 204 |
| POST | `/api/admin/automations/{id}/enable` | — | 204 |
| POST | `/api/admin/automations/{id}/disable` | — | 204 |

### Runs + metrics

| Method | Path | 200 |
| --- | --- | --- |
| GET | `/api/admin/automations/{id}/runs` | `AutomationRunRow[]` (last 100) |
| GET | `/api/admin/automations/metrics` | `MetricsDto` (active count, runs 24h, success rate, untriaged kinds) |

### Suggestions

| Method | Path | Body | 200 |
| --- | --- | --- | --- |
| GET | `/api/admin/automations/suggestions` | — | `SuggestionDto[]` (pending only) |
| GET | `/api/admin/automations/suggestions/{id}` | — | `SuggestionDto` |
| POST | `/api/admin/automations/suggestions/{id}/dismiss` | — | 204 (also writes muted-patterns row) |
| POST | `/api/admin/automations/suggestions/{id}/action` | — | 204 |

### Test-run + sample picker

| Method | Path | Body | 200 |
| --- | --- | --- | --- |
| GET | `/api/admin/automations/recent-events?kind=X&limit=N` | — | `RecentBusEventDto[]` |
| POST | `/api/admin/automations/{id}/test-run` | `TestRunRequest` | `DryRunResult` |

### Inference observability

| Method | Path | 200 |
| --- | --- | --- |
| GET | `/api/admin/inference/metrics` | `MetricsSnapshot` |

Validator errors surface as `400` with the validator's message
verbatim in `error.message` — operators see actionable text without
spelunking logs.


## Operator playbook

### Create your first automation

1. **Settings → Backends** — make sure the Standard backend row is
   configured. For automations with image attachments, also configure
   a `Vision` row.
2. **Sidebar → Automations** — click **+ New automation**.
3. **Trigger** — pick a `BusEventKind` (`webhook.received` is the
   common starting point). Optionally write a Rhai `when` clause
   like `event.source == "webhook:ring"`.
4. **Nodes** — drag in `Filter`, `Transform`, `AskAgent`, or `Branch`
   nodes. Connect them with edges; add `when` clauses on edges to
   route conditionally.
5. **Test run** — open the test-run drawer, pick a recent event from
   the dropdown (or synthesize a payload), click **Run**. The trace
   table shows each node's input/output/duration inline.
6. **Save** — server validates the graph + persists. Errors surface
   inline.

### Diagnose a stuck automation

- **Settings → Inference**: is the agent backend in flight forever?
  A non-zero `in_flight` count for Automations that doesn't drain
  means the LLM is hung. Restart the backend container.
- **`/automations/:id/runs`**: open the most recent run, expand the
  trace. The failing node's `error` field carries the message — Rhai
  parse error, missing config field, agent capability rejection, etc.
- **Bus dispatcher**: look for `automation runtime: ...` entries in
  the server log. The race guard logs already-claimed-skip at debug
  level.

### Mute a noisy pattern

Click **Skip** on the suggestion card. Future sweeps stop surfacing
that `(kind, source)` pair. Un-muting is currently a direct DB
operation (`DELETE FROM state_automation_muted_patterns WHERE …`); a
settings sub-page is on the M5 deferred list.


## File map

```
crates/core/migrations/
├── 0007_automation_bus.sql           — state_bus_events
├── 0008_automations.sql              — state_automations + runs
├── 0009_automation_suggestions.sql   — suggestions + muted_patterns
└── 0010_suggestion_drafts.sql        — draft_definition column

crates/core/src/
├── automation_bus.rs                 — BusEventKind, Event, BusEventStore
├── automation_runs.rs                — AutomationRunStatus, StepTrace, store
├── automation_suggestions.rs         — sweep algorithm + store
├── automations.rs                    — AutomationDef + NodeDef + EdgeDef
│                                       + validator + ExitToolDef + AskAgentConfig
└── bus_event_retention.rs            — daily sweeper

crates/server/src/
├── automation_bus.rs                 — AutomationBus + dispatcher + poller
├── automation_runtime.rs             — matcher + executor + Rhai sandbox + dry_run
├── automation_agent.rs               — AgentInvoker trait + pool + Vision routing
├── automation_suggestions_sweeper.rs — daily tokio actor
├── automations_admin.rs              — 15 HTTP endpoints, utoipa-annotated
├── inference_metrics.rs              — per-consumer counters + InflightGuard
└── inference_admin.rs                — /api/admin/inference/metrics endpoint

web/src/
├── api/automations.ts                — typed client (CRUD + suggestions + test-run)
├── api/inference.ts                  — typed client for the metrics page
├── routes/Automations.tsx            — chat-shell wrapper
└── settings/
    ├── AutomationsPage.tsx           — landing (metrics, suggestions, list)
    ├── AutomationDetailPage.tsx      — view toggle + test-run drawer + runs
    ├── AutomationCanvas.tsx          — ReactFlow renderer
    └── InferencePage.tsx             — observability dashboard

docs/automations.md                   — you are here
```


## Deferred to follow-ups

Listed by approximate complexity, smallest first:

| Item | Effort | Notes |
| --- | --- | --- |
| Cross-consumer metrics wiring | ~1 hr/consumer | Wrap chat/routines/research LLM calls with `state.inference_metrics.observe(consumer, …)`. Mechanical. |
| Muted-patterns settings sub-page | ~½ day | Frontend table over `state_automation_muted_patterns` + unmute button. |
| Per-automation rate limits | ~1 day | New column on `state_automations`; check in matcher before queueing. |
| Vision-model managed-mode preset | ~1 day | Add a `BackendPreset` entry that pre-fills the Vision row with a known VL model + image tag. |
| `CallAutomation` (sub-automations) | ~2 days | Recursive `execute_graph` with cycle detection + depth cap. |
| Agent-drafted suggestions (LLM path) | ~3 days | Sweep extension calls `AgentInvoker` on top-N patterns; writes via `set_draft_definition`. Needs prompt iteration + daily LLM-call quota. |
| Bidirectional canvas editing | ~3–4 days | Drag-to-create-edge + kind-specific config panels on ReactFlow. |
| `AwaitApproval` node | ~3–4 days | Runtime resume architecture (paused-run status, approval-decision listener). The hardest of the M5 runtime items. |
| `Parallel` / `Join` nodes | ~3 days | Concurrent in-flight tracking, deterministic merge semantics. |
| Automation versioning | ~3–4 days | `state_automations` becomes immutable + `state_automation_versions`; runs reference `version_id`. |
| Per-user authoring | ~2 days | `created_by` column + RBAC check in admin routes. |


## Test coverage

| Layer | Modules | Tests |
| --- | --- | --- |
| Core | automation_bus, automation_runs, automations, automation_suggestions, bus_event_retention, migrations | 62 |
| Server | automation_bus, automation_runtime, automation_agent, automation_suggestions_sweeper, automations_admin, inference_metrics, inference_admin | 55+ |
| SPA | AutomationsPage, AutomationDetailPage, sidebar | 17+ |

Criterion benches in `crates/core/benches/core_hot_paths.rs`:

| Path | Baseline | Budget |
| --- | --- | --- |
| `automation_bus::publish_external` | ~2.6 µs | 30 µs |
| `automation_bus::mark_dispatched` (already claimed) | ~1.7 µs | 20 µs |
| `automation_bus::fetch_pending(256/1024)` | ~3.9 ms | 40 ms |
| `automation_suggestions::sweep_1024_events_4_sources` | ~272 µs | 3 ms |
| `automation_suggestions::list_recent_for_kind_50_of_1024` | ~26 µs | 300 µs |
