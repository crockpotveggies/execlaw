# execlaw — Agent Model

How a conversation in execlaw turns into model calls, tool calls, and durable state. This is the *how* doc — read [`architecture.md`](architecture.md) first for the system topology.

> **Status legend.** Each section is tagged with what's actually built today vs. designed but not yet wired:
> - `[shipped]` — code is in `main` and exercised by tests.
> - `[schema-ready]` — DB shape and storage layer exist; the runner-side glue still has to land.
> - `[design]` — committed plan, not yet implemented.
> Tags update as code lands; if you find a tag out of step with reality, fix it in the same commit as the code change.

---

## 1. The whole picture in one diagram

```
                 ┌─────────────────────────────────────────────┐
   inbound msg   │                  CONTROL PLANE              │
   (web / Signal │  ┌──────────────────────────────────────┐   │
   / email /     │  │  axum HTTP+WS  →  policy gate        │   │
   voice)        │  │       │                ↓             │   │
   ─────────────►│  │       ▼          inbound-guard       │   │
                 │  │  EventLog.append(user_msg) [HMAC]    │   │
                 │  │       │                              │   │
                 │  │       ▼                              │   │
                 │  │  spawn / wake hot-runner container   │   │
                 │  └──────────────────────────────────────┘   │
                 │           │                                  │
                 │           ▼  runner-protocol over UDS        │
                 │  ┌──────────────────────────────────────┐   │
                 │  │       PER-CONVERSATION RUNNER         │   │
                 │  │       (stateless vs. event log)       │   │
                 │  │                                       │   │
                 │  │   ┌─────────────────────────────┐    │   │
                 │  │   │ TurnExecutor::run_turn      │    │   │
                 │  │   └─────────────────────────────┘    │   │
                 │  │       │   ▲                           │   │
                 │  │       ▼   │ tool_result               │   │
                 │  │   ┌───────┴───────┐                   │   │
                 │  │   │  vLLM (local) │  Qwen3.5-27B-AWQ │   │
                 │  │   └───────────────┘                   │   │
                 │  │       │                               │   │
                 │  │       ▼                               │   │
                 │  │  commit_turn(events) — atomic         │   │
                 │  └──────────────────────────────────────┘   │
                 │           │                                  │
                 │           ▼                                  │
                 │  ┌──────────────────────────────────────┐   │
                 │  │  outbox-relay  →  transport plugins  │───┼──► reply on
                 │  └──────────────────────────────────────┘   │    same channel
                 │                                              │
                 └─────────────────────────────────────────────┘
```

Two shapes worth memorising:

1. **The event log is canonical.** Everything the runner does is replayable from `state_events`. If the runner crashes mid-turn, it dies; the supervisor respawns a fresh container; the new runner reads the log and resumes.
2. **The LLM does not commit anything directly.** Every tool call goes through dispatch; every effect goes through the outbox; every persistence write goes through `EventLog::commit_turn`. The model proposes; the framework disposes.

---

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Conversation** | A `state_conversations` row with a unique `conversation_id`. The unit of isolation: one runner container per active conversation. |
| **Event** | An append-only `state_events` row. Every event has `(conversation_id, seq, kind, payload, hmac_tag)`. Seq is monotonic per conversation. |
| **Turn** | One round of `user_msg → ... → model_turn` committed atomically. May contain N tool-call rounds in between. |
| **Tool round** | One `model → tool_calls → tool_results → model` bounce inside a turn. `max_tool_rounds` caps runaway loops. |
| **Trust class** | The conversation's classification: `Controller \| Delegated \| KnownTrusted \| KnownLimited \| UnknownPending \| Blocked`. Drives capability gating. |
| **Capability** | A scoped permission a tool needs (`MemoryRead`, `MemoryWrite`, `ResearchWrite`, `ConversationWrite`, ...). Granted per-trust-class in `config_tool_access`. |
| **Outbox** | Durable queue of effects (messages to send, tool side-effects). The LLM never makes external calls; it asks for outbox rows. |
| **Hot runner** | A short-lived runner container associated 1:1 with an active conversation. Stateless against the log. |

---

## 3. The turn — anatomy of one inference round

The turn is the single most important unit of execution. Everything else is plumbing around it.

```
┌──────────────────────────────────────────────────────────────────┐
│                       TurnExecutor::run_turn                     │
│                                                                  │
│  1.  EventLog.append(user_msg)                  ◄── inbound text │
│                  │                                               │
│                  ▼                                               │
│  2.  history = EventLog.replay_since(conv, 0)                    │
│      messages = [system_prompt] + hydrate(history)               │
│                  │                                               │
│                  ▼                                               │
│  ┌── tool-call loop, capped at max_tool_rounds ──┐               │
│  │                                                │              │
│  │   3.  resp = inference.chat_completions(req)   │              │
│  │                  │                              │              │
│  │                  ▼                              │              │
│  │   4.  if resp.tool_calls.is_empty():           │              │
│  │           push ModelTurn(text); break          │              │
│  │       else:                                    │              │
│  │           phase = AwaitingTool                 │              │
│  │           for tc in resp.tool_calls:           │              │
│  │               push ToolUse(ordinal, name, args)│              │
│  │               result = dispatch.call(...)      │              │
│  │               push ToolResult(ordinal, result) │              │
│  │               messages += tool_result          │              │
│  │           phase = Thinking                     │              │
│  │                                                │              │
│  └────────────────────────────────────────────────┘              │
│                  │                                               │
│                  ▼                                               │
│  5.  EventLog.commit_turn(pending) — ONE TRANSACTION             │
│      enforces tool_use ↔ tool_result pairing invariant           │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

Key invariants the executor enforces (`crates/runner-local/src/turn.rs`):

- **`tool_use ↔ tool_result` pairing.** Every `ToolUse` event committed in a turn has a matching `ToolResult` with the same `ordinal`. Even tool-dispatch errors get a paired `ToolResult` carrying the error string — the chat history must not contain a dangling tool_use, because that wedges resume. `[shipped]` — see `commit_turn` in `crates/core/src/events.rs`.
- **Atomic commit.** All events from one turn land in a single SQLite transaction. The runner can crash anywhere inside the loop; either the whole turn lands or none of it does. `[shipped]`
- **Bounded tool rounds.** `max_tool_rounds` (default 6) caps the loop. Exceeding it commits an `LlmCancelled` event and returns `TurnError::MaxRounds`. `[shipped]`
- **Phase observer.** `AwaitingTool` / `Thinking` transitions are signalled to a phase observer the server wires to the WebSocket bus, so transports can keep typing-indicators on through tool calls. `[shipped]`
- **HMAC chaining.** Every event the executor writes is signed with the server's HMAC key. The chain is `tag_n = HMAC(key, prev_tag || payload)`, making the log tamper-evident. `[shipped]`

### 3.1 Why a per-conversation runner

The runner is a separate container — **not** a thread inside the control plane. Two reasons:

1. **Capability shrinking.** The runner ships with the *least* set of mounts and env it needs. The control plane can hold secrets the runner can't see.
2. **Crash isolation.** A poisoned context, an OOM, a runaway tool plugin — the runner dies, the supervisor respawns, the log replays. The control plane is unaffected. This is the same containment pattern as a shell-process job runner.

The runner is **stateless against the event log**. Memorise this — it's the property that makes durability cheap.

---

## 4. Memory model

### 4.1 The four layers (from §2.7 of `MIGRATION_PLAN.md`)

```
   ┌────────────────────────────────────────────────────────────────┐
   │ TURN-LIFETIME                                                  │
   │                                                                │
   │   ┌──────────────────────┐     ┌──────────────────────────┐   │
   │   │ 1. Transcript        │     │ 2. Scratchpad            │   │
   │   │    state_events      │     │    (in-memory only)      │   │
   │   │    every msg/tool/   │     │    chain-of-thought,     │   │
   │   │    decision, signed  │     │    discarded post-turn   │   │
   │   └──────────────────────┘     └──────────────────────────┘   │
   │                                                                │
   ├────────────────────────────────────────────────────────────────┤
   │ DURABLE                                                        │
   │                                                                │
   │   ┌──────────────────────┐     ┌──────────────────────────┐   │
   │   │ 3. Compaction        │     │ 4. Long-term             │   │
   │   │    summary           │     │    memory_entries        │   │
   │   │    one row per       │     │    key/value, trust-     │   │
   │   │    summarised range  │     │    class-scoped          │   │
   │   └──────────────────────┘     └──────────────────────────┘   │
   │                                                                │
   └────────────────────────────────────────────────────────────────┘
```

| # | Layer | Lifetime | Storage | Trust scoping |
|---|---|---|---|---|
| 1 | Transcript | Forever | `state_events` (HMAC-chained) | Per-conversation; trust class is on the conversation row |
| 2 | Scratchpad | One turn | runner process memory | N/A |
| 3 | Compaction summary | Until superseded | `state_events` of kind `Summary` | Same as transcript |
| 4 | Long-term | Forever (with optional TTL) | `memory_entries` | **Per-row trust class — first-class** |

The novel part is **trust-class scoping on long-term memory**. The composite primary key `(scope, trust_class, key)` lets the same key carry different values at different trust levels. A `Controller`-class secret is invisible to a `KnownTrusted` caller even when scope+key match — the row simply doesn't exist at their trust level.

### 4.2 Lifecycle (migration 0035) `[schema-ready]`

The four-layer model describes *kinds* of memory. Migration 0035 adds **lifecycle dynamics** — recency, frequency, promotion, demotion. The columns added to `memory_entries`:

```sql
ALTER TABLE memory_entries ADD COLUMN tier         TEXT    NOT NULL DEFAULT 'warm';
ALTER TABLE memory_entries ADD COLUMN hits         INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory_entries ADD COLUMN last_used_at INTEGER;
ALTER TABLE memory_entries ADD COLUMN created_at   INTEGER NOT NULL DEFAULT 0;
```

```
                        ┌────────────────┐
                        │   write_memory │
                        └───────┬────────┘
                                ▼
                       ┌─────────────────┐
                       │      WARM       │  ← default tier
                       │  on-demand read │     for every fresh write
                       └─────┬───────────┘
                             │
              hits ≥ 3 in 7d │            idle ≥ 30d
              + APPROVAL     │            + APPROVAL
                             ▼
                       ┌─────────────┐    ┌──────────┐
                       │     HOT     │───►│  WARM    │
                       │  always in  │    └──────────┘
                       │ system slot │
                       └─────────────┘          │ idle ≥ 90d
                                                │
                                                ▼
                                          ┌──────────┐
                                          │   COLD   │
                                          │ excluded │
                                          │ from     │
                                          │ default  │
                                          │ reads    │
                                          └──────────┘
```

**Tier semantics:**

- **HOT** — auto-injected into the runner's per-turn system prompt via the HOT slot (§5). Bounded byte budget. Promotion *requires controller approval*. `[schema-ready, runner injection design]`
- **WARM** — reachable via `read_memory` / `list_memory`. The default tier for every fresh `write_memory`. `[shipped]`
- **COLD** — excluded from `list_memory` and the HOT slot. `read_memory` with a known scope+key still works (audit / never-truly-forget). `[shipped]`

**Promotion is approval-gated, not agent-driven.** The agent never sets `tier` directly. A promotion sweeper (or a planner-role reflection pass) inserts a row in `memory_promotions`; that row sits as a controller-approval event in the SPA dropdown, exactly like skill proposals or OAuth grants. Approval flips the target row's tier; rejection records the decision and leaves the tier alone.

This is the same Rule-of-Two posture used elsewhere: the agent can *propose* a self-modification, but a separate principal (the controller, or an auto-approve trust-policy rule) is the one who applies it. Without this gate, every controller correction would silently canonize itself into the always-loaded prompt — including the wrong ones.

**Schema:**

```
              memory_entries
              ┌───────────────────────────────────────────┐
              │ (scope, trust_class, key)  PK             │
              │ value_blob                                │
              │ tier         hot|warm|cold                │
              │ hits         INTEGER (read_memory bumps)  │
              │ last_used_at INTEGER (read_memory stamps) │
              │ created_at   INTEGER                      │
              │ updated_at   INTEGER                      │
              │ ttl_expires  INTEGER (NULL = permanent)   │
              └────────┬──────────────────────────────────┘
                       │ FK (scope, trust_class, key) — cascade
                       ▼
              memory_promotions
              ┌───────────────────────────────────────────┐
              │ id  (autoinc)                             │
              │ scope, trust_class, key                   │
              │ from_tier, to_tier                        │
              │ reason       frequency|recency|           │
              │              reflection|manual            │
              │ proposed_by  sweeper|planner|controller   │
              │ proposed_at, decided_at, decision         │
              └────────┬──────────────────────────────────┘
                       │ FK (id) — set null on delete
                       ▼
              memory_reflections
              ┌───────────────────────────────────────────┐
              │ id  (autoinc)                             │
              │ conversation_id, anchor_event_seq         │
              │ context_text, reflection_text,            │
              │ lesson_text                               │
              │ promotion_id  (optional link to a write)  │
              │ created_at                                │
              └───────────────────────────────────────────┘
```

---

## 5. The HOT slot — always-loaded working set `[design]`

Today, memory is only seen by the model if it calls `read_memory`. That's expensive: every turn the agent has to *remember to remember*, often via several speculative tool calls just to discover what's there. Skills like `claude-code`'s `CLAUDE.md` work because they are auto-injected into every turn's system prompt. The HOT slot is the execlaw equivalent.

```
   per-turn system prompt assembly (in chats.rs)
   ─────────────────────────────────────────────────────────────
   ┌─────────────────────┐
   │  personality        │
   ├─────────────────────┤
   │  static base prompt │
   ├─────────────────────┤
   │  routing prose      │  (per-tool one-liners)
   ├─────────────────────┤
   │  HOT MEMORY SLOT    │  ◄── new, capped at N bytes
   │  (always-loaded     │      MemoryStore::list_hot(
   │   tier='hot')       │        scope, readable_classes,
   │                     │        limit
   │                     │      )
   ├─────────────────────┤
   │  turn context       │  (clock, sender, trust)
   └─────────────────────┘
```

**Wiring plan (not yet shipped):**

1. Extend `assemble_system_prompt` to call `MemoryStore::list_hot` with the conversation's read-down trust chain.
2. Format each row as a single line (`<key>: <truncated value>`).
3. Cap at a configurable byte budget (default 2 KB; surfaced in `config_general`).
4. The runner enforces the cap — the agent cannot bloat its own prompt by promoting unbounded values.
5. Every HOT-slot read also bumps `hits` / `last_used_at` on the rows it shows, so a HOT entry that's actively shaping behavior keeps its place; one that's been irrelevant for 30 days starts showing up in the demotion sweeper's candidate list.

**Why this is bounded.** Promotion to HOT requires controller approval. The approval queue is the natural rate limiter — operators won't approve garbage at scale. Combined with the byte budget, the HOT slot stays focused.

---

## 6. The reflection loop `[design]`

After a turn completes, the agent has new evidence about what the user actually wanted, what worked, and what didn't. Without a structured forcing function, that signal evaporates. The reflection pass captures it.

```
                  turn N completes
                          │
                          ▼
                  heuristic gate
                  ───────────────
                  • correction detected?
                  • novel proper noun?
                  • controller approval/denial?
                  • repetition (same instruction
                    seen before in this scope)?
                          │
                          ▼ at least one trigger
                ┌──────────────────────────────┐
                │  planner-role inference call │
                │  (same Qwen3.5; planner-     │
                │  prompt; NO tools)           │
                │                              │
                │  emits 0..N {                │
                │    context_text:    "..."    │
                │    reflection_text: "..."    │
                │    lesson_text:     "..."    │
                │    propose_write?:  bool     │
                │    target_tier?:    hot|warm │
                │  }                           │
                └────────────┬─────────────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │ ReflectionStore      │  always
                  │ .append(conv, anchor,│  appended
                  │ context, refl, less, │
                  │ promotion_id?)       │
                  └──────────┬───────────┘
                             │
                             ▼
                if propose_write:
                  ┌──────────────────────┐
                  │ MemoryStore.upsert   │  warm row
                  │ (...)                │  written
                  └──────────┬───────────┘
                             │
                             ▼
                if target_tier == HOT:
                  ┌──────────────────────┐
                  │ PromotionStore       │  proposal
                  │ .propose(reflection) │  enqueued
                  └──────────┬───────────┘  for approval
                             │
                             ▼
                       SPA dropdown
                       controller decides
```

**Properties this design enforces:**

- **Reflection is event-driven, not clock-driven.** Runs at end of turn, gated by heuristics, not on a heartbeat timer. Honors the wakeup rate-limit posture and the silent-hold rule (`project_locked_decisions_2026_04_23.md`).
- **The executor has no reflection role.** Reflection is a planner-role call. The executor remains tool-using and untrusted-content-handling; the planner is the introspective role. Same model, different prompt + no tools — consistent with `[CaMeL §2.5]`.
- **Reflections are append-only and audit-anchored.** Every row points at the `anchor_event_seq` it reflects on. The HMAC chain on `state_events` gives tamper-evidence for the cited turn; the reflection row is derivative, not authoritative.
- **No autonomous HOT promotion.** A reflection that proposes a HOT promotion writes the value (warm) AND opens a `memory_promotions` row. The actual tier flip waits for approval.

---

## 7. Trust-class scoping and the policy gate

```
   inbound message
        │
        ▼
   ┌─────────────────────────────────────────────────────────────┐
   │  policy gate (crates/policy)                                │
   │                                                             │
   │  conversation.trust_class ──┐                               │
   │                             ▼                               │
   │              ┌──────────────────────────────────┐           │
   │              │  per-tool capability check       │           │
   │              │  config_tool_access[trust_class] │           │
   │              │  ⊇ tool.required_capabilities?   │           │
   │              └──────────────────────────────────┘           │
   │                             │                               │
   │              denied ────────┴──── allowed                   │
   │                │                       │                    │
   │                ▼                       ▼                    │
   │         tool not in        runner sees the tool             │
   │         the runner's       in its catalog this turn         │
   │         `tools` slice                                       │
   └─────────────────────────────────────────────────────────────┘
```

**The trust ladder** (high → low):

```
   Controller > Delegated > KnownTrusted > KnownLimited > UnknownPending > Blocked
```

Read-down memory cascade: a `KnownTrusted` caller can read entries written at `KnownTrusted`, `KnownLimited`, or `UnknownPending` — but never at `Controller` or `Delegated`. This is enforced in `tool_apis::DbMemoryApi::read` by computing the readable chain and walking it; only one row's value comes back. Writes always land at the caller's own class — there is no promotion, ever.

**`Blocked`** is universal. Whether the principal is unknown-and-untrusted or previously-trusted-and-revoked, `Blocked` is the same state: silent hold, message logged for audit, no agent response. (Locked decision `project_locked_decisions_2026_04_23.md`.)

---

## 8. Rule of Two / planner-executor split `[design]`

For turns that ingest *untrusted content* (web fetch, email body, PDF text), the runner switches to a two-role split:

```
   ┌─────────────────────────────────────────────────────────────┐
   │                       UNTRUSTED TURN                        │
   │                                                             │
   │   ┌──────────────┐                  ┌──────────────────┐   │
   │   │   PLANNER    │                  │     EXECUTOR     │   │
   │   │              │                  │                  │   │
   │   │  full prompt │── plan (text) ──►│  receives plan   │   │
   │   │  full tools  │                  │  + raw content    │   │
   │   │  trust:      │                  │                  │   │
   │   │  caller's    │                  │  trust: caller's │   │
   │   │              │                  │  NO TOOLS        │   │
   │   │  reasons     │                  │  output: text    │   │
   │   │  about goal  │                  │  only            │   │
   │   └──────────────┘                  └──────────────────┘   │
   │       Qwen3.5                            Qwen3.5            │
   │       reasoning on                       reasoning off      │
   │                                                             │
   └─────────────────────────────────────────────────────────────┘
```

Same model both times — cost is in the second forward pass, not in a different deployment. The split is *role*, not *deployment*. Three things make this useful:

- The planner sees the goal, the executor sees the untrusted content, **but they don't both see both**. An injection in the untrusted content cannot acquire tools because the role that has tools never sees the injection.
- The executor has **no `tools` array at all**. Even if injected text says "call `delete_everything`", the model can't — that capability isn't wired.
- The planner can ask the executor to "extract X from this text and report" — a constrained sub-goal — and the executor can only return text. That text gets fed back to the planner, which decides the next move.

This is the CaMeL pattern (DeepMind) translated to a single local model. It's not a complete defense — `The Attacker Moves Second` showed every defense shipped in 2025 is breakable in human red-team — but the architectural containment closes the most common vector cheaply.

---

## 9. Outbox + idempotency

```
   model emits a tool_call that has a side-effect
        │
        ▼
   ToolDispatch.call("send_signal_message", {...})
        │
        ▼
   tool implementation enqueues an OUTBOX row
   ┌──────────────────────────────────────────┐
   │ state_outbox                             │
   │   idempotency_key = HMAC(secret,         │
   │     conversation_id || turn_seq ||       │
   │     tool_call_ordinal)                   │
   │   payload = the actual side-effect       │
   │   target  = "signal" / "email" / ...     │
   │   attempts = 0                           │
   └──────────────────────────────────────────┘
        │
        ▼
   tool_result for the model: "queued, key=..."
        │
        ▼
   commit_turn (atomic) ── outbox row + events both land
        │
        ▼
   outbox-relay (separate task, retries with backoff)
        │
        ▼
   transport plugin (signal, email, ...) actually sends
        │
        ▼
   on success: stamp delivered_at, write delivery event
   on perma-fail: dead-letter, alert fires
```

**Why this matters for the agent model:** the LLM never makes a network call. It can only ask the framework to make one. Replaying the event log replays the *intent* (the outbox row was minted), not the *effect* (the relay handles dedup). Crash mid-turn → on respawn, the outbox already has the row keyed by the deterministic idempotency key; the relay either delivers or sees it already delivered.

The idempotency key is **never LLM-derived**. It's HMAC'd from `(conversation_id, turn_seq, tool_call_ordinal)` so the agent cannot cause a double-send by saying "use idempotency key X".

---

## 10. Wakeup, scheduling, subagents

### Wakeup `[shipped]`

A `wakeup` is a deferred re-entry into the conversation's runner, scheduled by a tool call. The pattern:

```
   turn N: agent calls schedule_wakeup(in: 30m, prompt: "check the build")
         │
         ▼
   state_routine_runs row inserted (deterministic id)
         │
         ▼
   commit_turn → relay → wakeup-queue
         │
         ▼
   30 minutes later, wakeup-fires
         │
         ▼
   spawn runner for this conversation, replay log,
   inject the scheduled prompt as a synthetic user_msg
         │
         ▼
   normal turn flow (§3) takes over
```

**Rate limit:** default 12 wakeups / hour / conversation. Anomaly above this rate fires an alert and pauses further wakeups pending controller approval.

### Routines `[shipped]`

`config_routines` rows are cron-shaped recurring tasks. The scheduler fires them via the same wakeup channel.

### Subagents `[shipped — DeepResearchExecutor]`

The primary agent can spawn a background subagent (`delegate_task`, `research_*`) that runs in its own context with a narrower tool slice. Subagent results return as a `tool_result` event in the parent's log when complete. The subagent's runner is a separate hot-runner container.

**Capability shrinking:** the subagent's tool catalog is the parent's catalog *minus* `delegate_task` and any other tool the parent's policy says cannot be re-delegated. Subagents cannot fan out unboundedly.

---

## 11. Self-improvement in execlaw

Mapping the patterns from §6 of the project memory (proactive-agent / self-improving) onto the execlaw architecture:

| Pattern | Their solution | execlaw equivalent | Status |
|---|---|---|---|
| Survive context loss | Write-Ahead Log + working buffer | Event log is canonical; runner is stateless | `[shipped]` |
| Compaction recovery | Read SESSION-STATE.md on resume | Replay `state_events` from seq 0 | `[shipped]` |
| Preferences are durable | `memory.md` always-loaded | HOT slot in system prompt | `[design]` (schema ready) |
| Promote what's repeatedly useful | Pattern applied 3+ times → HOT | `MemoryStore::promotion_candidates` + `PromotionStore::propose` | `[schema-ready]` |
| Demote what's stale | 30 days idle → WARM | `MemoryStore::demotion_candidates` | `[schema-ready]` |
| Archive what's dead | 90 days idle → COLD | `MemoryStore::set_tier(Cold)` via approved demotion | `[schema-ready]` |
| Reflect after work | CONTEXT / REFLECTION / LESSON | `memory_reflections` table + planner-role pass | `[schema-ready]` (planner pass `[design]`) |
| Anti-drift | Score before storing | Approval-gated promotion (Rule of Two) | `[shipped]` (gate exists) |
| Source attribution | Citation on each rule | `anchor_event_seq` on every reflection | `[shipped]` (column ready) |
| Heartbeat checks | Cron self-improvement | Wakeup + routines + reflection trigger | reflection trigger `[design]` |

**What execlaw does *not* adopt:**

- **External file-based working buffer.** Conflicts with the SQLite-only-state rule. Same recovery property comes from event sourcing — for free, with HMAC tamper-evidence.
- **Autonomous self-promotion of memory.** Agents propose; controller (or trust-policy auto-approve) applies. Without this gate, every wrong correction the user makes gets canonized.
- **Time-based "self-improvement" cron firing without explicit triggers.** Reflection runs at end-of-turn under heuristics, not on a clock. Honors the silent-hold rule for cold contacts.

---

## 12. What's actually wired today (2026-05-07)

A precise read of the codebase, not a status report:

**Shipped:**
- TurnExecutor with tool-use/result pairing, max-rounds cap, phase observer, HMAC chaining (`crates/runner-local/src/turn.rs`)
- Event log with HMAC chain, atomic commit_turn (`crates/core/src/events.rs`)
- MemoryStore with trust-class composite PK, read-down cascade in `DbMemoryApi`, write-at-caller-class only, tier/hits/last_used_at columns from migration 0035
- `read_memory` / `write_memory` / `list_memory` as built-in tools; `list_memory` performs real prefix scans (post-0035) and excludes COLD
- `read_memory` bumps `hits` and stamps `last_used_at` on the row that matched in the read-down cascade
- `PromotionStore` with idempotent propose, approve flips target tier, reject leaves tier alone
- `ReflectionStore` append + per-conversation list
- Wakeup, routines, subagents (DeepResearchExecutor)
- Outbox with idempotency keys, dedup, retry, dead-letter, alerts
- Capability tokens, `config_tool_access` per trust class
- Per-conversation runner containers, supervisor respawn

**Schema-ready, runner-side glue not yet in main:**
- HOT slot injection in `assemble_system_prompt` — needs a call to `MemoryStore::list_hot` with byte cap
- Promotion sweeper — periodic background task feeding `promotion_candidates` into `PromotionStore::propose`
- Demotion sweeper — periodic background task feeding `demotion_candidates` into `PromotionStore::propose`

**Designed, not yet implemented:**
- Planner/executor split for untrusted-content turns
- Post-turn reflection pass (planner role)
- Heuristic gate that decides when reflection fires
- SPA UI for the memory promotion approval queue (uses the same dropdown infra as skill proposals)

---

## 13. File-level reading order

Want to verify a claim in this doc? Here's where to look:

| Claim | Verify in |
|---|---|
| Turn loop, tool pairing | `crates/runner-local/src/turn.rs` |
| Event log + HMAC | `crates/core/src/events.rs`, `event_hmac.rs` |
| Memory storage + lifecycle | `crates/core/src/memory.rs` |
| Promotion + reflection stores | `crates/core/src/memory_lifecycle.rs` |
| Memory tools | `crates/core/src/builtin_tools.rs` |
| MemoryApi + trust cascade | `crates/core/src/tool_apis.rs` |
| System prompt assembly | `crates/server/src/chats.rs::assemble_system_prompt` |
| Migration 0035 schema | `crates/core/migrations/0035_memory_lifecycle.sql` |
| Trust policy ladder | `crates/policy/src/lib.rs` |
| Outbox | `crates/outbox/`, `crates/server/src/outbox_relay.rs` |
| Container lifecycle | `crates/container-manager/`, `crates/server/src/runner_supervisor.rs` |

If a description here drifts from the code, the code is right and this doc is wrong — fix the doc in the same commit.
