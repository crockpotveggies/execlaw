# execlaw — Migration Plan

A from-scratch rebuild of the `selfhosted-claw` claw-agent (which itself forked nanoclaw). This document specifies the target architecture and the phased path from today's Node.js + Signal-centric system to a Rust control plane with a pluggable framework, unified container manager, chat-first UI, and a conversation-aware agent model.

---

## 0. Grounding Principles

These rules override convenience. When a design decision is ambiguous, resolve it by asking which option honors these.

1. **Self-hosted only.** execlaw runs entirely on the operator's hardware. **No cloud LLM providers, period** — not as defaults, not as opt-in plugins, not as bridge adapters, not via any path. No Anthropic API, OpenAI, Gemini, or equivalent, ever. No hosted registry for plugins, no phone-home telemetry, no external KMS, no cloud DB, no hosted STT/TTS/inference of any kind. All models are local (vLLM, OpenArc, llama.cpp, Ollama, or equivalent local inference servers). First-run, airplane-mode install must succeed with reduced capabilities until local inference and at least one transport are configured. The rule is **strict**: any proposal that puts a cloud LLM on the critical path — default or optional — is rejected on sight. This rule has been violated in earlier drafts and is now invariant.
2. **SQLite is the source of truth for configuration and state.** No `.env`, no shared config files edited by the UI and consumed by other processes. Secrets in an encrypted vault with the master key in the OS keyring.
3. **The event log is the source of truth for conversations.** Every agent action is an event in an append-only table. Runners are stateless; on crash, replay reconstructs state. This is the Temporal/LangGraph durable-execution pattern, implemented on SQLite for self-hosted deployment. (§2.3)
4. **Effects go through an outbox; the LLM never calls external APIs directly.** Framework-minted idempotency keys, consumer-side inbox dedup, at-least-once delivery. The LLM produces intent; a separate relay produces effects. (§2.4)
5. **Every tool_use pairs with a tool_result in the same commit.** Interrupted turns synthesize cancellation results. Asymmetric logs are the most common corruption mode in shipping agent systems and the one that breaks resume. (§2.2, §2.10)
6. **Plugins, not hardcoded built-ins.** Anything crossing a trust, process, or network boundary is a plugin with a manifest and declared capabilities — transports, tools, skills, identity providers, services, inference backends. The term is always "plugin" in execlaw (in docs, UI, code, settings); "integration" was the selfhosted-claw term and is retired.
7. **One control plane, one container manager.** Every `docker` interaction goes through one Rust crate. The UI never shells out. Plugins never `docker run`.
8. **Participant-aware; trust is per-principal and derived into the conversation.** Every participant has a trust level on a ladder — `Controller` / `Delegated` / `KnownTrusted` / `KnownLimited` / `UnknownPending` / `Blocked`. Conversation kind and effective policy are *derived* from the participant composition, not assigned. Identity-provider plugins (Google Contacts, local address book, Signal safety-numbers) resolve inbound identifiers to stable principals. Contacts auto-trust by default; cold contacts escalate to the controller via sideband for a trust decision. Long-term memories are indexed by trust class so untrusted principals can't retrieve `Controller`-scoped memories. (§2.6, §2.7, §2.14)
9. **Rule of Two.** In any turn that ingests untrusted input, at most two of {untrusted input, sensitive data, external effect} may hold. The third forces HITL. This is the honest posture given that prompt injection is unsolved. (§2.6, §7)
10. **Sideband HITL.** Approval requests route through a *different* transport than the one that introduced the untrusted content. The originating transport might be the attacker. (§2.11)
11. **Portable control-plane container.** The deployment artifact is a **container image**, not a bare binary — matching selfhosted-claw's proven `Dockerfile.control-plane` + `docker-compose.control-plane.yml` pattern. Build on WSL2, ship as a Linux container, deploy via `docker compose up` on either a WSL2 dev host or a bare-Linux production host — same image. The Rust binary is an implementation detail inside the image (kept minimal per axiom #12). No platform-specific code paths in core crates; platform adapters live behind trait boundaries. `execlaw up` is a thin wrapper around `docker compose` for the operator.
12. **Minimal containers.** Every docker image ships only what its single job requires — the Unix philosophy, applied at the container level. The **control-plane container** contains the Rust binary + its shared-lib deps + CA certs + a small embedded PCI ID database, **nothing else**: no CUDA toolkit, no OpenVINO runtime, no `nvidia-smi`, no provider SDKs, no ffmpeg, no Python. When the control plane needs vendor-specific information (GPU details, codec probes, device quirks), it reads host sysfs via read-only bind-mount or spawns a purpose-built one-shot **probe container** that has the right tooling, prints JSON, and exits. This rule applies to every container execlaw ships — runner containers, service containers, transport plugins, probe containers — each with the minimum software to do its single job. Concrete consequences: small image pulls, smaller attack surface, faster cold starts, vendor tooling upgrades don't require control-plane rebuilds.
13. **Extensive testing is non-negotiable.** Every non-trivial function has a unit test. Every load-bearing invariant (tool_use/tool_result pairing, idempotency-key dedup, trust-class scoping, turn-as-transaction atomicity, capability-token binding, barge-in rescind, HMAC tamper detection) has an *explicit* test that would fail if the invariant were broken. Every public API has at least one integration test. Every crate runs `cargo test` clean as part of `cargo build --workspace`. No module lands without tests for its happy path + its one or two failure modes. Security-critical code (policy engine, capability verification, trust-level cascading) has adversarial tests — "what if a caller tries to read memory at a higher trust level than their own" is a test case, not an assumption. CI fails the build on `cargo clippy --workspace --all-targets -- -D warnings` and on any test regression. The bar is: a contributor adding a new feature should be forced to write a test that proves it; a contributor changing existing behavior should watch a test fail before they get to ship. This rule is as strict as #1: a feature without tests is not done.
14. **Performance is measured, not guessed.** On top of tests, execlaw continuously benchmarks and micro-benchmarks every hot path. The load-bearing hot paths — event-log append, `commit_turn`, snapshot hydration, outbox `ready_pending` + `claim`, HMAC sign/verify, idempotency-key mint, capability-token issue/verify, policy `evaluate_turn`, memory-tool dispatch (read/write with trust cascade), spotlight wrap, plugin manifest parse, endpointer `classify_tail`, bargein `decide`, UI event broadcast — all have **Criterion** benchmarks under `benches/`. Each benchmark asserts an explicit budget (e.g. "`evaluate_turn` ≤ 1µs p99", "`sign_event` ≤ 10µs p99", "`commit_turn(10 events)` ≤ 2ms p99 on SQLite in-memory"). A **performance regression is a build failure** the same way a test failure is — Criterion's baseline comparison runs in CI and blocks merges that regress more than 10%. Optimizations are justified by *before/after microbenchmark numbers in the PR*, not by intuition. The rule scales with cost: a 2x speedup on a 100ns function that runs once per minute is noise; a 10% speedup on `commit_turn` is shipping priority. **Never optimize without a benchmark that shows the regression**; never *claim* a speedup without a benchmark that proves it. This rule is as strict as #13: a feature with known-unmeasured performance characteristics is not done.

---

## 1. North-Star Architecture

```
                        ┌──────────────────────────────────────┐
                        │              Chat-First UI            │
                        │   (React 19 + Vite, CoreUI, ported)   │
                        └────────────┬─────────────────────────┘
                                     │  HTTPS + WebSocket (event stream)
                                     │
                 ┌───────────────────▼────────────────────────────────┐
                 │           RUST CONTROL PLANE (execlaw-core)         │
                 │                                                      │
                 │  ┌──────────────┐  ┌──────────────┐  ┌────────────┐ │
                 │  │  Conversation │  │  Agent Loop  │  │ Scheduler   │ │
                 │  │    Router     │──▶  (state mc)  │──▶ + Wakeups   │ │
                 │  └──────┬────────┘  └──────┬───────┘  └─────┬──────┘ │
                 │         │                   │                │         │
                 │   ┌─────▼────────┐    ┌─────▼───────┐   ┌────▼─────┐ │
                 │   │ Plugin Host  │    │  Container  │   │  Policy   │ │
                 │   │ (manifest +  │    │   Manager   │   │  Engine   │ │
                 │   │ capabilities)│    │ (runners +  │   │(capabilities│
                 │   └──────────────┘    │  services)  │   │  +policy) │ │
                 │                       └─────────────┘   └───────────┘ │
                 │                                                       │
                 │          Event Bus  |  Persistent Work Queue  |  Vault│
                 └──────┬──────────────────────────┬───────────────────┬─┘
                        │                          │                   │
               ┌────────▼────────┐     ┌───────────▼─────────┐   ┌─────▼──────┐
               │ Transport       │     │  Runner containers  │   │  Service   │
               │ plugins         │     │  (OpenAI-compat API)│   │ containers │
               │ (Signal, voice, │     │                     │   │ (vLLM, STT,│
               │  email, web …)  │     │                     │   │  TTS, DBs) │
               └─────────────────┘     └─────────────────────┘   └────────────┘
```

Every box outside the control plane is a container or a plugin. The control plane never calls `docker` from more than one place and never assumes a specific transport.

---

## 2. The Agent Model

Synthesized from the selfhosted-claw audit and 2024-2026 best-practice research — *as design patterns only*, implemented entirely in our own Rust crates against local models. Sources include Anthropic engineering posts (effective harnesses, context engineering, multi-agent research system), Temporal (durable execution), LangGraph (state-machine persistence), Cognition ("Don't Build Multi-Agents"), Willison (lethal trifecta), DeepMind (CaMeL), Microsoft (Spotlighting), Meta ("Agents Rule of Two"), and the Oct 2025 "The Attacker Moves Second" adaptive-defense survey. **No vendor SDKs are imported; no vendor-specific tool schemas are used.**

### 2.1 Root causes found in selfhosted-claw

| Weakness | Root cause (evidence) |
|---|---|
| Duplicate messages | `seenMessageIds` is an **in-memory Set** in [channels/signal.ts:149](../selfhosted-claw/src/channels/signal.ts). Lost on restart. Message ID is synthesized from `{sender}-{timestamp}-{content.length}`, not from Signal's stable `sourceTimestamp + sourceUuid`. |
| Race conditions | `startMessageLoop` in [src/index.ts:804-928](../selfhosted-claw/src/index.ts) **advances the DB cursor before the container confirms completion**. Crashed containers leave a gap. `groupsWithSideEffects` flag is set in IPC and read outside any lock. |
| Failure handling | Retry is best-effort: 5 attempts + 10-min cooldown, no dead-letter queue. Malformed messages can silence a whole group. IPC errors move files to `data/ipc/errors/` and are never retried. |
| No scheduled wakeups | Agent can emit `schedule_task` to IPC, scheduler polls every ~60s → 0-120s jitter. No "pause this exact context, resume in N seconds with memory intact" primitive. |
| Signal entanglement | JID scheme `signal:user:*` / `signal:group:*` is baked into routing, contact resolution, outbound directives, control commands. Mention format (U+FFFC) is Signal-specific. |
| Voice is bolted on | Phone voice has its own runner (`voice-runner/`), its own WebSocket stack, its own LLM container. It doesn't flow through `startMessageLoop`. |
| Coarse auth | Single `isMain` boolean. No per-member, per-tool, per-conversation policy. `actorIdentity` is a string, not a capability. |

### 2.2 Design axioms

1. **The event log is the source of truth.** Every user message, model turn, tool call, tool result, approval, and committed effect is an event in an append-only SQLite table. Runners are stateless against this log; on crash, the control plane replays events to reconstruct state and reissues only the uncommitted step. This is the Temporal/LangGraph/Codex-CLI pattern rebuilt on SQLite for self-hosted deployment.
2. **Idempotent at the edge, transactional at the turn.** Every inbound event from a transport plugin carries a stable `(plugin_id, source_event_id)` and is deduplicated in a persistent table before the agent sees it. Every agent turn (model call + tool calls + state mutations + outbox writes) commits atomically — either the whole turn is in the log or none of it is.
3. **Every `tool_use` must pair with a `tool_result` in the same commit.** This is the single most violated invariant in shipping agent systems (see the open bug in Claude Code itself where interruption leaves dangling `tool_use` blocks that corrupt resume). execlaw's rule: if a tool call is emitted but cannot complete, the turn commits a synthesized cancellation `tool_result("cancelled: <reason>")` alongside it. Never an asymmetric log.
4. **Effects go through an outbox; the LLM never calls external APIs directly.** The agent produces tool calls; the control plane records intent in an outbox atomically with the turn commit; a separate relay process — outside the LLM retry loop — drains the outbox with framework-minted idempotency keys and at-least-once delivery. Consumer-side inbox dedup guarantees no double-send. Idempotency keys are derived from `(conversation_id, turn_seq, tool_call_ordinal)` — **never** from LLM output (a subtle bug: the model rephrases its rationale and collision checks silently fail).
5. **Runners are stateless; sessions live in the log.** A runner container may crash, be killed, or re-spawn on a different host. It reads the session transcript from the control plane at start, runs one turn, writes its output back, and exits (or stays warm for the next turn). Nothing durable lives in the runner filesystem or memory.
6. **Per-conversation serialization; cross-conversation parallelism.** One worker per conversation at a time (distributed lock keyed on `conversation_id`). Different conversations run in parallel, bounded by container pool size.
7. **Scheduled wakeups are just resume events.** The `schedule_wakeup(delay, note)` tool writes a wakeup row with a target timestamp. The scheduler fires it; the control plane appends a `wakeup` event; a runner picks up from the log. No special "wakeup path" — sub-second precision via priority queue + notify, not a 60s poll.
8. **Planner/executor separation for untrusted input.** For any conversation handling untrusted content, the runner executes two model roles: a *planner* that sees trusted metadata and holds tools, and an *executor* that sees untrusted content and holds no tools. Data across the boundary carries provenance tags. This is CaMeL / lethal-trifecta containment — architectural, not model-level.
9. **Rule of Two, per turn.** Untrusted input + sensitive-data access + external-effect — at most two of the three may be true in a single turn without HITL. Enforced by the policy engine, not by prompting the model to behave.
10. **Local inference is the default; cloud models are bridge plugins, never core.** execlaw's runner speaks an **OpenAI-compatible API** (`/v1/chat/completions` with function calling, streaming SSE) to whatever backend is configured. The default backend is a **local inference server** — vLLM hosting a Qwen-class instruct model — managed by the container manager. Cloud models (Anthropic, OpenAI, Gemini) are available *only* through optional "inference bridge" plugins that adapt their APIs to the internal OpenAI-compatible contract. The core control plane has zero cloud-SDK dependencies. Agent-loop features that cloud SDKs typically bundle — sessions, memory primitives, streaming, tool use, compaction, extended-thinking selection — are implemented in the execlaw Rust crates against our own SQLite event log and `memory_entries` table.
11. **Two modalities, one event log, one runner.** Text and voice share the same event log, policy engine, capability tokens, outbox, memory, conversation types, and runner (`runner-local`). Voice is a streaming STT → LLM → TTS pipeline (Whisper / Qwen / Kokoro on the hybrid hardware target) with Silero VAD, LiveKit turn-detector, WebRTC AEC3, and barge-in + backchannel-rescind. A "turn" is a commit unit, not a request boundary. Native-audio full-duplex (model-emits-audio-tokens) is not achievable self-hosted with current OSS; we approximate with aggressive streaming + barge-in and document the latency delta honestly (§2.13.6). Big-model reasoning is available via sub-agent escalation from either modality.

### 2.3 The event-sourced state machine

Every conversation is an append-only event log. Resuming = replaying events (cheaply, from a snapshot) to reconstruct state. Failing mid-turn = committing cancellation results so the log is always internally consistent.

```sql
CREATE TABLE state_events (
    conversation_id TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    kind            TEXT NOT NULL,   -- 'user_msg' | 'model_turn' | 'tool_use'
                                     -- | 'tool_result' | 'interrupt' | 'resume'
                                     -- | 'approval' | 'effect_committed' | 'wakeup'
    payload         BLOB NOT NULL,   -- MessagePack
    committed_at    INTEGER NOT NULL,
    actor           TEXT,            -- principal_id or 'system'
    PRIMARY KEY (conversation_id, seq)
);

CREATE INDEX idx_state_events_time ON state_events(conversation_id, committed_at);

CREATE TABLE state_conversations (
    conversation_id TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    last_seq        INTEGER NOT NULL,
    phase           TEXT NOT NULL,   -- 'idle' | 'thinking' | 'awaiting_tool'
                                     -- | 'awaiting_approval' | 'awaiting_wakeup'
                                     -- | 'awaiting_reconnect'
    controller_id   TEXT,
    trust_class     TEXT NOT NULL,
    snapshot_blob   BLOB,            -- materialized view for fast resume
    snapshot_seq    INTEGER,
    lease_owner     TEXT,            -- worker id, NULL if idle
    lease_expires   INTEGER
);
```

**Snapshotting.** Every ~50 events the worker materializes a snapshot; replay reads events after `snapshot_seq`. Constant-time resume for long conversations.

**Determinism under a non-deterministic model.** The log records *outputs* of model turns, not just inputs. On replay, a completed `model_turn` event is treated as given — the model is invoked only for turns without a recorded output. This is the Temporal "side-effect" pattern.

### 2.4 The turn as a transaction (a commit unit, not a request boundary)

A **turn** is whatever commits atomically to the event log. For text, that's one user-message / model-response exchange. For voice, it's the span from end-of-user-utterance to end-of-agent-utterance (or the barge-in that interrupted it) — containing streaming STT events, LLM tokens, TTS audio chunks, and any tool calls. Either way the invariants below hold identically; §2.13 covers the per-modality specifics.

A single agent turn:

1. Worker leases the conversation (exclusive SQLite row-level lock on `state_conversations.lease_owner`, with `lease_expires` for crash recovery).
2. Worker loads snapshot + events-since-snapshot → reconstructs transcript, scratchpad, trust class, capability set, pending tool calls.
3. Worker spawns or reuses a runner container; hands it session, tools, capabilities, memory-tool mount.
4. Runner calls the configured inference backend over the internal OpenAI-compatible API → model produces a turn: zero or more `tool_use` blocks plus a (possibly empty) text response.
5. For each `tool_use`: policy engine verifies capability → execute tool (side effects go to outbox, never direct) → produce `tool_result`.
6. Worker commits the turn as ONE SQLite transaction:
   - `model_turn` event (model ID, prompt digest, token counts, thinking summary)
   - paired `tool_use` + `tool_result` events (cancellation results synthesized for any failed/interrupted call)
   - `outbox` rows for side effects
   - `state_conversations` phase + last_seq + snapshot if due
7. If the turn requested a wakeup or awaits approval, phase moves to `awaiting_*`; lease released.

**Outbox relay** (separate process, no LLM involvement):

```sql
CREATE TABLE state_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,   -- framework-minted
    conversation_id TEXT NOT NULL,
    effect_kind     TEXT NOT NULL,          -- 'transport.send' | 'schedule.wakeup' | ...
    payload         BLOB NOT NULL,
    status          TEXT NOT NULL,          -- 'pending' | 'in_flight' | 'delivered' | 'failed'
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,                -- exponential backoff
    last_error      TEXT,
    enqueued_seq    INTEGER NOT NULL
);

CREATE TABLE state_inbox (
    idempotency_key TEXT PRIMARY KEY,
    received_at     INTEGER NOT NULL
);
```

Transport plugins record delivered IDs in `state_inbox`; the relay skips keys already seen. At-least-once delivery + consumer-side dedup = effectively exactly-once at the sink.

### 2.5 Planner/executor separation (the lethal-trifecta firewall)

Prompt injection is **not solved** at the model level. Every defense evaluated against static benchmarks has been adaptively bypassed; the Oct 2025 "Attacker Moves Second" paper reports 100% attack success in human red-team competition across 12 recent defenses. execlaw's architectural mitigation:

For any conversation whose kind is `ExternalWithOutsider`, or any turn ingesting untrusted content in higher-trust conversations, the runner executes **two model roles**:

- **Planner.** Inputs: trusted system prompt, conversation metadata (kind, controller id, capability set), structured summaries of untrusted content (not raw text), scratchpad. Holds all available tools. Produces a plan as parameterized tool calls with placeholder slots where untrusted content is needed.
- **Executor.** Inputs: raw untrusted content (spotlighted — delimited with a random per-conversation token, tagged as untrusted data, per Microsoft's Spotlighting). Holds **no tools**. Produces values to fill the planner's placeholders.

The control plane — not the model — invokes tools. Each placeholder-filled tool call is re-checked against the turn's capability set. Values from the executor carry a **tainted** provenance tag; the policy engine refuses to pass tainted values into capability-sensitive sinks (external transport sends outside the current conversation, secret reads, shared-state writes) unless the conversation is `ControllerDM` with an explicit trust assertion.

This is CaMeL restated for execlaw's trust classes. It does not prevent injection; it contains blast radius to what the conversation's capabilities already permit.

### 2.6 Participants, trust levels, and derived conversation kinds

execlaw is **participant-aware**. Trust is an attribute of each *principal*, not a property bolted onto the conversation. A conversation's effective policy is **derived** from its participants — not set as a static tag. The same person reaching the agent via Signal, email, or voice is one trust subject, and a decision made once applies everywhere.

#### Trust ladder

```rust
enum TrustLevel {
    Controller,                 // The admin. Cryptographic-key bound (§7.1).
    Delegated {                 // Explicit, time-bounded grant from the controller.
        by: PrincipalId,
        scope: CapabilityScope,
        expires_at: Option<Instant>,
    },
    KnownTrusted {              // Identity resolved by a plugin, controller-approved.
        resolvers: Vec<PluginId>,
        approved_by: PrincipalId,
        approved_at: Instant,
    },
    KnownLimited {              // Identity resolved; controller approved with scope.
        resolvers: Vec<PluginId>,
        allowed_topics: Vec<TopicTag>,
        allowed_tools: Option<Vec<ToolId>>,
    },
    UnknownPending {            // First-time contact; awaiting controller decision (§2.14).
        first_seen: Instant,
        notification_event_seq: Option<u64>,
    },
    Blocked {             // Controller blocked. Universal state — applies
                                //   to previously-unknown contacts AND to
                                //   previously-trusted principals the controller
                                //   later decides to block.
        blocked_by: PrincipalId,
        blocked_at: Instant,
        reason: Option<String>,
    },
}

struct Principal {
    id: PrincipalId,
    identifiers: Vec<Identifier>,          // (transport, handle) pairs across channels
    trust_level: TrustLevel,
    resolved_by: Vec<PluginId>,            // Which identity-provider plugins matched
    metadata: HashMap<String, Value>,      // Name, tags, first_seen (from plugins)
    first_seen: Instant,
    last_seen: Option<Instant>,
    controller_notes: Option<String>,      // UI-editable free text
}
```

Trust decisions persist in the SQLite `principals` table. Every inbound identifier resolves to one stable Principal via the identity-resolution pipeline (§2.14).

#### Conversation kind is derived, not assigned

```rust
enum ConversationKind {
    ControllerDM,               // 1:1 with Controller (or Delegated acting as controller)
    GroupWithControllerPresent, // Controller in participant set, no outsiders
    GroupWithControllerAbsent,  // Trusted members only, no controller in the room
    ExternalWithOutsider,       // 1:1 with a non-controller, non-trusted principal
    MixedTrust,                 // Group containing both trusted and outsider participants
}

struct Conversation {
    id: ConversationId,
    transport: TransportRef,
    participants: Vec<PrincipalId>,
    kind: ConversationKind,                // Derived from participant composition
    effective_trust: TrustLevel,           // Minimum trust across readers (most restrictive)
    modality: Modality,                    // Text | Voice
    planner_executor_required: bool,       // True if any reader has trust < KnownTrusted
    last_controller_activity: Option<Instant>,
}
```

Kind is a function of participants:

- **1 participant, Controller** → `ControllerDM`
- **≥2 participants, includes Controller, no outsiders** → `GroupWithControllerPresent`
- **≥2 participants, all KnownTrusted+ but no Controller** → `GroupWithControllerAbsent`
- **1 participant, non-trusted** → `ExternalWithOutsider`
- **≥2 participants, mix of trusted and non-trusted** → `MixedTrust` (new — most restrictive policy wins)

#### Effective policy inputs, per turn

On every turn, the policy engine (§7.3) receives:

- `effective_trust` — minimum trust across principals who can read the message
- `sender_trust` — trust level of the principal whose message triggered this turn
- `addressee_trust` — trust level the agent's reply is aimed at (default: the sender; for broadcasts: the minimum in the room)

Rules match on these directly. The `ConversationKind` tag is a useful shorthand for UI and for default rules, but fine-grained decisions key on trust values.

#### Controller presence and sideband routing

- **Controller presence** in a group is inferred from recent activity + explicit transport hints (read receipts, typing indicators). Never a static flag.
- **Controller absent + agent needs guidance** → `ask_controller(question, urgency)` creates a sideband conversation over a *different* transport than the originating one (§2.11).
- **`ExternalWithOutsider` and `MixedTrust` inherit** default-deny semantics, Rule of Two, planner/executor split, and rate limits. The minimum trust in the room sets the floor.

#### Outsider / `UnknownPending` defaults (default-deny)

- Reply on the current transport only
- Rate-limited per principal (tool-call and token budgets)
- No outbound to new principals
- No writes to shared state
- No future wakeups without approval
- No subagent fan-out (peer-agent privilege escalation is a documented jailbreak vector)

#### Rule of Two (§2.2 axiom #9)

Per turn: `untrusted_input_in_turn + accesses_sensitive_data + produces_external_effect ≤ 2`. "Untrusted input" is any message from `UnknownPending`, `Blocked`, or (configurably) `KnownLimited` when the topic is outside the allowed set. The third property forces HITL.

#### Threads, the controller-thread merge, and inbound conversation resolution

A **thread** is execlaw's user-facing name for a `ConversationId` — one append-only event log, one chat context. (The word *session* is reserved for JWT auth state; chat context is always *thread*.)

UI channels (web SPA, eventual mobile app) mint a fresh `ConversationId` whenever the user clicks "new chat." Non-UI channels (Signal, email, voice) have no such affordance — a new message just continues the existing thread. Plugin-side code must therefore resolve every inbound message to a `ConversationId` deterministically.

**`transport_conversations` table** persists the mapping:

```sql
CREATE TABLE transport_conversations (
    plugin_id          TEXT    NOT NULL,
    transport_handle   TEXT    NOT NULL,
    principal_id       TEXT    NOT NULL,
    conversation_id    TEXT    NOT NULL,
    is_current         INTEGER NOT NULL DEFAULT 1,
    last_message_at    INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, transport_handle, principal_id, conversation_id)
);
```

**`ConversationResolver::resolve_or_mint(plugin_id, transport_handle, principal_id, idle_timeout_ms)`** is the helper every transport plugin calls on inbound:

1. **Controller short-circuit** — if the resolved principal is the Controller, ALWAYS return the fixed `controller-thread:<controller_principal_id>` ConversationId regardless of the originating channel. The controller has exactly one DM with the agent.
2. Otherwise look up the `is_current = 1` row for the triple. If found and `now - last_message_at < idle_timeout_ms`, return its `conversation_id`.
3. Else mark the old row's `is_current = 0`, mint a new `ConversationId`, insert a fresh row, return the new id.

Default `idle_timeout_ms` per transport tier:

| Transport | Default | Rationale |
|---|---|---|
| Web / mobile UI | n/a (resolver not called — UI mints explicitly) | |
| Signal-like long-thread | 24 h | Continuous chat stays one thread; a 24-hour gap rotates |
| Email | none (every reply continues the thread) | Threading-by-subject is the user's responsibility |
| Voice | ~5 min | Call ends → next call is a new thread |
| SMS | 4 h | Same as Signal but tighter (SMS volume is bursty) |

Old conversation rows stay linked to their `principal_id` so the UI can show "previous threads with Aunt Marge" without losing history.

**Controller-thread merge** is the load-bearing UX consequence: every event payload carries a `channel_origin` field (`"web"` / `"signal"` / `"email"` / `"voice"` …). The SPA renders one pinned **Control thread** that aggregates every Controller message regardless of channel; per-message channel icons let the controller see at a glance which transport delivered each line.

**Conversation metadata extensions** (migration 0006 — landed as Phase-6 pre-flight alongside `ConversationResolver` and `EphemeralSweeper`; migration 0005 was claimed by the `users` table that landed earlier in the same pre-flight pass):

```sql
ALTER TABLE state_conversations ADD COLUMN display_name TEXT;
ALTER TABLE state_conversations ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE state_conversations ADD COLUMN is_ephemeral INTEGER NOT NULL DEFAULT 0;
ALTER TABLE state_conversations ADD COLUMN ephemeral_expires_at INTEGER;
```

- `display_name` — the LLM-generated 3-word thread title, written via the agent tool `set_thread_name(name)`. For external groups, set to the transport-supplied group name. For the controller thread, hard-coded to "Control thread."
- `is_pinned` — surfaces the Control thread at the top of the SPA's thread list.
- `is_ephemeral` — incognito threads. Events ARE persisted during the conversation (so crash recovery still works) but get DELETEd by the **`EphemeralSweeper`** task once `now > ephemeral_expires_at`. Audit posture: the conversation row stays with `last_seq = 0` after purge so reports can still show "N incognito threads existed but their content was purged." `execlaw replay` skips ephemeral conversations after expiry.

The sweeper runs every ~5 min in the same tokio task pool as the outbox drain. Default new-thread incognito expiry is 1 h; the UI's incognito toggle accepts an override.

### 2.7 Four memory layers

Every surveyed production system converged on this stack. execlaw ships it thin:

| Layer | Storage | Scope | Purpose |
|---|---|---|---|
| **Transcript** | `state_events` | Per conversation | Raw history. Source of truth. |
| **Scratchpad** | `state_conversations.scratchpad_blob`, exposed to the LLM as function-call tools `read_scratchpad()` / `write_scratchpad(content)` | Per conversation | Agent's working notes, plan, "what I've done / what's next." Crucial for long-horizon work. |
| **Summary / compaction** | Rust-side compaction pass runs when context nears budget; explicit summaries the agent can write via `write_scratchpad` | Per conversation | Older turns summarize when context hits ~60% of budget; raw transcript stays in `state_events`. |
| **Long-term memory** | `memory_entries(scope, trust_class, key, value_blob, ttl)` table, exposed to the LLM as `read_memory(scope, key)` / `write_memory(scope, key, value)` / `list_memory(scope, prefix)` function-call tools | **Keyed by trust class** | Facts, preferences, prior decisions. An `ExternalWithOutsider` conversation cannot retrieve `Controller`-scoped memories — enforced at the tool-shim layer by the policy engine. |

**Everything is our own.** No vendor SDK, no `memory_20250818`-style provider-specific tool schema. The LLM sees ordinary OpenAI function-call tools implemented in the `runner-local` crate; the tool handlers read/write the SQLite tables above. Any OpenAI-compatible model with function-calling support (Qwen included) can use them.

**Compaction** is a Rust-side pass: when the composed prompt approaches the model's context window (threshold in `config_runtime_settings`), the runner calls a summarization tool (same model, different prompt) to collapse older turn pairs into a `compaction_summary` row and replaces those turns in the prompt with the summary. The raw transcript stays immutable in `state_events` for forensic replay.

**Safety.** Long-term memory retrieval is policy-gated: the tool shim checks the current conversation's trust class against the memory row's `trust_class` before returning any value. `KnownLimited` principals only see memories tagged for their allowed topics. `Blocked` principals never get a chance to call the memory tools at all (the conversation doesn't reach the agent).

### 2.8 The runner: self-hosted, OpenAI-compatible, no vendor SDK

**One runner crate, many runner containers.** `runner-local` is the Rust binary; the container manager spawns **one hot runner container per active conversation** (matching selfhosted-claw's HotRunnerPool isolation model, correcting its state problems). Each runner talks to the configured OpenAI-compatible inference backend — default on the dev target is `service-vllm` hosting `QuantTrio/Qwen3.5-27B-AWQ`. For voice conversations the LLM stage is one component inside the `voice-pipeline` graph (§2.13.2); for text it drives the turn directly.

#### Per-conversation container isolation (learned from selfhosted-claw, improved)

Selfhosted-claw's `HotRunnerPool` got isolation right: each group gets its own container, so data from conversation A can never leak into conversation B via shared memory, file descriptors, or errant tool-call routing. execlaw keeps that. But selfhosted-claw's runners were *stateful mid-run* — a crash mid-turn dropped work and left cursors out of sync. execlaw fixes that: **runner containers are stateless against the event log.**

Lifecycle:
- **Spawn on demand.** When a conversation has a pending turn and no warm runner is pinned to it, the container manager spawns one. Hydration cost: read the conversation's events-since-snapshot from SQLite (~10-50ms), reconstruct context in memory. No persistent runner-side state.
- **Stay warm per conversation.** After the turn finishes, the runner stays up (idle) for a configurable window (default ~10 min). Subsequent turns in the same conversation reuse it — no cold-start.
- **Reap on idle or resource pressure.** Past the idle window, or under memory pressure, the container manager sends `SIGTERM`; the runner drains any in-flight work and exits. Any pending turn re-spawns a fresh runner.
- **Crash-respawn.** If a runner dies (OOM, panic, docker kill), the container manager detects via the bollard event stream; any open `tool_use` gets a synthesized cancellation `tool_result`; a new runner spawns and resumes from the event log. No work is lost; no work is double-executed (the turn-as-transaction invariant guarantees this, §2.4).
- **Per-runner capability scoping.** The runner's capability token (§7.2) is scoped to the single `conversation_id` it serves. Even if a runner is compromised via prompt injection, its API calls can only affect *its* conversation — attempts to read or write other conversations' event-log rows, memories, or outbox entries are rejected by the policy engine before any side-effect happens. This is the hardened version of "every group gets its own container": isolation by containers *and* by capability.

Improvements over selfhosted-claw's pool:
- Stateless against the event log (no mid-run state to lose on crash).
- Structured capability-token-authenticated IPC to the control plane — replaces selfhosted-claw's stdout-marker parsing (fragile to concurrent logging).
- Transactional turn commit (§2.4) — a crashed runner's partial work either fully commits or cleanly rolls back via cancellation results.
- Uniform lifecycle in the `container-manager` crate instead of a separate `HotRunnerPool` class.
- Per-runner resource limits (CPU/memory) declared via Docker, enforced by the kernel.
- Minimal runner image (axiom #12): Rust binary + shared-lib deps + CA certs, nothing else. No vendor SDKs, no model weights, no Python runtime — all heavy inference lives in the `service-*` backends.

Switching modality mid-session keeps the same pinned runner, same session, same memory — the `voice-pipeline` crate just wraps around it for voice turns.

**No cloud LLMs, ever** (§0 axiom #1). The runner targets a *local* OpenAI-compatible endpoint only. Any proposal to add a cloud-bridge adapter is rejected. Models must be served by local inference servers (vLLM, OpenArc, llama.cpp, Ollama). This is strict, not aspirational.

**Per-turn invocation inputs:**

- `session_id` (deterministic, derived from `conversation_id`)
- System prompt assembled by `runner-local` (conversation kind, trust class, modality, controller info, capability manifest)
- **Function-call tool definitions** from installed plugins (OpenAI-standard `tools` array), filtered by `latency: low` in voice mode
- Capability token (EdDSA-signed, bound to this turn, expiring) — carried on every tool-result return trip
- Planner/executor mode flag for untrusted turns
- Modality flag (`Text` / `Voice`)

**Agent-loop features implemented in the `runner-local` crate — every one of them ours:**

- **Sessions.** The conversation's event log *is* the session. The runner hydrates its context from `state_events` since the last snapshot on each invocation; there's no separate session store to sync.
- **Memory.** Function-call tools `read_memory` / `write_memory` / `list_memory` backed by the `memory_entries` SQLite table (§2.7). Trust-class scoping enforced at the tool-shim layer by the policy engine.
- **Checkpointing / undo.** Event-log snapshot mechanism (§2.3). A `/undo` commits a new user message with a `rewind_to: <seq>` intent.
- **Reasoning.** If the configured model (e.g. certain Qwen3 builds) has a native reasoning mode, the runner enables it for high-stakes turns — capability grants, planner plan-generation, approval routing — via the model's own thinking-mode flag in the request. If the model doesn't support reasoning mode, the runner just uses a more deliberate prompt for those turns. No separate reasoning deployment, no model swap at runtime.
- **Tool search.** When a conversation's plugin inventory exceeds ~20 tools, the runner indexes tool descriptions locally and offers a `search_tools(query)` function-call so the model can fetch relevant tools on demand instead of dumping the full list into context.
- **Streaming + interrupt.** SSE streaming from the inference server; on user cancel the control plane commits cancellation `tool_result`s for any open `tool_use` before releasing the lease.

**What we explicitly do not do:**
- Import any cloud-vendor SDK in any core crate (Anthropic, OpenAI, Gemini). Period.
- Use any vendor-proprietary tool schema (e.g. Anthropic's `memory_20250818` or equivalents). All tools are plain OpenAI function-call shape.
- Scatter raw HTTP calls across the codebase (selfhosted-claw's `fetch()` anti-pattern). The runner crate owns prompt construction and the inference client end-to-end.

### 2.9 Subagents: on by default, deep research is the flagship

Subagents are **enabled by default** — the primary agent can spawn background workers for parallel research, guardrail classification, or deferred deep reasoning. Policy (§7.3) gates *which* sub-agent kinds can be spawned in which conversations; the default is permissive for trusted conversations and restrictive for untrusted ones.

Cost honesty: multi-agent fan-out uses more tokens than single-agent (Anthropic's own figure: ~15× for research-style tasks). Acceptable because the alternative — primary agent serially reading a dozen sources — is slower *and* worse. The research payoff dwarfs the token overhead for the right use cases.

**Three endorsed sub-agent patterns:**

1. **Guardrail subagents** — one-shot, boolean-output, parallel with the main turn. Input-risk classifier on ingress; output-policy-check on draft replies. Failure doesn't cascade; the main turn continues with the guardrail result as metadata.
2. **Deep research fan-out** — the flagship use case. See §2.9.1 below.
3. **Primary-to-deep-model escalation** (§2.13). A fast primary model (voice path) spawns a sub-agent against the main reasoning model for hard problems; synthesizes a short answer back to the primary.

**Per-conversation-kind defaults:**

| Conversation kind | Guardrail | Research | Deep escalation |
|---|---|---|---|
| `ControllerDM` | Auto | Auto (on any research-shaped request) | Auto |
| `GroupWithControllerPresent` | Auto | Auto, capped at max_subagents=3 | Auto |
| `GroupWithControllerAbsent` | Auto | Requires approval (sideband) | Auto, small-scope |
| `MixedTrust` | Auto | Requires approval | Requires approval |
| `ExternalWithOutsider` | Auto | **Never** | **Never** |

Untrusted conversations get guardrails only — research fan-out and deep escalation are disabled because **peer-agent privilege escalation is a documented jailbreak vector** (a subagent spawned from an untrusted prompt can be coerced into actions the primary would refuse).

#### 2.9.1 Deep research — mirroring selfhosted-claw's proven pattern

execlaw's deep research mirrors the structure of [`selfhosted-claw/src/research/`](../selfhosted-claw/src/research/) — the controller is happy with this architecture and execlaw should preserve its shape. The port hardens a few details (container isolation, capability scoping, event-log integration) without rewriting what works.

**Tools the primary agent calls:**

| Tool | Purpose |
|---|---|
| `deep_research_start(prompt, [followup_answers])` | Spawn an async research job. Returns immediately with an `ack_text` the agent relays to the user + a `research_id`. |
| `deep_research_status(research_id)` | Check status/progress mid-job. |
| `deep_research_cancel(research_id)` | Stop a running job; finalize gracefully. |
| `deep_research_answer_followup(research_id, answers)` | Provide clarifications the research phase asked for. |

`deep_research_start` is **fire-and-forget**: the agent gets the ack immediately, relays it to the user ("Starting research on X, I'll send the report when it's ready"), and the conversation turn commits. The actual research runs as a background job.

**Background job model:**
- A `research_job` row in SQLite (mirrors selfhosted-claw's `cp_actions` with `type='deep_research'`), status `queued → executing → succeeded|failed|cancelled`, plus a `progress_json` blob holding the full `ResearchProgress` state (prompt, plan, sources, summaries, image refs, budget counters, deadline).
- The container manager spawns a **dedicated research runner container** (`runner-local` with the narrower research tool-set) scoped to the research job. Isolation + capability token just like regular runners (§2.8). The capability token grants: `search_web`, `fetch_url`, `read_pdf`, `describe_image`, `write_research_note`, `write_artifact`. It does **not** grant: read/write of the parent conversation's memory, external transport sends, shared-state writes.
- Trust cap: research-runner capabilities are capped at `KnownTrusted` even when the parent is `Controller` (same jailbreak-defense logic as before).
- Crash safety: job state is in `progress_json`; each phase checkpoint updates it; a crashed research-runner respawns and resumes from the last checkpoint.

**Phases (port of selfhosted-claw's `DeepResearchExecutor`):**

1. **SCOPE** — LLM produces a `ResearchPlan`: objectives, sections, subqueries, optional `followup_questions` for clarification. If follow-ups are needed, job parks in `awaiting_followup` and the agent surfaces the questions to the user.
2. **SEARCH** — Execute subqueries via the provider chain with budget caps (`maxSearchCallsPerJob`, default 30).
3. **FETCH** — For each promising result URL, fetch + convert HTML to markdown; extract `og:image` URLs as candidate images. Budget cap `maxFetchesPerJob`, default 40.
4. **SUMMARIZE** — Per-source LLM summarization producing `key_points` + quotes with citations.
5. **IMAGE PROCESSING** — see §2.9.2.
6. **DRAFT** — LLM writes the report body section-by-section with inline image references.
7. **EXECUTIVE SUMMARY** — LLM writes top-of-report bullets after seeing the drafted body.
8. **PDF** — `plugin-research-pdf` renders markdown + embedded images to a PDF artifact; stored at `~/.execlaw/artifacts/research/<job_id>.pdf` with a row in `state_artifacts`.
9. **DELIVER** — the research-runner calls `channel.send_attachment(conversation_id, pdf_path)` via the outbox; the agent's next prompt includes `[Research <job_id> complete, PDF delivered]` so it can acknowledge to the user.

**Search provider chain with circuit breaker** (port of `ChainProvider`):
- `plugin-search-exa`, `plugin-search-brave`, `plugin-search-duckduckgo` — each implements the `ResearchProvider` trait (`search(query, opts)` + `fetch(url)`).
- `ChainProvider` composes them with per-provider circuit breakers: after N consecutive failures a provider is skipped for a cooldown window.
- DuckDuckGo is always in the chain as a last-resort (no API key required).
- **Quota degradation** (port of `quotaMode = 'ddg-only'`): daily API call ceiling tracked in `config_research_quota`; when exceeded, new jobs auto-downgrade to DDG-only and the ack_text warns the user. Jobs run; they don't get rejected.

**Budget controls** (defaults mirror selfhosted-claw):
- `maxRuntimeMs` = 20 minutes (hard deadline)
- `maxConcurrency` = 2 parallel subqueries per job
- `maxSearchCallsPerJob` = 30
- `maxFetchesPerJob` = 40
- `dailyProviderQuota` = 2500 (system-wide)
- `progressPingIntervalMs` = 60s — how often the research-runner sends status events
- `maxFollowups` = 2 — limit clarification rounds
- All editable in `config_runtime_settings`.

**Progress updates.** Every `progressPingIntervalMs`, the research-runner commits a `research_progress_updated` event with a short status line ("searching: 3 queries done, 12 sources fetched, 4 summarized"). The admin UI shows these live; the agent may optionally relay them to the user via the originating conversation.

**Cancellation.** `deep_research_cancel(research_id)` sets job status to `cancelled`; the research-runner checks the deadline/status on each phase boundary and exits gracefully with whatever partial artifacts it produced.

**Plugins to port from selfhosted-claw** (Phase 2):
- `plugin-search-exa`, `plugin-search-brave`, `plugin-search-duckduckgo` — search providers
- `plugin-url-fetch` — HTTP fetch + HTML-to-markdown
- `plugin-research-vision` — image classification (see §2.9.2)
- `plugin-research-pdf` — markdown-to-PDF rendering
- `plugin-research-orchestrator` — the phase executor (SCOPE → … → DELIVER)

#### 2.9.2 Image handling — research images and inbound/outbound conversation images

Also ported from selfhosted-claw (`src/research/images.ts`, `src/research/vision.ts`, plus channel `sendAttachment` flow).

**Images inside research reports** (two-stage filter + rank):

1. **Heuristic filter** (cheap, CPU-only): reject URLs matching `/logo`, `/icon`, `favicon`, `gravatar`, `placeholder`, etc.; reject images below 400 px long-edge or below 120k px² area; reject extreme aspect ratios (>4:1 banners); compute Shannon entropy of the pixel distribution, reject below ~4.5 bits.
2. **Vision filter** (optional, LLM-based): `describe_image(image_bytes)` calls a vision-capable model endpoint (ideally a vision-capable Qwen variant on the local inference server — the `service-vllm` / `service-openarc` deployment if the chosen model supports images). Returns `{ kind: chart|diagram|screenshot|map|photo|portrait|logo|text|other, is_informative: bool, description: string }`. Uninformative (logos, stock photos, decorative covers) get rejected. Timeout 25s; graceful fallback to heuristic-only.
3. **Ranking:** entropy × √(area). Top 1 per source kept; max 6 per report. Compressed to JPEG within 120 KB budget.

**Inbound conversation images** (user sends an image via any transport):
- Transport plugin receives the attachment, stores blob under `~/.execlaw/blobs/inbound/<sha256>.<ext>` with a row in `state_attachments(id, conversation_id, mime_type, path, sha256, received_at)`.
- Attachment reference surfaces in the agent's context as `[Attachment: image/jpeg, sha256=abc..., id=att-xyz]`.
- The agent can call `describe_image(attachment_id)` — same vision endpoint as research — to have the model actually "see" it. This is opt-in per-turn; we don't silently feed every inbound image to a vision model.
- Blobs follow the §10-equivalent retention policy: transcripts-only default, operator can enable long-term attachment retention per conversation.

**Outbound conversation images** (agent sends an image):
- Agent calls `send_attachment(path_or_ref, caption?)` as a tool; path resolves to a file inside the conversation's `groups/<folder>/` workspace or a newly-created blob.
- Transport plugin's `send_attachment(conversation_id, mime_type, path)` handles the channel-specific delivery (Signal binary attachment, email attachment, webchat CDN upload, voice not applicable).
- Same outbox + idempotency key semantics as regular sends.

**Vision model deployment.** Uses whichever local model the operator configured for the Standard deployment, *if* it's vision-capable. Many Qwen-VL variants exist; if the chosen Qwen3.5-27B-AWQ build supports vision, it serves double-duty. If not, the operator installs a separate `service-vllm-vision` deployment with a vision-capable model (e.g., Qwen2.5-VL-7B or similar) and sets it as the backend for the `describe_image` tool. Still local, still self-hosted.

**Safety.** Inbound images from untrusted contacts (`ExternalWithOutsider`) are still bound by policy — `describe_image` on such attachments is allowed but its output is treated as tainted data (§7.2) and carries the same capability restrictions as STT transcripts from the same source. Images from a `Blocked` principal are dropped at the transport layer and never stored.

### 2.10 Interruption and resumption

Interruption is the common case for day-spanning conversations on flaky transports. Protocol:

- **User cancels mid-turn** (UI button or `cancel` message in transport): control plane signals runner to interrupt; runner closes the in-flight SSE stream from the inference server and drops any unprocessed tokens; control plane commits the turn so far with synthesized cancellation `tool_result`s for open `tool_use` blocks; phase → `idle`.
- **Control plane crash mid-turn**: on restart, scan `state_conversations` for non-idle phases with stale leases (`lease_expires < now`). For each, load events-since-snapshot; if tail is a `tool_use` without `tool_result`, commit a cancellation result with reason "control plane restart"; phase → `idle`. Scheduler picks up any pending wakeup or user message.
- **Runner crash mid-turn**: worker detects container exit via bollard events; commits cancellation results for open tool uses; returns lease.
- **Transport drops** (Signal disconnect, voice call ends): transport plugin records disconnect as a `state_events` row. If the agent was awaiting a user response, phase → `awaiting_reconnect`; a wakeup is scheduled to give up after N minutes.

Scheduled wakeups are just resume events (axiom #7): wakeup fires → event appended → worker picks up → runs a turn with the wakeup note as context. One code path.

### 2.11 Approvals and HITL

Approval gates at the **tool-call layer**, not the task layer. Three verbs: `approve`, `edit` (controller modifies tool args), `reject`.

Policy declares per-tool approval mode:
- `off` — no approval (safe tools in trusted conversations)
- `required` — always gate (payments, mass notifications, irreversible deletes)
- `confidence_gated(threshold)` — gate if model confidence is below threshold **AND** a policy predicate marks the operation sensitive. Never let the model alone decide whether it needs approval.

Approval dispatch:

1. Turn commit includes `awaiting_approval` event; phase → `awaiting_approval`.
2. Control plane mints a short-lived approval token, serializes the full tool-call proposal (args + rationale + capability snapshot), and dispatches a notification to the controller **via a sideband transport — deliberately not the originating transport**, which might be the attacker. Default sideband: ControllerDM over Signal; fallback: email, web-chat.
3. Controller responds with a signed message carrying the approval token + verb.
4. Response recorded as `approval` event; phase → `thinking`; a new turn proceeds or aborts based on the verb.

Every approval decision (who, when, which tool, verb, edits) is a first-class event. This is the audit trail.

### 2.12 Observability and evaluation

**Keep it simple — mirror selfhosted-claw's logging.** No OpenTelemetry, no OpenInference, no Langfuse/Phoenix — those added dep bloat for minimal benefit on a single-operator self-hosted system. Two logging streams, both local:

1. **Structured runtime logs via `tracing` → JSONL + SQLite `log_*` tables.** Same pattern selfhosted-claw uses today: JSON lines for tailability + SQLite for querying/filtering. Admin UI has a log viewer with filter by level/plugin/conversation/time window. Retention is a config setting; `execlaw logs vacuum` prunes on demand.

2. **Forensic event log** (`state_events`) for replay and audit. Unsampled, append-only, sufficient to deterministically reconstruct exactly what the model saw at any turn. `execlaw replay <conversation_id> --at <seq>` rebuilds the prompt, capability set, and policy decisions for that turn — for postmortems that don't depend on re-running against a drifted model.

**Evaluation:**
- `eval_flagged` table — controller ratings, user retries, explicit complaints tag event ranges.
- Nightly LLM-judge over a rolling sample. Rubric includes **trust-class compliance** (did an `ExternalWithOutsider` attempt a privileged memory lookup? did `GroupWithControllerAbsent` invoke a controller-only tool without approval? did any turn violate Rule of Two?).
- Flagged traces feed a regression set run before any prompt/model change.
- No bespoke eval framework; `pytest` + a judge call is enough.

### 2.13 Modality shapes the turn — two paths, one event log

A **turn** in execlaw is a *commit unit* — a span that commits atomically to the event log — not a request/response boundary. Two first-class modalities share the same event log, policy engine, capability tokens, outbox, memory layers, conversation types, and runner (`runner-local`): **text** and **voice**. What differs is the pipeline structure and commit cadence.

| Modality | Pipeline | Default deployment on the dev target (hybrid nvidia + Intel Arc) | Commit cadence | Critical-path latency |
|---|---|---|---|---|
| **Text (Standard)** | LLM only | `service-vllm` on nvidia, `QuantTrio/Qwen3.5-27B-AWQ` | One commit per turn (§2.4) | User-tolerant |
| **Voice** | VAD → streaming STT → streaming LLM → streaming TTS, with echo-cancellation and barge-in | Hybrid: STT (`service-whisper` on Intel Arc via OpenVINO GenAI) + LLM (`service-vllm` on nvidia + Qwen) + TTS (`service-kokoro` — OpenVINO on Intel Arc or ONNX+CUDA on nvidia) + VAD/turn-detector/AEC on CPU | Streaming events per stage + turn-boundary commit | ~700-1100ms EoS → first audio; ≤200ms barge-in halt |
| **Reasoning (deep sub-agent)** | LLM only (invoked as a tool from either modality) | Same `service-vllm` + `QuantTrio/Qwen3.5-27B-AWQ` as Standard — if the model supports native reasoning mode, enabled for deep turns; otherwise just a more deliberate prompt | One commit per sub-agent turn | 2-5s acceptable |

Every row is a **configurable RunnerDeployment** (§5.4), not hardcoded. Voice is a *distributed pipeline* where each stage can run on different hardware — a natural fit for the hybrid nvidia + Intel setup, where the voice-peripheral stack (STT/TTS) runs on Intel Arc and the heavyweight LLM runs on nvidia.

**One runner, not two.** There is no separate `runner-realtime`. Native-audio full-duplex (GPT-4o-Realtime / Gemini-Live style, where the model itself emits audio tokens bidirectionally) is not achievable self-hosted with current OSS — every OSS voice-agent framework in 2025-2026 (Pipecat, LiveKit Agents, Vocode) uses the cascaded STT → LLM → TTS pattern and engineers around its latency. execlaw does the same and documents the trade-off honestly in §2.13.5.

#### 2.13.1 Text turn (baseline)

As described in §2.4. User message → model turn → zero or more `tool_use`/`tool_result` pairs → turn finalized → one commit per turn. The simplest case.

#### 2.13.2 Voice pipeline — streaming STT → streaming LLM → streaming TTS

Modeled after Pipecat (Daily) and LiveKit Agents — the two production-dominant open-source voice-agent frameworks — implemented as a Rust Tokio graph of async channels with a **two-lane priority scheme**: a bounded **data lane** for audio / text / tokens, and an unbounded **system lane** for interruption and turn-boundary events that must preempt the data lane to reach every stage within milliseconds.

**Frame vocabulary** (adopted from Pipecat):

*System lane (preemptive):* `UserStartedSpeaking`, `UserStoppedSpeaking`, `Interruption`, `Error`.

*Data lane (ordered):* `AudioInChunk`, `TranscriptPartial`, `TranscriptFinal`, `LLMTokenDelta`, `LLMResponseStart/End`, `TTSTextChunk`, `AudioOutChunk`.

*Control:* `TTSStarted`, `TTSStopped`, `ToolUse`, `ToolResult`.

Every processor runs a `tokio::select!` that polls the system lane first. On `Interruption`, all downstream processors drop data-lane state and prepare for the next user utterance.

**Latency budget (target ≤1.1s EoS → first audio, ≤200ms barge-in halt):**

| Stage | Warm | Notes |
|---|---|---|
| VAD end-of-speech detection | ~30-100ms | Silero VAD ONNX on CPU, ~1ms per 30ms frame |
| Endpoint detection (punctuation + dynamic silence) | ~0ms | STT transcript already has punctuation; heuristic reads it, no extra model |
| Streaming STT (Whisper) | 200-400ms | faster-whisper on CUDA OR OpenVINO GenAI WhisperPipeline on Intel; partials every ~300ms, final on EoT |
| Worker lease + snapshot | 2-10ms | SQLite row lock + warm runner pinned |
| Streaming LLM first-token | 200-500ms | Qwen on nvidia (hybrid default) — depends on model size + prompt length |
| First LLM token → TTS input | 50-150ms | Sentence-boundary buffering; first complete clause flows to TTS |
| Streaming TTS first audio | 100-400ms | Kokoro-82M — ISTFT-GPU on OpenVINO 2025.2 (Intel) or ONNX on CUDA |
| **Total warm** | **~700-1100ms** | |
| **Barge-in halt** | **~100-200ms** | System-lane `Interruption` → TTS-cancel + client-buffer flush |

Non-negotiables: warm pipeline (STT/LLM/TTS all hot, pinned per active call) — cold-start is fatal for voice UX. Separate GPU streams for STT and LLM when both are on the same device (prevents one from blocking the other). All three streaming stages overlap: STT finalizes while LLM is already mid-generation; LLM is still producing tokens while TTS is already speaking earlier sentences.

**Event schema (for SQLite event log + replay)**, adopted from Pipecat's transcript semantics and normalized to execlaw's `state_events` table:

```
voice.session_started       { session_id, conversation_id, stt_model, llm_model, tts_model,
                              sample_rate, vad_config }
voice.session_ended         { session_id, duration_ms, reason }

audio.in_chunk              { session_id, blob_ref, start_ms, end_ms }  -- PCM stored as blob-ref, not inline
vad.speech_started          { session_id, ts_ms }
vad.speech_ended            { session_id, duration_ms }

stt.partial                 { session_id, text, is_stable, audio_range }
stt.final                   { session_id, text, audio_range, confidence }

turn.user_ended             { session_id, source: "vad" | "turn_detector", confidence }

llm.request                 { session_id, prompt_hash, context_turns }
llm.token                   { session_id, text }
llm.response_final          { session_id, text, ttft_ms, total_ms, tokens }
llm.cancelled               { session_id, reason: "barge_in" | "error", partial_text }

tool_use                    { session_id, tool, args }     -- atomic with its tool_result
tool_result                 { session_id, result, error? }

tts.request                 { session_id, text, voice, engine }
tts.first_audio             { session_id, ttfa_ms }
tts.audio_chunk             { session_id, blob_ref, duration_ms }
tts.ended                   { session_id, reason: "completed" | "interrupted" }

interrupt.started           { session_id, source: "user_speech" | "user_cancel" | "net_drop",
                              stt_partial_at_time }
interrupt.rescinded         { session_id, reason: "backchannel", matched_phrase }
interrupt.confirmed         { session_id }
```

Append-only, crash-safe, fully replayable. PCM bytes live in a separate `blobs` table (referenced by `blob_ref`) to keep the main events index cheap. Audio retention per conversation is controlled by policy — transcripts-only by default; full audio opt-in.

Every barge-in leaves a crisp 4-event trail: `interrupt.started` → `llm.cancelled` → `tts.ended(interrupted)` → optional `interrupt.rescinded`. Replay shows exactly why a turn died.

**Component choices** (each stage is its own plugin / in-tree component; each picks its backend based on the RunnerDeployment):

- **VAD: Silero VAD ONNX** on CPU. ~1MB model, sub-millisecond inference per 30ms frame. Universal default in Pipecat / LiveKit / sherpa-onnx. Config: threshold 0.6, min_speech_duration 150ms, min_silence_duration 300-500ms, speech_pad 100-200ms.
- **STT: Whisper (whisper-small.en int8)**, two backends behind one trait:
  - CUDA path: **faster-whisper** (CTranslate2)
  - Intel path: **OpenVINO GenAI WhisperPipeline** (chunked long-form with streaming callbacks; the whisper.cpp OpenVINO encoder path only accelerates encoder — use GenAI for the full pipeline)
- **Endpoint detection: punctuation-aware dynamic silence** (no extra model). Whisper emits punctuation in its transcripts; a simple rule-based endpointer reads the latest partial and adjusts the silence timeout: ends with `.`, `?`, `!` → shorten to ~200-300ms (likely end of turn); ends with `,` or no terminator → extend to ~800-1000ms (mid-thought). Silero VAD silence is the baseline; punctuation just tunes the threshold. Cheap, deterministic, no additional inference cost. If false-interruption rate turns out to be a problem in practice, a model-based turn-detector can be added later as a sub-milestone — not a first-ship requirement.
- **LLM: `runner-local`** against the configured inference deployment (Qwen on nvidia is the hybrid-setup default). Streams tokens; buffers at sentence boundaries for TTS handoff.
- **TTS: Kokoro-82M**, the clear winner for the dual-GPU requirement:
  - OpenVINO 2025.2 shipped the ISTFT-GPU operator *specifically for Kokoro*; official `magicunicorn/kokoro-tts-intel` FP16 OpenVINO build runs on Intel Arc with 3-5× CPU speedup
  - ONNX build (`onnx-community/Kokoro-82M-v1.0-ONNX`) on CUDA or CPU via onnxruntime; maintained Rust port (`lucasjinreal/Kokoros`) gives us direct FFI
  - Apache-2.0, 82M params, <1GB weights, 54 voices, TTFA ~100-400ms on consumer GPU, RTF ~0.03 on A100
  - **Default voices shipped:** `bf_emma` (British female) and `am_michael` (American male), selectable per conversation in the UI; operator can pick any of the 54 voices Kokoro ships
  - **Replaces F5TTS**, which has a non-commercial weights license (CC-BY-NC-4.0) that's a blocker, no clean OpenVINO port, and generates whole utterances rather than streaming
- **TTS fallback: Piper** — CPU-first, <50ms TTFA, ONNX-native, MIT. Low-latency floor if Kokoro fails to load on a given backend.
- **Echo cancellation: WebRTC AEC3** (libwebrtc audio-processing module, BSD-licensed). **Mandatory when using speakers** — without AEC, the agent's own TTS output gets picked up by the mic and triggers false barge-ins every utterance. Skippable only with headset / push-to-talk.
- **Semantic endpoint detection (§2.13.3)** stacks on top of silence-based VAD.

Each component is configured per-deployment. On the hybrid dev target: STT and TTS on Intel Arc via OpenVINO; LLM on nvidia via vLLM; VAD / turn-detector / AEC on CPU.

#### 2.13.3 Barge-in and the "false barge-in" rescind

The 200ms halt sequence when user speech interrupts agent speech:

1. Silero VAD detects voice activity (threshold 0.6, min_speech 150ms filters taps/clicks).
2. `Interruption` frame pushed on the **system lane**, bypassing all data-lane queues.
3. **TTS halt:** cancel in-flight synthesis; drop unplayed PCM in the output buffer.
4. **Client playback:** send out-of-band STOP/FLUSH to the audio sink. Stop-and-flush, not drain — drain leaves 100-300ms of trailing audio.
5. **LLM cancel:** both vLLM and CTranslate2 support cancellation tokens. Partial text goes to the event log as `llm.cancelled(partial_text)`; no attempt to salvage.
6. **STT reset:** rolling context reset to the interruption timestamp; begin new transcription.

**False barge-in rescind** (LiveKit pattern, adopted):

Short backchannels like "mm-hmm", "uh huh", "yeah", "ok" shouldn't interrupt the agent. So: fire the `Interruption` frame at VAD-on *immediately*, but delay the actual TTS-halt by ~120ms while STT transcribes. If the STT transcript matches a backchannel allowlist AND the utterance ends within 400ms, *rescind* the interrupt (`interrupt.rescinded` event) and resume TTS from where it paused. Costs 120ms on real interrupts; saves the common case of the user making agreement sounds while the agent is explaining.

#### 2.13.4 Modality-adaptive behavior (voice)

Triggered by `conversation.modality == Voice`:

- **Extended thinking off** — tokens are seconds of silence.
- **Response-length budget** — short sentences, no markdown, no code blocks. Enforced as both a system-prompt constraint and a runner-side hard truncation. Configurable per-voice (the F5TTS-style long-form TTS and Kokoro-style chunked TTS have different sweet spots).
- **Tool-call budget** — max 1-2 per turn, only tools declaring `latency: low` in their manifest. High-latency tools (deep research, long code execution) are hidden from the voice runner's tool list.
- **Context slice** — voice runner sees last N turns + scratchpad + compaction summary, not full transcript. Full transcript stays in `state_events` for forensic replay.
- **Planner/executor split** preserved for `ExternalWithOutsider` voice (speech-to-text + prompt injection is a real attack surface — the spotlighting defense applies to STT transcripts). Executor is the same small model with no tools; the architectural invariant holds.

#### 2.13.5 Dual-runner escalation (same pattern across text and voice)

The escalation pattern (§2.9 case 3) works identically in both modalities:

- **Primary runner** handles the modality — fast path, latency-bounded.
- **Deep runner** spawned on demand for heavy reasoning — the **same local model** (Qwen3.5-27B-AWQ) invoked with native reasoning mode if supported, or a more deliberate system prompt if not. No separate deployment; no model swap; just a different invocation of the same service.

Invocation is a tool call from the primary: `escalate_to_deep_agent(question, context_summary, urgency)`. In voice the primary emits a filler utterance ("one sec, let me think about that") while the deep runner grinds; result returns via `tool_result`; primary synthesizes a short answer. Typical total: 2-5 seconds. Acceptable because the alternative is a hallucination.

`ExternalWithOutsider` conversations allow only guardrail sub-agents — no deep escalation that would broaden capabilities through a richer-context sub-agent.

#### 2.13.6 Honest limits

The self-hosted ceiling in 2026 is cascaded STT→LLM→TTS with aggressive barge-in. True phoneme-level full-duplex (where the model emits audio tokens bidirectionally and can start responding before the user finishes a sentence) requires a native-audio LLM, and the available native-audio models are either cloud-only (GPT-4o-Realtime, Gemini Live) or research-grade with insufficient quality and no cross-GPU portability (Moshi, early Ultravox builds). Every OSS production voice agent in 2025-2026 uses the cascaded pattern and engineers around its latency — and so do we.

The latency delta we're accepting:
- Best-case self-hosted cascaded (our target): ~700-1100ms EoS → first audio
- Best-case cloud native-audio (GPT-4o-Realtime et al): ~300-500ms
- The gap is what we're paying for self-hosted + portable + Apache-licensed + privacy-preserving. We judge it worth it.

**Safety note for voice specifically.** STT transcripts are untrusted input like any other inbound channel — prompt injection via speech is real. The spotlighting defense (§2.5, §7.4) wraps transcripts with a random per-conversation delimiter before the LLM sees them. Planner/executor split applies for `ExternalWithOutsider` voice. Rate limits and anomaly tripwires still apply.

#### 2.13.7 What Phase 13.A-D actually shipped (and what's deferred)

The voice pipeline above is the *target*. Phase 13.A-D landed a working subset; the rest is queued in `docs/voice-followups.md`. This subsection is the reader's source of truth for "what runs today."

**Shipped (Phase 13.A-D, commits `56e4c90` → `72d1f63`):**

- **Wire framing** — SPA's `framePayload` emits `[u32 header_len BE][JSON header][audio payload]`. Header carries `{session, seq, codec, sample_rate, channels, ts_ms}` so future mobile-native + phone-bridge sources can use the same protocol.
- **Server-side jitter buffer** — `crates/server/src/voice_session.rs::VoiceSessionRegistry`. Per-session `BTreeMap`-backed reorder, `MAX_BUFFER_FRAMES = 8`, `SESSION_IDLE_TIMEOUT = 30s`, periodic reaper (`voice_reaper.rs`).
- **STT/TTS HTTP clients** — `voice_clients/whisper.rs` posts WAV to faster-whisper's OpenAI-compat `/v1/audio/transcriptions`; `voice_clients/kokoro.rs` posts JSON to Kokoro-FastAPI's `/v1/audio/speech` and parses raw PCM16 LE. Both have request-shape-validating tests against a hand-rolled TCP fixture.
- **Voice runtime** — `voice_runtime.rs::VoiceRuntime` wires registry chunks → STT → agent callback → TTS → outbound events. Synthesize runs **lock-free** via a take-tts/put-tts swap so other sessions' ingest + barge-in proceed in parallel; cancel epoch is checked at three points so an interrupt at any pipeline stage halts cleanly.
- **WS control protocol** — `events.rs::handle_voice_control` parses `{"op":"voice_stop","session":"…"}` and `{"op":"voice_interrupt","session":"…"}`. The WS handler tracks per-connection owned sessions and drops them on disconnect (no waiting for the 30s reaper). The `voice_stop` spawn is panic-safe via a `Drop`-guard.
- **Backend wizard (Phase 13.B.1)** — `Settings → Backends` exposes a hardware-aware preset picker (Whisper/Kokoro/Piper/vLLM × NVIDIA/Intel Arc/CPU) so the operator clicks a card instead of hand-crafting `model_spec_json`.
- **SPA playback + transcript banner** — `web/src/chat/VoicePlayback.ts` decodes PCM16 from the server through an `AudioContext`; `VoiceStatusBar.tsx` shows the live transcript + Interrupt button + an explicit "VoiceSTT didn't return a transcript" message when the server returns an empty final.

**Locked-decision defaults that ship today** (per `project_locked_decisions_2026_04_23.md`):

- Voice id default: `bf_emma+am_michael` (Kokoro's combined-voice syntax). Migration `0016_personality_voice_id_blend.sql` sets the default-personality row's `voice_id`.
- Push-to-talk only — the SPA's mic button toggles capture; mic-off sends `voice_stop`.

**Explicitly deferred (`docs/voice-followups.md`):**

| Deferral | Status | Why deferred |
|---|---|---|
| **AEC3 (Phase 13.E)** | Browser AEC stays OFF; **headphones required** | C++ FFI yak shave or sidecar service; ~3-5d of work. Revisit when phone-bridge sources land — they have no headphone option. |
| **SPA mic codec (Opus → PCM16)** | TTS playback round-trip works; STT round-trip is wired but inert | `VoiceCaptureButton` emits Opus via `MediaRecorder`; `voice_runtime` accepts only `pcm16le`. The SPA's `VoiceStatusBar` surfaces the empty final explicitly so this isn't a silent failure. Fix: AudioWorklet PCM capture (also unlocks future phone-bridge / mobile-native sources). |
| **Continuous VAD endpointing** | Push-to-talk only | The `voice-pipeline` crate has the `Vad` trait; a server-side `webrtc-vad` adapter is the cleanest first cut so phone-bridge sources work too. |
| **Real chat-path agent reply on `voice_stop`** | Echoes `you said: <transcript>` | Avoids coupling `voice_runtime` to `chats::dispatch_turn` until the chat path supports cancellable streams. |
| **`played_through_sentence` on barge-in** | Not tracked | Conversation history truncates the agent's outbound message to "all of it" on barge-in. Fix is bookkeeping in the runtime + a richer `VoiceInterrupted` payload. |

**Verification surface as of `72d1f63`:** 281 server lib + 22 integration + 2 voice WS round-trip = 305 server tests. 207 SPA tests across 33 files. Clippy clean, tsc clean. Criterion benchmarks land for `decode_pcm16_le`, `encode_pcm16_le`, `pcm_to_wav`, `observe_frame`, and `ingest_chunks`.

### 2.14 Identity resolution and cold-contact escalation

Every inbound message is resolved to a Principal before the agent sees it. A principal is never seen twice as a different person just because they switched transports, and no stranger reaches the agent without the controller having chosen to admit them.

#### Resolution pipeline

```
inbound message (transport plugin)
    │
    │  transport-specific identifier: signal:+15551234567, email:a@b.com,
    │                                  voice:+15551234567, web:sess-xyz, ...
    ▼
┌──────────────────────────────────────────────────────────────┐
│ 1. principals table lookup by identifier                      │
│      hit  → cached Principal + trust_level, proceed            │
│      miss → fan out to step 2                                  │
└──────────────────────────────────────────────────────────────┘
    │  miss
    ▼
┌──────────────────────────────────────────────────────────────┐
│ 2. Run all installed identity-provider plugins in parallel    │
│    (google-contacts, local-address-book, signal-safety, ...). │
│    Each returns Option<IdentityMatch { confidence,            │
│    trust_hint, stable_id, metadata }>.                         │
└──────────────────────────────────────────────────────────────┘
    │
    ▼
┌──────────────────────────────────────────────────────────────┐
│ 3. Aggregate                                                   │
│      best-confidence wins; metadata merged                     │
│      conflicts → identity_resolution_conflict event            │
│      no match → principal is cold                              │
└──────────────────────────────────────────────────────────────┘
    │
    ├── matched, trust_hint ≥ Contact, config.auto_trust_contacts=true  [DEFAULT]
    │     trust_level = KnownTrusted; conversation proceeds
    │
    ├── matched, trust_hint = Contact, auto_trust_contacts=false
    │     trust_level = UnknownPending; sideband notify controller
    │
    ├── matched, low-confidence / trust_hint = Unknown
    │     trust_level = UnknownPending; sideband notify controller
    │
    ├── no match, config.escalate_cold_contacts=true  [DEFAULT]
    │     trust_level = UnknownPending
    │     holding auto-reply (configurable) sent on originating transport
    │     sideband notification to controller
    │     conversation phase = awaiting_trust_decision
    │
    └── no match, escalate_cold_contacts=false
          drop silently
```

#### Identity-provider plugin contract

A new plugin kind (see §4.2 for the full list). Manifest:

```toml
[plugin]
kind = "identity_provider"

[identity_provider]
resolves = ["phone", "email", "signal_uuid", "web_session"]   # identifier types handled
trust_hint_default = "Contact"          # Contact | Colleague | Family | Organization | Unknown
confidence_ceiling = 0.95               # plugin self-caps its confidence
```

Rust trait (simplified):

```rust
#[async_trait]
trait IdentityProvider {
    async fn resolve(&self, id: &Identifier) -> Option<IdentityMatch>;
}

struct IdentityMatch {
    stable_principal_id: PrincipalId,   // Durable ID across this provider's invocations
    confidence: f32,                     // 0.0..=1.0
    trust_hint: TrustHint,
    metadata: Metadata {                 // Surfaced in the approval UI
        display_name: Option<String>,
        tags: Vec<String>,
        notes: Option<String>,
    },
}
```

Reference plugins execlaw ships:
- **`identity-local-address-book`** — flat contact list the controller maintains via the UI. First-run onboarding can seed from CSV or vCard import.
- **`identity-signal-safety-numbers`** — promotes Signal contacts with mutually-verified safety numbers to at least `Contact`.
- **`identity-google-contacts`** — optional, OAuth'd, read-only. Ships as a separate plugin (not a core dependency, per the self-hosted grounding rule).

Community can add: LinkedIn, CRM (Salesforce / HubSpot / Attio), iCloud / CardDAV, domain-heuristics (`@mycompany.com` → Colleague), etc.

#### Cold-contact flow — silent hold, no auto-reply

When a message from `UnknownPending` arrives:

1. Control plane appends a `cold_contact_arrived` event. Message is persisted for audit.
2. **No reply goes out.** The agent stays silent. Selfhosted-claw tried an auto-acknowledgement ("give me a moment") but that leaks agent presence to spammers and creates edge cases; execlaw doesn't do it.
3. **UI notification** fires to the controller (notification dropdown, unread badge). Rendered as:

   > **New contact wants to talk**
   > From: `+15551234567` (via Signal)
   > Identity matches: *none* — or — *"Jane Doe" from Google Contacts (conf. 0.92)*
   > First message: *"Hey, is this the right number for..."*
   > Decide: **[Trust]** · **[Trust (limited)]** · **[Block]** · **[Ignore once]**

   Signal fallback (§10.8) fires only if the notification goes un-acked for the configured window *and* Signal fallback is enabled.
4. Conversation phase → `awaiting_trust_decision`. The agent does not run.
5. Controller response:
   - `Trust` → `trust_level = KnownTrusted`; the next inbound from this principal (or any identifier mapped to them) starts a normal conversation.
   - `TrustLimited(topics)` → `trust_level = KnownLimited { allowed_topics: ... }`.
   - `Block` → `trust_level = Blocked`; future messages dropped at ingress; transport plugins that can block natively (e.g., Signal) receive a `trust_changed` event.
   - `IgnoreOnce` → drop this message; next contact re-notifies.
6. **Auto-trust via identity-provider plugins still applies** — if the controller installs `plugin-google-contacts` and that plugin's `identity_provider` hook resolves the sender as a known contact with `trust_hint ≥ Contact`, the cold-contact notification never fires; the principal is treated as `KnownTrusted` from the first message.

If the controller never responds, nothing happens — messages queue silently in the event log for audit, but no reply is ever sent. This is the simplest and safest default.

#### Configurable defaults (in `config_trust_policy` table)

| Setting | Default | Effect |
|---|---|---|
| `auto_trust_contacts` | `true` | Plugin-matched contacts with `trust_hint ≥ Contact` → `KnownTrusted` without asking |
| `min_trust_hint_for_auto_trust` | `"Contact"` | Minimum trust_hint that qualifies |
| `identity_plugin_order` | install order | First-match tiebreak |
| `delegated_trust_default_ttl` | `"7d"` | Default expiry for Delegated grants |
| `mixed_trust_policy` | `"min_wins"` | Effective policy for groups with mixed-trust participants |

No auto-reply setting, no escalation rate-limit setting — the silent-hold default removes the need for those.

#### Edge cases

- **Same person on multiple transports.** Identity-provider plugins can match across identifier types (Google Contacts maps phone ↔ email). The `principals` table holds the union; one trust decision applies everywhere.
- **Revoking trust.** Controller flips a principal to `Blocked` at any time. Any in-flight turn completes; the conversation phase transitions to `trust_revoked`; the agent is told to stop, the conversation is archived, and transport plugins receive a `trust_changed` event to block natively if supported.
- **Trust downgrade** (KnownTrusted → KnownLimited → UnknownPending). Same flow as revoke, with the new trust level applying to the next turn and after. Memories previously accessible to the principal remain; future retrieval is scoped by the new trust class (§2.7).
- **Plugins disagree.** Higher confidence wins; conflict logged as `identity_resolution_conflict` event; the controller sees it in the UI and can override manually.

### 2.15 Durability: surviving connection drops, power loss, and runner crashes

The agent must recover from every common interruption — transport WebSocket drop, OS-level container kill, power loss, host reboot — without losing work, without creating infinite loops, and without duplicate messages to users. The pieces of the design that collectively guarantee this are already spread across §§2.2–2.10; this section consolidates the guarantees and the invariants that enforce them.

#### What we guarantee

1. **No committed work is ever lost** across a restart of any component.
2. **No infinite loops** from a crash-restart cycle.
3. **No duplicate external effects** (messages sent twice, calendar events double-created, etc.) across retries, crashes, or reconnects.
4. **No dangling `tool_use`** blocks that would break the next turn.
5. **No dropped user messages** silently — every inbound event is either logged + processed, or logged + explicitly marked un-processable.

#### How each interruption kind is handled

| Interruption | What happens | What prevents loss / loops / duplicates |
|---|---|---|
| Transport WebSocket drop (mid-turn) | Transport plugin reconnects with exponential backoff; on reconnect it resumes its inbound poll from the last `source_event_id` stored in the `transport_cursor` row (SQLite); control plane dedupes inbound events by `(plugin_id, source_event_id)` before they reach the work queue. | Stable identifier dedup at ingress; cursor persisted per-transport. |
| Runner container crash (OOM / panic / kill) | Container manager detects via bollard event stream; any open `tool_use` gets a synthesized cancellation `tool_result` committed to `state_events`; new runner spawns, hydrates from the event log, and continues from the conversation's current phase. | `tool_use` / `tool_result` pairing invariant (§2.2 axiom #3); stateless runners (§2.2 axiom #5); runner respawn via container manager (§2.8). |
| Control-plane container restart (docker kill, docker-compose down/up, upgrade) | On boot, control plane scans `state_conversations` for non-idle phases with expired leases, commits cancellation results for any dangling `tool_use`, rolls phase back to `idle`, and the scheduler picks up pending wakeups / work-queue entries. | Lease expiry; same pairing invariant; event log is source of truth. |
| Host power loss / hard reboot | SQLite WAL replay restores the last committed state (all turn commits are atomic — a half-written turn doesn't persist); same flow as control-plane container restart then runs. Outbox state is safe (rows with `status = in_flight` at crash time are retried on startup; idempotency keys prevent double delivery to transports that have already received them — consumer-side inbox dedup, §2.4). | SQLite WAL atomicity; outbox idempotency keys derived from `(conversation_id, turn_seq, tool_call_ordinal)`; inbox dedup at every transport. |
| Approval mid-interrupt (controller sent `approve` via Signal sideband while control plane was down) | Signal transport's `transport_cursor` on reconnect returns the approval message with its stable `source_event_id`; ingress processes it, matches the awaiting-approval conversation, continues. Duplicate-delivery is handled by dedup. | Inbound dedup; event log consistency; approval tokens are signed + single-use. |
| External API at the end of the outbox returns before we record success (e.g., Signal sent the message but our HTTP client timed out before reading the response) | Outbox row stays `in_flight` with retry backoff; next attempt uses the same idempotency key; the transport's inbox sees the duplicate attempt, returns the already-stored delivery ID, we mark success. No double-send. | Framework-minted idempotency keys (never LLM-derived); consumer-side inbox dedup. |
| Infinite-retry risk (same tool keeps failing) | Per-effect retry budget: 5 attempts with exponential backoff to a ceiling, then move to a `dead_letter` table. The conversation continues (the agent sees a tool_result with `error: "retry_budget_exhausted"` and can adapt). An `Error` alert fires (§10). | Hard retry caps; dead-letter queue; no unbounded loops. |
| Infinite-wakeup risk (agent keeps scheduling wakeups for itself in a tight cycle) | Wakeup rate limit per conversation: default max 12 wakeups per hour + a cooldown window after a 4× schedule burst. Exceeding triggers an `Error` alert and suspends further wakeups from that conversation pending controller review. | Wakeup rate limit + alert; controller-review gate. |
| Controller permanently disappears (laptop bricked, lost phone) | Vault backup (§6.4) — `execlaw vault export` produces an encrypted bundle the operator stored somewhere safe. Restore flow rebuilds the Controller identity from the bundle; verified identifiers are re-bound via the normal verification flow on the new device. No access-loss footgun beyond what the operator's own backup hygiene allows. | Vault portability; no single-device lock-in. |

#### What we explicitly refuse to do

- **Infinite silent retries.** Every retry loop has a ceiling and an alert.
- **Best-effort fire-and-forget sends.** Every outbound message goes through the outbox with a framework-minted idempotency key; no direct-send code path exists anywhere in the codebase.
- **Cursor-before-commit.** selfhosted-claw's bug pattern ([src/index.ts:804-928](../selfhosted-claw/src/index.ts)) was advancing the message cursor before the container confirmed completion. execlaw's turn-as-transaction (§2.4) makes this impossible — the cursor advance is in the same commit as the `tool_result`, so a crash either both happen or neither does.
- **Ambient global state in runners.** Runners are stateless; nothing durable lives in their memory or filesystem. They can be killed and respawned freely.

---

## 3. Rust Control Plane (`execlaw-core`)

### 3.1 Crate layout (tentative)

```
execlaw-core/
├── Cargo.toml                (workspace)
├── crates/
│   ├── core/                 Event log, conversation FSM, turn commit, work queue
│   ├── session/              Session abstraction; drives pipeline composition per modality
│   ├── inference-api/        OpenAI-compatible client (streaming SSE + function calling);
│   │                          the single internal contract for LLM access
│   ├── runner-local/         In-tree runner for text and voice. Talks to any
│   │                          OpenAI-compatible LLM backend. No cloud SDKs.
│   ├── voice-pipeline/       Two-lane Tokio graph (system + data lanes);
│   │                          VAD, STT, LLM-stream, TTS, turn-detector, AEC,
│   │                          barge-in + backchannel-rescind (§2.13)
│   ├── plugin-host/          Dynamic plugin loading (subprocess)
│   ├── plugin-sdk/           Rust traits + generated schemas for plugins
│   ├── container-manager/    Docker client, service orchestration, GPU abstraction
│   │                          (Intel /dev/dri/renderD128 via OpenVINO + nvidia-
│   │                          container-toolkit; operator picks)
│   ├── policy/               Capability tokens, RBAC, input-guard, Rule of Two
│   ├── vault/                Encrypted secret storage (SQLCipher)
│   ├── transport-api/        Trait: Transport, ConversationEvent, Identity
│   ├── identity-api/         Trait: IdentityProvider (§2.14)
│   ├── outbox/               Effect relay with idempotency + inbox dedup
│   ├── server/               Axum HTTP + WebSocket (admin + chat + plugin cp)
│   └── cli/                  execlaw CLI: plugin install, db migrate, doctor, replay
└── plugins/                  (git-submodule or separate repo) reference plugins
    ├── transport-signal/
    ├── transport-voice/              (audio bridge for the voice pipeline: mic/speaker ↔ control plane)
    ├── service-openarc/              (DEFAULT inference backend: OpenVINO on Intel GPU)
    ├── service-vllm/                 (alternative inference backend: nvidia GPU)
    ├── service-whisper/              (STT: faster-whisper on CUDA; OpenVINO GenAI WhisperPipeline on Intel)
    ├── service-kokoro/               (TTS: Kokoro-82M; OpenVINO on Intel, ONNX+CUDA on nvidia)
    ├── service-piper/                (TTS fallback: Piper, CPU-first)
    ├── identity-local-address-book/
    ├── identity-signal-safety-numbers/
    ├── identity-google-contacts/     (optional; OAuth; not core)
```

**In-tree vs plugin split:**
- **In-tree (core):** event log, session semantics, runner contract, voice-pipeline graph (VAD + STT-adapter + LLM-stream + TTS-adapter + turn-detector + AEC orchestration), inference client protocol, policy, vault, container manager, CLI. Zero cloud dependencies.
- **Plugin (optional):** transports, identity providers, inference backends (local model servers), STT/TTS engines, inference bridges (cloud adapters), services. The default plugin set shipped in the installer: `service-openarc`, `service-vllm`, `service-whisper`, `service-kokoro`, `transport-signal`, `transport-voice`, `identity-local-address-book`.

### 3.2 Chosen dependencies (first pass; revisit per-crate)

- `tokio` runtime, `tower` / `axum` for HTTP, `tokio-tungstenite` for WS
- **Compilation target: `x86_64-unknown-linux-gnu`** for the Rust binary, which ships inside the control-plane container image. The deployment artifact is the image, not the bare binary — same container runs on WSL2 dev and bare-Linux production. Windows-native build is not a goal.
- **Dockerfile + docker-compose** are first-class deliverables: `Dockerfile.control-plane` produces the image; `docker-compose.yml` orchestrates the control plane alongside its service dependencies (inference backends, transports, etc.). The compose file is the operator-facing deployment entrypoint, as in selfhosted-claw (where `docker-compose.control-plane.yml` and `Dockerfile.control-plane` are the production pattern).
- **Image discipline (per axiom #12):** control-plane image stays small — Rust binary + shared-lib deps + CA certs + embedded PCI ID DB, nothing else. No vendor SDKs, no Python, no `nvidia-smi`. Vendor tooling lives in probe / service / runner images as needed.
- `rusqlite` with **`bundled-sqlcipher-vendored-openssl`** — SQLCipher statically linked, OpenSSL vendored. One binary, one encrypted DB file.
- Schema migrations via `refinery` or `sqlx-migrate` — forward-only, embedded. `execlaw db migrate` CLI; server refuses to start on schema mismatch.
- `keyring` crate for OS-keyring access (Linux Secret Service), with passphrase-file fallback for headless hosts.
- `bollard` for Docker API over `/var/run/docker.sock`. Identical on WSL2 and bare Linux.
- **GPU passthrough — dual-path, per-host-hardware (no vendor tooling in the control plane):**
  - **Intel GPU:** `/dev/dri/renderD128` device passthrough + Intel Compute Runtime. Drives OpenArc/OpenVINO-based `service-openarc` for LLM, `service-whisper` for STT, `service-f5tts` for TTS.
  - **nvidia GPU:** `nvidia-container-toolkit` + `--gpus all`. Drives `service-vllm` for LLM, optionally Kokoro on CUDA (`service-kokoro`).
  - Detection is tiered (§5.3): cheap sysfs reads + docker-daemon query + on-demand one-shot probe containers. The control-plane image itself ships zero vendor tooling (axiom #12).
- **Embedded PCI ID database** (~500KB static dataset, compiled into the binary from `pciutils`) for mapping PCI vendor/device IDs to human-readable GPU model names without invoking vendor tools.
- **Host sysfs bind-mount** at runtime (`/proc:/proc:ro` and `/sys:/sys:ro`) so the control plane can enumerate PCI devices, render nodes, and nvidia-driver info without carrying any vendor runtime itself.
- **Inference client: HTTP (reqwest) against OpenAI-compatible API.** The internal `inference-api` crate speaks only this protocol — `/v1/chat/completions`, `/v1/embeddings`, streaming via SSE, tool calls via OpenAI function-calling format. Backends (OpenArc, vLLM, llama.cpp server, Ollama) all conform to it. **Cloud endpoints are not a supported backend** (§0 axiom #1).
- ~~`wasmtime` for plugin sandbox (WASI preview 2)~~ — explicitly de-scoped (2026-04-25). Subprocess tier is enough for the self-hosted single-operator threat model; every plugin in `docs/plugin-inventory.md` wraps a native binary or talks HTTPS, none compile to wasm. Reconsider only if a plugin port surfaces a real need.
- `serde` + `schemars` for plugin manifest & capability schemas.
- `rustls` + `ed25519-dalek` / `jsonwebtoken` for capability tokens (EdDSA signed).
- `tracing` + `tracing-subscriber` with a JSON layer writing to both a rolling `~/.execlaw/logs/*.jsonl` file and an SQLite `log_*` table (mirrors selfhosted-claw's dual sink). No OpenTelemetry, no OpenInference, no external observability services — local SQLite + JSONL is sufficient for a single-operator self-hosted system.
- `rmp-serde` (MessagePack) for event-log payloads.
- **No `dotenv`.** Bootstrap config from CLI flags + optional `~/.execlaw/bootstrap.toml`. See §6.
- **No cloud-vendor SDKs, no cloud LLM paths.** Not `anthropic-sdk`, not `openai`, not `google-genai`, not any equivalent. Not in core, not in plugins, not as opt-in adapters. Models must be hosted locally. The control plane binary has zero direct linkage to any cloud provider and the plugin framework does not permit inference-bridge adapters (§0 axiom #1).

### 3.3 Control-plane responsibilities

| Module | Owns |
|---|---|
| `core` | Conversation FSM, work-queue lease/commit, wakeup scheduler, event bus |
| `plugin-host` | Plugin discovery, lifecycle (install/enable/disable/uninstall), isolation |
| `container-manager` | Single entry point for runner + service containers, GPU broker, reaper |
| `policy` | Identity resolution, capability evaluation, input guard pipeline |
| `vault` | Secret storage, per-plugin secret refs, rotation hooks |
| `server` | Admin/Chat API, WS event stream to UI, plugin-facing control-plane API |

Containers and UI **never bypass the control plane**. If the UI wants to restart a service, it calls the control plane, which calls the container manager.

---

## 4. Plugin Framework (WordPress-style)

### 4.1 Manifest (plugin.toml)

```toml
[plugin]
id = "transport-signal"
name = "Signal Transport"
version = "1.0.0"
kind = "transport"         # transport | runner | tool | skill | service | ui-panel
core_version = ">=0.1, <1.0"
author = "..."
homepage = "..."
license = "..."

[runtime]
# How the plugin runs. Phase 2 ships `subprocess` only. `container`
# is reserved for heavy services (vLLM etc.) and lands when one of
# those needs it. WASM and in-process tiers were considered and
# dropped — see §4.4.
kind = "subprocess"        # subprocess | container
entry = "dist/plugin.js"   # path inside the staged ZIP (subprocess)
# For container: image, mounts, etc.

[capabilities]
requires = ["transport.receive", "transport.send", "vault.read:signal_*"]
grants   = ["jid-scheme:signal"]

[secrets]
required = ["SIGNAL_DEVICE_KEY"]

[hooks]
# Plugin declares which hook points it attaches to.
on_inbound_message = true
on_outbound_message = true
on_controller_presence = true

[ui]
# Optional admin panel mount point.
admin_panel = "panels/signal/index.js"
```

### 4.2 Plugins and hook points — every plugin is a plugin

**A plugin is a plugin.** There are no typed "plugin kinds" — a plugin is a ZIP containing code + a manifest that declares which **hook points** it attaches to in the control plane. One plugin can register tools, add UI panels, run a service container, subscribe to events, and declare OAuth connections all at once. The WordPress/filter-hook pattern, translated for a Rust agent system.

**Hook points we expose** (initial set; the manifest is additive — new hooks land without breaking existing plugins):

| Hook | What it does | Who uses it |
|---|---|---|
| `tools` | Register native function-call tools (JSON Schema + required capabilities + `latency: low\|medium\|high`) | Anything that gives the agent a new capability |
| `transport` | Connect an external messaging channel (inbound + outbound) | `plugin-signal`, `plugin-voice`, `plugin-email`, `plugin-webchat`, `plugin-matrix` |
| `identity_provider` | Resolve transport identifiers → Principal + metadata + trust hint (§2.14) | `plugin-google-contacts`, `plugin-local-address-book`, `plugin-signal-safety-numbers` |
| `inference_backend` | Declare a local OpenAI-compatible LLM/STT/TTS service container (for the deployment registry, §5.4) | `plugin-service-vllm`, `plugin-service-openarc`, `plugin-service-whisper`, `plugin-service-kokoro` |
| `hardware_probe` | One-shot container that queries vendor hardware info as JSON (§5.3 Tier 3) | `plugin-probe-nvidia`, `plugin-probe-intel` |
| `service` | Declare a long-running background container (DBs, queues, helpers) | any integration whose behavior needs a helper process |
| `oauth_accounts` | Declare OAuth connections for the monitor (§10.10) — scopes, refresh policy, warn-before-expiry | `plugin-google-calendar`, `plugin-google-contacts`, any OAuth'd integration |
| `ui_panels` | Add admin-UI pages (ES modules loaded into the chat-first shell, §8) | any integration with config UI |
| `chat_components` | Inline renderers for specific attachment / card types in the chat surface | visualization plugins, interactive widgets |
| `event_subscriptions` | Listen to event-bus events (`conversation.message_inbound`, `plugin.status_changed`, etc.) | integrations that react to system state |
| `alert_sources` | Declare alert emitters with fingerprints (§10.6) | integrations with operational failure modes |
| `health_checks` | Declare probes the control plane runs periodically | integrations with reachability dependencies |
| `skills` | Structured knowledge / prompt library loaded by the runner | domain-specific guidance packs |

**Manifest sketch** (illustrative — exact schema firms up in Phase 2):

```toml
[plugin]
id = "google-calendar"
version = "1.0.0"
description = "Google Calendar integration"

[[tools]]
name = "calendar_list_events"
schema = "schemas/list_events.json"
latency = "medium"
required_capabilities = ["plugin.google-calendar.calendar.read"]

[[tools]]
name = "calendar_create_event"
schema = "schemas/create_event.json"
latency = "medium"
required_capabilities = ["plugin.google-calendar.calendar.write"]

[[oauth_accounts]]
name = "controller"
provider = "google"
scopes = ["calendar.readonly", "calendar.events"]
proactive_refresh_window = "10m"
warn_before_expiry = ["7d", "3d", "1d"]

[[ui_panels]]
mount = "admin/plugins/google-calendar"
entry = "ui/panel.js"

[[alert_sources]]
fingerprint_prefix = "plugin.google-calendar"

[[health_checks]]
name = "calendar_api_reachable"
interval = "5m"
probe = { kind = "http", url = "https://www.googleapis.com/discovery/v1/apis/calendar/v3/rest", expect_status = 200 }
on_fail_severity = "Error"
```

**How hooks attach.** When a plugin is enabled, the control plane's **hook registry** reads the manifest and registers each declaration into the appropriate live map — `tools_by_name`, `ui_panels_by_mount`, `identity_providers`, `event_subscriptions_by_kind`, etc. When the agent calls a tool, the UI renders a panel, or the event bus fires an event, the control plane looks up registered handlers and dispatches.

**Detailed shape TBD in Phase 2.** Exact schemas per hook, Rust trait shapes, capability-namespace rules, hot-reload semantics, and sandboxing-per-hook-point all firm up when the plugin host lands. The point of this section is: the set of hook points is the extension surface, and the set is extensible. Every integration from selfhosted-claw ports by declaring hook attachments (see §12 port inventory).

The runner (`runner-local`) and `voice-pipeline` are **in-tree core crates**, not plugins (§2.8, §3.1). Plugins register tools the runner uses, not runners themselves.

### 4.3 Native function-call tools vs MCP — the decision

execlaw plugins expose their tools as **native in-process function-call tools** (registered with the `runner-local` crate and surfaced to the LLM via the OpenAI-compatible `tools` array in each request), **not** as external MCP servers. Rationale:

- **Isolation:** runners are ephemeral containers with already-bounded capabilities via their token. An external MCP sidecar would add a second process boundary with its own auth surface for no gain.
- **Discoverability:** native tools are introspectable by the SDK and included in context; external MCP tools are fetched opaquely at init.
- **Versioning:** plugin versions are immutable per container image — no runtime mismatch risk between control plane and a separately-versioned MCP server.
- **Context cost:** mitigated by SDK **tool search** when a conversation exposes >~20 tools.
- **Self-hosted alignment:** no sidecar orchestration, no separate daemon to health-check.

**External MCP is reserved for two cases only:**
1. Third-party SaaS reach (Slack, GitHub, Stripe, etc.) where Anthropic Managed Services or a community-maintained MCP server is the sane integration path.
2. Plugins whose implementation is inherently out-of-process and pre-existing (e.g., a user points execlaw at an MCP server they already run).

When execlaw speaks MCP, it does so *outbound* to someone else's MCP server — it does not force plugin authors to implement one.

### 4.4 Isolation tiers

| Tier | Mechanism | Status | Use for |
|---|---|---|---|
| 0: in-process | Rust dylib, signed by maintainer | dropped | Was reserved for trusted core plugins; folded into the host instead. |
| 1: wasm | `wasmtime` component, no ambient authority | dropped 2026-04-25 | Considered for 3rd-party tools but no plugin in `docs/plugin-inventory.md` benefits — every port wraps a native binary or talks HTTPS, none compile to wasm. Threat model is self-hosted single-operator, so the deny-by-default story doesn't pay for the wasmtime dep + Cranelift weight. Reconsider only if a future port surfaces a real need. |
| 2: subprocess | Separate OS process, JSON-RPC over stdio | shipped (Phase 2) | Every plugin today: Node/Python/Go workers talking JSON-RPC over stdio. |
| 3: container | Full container with declared mounts/network | reserved | Heavy services (vLLM, Whisper, Kokoro). Lands with the first such plugin port (Phase 8). |

Capabilities are enforced at every tier: the plugin host *refuses* to pass a message or perform a side-effect the plugin didn't declare permission for in its manifest.

### 4.5 Install/enable/disable

Plugin installation is **ZIP upload with a manifest** — nothing else. No git-URL fetching, no hosted registry, no "branch" install method (nanoclaw's pattern is explicitly rejected).

**Plugin archive format:**
```
my-plugin.zip
├── plugin.toml              # manifest (required)
├── dist/                    # compiled plugin code/image refs
│   └── ...
├── README.md                # optional
└── LICENSE                  # optional
```

**Install flow:**
1. Operator uploads a ZIP via the admin UI (Admin → Plugins → Install) or `execlaw plugin install ./my-plugin.zip` from the CLI.
2. Control plane extracts to a temp directory, validates `plugin.toml` against the manifest schema, rejects with a specific error if the manifest is malformed or capabilities are missing.
3. On success: contents move to `~/.execlaw/plugins/<id>/<version>/` (versioned directory so upgrades and rollbacks are atomic directory swaps).
4. `install` hook runs if declared in the manifest (one-shot, e.g. OAuth redirect-URL registration).
5. Plugin appears in the Plugins list, `active = false` by default.

**Enable / disable / uninstall:**
- **Enable** — DB write + hot attach. No restart for tier-1/2/3 plugins; tier-0 (in-process Rust dylib) requires restart.
- **Disable** — DB write + graceful drain of pending work; plugin stays on disk for quick re-enable.
- **Uninstall** — removes the versioned directory; `plugin_settings` rows scoped to `plugin_id` are dropped.
- **Upgrade** — upload a new ZIP; the new version installs alongside; operator flips `active_version` in the Plugins UI; old version can be rolled back instantly.

This replaces selfhosted-claw's hardcoded `registerIntegration()` + static build-time wiring and explicitly does *not* carry forward nanoclaw's git-branch-based plugin distribution.

---

## 5. Unified Container Manager

Single Rust crate owns all container lifecycle. Kills the fragmentation between `container-runner.ts`, `container-runtime.ts`, `hot-runner-pool.ts`, `group-queue.ts`, and `service-manager.ts`.

### 5.1 Interface

```rust
trait ContainerManager {
    async fn spawn_runner(&self, spec: RunnerSpec) -> Result<RunnerHandle>;
    async fn ensure_service(&self, spec: ServiceSpec) -> Result<ServiceHandle>;
    async fn get_service_endpoint(&self, kind: ServiceKind) -> Result<Endpoint>;
    async fn stop(&self, id: ContainerId, grace: Duration) -> Result<()>;
    async fn logs(&self, id: ContainerId, opts: LogOpts) -> LogStream;
    async fn events(&self) -> EventStream;  // Exit, OOM, health change
}
```

### 5.2 Capabilities

- **Runner pool with warm containers** — replaces `HotRunnerPool` but governed by the same manager.
- **Hardware profile + GPU broker** — §5.3.
- **Runner deployment registry** — §5.4.
- **Declarative service graph** — `service-openarc depends_on [intel_gpu_alloc], health /v1/models`; the manager computes start order and waits for health.
- **Structured log aggregation** — one ingestion path, not "markers in stdout + JSONL + SQLite".
- **Event bus, not polling** — replaces `ContainerReaper` polls and health-monitor polls. Docker events → bus → subscribers (core agent loop, admin UI stream).

### 5.3 Hardware profile and GPU awareness

Per the minimal-containers rule (§0 axiom #12), the control plane **does not** carry vendor GPU tooling. Detection is tiered: cheap host-level reads for presence, docker-daemon queries for runtime capability, and one-shot probe containers when vendor-specific detail is required. Every tier is optional — the next tier runs only if a richer answer is needed.

```rust
struct HardwareProfile {
    gpus: Vec<GpuDevice>,
    docker_runtimes: Vec<String>,      // from `docker info` — e.g. ["runc", "nvidia"]
    detected_at: Instant,
    source: DetectionSource,            // Sysfs | Probe | Manual
}

struct GpuDevice {
    id: GpuId,                          // stable, e.g. "pci:0000:01:00.0"
    vendor: GpuVendor,                  // Nvidia | Intel | Amd | Unknown
    pci_id: String,                     // "10de:2684"
    model_from_pci_db: Option<String>,  // "NVIDIA GeForce RTX 4090" (from embedded PCI ID db)
    device_files: Vec<PathBuf>,         // For container passthrough (/dev/dri/renderD128, ...)
    probed: Option<ProbeData>,          // None until a probe has run
    in_use_by: Vec<ContainerId>,
}

struct ProbeData {
    probed_at: Instant,
    probed_by: PluginId,                // which probe plugin produced this
    vram_mb: Option<u64>,
    driver_version: Option<String>,
    runtime_support: Vec<GpuRuntime>,   // Cuda | OpenVino | Rocm | Vulkan
    extras: HashMap<String, Value>,     // vendor-specific: compute_capability, arch, etc.
}
```

#### Tier 1 — zero-dependency host-sysfs read

The control plane bind-mounts `/proc` and `/sys` read-only (`--mount type=bind,src=/sys,dst=/sys,readonly`) and reads:

- `/sys/class/drm/card*/device/vendor` — PCI vendor ID (`0x10de` = nvidia, `0x8086` = Intel, `0x1002` = AMD)
- `/sys/class/drm/card*/device/device` — PCI device ID, looked up in an **embedded PCI ID database** (a ~500KB static dataset shipped in the binary, sourced from `pciutils`) to resolve the model name
- `/dev/dri/renderD*` — existence enumerates render-capable devices for passthrough
- `/proc/driver/nvidia/gpus/*/information` — populated by the nvidia kernel module when loaded; includes GPU UUID and model without needing nvidia-smi

This tier runs on every control-plane startup and is ~instant. Output: vendor, count, PCI IDs, probable model names, render device paths. **Zero vendor tooling in the control plane.** Works even with no GPU (returns empty list plus a CPU pseudo-device).

#### Tier 2 — Docker daemon runtime query

`docker info` via bollard reports configured runtimes. `nvidia` in the runtime list means `nvidia-container-toolkit` is installed and functional on the host, which is sufficient to answer "can I pass nvidia GPUs through to containers" without invoking any nvidia tooling from inside the control plane. Same for other accelerator runtimes as they become relevant.

Also ~instant; runs alongside Tier 1 at startup.

#### Tier 3 — one-shot probe containers (on-demand)

When the setup wizard, `execlaw hw rescan`, or a deployment-change event needs details Tier 1/2 can't provide (vRAM, compute capability, driver version, OpenVINO device list), the control plane spawns a **purpose-built probe container** — a minimal image whose only job is to dump the info and exit:

| Probe image | Base | Contents | Purpose |
|---|---|---|---|
| `probe-nvidia` | `nvidia/cuda:X.Y-runtime-ubuntu22.04` | `nvidia-smi` + a ~50-line JSON-emitter script | dump nvidia details as JSON |
| `probe-intel` | `openvino/ubuntu22_runtime:latest` or similar | `clinfo`, `sycl-ls`, level-zero utils + JSON emitter | dump Intel GPU details |
| `probe-amd` | `rocm/rocm-terminal` | `rocm-smi` + emitter | dump AMD GPU details (future) |

Probes ship as **reference plugins** of kind `hardware_probe` (§4.2), not core code. The plugin manifest declares which vendors it handles; the control plane picks the matching probe when Tier 1 identifies a GPU of that vendor.

Probe invocation: `docker run --rm --gpus all probe-nvidia:latest` → container prints JSON to stdout, exits. Control plane parses and caches to `state_hardware_profile`. Cached for a configurable TTL (default: 24h, refreshed on demand).

**Probe images stay tiny relative to their vendor base** — they contain only the vendor's query tool + a small emitter script, nothing else. The image is bulk due to the vendor base (CUDA runtime, OpenVINO runtime), which is unavoidable for the probing task itself — but it lives *only* in the probe image, never in the control plane or any runner.

#### Tier 4 — manual override via the setup wizard

For locked-down hosts that can't pull probe images, or operators who prefer explicit config, the setup wizard lets the controller enter hardware info directly. Source of truth is `config_hardware_profile_overrides` in SQLite; probes populate, operator overrides. Overrides are audit events.

#### Handling the absent case

No GPU of a given vendor → Tier 1 returns an empty list for that vendor → Tier 3 probe never runs. The control plane happily reports the available hardware and drives the deployment-registry defaults accordingly (CPU fallback, single-vendor only, etc.). Nothing in the control plane requires a specific GPU vendor to be present.

#### Refresh lifecycle

- **Startup:** Tier 1 + Tier 2 always run. Tier 3 runs only if there's no cached probe data or cache is expired.
- **`execlaw hw rescan`:** full refresh, invalidates cache, re-runs all tiers.
- **Hot-plug / new GPU detected at Tier 1 that wasn't there before:** alert emitted (§10) and probe auto-triggered.
- **Probe failure:** emits `Warning` alert `core.container-manager:probe_failed:<vendor>`; falls back to Tier 1 info; deployment registry flags affected deployments as "limited info" until resolved.

The result is a small, portable control-plane image that still knows everything the deployment registry (§5.4) needs to make smart default proposals — and that knows it *correctly* by running the real vendor tools when it matters, just not in its own address space.

### 5.4 Runner deployment registry

execlaw does not hardcode which model runs which runner on which GPU. The operator configures **backends** — mappings of (purpose → inference backend plugin → model → GPU) — stored in SQLite and editable in the UI. The container manager starts and manages the services accordingly.

```rust
enum BackendPurpose {
    Standard,    // Default text-conversation inference + voice LLM stage; reasoning_enabled toggles native <think> mode
    Small,       // Fast-path model for voice mode and other latency-sensitive routes
    VoiceStt,    // Whisper backend for the voice pipeline
    VoiceTts,    // Kokoro (or fallback) backend for the voice pipeline
}

struct BackendRow {
    purpose: BackendPurpose,            // PK
    inference_backend: PluginId,        // service-vllm | service-openarc | service-whisper | service-kokoro | other local backend
    model_spec_json: Value,             // repo, quantization, revision
    gpu_id: Option<String>,             // None = CPU
    endpoint: Option<Url>,              // Usually unset (local service manages endpoint); present only for user-pointed local endpoints like an existing Ollama install
    notes: Option<String>,
    reasoning_enabled: bool,            // Standard-only: opt into Qwen3.5 <think> blocks etc. Server zeroes for other purposes.
}
```

Stored in `config_backends` (renamed from the original `config_runner_deployments`; Phase 8.5/8.8 reshape). Changes are audit events. Phase-7f advanced-subagent work — including separate Guardrail-classifier deployments and Reasoning-escalation paths — was de-scoped from Phase 7 and lives in its own roadmap; until then, Standard with `reasoning_enabled = true` is the only reasoning surface.

**Default recommendations** are computed from the detected `HardwareProfile` at first run and presented in the setup wizard; the operator accepts or overrides. For the hybrid nvidia + Intel Arc development setup this plan targets:

| Purpose | Inference backend | Model | GPU |
|---|---|---|---|
| Standard (text + voice LLM; reasoning toggle on) | `service-vllm` | `QuantTrio/Qwen3.5-27B-AWQ` | nvidia |
| Small (voice mode, fast path) | `service-vllm` or `service-openarc` | small Qwen variant (e.g. Qwen2.5-3B) | whichever GPU has headroom |
| VoiceSTT | `service-whisper` (OpenVINO GenAI backend on Intel; faster-whisper on CUDA) | whisper-small.en int8 | Intel Arc |
| VoiceTTS | `service-kokoro` (OpenVINO on Intel; ONNX on CUDA) | Kokoro-82M v1.0 | Intel Arc |

The voice pipeline composes these — one conversation can route STT to Intel, LLM to nvidia, TTS back to Intel, using the hardware in parallel.

On nvidia-only hosts, everything runs on nvidia (Kokoro via ONNX+CUDA, Whisper via faster-whisper, LLM via vLLM). On Intel-only hosts, everything runs on Intel Arc via OpenVINO. On CPU-only hosts, everything runs CPU with a warning about latency.

**UI flow for the operator:**
1. Setup wizard shows detected hardware.
2. For each `RunnerPurpose`, the wizard proposes a deployment based on the hardware profile and the models already pulled.
3. Operator confirms, edits, or adds a new deployment (picks a different model, a different GPU, a different backend).
4. Container manager starts the configured services; readiness is reflected in the admin UI.
5. Later changes (swap model, swap GPU, disable voice runner) are diff-based — only the affected services restart.

**Runtime behavior.** When a conversation's turn starts, the core looks up the default deployment for the runner purpose implied by modality + trust class, fetches the corresponding endpoint from the container manager, and hands the `runner-local` crate an `InferenceClient` pointed at that endpoint. `runner-local` is model- and vendor-agnostic — it speaks OpenAI-compatible API regardless of what's on the other end.

### 5.5 Personality & system-prompt composition

The operator's voice for the agent — name, tone, custom instructions, persona, voice ID — lives in `config_personality`. This is the *user-editable* half of the agent's system prompt. The other half (trust-class rules, tool-allowlist boilerplate, refusal behaviour) is built-in and not operator-tweakable; see §2.3.

selfhosted-claw split this into a "Personality" admin page with ~9 editable fields scoped at three levels (global / main / per-group). execlaw inherits the same field set and a simplified two-level scope:

- **`default`** scope — exactly one row, always present. The fallback for every conversation.
- **`conversation`** scope — sparse, keyed by `conversation_id`. Optional per-conversation overrides; absent rows fall back to `default`.

A third level (per-transport-group) is available later by extending the `scope_kind` enum without a schema change; v1 ships with the two scopes above.

#### 5.5.1 Schema (`config_personality`)

```rust
struct PersonalityRow {
    scope_kind: PersonalityScopeKind,   // "default" | "conversation"
    scope_ref: String,                  // "" for default; conversation_id for conversation
    display_name: String,               // "Earl" — what the agent calls itself
    role: String,                       // "Personal assistant" / "Research analyst"
    tone: String,                       // "Concise, practical"
    communication_style: String,        // formatting rules ("single-sentence replies, no markdown")
    initiative: String,                 // proactivity bounds ("ask before scheduling")
    about_agent: String,                // persona / backstory in the agent's voice
    about_controller: String,           // facts the agent knows about you (operator-curated)
    custom_instructions: String,        // freeform multi-paragraph directives — biggest field
    voice_id: Option<String>,           // pin TTS voice for this scope (default `bf_emma+am_michael` — Kokoro blend)
    version: i64,                       // monotonic; bumped on every save so audit can trace prompt drift
    created_at: i64,
    updated_at: i64,
}
```

Composite PK is `(scope_kind, scope_ref)`. The default row is seeded by migration 0013 with conservative built-in defaults (e.g. `display_name = "execlaw"`, empty free-form fields) so first-run conversations have a deterministic prompt before the operator edits anything.

CHECK constraints: `scope_kind ∈ {'default','conversation'}`; if `scope_kind = 'default'` then `scope_ref = ''`.

#### 5.5.2 Composition algorithm

```
compose_system_prompt(conversation_id) -> String:
    base   = load(scope_kind=default,      scope_ref="")          # always present
    over   = load(scope_kind=conversation, scope_ref=conversation_id)   # may be None
    fields = field_by_field_merge(base, over)   # most-specific scope wins per field
    return render_markdown(fields)
```

**Field-by-field merge, not row-replacement.** A conversation override that only sets `tone` inherits every other field from `default`. An override is a sparse patch, not a complete replacement.

**Empty string ≠ unset for overrides.** A conversation row whose `tone = ""` deliberately blanks the tone for that conversation; only fields the SPA explicitly sends are stored. The store distinguishes "not in override" (use base) from "blank in override" (force blank).

**Render order** in the produced markdown:

1. `# Identity` — display_name, role
2. `# Tone` — tone
3. `# Communication style` — communication_style
4. `# Initiative` — initiative
5. `# About me (the agent)` — about_agent
6. `# About you (the controller)` — about_controller
7. `# Additional instructions` — custom_instructions

Empty sections are omitted entirely so the prompt stays small for fresh installs. The renderer is in `crates/core/src/personality.rs::compose_system_prompt`; `runner-local` calls it once per turn with the conversation id.

#### 5.5.3 Memory layer interaction (§2.5)

`about_controller` is the **operator-curated** facts surface. The agent-learned `controller_facts` memory layer (§2.5) is appended *after* `about_controller` in the rendered prompt, never overwritten. This keeps the operator's hand-edited canonical truth above the agent's probabilistic recollections — the agent sees the operator's note first, then its own running notes.

If the two ever conflict, the operator's edit wins by virtue of source ordering; the agent is instructed (in its built-in system prompt half) to defer to the `# About you` section over its own memory layer when in doubt.

#### 5.5.4 Voice tie-in

`voice_id` pins the TTS voice for that scope. Default is the locked-decision blend `bf_emma+am_michael` (per `project_locked_decisions_2026_04_23.md`; populated by migration `0016_personality_voice_id_blend.sql`). The voice-pipeline runner reads this through the same composer and passes it to the `service-kokoro` backend on each TTS request. Per-conversation override lets the operator pick a single voice (e.g. `am_michael`) for that scope without changing the global default.

#### 5.5.5 Versioning + audit

Every save bumps `version` and writes a `config_audit` row with old/new JSON. The audit page already shows `config_*` table changes, so personality drift is visible without extra UI. The version number is exposed in the prompt as a discreet trailing comment (e.g. `<!-- personality v=42 -->`) so a turn's events can be correlated with a specific personality version when debugging "why did the agent suddenly start replying in haiku."

#### 5.5.6 Migration from selfhosted-claw

| selfhosted-claw `PersonalityProfile` | execlaw `PersonalityRow` | Notes |
|---|---|---|
| `displayName` | `display_name` | Direct |
| `role` | `role` | Direct |
| `tone` | `tone` | Direct |
| `communicationStyle` | `communication_style` | Snake-case rename |
| `initiative` | `initiative` | Direct |
| `aboutAgent` | `about_agent` | Direct |
| `aboutController` | `about_controller` | Direct; semantic merge with `controller_facts` memory layer documented above |
| `customInstructions` | `custom_instructions` | Direct |
| `updatedAt` | `updated_at` (+ new `version`) | Existing audit infra obviates the explicit field |

Operators migrating from selfhosted-claw paste their existing `personality.json` content field-by-field into the SPA. There is no automatic JSON import in v1 — the field set is small enough that copy/paste is faster than maintaining an importer for a single one-time event.

#### 5.5.7 What's NOT in personality

- **Trust-class rules** — system-managed, never operator-editable here; configured in §2.9 trust-class capabilities.
- **Tool allowlists** — managed in `config_tool_access` (Settings → Tools).
- **Plugin settings** — `plugin_settings` table, scoped per plugin.
- **Schedules / routines** — separate `config_routines` table; see §5.6.

The personality table is *only* the natural-language voice. Anything that needs typed validation goes elsewhere.

### 5.6 Routines (cron-shaped agent automations)

A **routine** is a cron-scheduled prompt the agent runs on its own. Examples:

- `0 8 * * 1-5` — every weekday at 8am, summarise overnight emails into the control thread.
- `*/15 * * * *` — every 15 minutes, poll the on-call queue and alert on new pages.
- `0 9 * * 1` — every Monday at 9am, post a "what's on this week" digest.

This is the cron-job-shaped half of the agent's autonomy (the other half is event-driven turn dispatch from inbound transport messages). selfhosted-claw exposed a similar surface as "Scheduled tasks"; execlaw renames it Routines because the shape is recurring routine, not one-shot task.

#### 5.6.1 Schema (`config_routines` + `state_routine_runs`)

```rust
struct RoutineRow {
    id: RoutineId,                     // ulid for stable refs from runs
    name: String,                      // operator-visible label
    schedule_cron: String,             // 5-field cron (`m h dom mon dow`)
    timezone: String,                  // IANA tz, e.g. "America/New_York"; default "UTC"
    prompt: String,                    // the user-message the agent sees
    target_conversation_id: Option<ConversationId>,
                                       // None → mint a fresh conversation per run
    enabled: bool,
    last_run_at: Option<i64>,
    last_run_status: Option<RoutineRunStatus>,  // Success | Failed | Skipped
    next_run_at: Option<i64>,          // computed at save + after each run
    created_at: i64,
    updated_at: i64,
}

struct RoutineRun {
    id: RoutineRunId,
    routine_id: RoutineId,
    fired_at: i64,                     // when the scheduler decided this turn fires
    started_at: Option<i64>,           // when the runner picked it up
    finished_at: Option<i64>,
    status: RoutineRunStatus,
    error: Option<String>,             // populated on Failed
    conversation_id: Option<ConversationId>, // resolved at fire time
}
```

`config_routines` is operator-edited, audit-logged. `state_routine_runs` is append-only — the run history. Retention sweeper trims runs older than the configured window (default 90d).

#### 5.6.2 Cron parsing & evaluation

Standard 5-field cron: `minute hour day-of-month month day-of-week`. Comma lists, ranges, and `*/N` step values supported via the `cron` crate's standard parser. Six-field syntax (with seconds) and the `@hourly` / `@daily` shortcuts are deferred.

**Validation on save.** Reject syntactically-invalid expressions before persisting. Reject schedules whose `next_run_at` is more than 1 year out (catches `0 0 1 1 *` typos that would mean "once per year").

**Timezone-aware**. The cron expression evaluates in the operator's chosen tz. `next_run_at` is stored as Unix epoch (UTC) so the scheduler tick is timezone-agnostic.

**Drift handling.** If the control plane is offline when a run was due, the scheduler fires *one* catch-up at startup and skips intervening misses (no thundering-herd backfill). Operators who need missed-runs replay invoke a routine manually.

#### 5.6.3 Scheduler tick

A single `RoutineRunner` task in the server crate ticks every minute (aligned to the wall clock so the on-the-minute schedules don't lag by tick offset). On each tick:

1. Query routines where `enabled = true AND next_run_at <= now`.
2. For each: insert a `state_routine_runs` row in `Pending` status, dispatch the prompt to a stub turn executor (Phase 10 ships the dispatch path; the actual turn execution lands when `runner-local` is real), update `last_run_at`, recompute `next_run_at`.
3. Errors during dispatch update the run row to `Failed` with the error string and DON'T advance `next_run_at` past the next normal occurrence — i.e. a transient failure doesn't permanently desynchronize the schedule.

**Concurrency**: a routine can never fire twice concurrently. The runner takes a per-routine advisory lock for the duration of the dispatch.

**Manual trigger**: `POST /api/admin/routines/{id}/run-now` queues a one-off run that doesn't affect `next_run_at`. Used for testing prompts and for manual catch-up.

#### 5.6.4 SPA surface

The placeholder `RoutinesPage` becomes a real list/edit/create page:

- List view: table rows with name + cron + last status + next fire time + enabled toggle.
- Editor: name, cron (with a live "next 5 fires" preview), timezone, prompt textarea, target conversation (dropdown of existing conversations + "fresh per run" option), enabled checkbox.
- Run history: per-routine drawer showing the last 50 runs with status, duration, and any error.
- Manual fire button.

Phase 10 ships v1 of all four. Backfill of missed runs and routine cloning are nice-to-haves for v1.x.

#### 5.6.5 Trust + capability scoping

Routine-fired turns inherit the controller's trust class. Tools are subject to the same `config_tool_access` allowlist as a normal controller turn — no extra grants and no extra bypasses. A routine can't escalate beyond what the operator typing the same prompt manually would get.

This keeps the feature side-effect-free for security: enabling a routine is exactly equivalent to setting an alarm on the operator's calendar to type that prompt.

#### 5.6.6 Audit + drift

Every save bumps `updated_at` and writes a `config_audit` row. Every run writes a `state_routine_runs` row. The two together let the operator answer "why did the agent do X at 8am yesterday" with one query: routine row → run history → conversation events.

---

## 6. Configuration & Secrets (No `.env`, SQLite-Backed)

### 6.1 Why we're moving away from shared config files

In selfhosted-claw, the admin UI edited config files (env-style and JSON) that were mounted into or copied into containers. When an edit was partial, stale, or raced with a consumer read, the system wedged — tool settings out of sync, stuck containers, "did this save?" ambiguity.

execlaw's rule: **no process ever reads user-editable configuration from a file that another process writes.** Config lives in SQLite. UIs read and write through the control plane. Containers receive snapshots at spawn time; they never re-read a mounted config file mid-run.

### 6.2 What lives where

| Data | Storage | Notes |
|---|---|---|
| User/admin configuration (personality, policy, plugin settings, schedules, feature knobs) | `execlaw.db` — `config_*` tables | Source of truth. UI and API both read/write via the control plane. |
| Runtime state (conversations, messages, work queue, wakeups, audit) | `execlaw.db` — `state_*` tables | Atomic transactions, no file writes. Same DB, different table namespace. |
| Logs | `execlaw.db` — `log_*` tables with retention, or a separate `logs.db` if size gets unruly | Use `PRAGMA auto_vacuum=INCREMENTAL`. Retention job. |
| Secrets (API keys, Signal device keys, plugin credentials, OAuth tokens) | `execlaw.db` — `vault_*` tables, **SQLCipher-encrypted DB** | Whole DB encrypted; master key from OS keyring (fallback: passphrase file). |
| Bootstrap (data dir path, bind address, log level) | CLI flags + optional `~/.execlaw/bootstrap.toml` | Tiny. Not env-based. |
| Ephemeral runner input (conversation tail, capability token, prompt) | Injected at spawn as stdin/env | Never a host-mounted file; never re-edited. |
| Plugin code and manifests | `~/.execlaw/plugins/<id>/<version>/` (read-only after install) | Installed via CLI/UI; treated as immutable per-version. |

Runners needing conversation-scoped settings call the control plane over loopback/uds with their capability token and get a fresh snapshot. No filesystem sync dance.

### 6.3 SQLite best practices we'll follow

- **WAL mode** (`PRAGMA journal_mode=WAL`) so readers don't block the writer
- **Foreign keys on** (`PRAGMA foreign_keys=ON`) — off by default in SQLite, must be set per-connection
- **`synchronous=NORMAL`** with WAL (good durability/speed tradeoff), `FULL` for the vault DB
- **Single writer per DB, many readers.** All writes go through one `Db` actor in Rust; reads use a connection pool
- **Migrations with `sqlx migrate`**, versioned, forward-only except for explicit rollbacks. `execlaw db migrate` is a CLI command; the server refuses to start on a schema mismatch
- **Typed tables, not KV**, for anything the UI validates. `personality(id, tone, voice_id, ...)` beats `settings(key, value)`. A small `kv` table is fine for truly freeform plugin state
- **Per-plugin isolation**: `plugin_settings(plugin_id, key, value_json)` scoped by `plugin_id`. Uninstalling a plugin drops its rows cleanly
- **Single DB, logical separation via table prefix** (`config_*`, `state_*`, `log_*`, `vault_*`). One file to back up, simpler ops. If log volume becomes a problem we'll split logs into `logs.db`; config and vault stay together. Rust repository layer enforces access — vault_* rows only touched through the vault API, never bare SQL from feature code.
- **Audit everything**: `config_audit(ts, actor, table, row_id, old_json, new_json)` populated by the Rust repository layer (not a trigger — we want the actor recorded)
- **Validate on write**: CHECK constraints where cheap; app-level schema (`serde` + `schemars`) for complex shapes. The UI never writes a row the agent can't parse
- **Atomic multi-field saves**: UI form submit = one transaction. No partial-save window
- **`VACUUM INTO`** for consistent point-in-time backups without stopping the server
- **Size discipline**: logs in a separate table or DB with retention; `PRAGMA auto_vacuum=INCREMENTAL` on high-churn DBs

### 6.4 Secrets — SQLCipher (self-contained)

**Decision: SQLCipher**, because it's self-contained.

- `rusqlite` with `bundled-sqlcipher-vendored-openssl` statically links SQLCipher + OpenSSL into the execlaw binary. No `libsqlcipher-dev` system package, no runtime linker resolution, no per-distro packaging quirks. One binary, one encrypted DB file.
- Works identically on WSL2, bare Linux, and any future deploy target — the container image is the artifact, host just needs Docker or Podman.
- The `vault_*` tables live inside `execlaw.db`, which is SQLCipher-encrypted as a whole. Every connection opens with `PRAGMA key = '...'` where the key comes from the OS keyring at startup.
- First run: Rust generates a 256-bit key, stores it in the Linux Secret Service (gnome-keyring / KWallet), uses it to initialize the DB.
- Subsequent runs: fetch the key silently from the keyring; no prompt.
- Headless Linux fallback (no Secret Service running): key is stored in a file sealed by a passphrase that the admin enters once at `execlaw up` via an interactive prompt; the key is then held in memory for the process lifetime.
- Re-key (rotate master key) is one SQLCipher command (`PRAGMA rekey`); CLI exposes `execlaw vault rekey`.

No `age`, no per-row encryption, no KMS. If per-secret blast-radius ever becomes a concern we'll add a wrapping layer on top; for now, SQLCipher alone is enough and matches the self-hosted grounding rule.

### 6.5 Do we need a `.env`? — No.

The three things projects typically use `.env` for, and where they land in execlaw:

1. **Secrets (API keys, device keys, OAuth tokens)** → encrypted vault, keyed by the OS keyring. Admin enters them once via UI or `execlaw secret set <name>`. Never on disk in plaintext, never in process environment where `ps e` or a crash dump could leak them.
2. **Deployment config (port, bind address, data dir)** → CLI flags with defaults (`~/.execlaw/`, `127.0.0.1:3030`). Optional `~/.execlaw/bootstrap.toml` if you want to persist overrides — typed TOML parsed into a Rust struct, not loaded as env vars.
3. **Runtime knobs (poll intervals, concurrency caps, feature flags)** → SQLite `runtime_settings` table, editable in the UI. Changes apply without restart where possible (reloadable config pattern: config actor broadcasts updates on a channel; subscribers re-read).

The only compiled-in string: the OS-keyring service name (`"execlaw"`). That locates the master key; it's not something a user configures.

**Plugin legacy compatibility.** If a plugin bundles a third-party library that insists on reading `SOMETHING_API_KEY` from env, the plugin host injects that var from the vault **at spawn time, into the plugin process only** — scoped to that plugin's capability grant. The control-plane host environment never contains the value; neither does any file.

**End result:** a fresh `git clone` + `execlaw up` walks you through setup via the UI. No editing `.env`, no chmod dance, no "did you restart after editing?" When the UI saves a setting, that save is durable and visible to every consumer in the next read — guaranteed by SQLite transactions, not filesystem hope.

---

## 7. Security Model (Holistic)

Grounded in the 2024-2026 reliability/safety research. Starting assumption: **prompt injection is not solved and will succeed in some conversation**. The security model is about containing blast radius to what that conversation's capabilities already permit.

### 7.1 Identity and authentication

Trust is per-principal, not per-conversation. The full ladder and principal schema live in §2.6; the identity-resolution pipeline and cold-contact flow live in §2.14. This subsection covers authentication, controller-identity registration, and security-facing implications.

#### Principal kinds

`Controller`, `Delegated`, `KnownTrusted`, `KnownLimited`, `UnknownPending`, `Blocked`. Plus non-human principals: `Agent`, `Plugin`, `Service`.

#### Stable IDs across transports

Transport plugins produce transport-specific identifiers (Signal UUID, phone number, email, web session). The identity-resolution pipeline (§2.14) maps them to stable `PrincipalId`s. One person reaching the agent through multiple transports is one principal; one trust decision applies everywhere.

#### Controller: authentication and multi-channel identity

The Controller is a single principal with a single `PrincipalId` that can have *many* identifiers across channels — the web UI login, Signal number, email, phone, etc. All of them resolve to the same Controller principal and grant full-trust capabilities, so the controller can message themselves from any channel and have the agent recognize them.

**First-run setup (web UI):**
1. Operator opens the web UI for the first time; it presents a **setup screen** before anything else.
2. Operator sets an **admin password** (Argon2id hashed, stored in `vault_*`).
3. Control plane generates the Controller's `PrincipalId` + an Ed25519 signing keypair (private key stored in the vault).
4. A first JWT session is issued. SPA stores both tokens in `localStorage` and mirrors the access token in an in-memory ref for the apiFetch hot path. Cookie-based + CSRF-token storage was considered and explicitly dropped: the SPA is same-origin (rust-embed serves the bundle), there's no third-party JS in the page, the threat model doesn't include a cooperating XSS vector that localStorage solves but in-memory doesn't, and the extra moving parts aren't worth the complexity.

**Authentication (ongoing):**
- **Login** = admin-password POST → control plane verifies Argon2 hash → issues short-lived **JWT access token** (~15 min, signed with Ed25519) + **refresh token** (~7 days, rotated on use).
- SPA calls `/api/token/refresh` transparently on near-expiry; single-flight so concurrent API calls don't race.
- JWT payload: `{ principal_id, issued_at, expiry, nonce, session_id }`. No roles or capabilities embedded — the control plane resolves capabilities per-request.
- Logout invalidates the refresh token server-side.
- WebAuthn is not a Phase-1 requirement; the plan originally considered it for §8, but JWT + password is what ships first and remains the supported path unless WebAuthn is added later as an optional second factor.

**Registering additional Controller identifiers (multi-channel trust):**

After first-run, the operator adds identifiers they control on other channels so those channels are recognized as them:

1. Admin UI → **Settings → My Identities** → "Add Identity" → pick transport (Signal, email, phone, etc.).
2. Control plane issues a **verification challenge** appropriate to the channel:
   - **Signal**: agent DMs the claimed Signal number with a one-time 6-digit code; operator enters it in the UI.
   - **Email**: email with a signed verification link; single-click confirms.
   - **Phone (voice/SMS)**: SMS code, or voice call reading the code.
   - **Generic webhook/matrix/etc.**: plugin-defined challenge flow.
3. On correct response within a time window, the identifier is bound to the Controller principal in the `principals` table (added to `identifiers: Vec<Identifier>`).
4. Next time a message arrives from that identifier via that transport, `trust_level = Controller` resolves immediately.
5. Every bind is an audit event. Operator can remove a bound identifier at any time; removal broadcasts a `trust_changed` event to transports that cache.

**Why verify each channel:** without verification, anyone who knows the controller's phone number could spoof a message from it (Signal spoofing is non-trivial but possible with compromise; SMS spoofing is easy). The verification step proves *this operator controls this identifier right now*.

**Cryptographic key binding.** The Controller's Ed25519 keypair — not any transport identifier — is the ultimate anchor. Losing a Signal account doesn't un-controller them; losing the private key does. The vault backup/restore flow (§6.4) is how key portability works.

#### Delegated trust

Time-bounded and capability-scoped grants. Default TTL 7 days. No "permanently trusted as me." Revocation is immediate and broadcasts a `trust_changed` event.

#### Cold contacts and Blocked state

Cold contacts never reach the agent before the controller admits them. If no identity-provider plugin auto-trusts and the controller doesn't approve, messages are received and logged but the agent **does not reply** — the conversation parks silently. See §2.14 for the simplified cold-contact flow.

`Blocked` is a universal state — applies equally to a previously-unknown contact the controller rejected and to a previously-trusted principal the controller decided to block. Messages from a Blocked principal are received (for audit) but dropped before reaching the agent; transports that can enforce natively (Signal block list) receive a `trust_changed` broadcast.

#### Trust downgrades and revocations

Auditable events in `state_events`. Reversible (Blocked → UnknownPending → KnownTrusted flows are allowed; any change is an audit event with actor, reason, timestamp).

### 7.2 Capability tokens

- Every runner container launches with a short-lived, signed EdDSA JWT.
- Token payload: `{ conversation_id, turn_seq, principal, capability_set, exp, nonce }`. Bound to a specific turn, not the whole session — the token issued for turn 47 cannot authorize actions claimed as part of turn 48.
- Every tool call from runner → control plane carries the token. Policy engine verifies capability *and* that the claimed turn matches the current open turn for that conversation.
- **Data provenance.** Values the executor produces from untrusted input carry a `tainted` tag that flows with the value across tool boundaries. Policy refuses to pass tainted values into capability-sensitive sinks (cross-conversation sends, secret reads, shared-state writes) unless explicitly untainted by a `ControllerDM` approval. This is CaMeL's per-value capabilities (§2.5).

### 7.3 Policy engine

Policy is a declarative document (TOML). Rules match on **principal trust levels** (sender, addressee, effective) and — as a coarser shorthand — conversation kind. The fine-grained rules win; kinds are for defaults and UI.

```toml
default = "deny"

# ─── By sender / effective trust level (preferred) ───

[[rules]]
match = { sender_trust = "Controller" }
allow = ["*"]

[[rules]]
match = { sender_trust = "Delegated" }
# Capabilities granted by the Delegated.scope at grant time; policy just honors them.
allow_from_delegation = true

[[rules]]
match = { sender_trust = "KnownTrusted" }
allow = ["messaging.*", "calendar.read", "tools.safe", "controller.ask"]

[[rules]]
match = { sender_trust = "KnownLimited" }
# KnownLimited carries allowed_topics / allowed_tools on the Principal itself.
allow_from_principal_scope = true
fall_through_to = "KnownLimited.default"

[[rules.KnownLimited.default]]
allow = ["messaging.reply_current_transport"]
rate_limit = { turns_per_hour = 30 }

[[rules]]
match = { sender_trust = "UnknownPending" }
# Should never actually reach a turn — conversation parks in awaiting_trust_decision.
# If it somehow does, deny everything except the holding auto-reply.
allow = []

[[rules]]
match = { sender_trust = "Blocked" }
# Silently drop. Transport plugins may also block natively.
allow = []
drop = true

# ─── Broadcast floor (min trust in the room when replying to the whole group) ───

[[rules]]
match = { broadcast_min_trust = { "<=": "KnownLimited" } }
# A group reply seen by even one limited principal gets the limited-level treatment.
planner_executor_split = true
forbid_tainted_into = ["messaging.send_new_principal", "vault.read", "state.write_shared"]

[[rules]]
match = { broadcast_min_trust = { "<=": "UnknownPending" } }
require_approval = true

# ─── Rule of Two (§2.2 axiom #9) ───

[rule_of_two]
# At most 2 of these may be true in a single turn for non-Controller senders.
properties = ["untrusted_input_in_turn", "accesses_sensitive_data", "produces_external_effect"]
on_third = "require_approval"

# ─── Kind-based defaults (shorthand, lowest precedence) ───

[[rules]]
match = { conversation_kind = "MixedTrust" }
# Defensive default; per-sender rules above will usually win.
planner_executor_split = true
rate_limit = { turns_per_hour = 40 }
```

Evaluated on every tool call, outbound send, plugin capability check, and memory read. Every decision (match, allow/deny, Rule-of-Two, require_approval) is an event in `state_events`.

### 7.4 Input guard — spotlighting + architectural containment

Band-aid regex and model-based injection detectors are *not* the primary defense (the research is definitive: all known model-level defenses have been adaptively bypassed; static benchmarks are games, not evidence). execlaw's defense-in-depth:

- **Spotlighting** (Microsoft 2403.14720) applied to every inbound untrusted content block: delimited with a random per-conversation token, tagged "untrusted data", encoded so the model can visibly tell "this is something a user said" from "this is a system instruction." Reduces naive-attack ASR dramatically but is not a complete defense on its own.
- **Planner/executor separation** (§2.5) is the architectural containment — it's what actually bounds blast radius when spotlighting is bypassed.
- **Rule of Two** (§2.6, §7.3) — enforced at policy-engine level, independent of any model's judgment.
- **Identity-spoofing pre-filters** before even reaching the model: homoglyph detection, lookalike JID checks, zero-width/bidi-control stripping. Deterministic and cheap.
- **Rate limits + anomaly tripwires** per untrusted principal: burst of tool calls or token usage trips the conversation into `awaiting_approval` automatically.

### 7.5 Human-in-the-loop is part of the security model

Approvals (§2.11) aren't just UX — they're the firewall when Rule of Two trips, when policy marks an operation sensitive, or when an anomaly tripwire fires. Crucial detail: the approval notification goes out over a **sideband transport** (not the originating transport), carries a signed approval token the controller's response must echo, and is recorded as a first-class event. Dropping any of these opens an escalation path for an attacker who already controls the originating transport.

### 7.6 Secret vault

- Encrypted at rest via SQLCipher (§6.4). Whole `execlaw.db` encrypted; master key in OS keyring with passphrase-file fallback.
- Plugins never see raw secrets; they receive opaque references (`secret://signal/device_key`) and the control plane injects the value into the plugin process only at the moment of use, scoped to the plugin's capability grant.
- Rotation is a vault operation (`PRAGMA rekey`); plugins receive a `SecretRotated` event.

### 7.7 What we're accepting we cannot prevent

- **Prompt injection succeeding in at least some turns.** The mitigation is capability scoping + Rule of Two + sideband HITL + planner/executor split so that a successful injection is bounded by the conversation's existing rights.
- **Model-version drift breaking forensic replay.** Exact replay against a drifted model is approximate; our forensic log captures everything *we* control (prompt, capability set, policy decisions) so postmortems are tractable.
- **Sandbox escapes.** An inference runtime could in principle escape container isolation. We mitigate by making runners stateless, capability-scoped, and time-bounded, so an escape steals nothing durable beyond what the token already permitted.

### 7.8 Concrete defenses ported from selfhosted-claw

selfhosted-claw already has a working set of security measures the controller is happy with. execlaw ports them directly (with improvements noted). Each defense has a file reference in the predecessor project for the implementation pattern.

**Inbound content normalization + injection pattern matching** — from [`selfhosted-claw/src/inbound-guard.ts`](../selfhosted-claw/src/inbound-guard.ts) and [`scripts/inbound-message-guard.mjs`](../selfhosted-claw/scripts/inbound-message-guard.mjs). Normalize homoglyphs (Cyrillic `а`→`a`), strip zero-width + bidi-control characters, fold fullwidth Latin and leetspeak; then match against a 45+ pattern list ("ignore previous instructions", "system prompt", "jailbreak", XML/ChatML delimiters, etc.). Line-level stripping removes matching lines; the message is marked `[untrusted-instruction-like content stripped]`. Full-message block on large base64 payloads. Sender display-name checked against `DANGEROUS_SENDER_PATTERNS`. **Execlaw port:** apply to every untrusted content path — inbound transport messages *and* fetched web content in research (not just inbound), with trust-level-dependent strictness.

**Web-fetch SSRF + content-type gating** — from [`selfhosted-claw/src/research/providers.ts`](../selfhosted-claw/src/research/providers.ts) (`validateResolvedHost`, `fetch` body). DNS-resolve the hostname, check each IP against a blocked-prefix list (`10.0.0.0/8`, `127.0.0.0/8`, `192.168.0.0/16`, `172.16.0.0/12`, `169.254.0.0/16` including AWS metadata `169.254.169.254`, `fc00::/7`). Enforce 15s timeout, 10MB max response, 5-hop redirect limit, allowlist content types (`text/html`, `text/plain`, `application/json`, `application/pdf`). Strip `<script>`, `<style>`, HTML comments before the content reaches any prompt. **Execlaw port:** identical in `plugin-url-fetch`, plus an improvement — hidden content (`display: none`, `visibility: hidden`) is also stripped because it can still influence the model.

**Mount allowlist outside project root** — from [`selfhosted-claw/src/mount-security.ts`](../selfhosted-claw/src/mount-security.ts). Security policy file at `~/.config/nanoclaw/mount-allowlist.json` lives *outside* the project root so a compromised container can't edit its own policy. Default-blocked patterns: `.ssh`, `.aws`, `.docker`, `.env`, `.npmrc`, `id_rsa`, `credentials*`. Symlinks resolved with `realpath`; `..` rejected; non-main groups read-only. **Execlaw port:** same file at `~/.config/execlaw/mount-allowlist.json`, with an improvement — changes take effect within 60s (selfhosted-claw only reloads at process restart).

**Tool access gating by lane (capability-scoped in execlaw)** — from [`selfhosted-claw/src/tool-registry.ts`](../selfhosted-claw/src/tool-registry.ts). selfhosted-claw has three lanes (internal/external/audio); tools declare which lanes they're visible in. `controllerOnly: true` tools are hidden from the external lane. **Execlaw port:** the lane concept generalizes to the capability-token system (§7.2) — each tool declares `required_capabilities` in its manifest, and the runner's capability token determines whether the tool is exposed in the turn's `tools` array. Finer granularity than three lanes; same goal.

**Outbound recipient whitelisting** — from [`selfhosted-claw/src/outbound-directives.ts`](../selfhosted-claw/src/outbound-directives.ts) (`resolveSignalTarget`) and [`selfhosted-claw/src/contact-resolution.ts`](../selfhosted-claw/src/contact-resolution.ts) (`resolveLiteralTarget`). Sends require the target to match a principal in conversation history; bare phone numbers not in history are rejected; ambiguous matches throw "Multiple matches" rather than silently picking one. **Execlaw port:** tightened via the principal/trust system. Outbound sends must resolve to a `KnownTrusted`+ principal *and* that principal must have participated in the current conversation (or the controller explicitly approved a new-recipient send). Untrusted conversations can only reply on the originating transport to the originating principal (§2.6 ExternalWithOutsider defaults).

**Idempotency-keyed outbound** — from [`selfhosted-claw/src/egress/agent-send-finalizer.ts`](../selfhosted-claw/src/egress/agent-send-finalizer.ts). Each outbound send is keyed + audit-logged before dispatch; a prior key match prevents replay. **Execlaw port:** already in §2.4 (framework-minted idempotency keys derived from `(conversation_id, turn_seq, tool_call_ordinal)`), improved over selfhosted-claw which used `signal-send:${jid}:${text}` (ambiguous on legitimately identical messages). Our keys are ordinal-based so identical content in different turns gets different keys.

**Audit trail (JSONL + SQLite)** — from [`selfhosted-claw/src/control-store.ts`](../selfhosted-claw/src/control-store.ts) and `cp_audit_logs` table in `src/db.ts`. Every side-effect, capability decision, approval verb, and send event persists. **Execlaw port:** the event log (`state_events`) IS the audit trail — every decision is an event; no separate audit table. Improvement over selfhosted-claw: HMAC-signed events (tamper evidence) via a key in the vault; any post-hoc event modification is detectable.

**Non-root runner containers** — from [`selfhosted-claw/container/Dockerfile`](../selfhosted-claw/container/Dockerfile). Runs as `node` user; explicit workspace directories; minimal mounts. **Execlaw port:** same, with the minimal-containers discipline (§0 axiom #12) — Rust binary + shared libs + CA certs, `USER nobody` after binary install, read-only root filesystem except for specified writable paths, no shell, no package manager in the image.

**Secrets as opaque references** — *improvement over selfhosted-claw*. Today's codebase exposes API keys as env vars that reach the model context. Execlaw: plugins declare `required_secrets` in the manifest; the vault issues opaque refs (`secret://<plugin>/<name>`) to the agent; the actual value is injected only into the plugin process at the moment of use. The prompt never contains a raw key.

**Rate-limit + anomaly tripwires** — *improvement over selfhosted-claw*. selfhosted-claw has no burst-detection. Execlaw tracks per-principal tool-call counts, policy-denial counts, new-contact-attempt counts; bursts above configured thresholds automatically escalate conversations to `awaiting_approval` and emit an Error alert.

**Subagent capability shrinking** — *improvement over selfhosted-claw*. selfhosted-claw's spawned tasks inherit the parent's lane. Execlaw caps sub-runner trust at `KnownTrusted` max regardless of parent (§2.9.1), with a narrower tool-set by default.

**Gaps we're NOT addressing in Phase 1** (documented for future): tamper-detection beyond HMAC (signed offsite backups); full WebAuthn on Controller identity (currently admin password); formally-verified capability system.

---

## 8. Chat-First UI — OpenWebUI-inspired SPA

### 8.1 Design language

The primary interface is a **single-page application** whose **look and feel is modeled on [OpenWebUI](https://openwebui.com)**. We don't use OpenWebUI's code or framework; we just borrow the aesthetic — clean dark-first chat UI, left sidebar for chat list, centered conversation pane with streaming assistant responses, minimal chrome. Build on the existing React 19 + Vite stack ported from selfhosted-claw's admin-ui, restyled.

### 8.2 Landing layout

- **Left sidebar:** chat list (DMs, groups, pinned), search, unread badges, new-chat button, settings gear
- **Center:** conversation viewer with streamed messages (tokens arrive via WebSocket); input at the bottom with file/image attach and voice-call button
- **Right (collapsible):** conversation details — participants, resolved trust levels, tools available in this conversation (from policy), research-session progress, voice-call status
- **Top bar:** controller avatar, notification bell (§10.8 alerts), Admin dropdown (Plugins, Schedules, Personality, Policy, Tools, Skills, Audit, Logs, Identities, Artifacts, Approvals)

### 8.3 First-run and auth (SPA + JWT)

- **First time the UI opens** → setup screen: operator enters an admin password (Argon2id hashed, stored in vault).
- Control plane generates the Controller `PrincipalId` + Ed25519 signing keypair (§7.1).
- Login returns a **short-lived JWT access token** (~15m) + a **refresh token** (~7d, single-use rotation, persisted server-side in `state_refresh_tokens`). SPA holds both in `localStorage`; access token is mirrored in an in-memory ref for the hot path. No HttpOnly cookies, no CSRF tokens — same-origin SPA, no third-party JS, not worth the extra moving parts.
- SPA calls `/api/token/refresh` on near-expiry (background timer at 80% of TTL) AND silently retries once on a mid-action 401, both coalesced behind one in-flight promise.
- Subsequent visits: login form; successful login → chat landing.
- **Additional Controller identifiers** (Signal / email / phone) registered from Admin → Settings → My Identities with per-channel verification (§7.1).

### 8.4 API contract — REST + WebSocket

**REST (OpenAPI 3.x spec published; see Phase 0 deliverable):**

```
# Auth
POST   /api/setup                            first-run admin-password bootstrap
POST   /api/login                            admin-password → JWT
POST   /api/token/refresh                    rotate refresh token
POST   /api/logout                           invalidate refresh token

# Chat
GET    /api/chats                            list conversations
GET    /api/chats/:id/messages?before=...    paginated history
POST   /api/chats/:id/messages               send as controller
POST   /api/chats/:id/read                   mark read
POST   /api/chats/:id/voice/start            start voice session
POST   /api/chats/:id/voice/stop             end voice session

# Admin
GET    /api/admin/plugins                    installed plugins + status
POST   /api/admin/plugins/install            upload ZIP; returns staged plugin_id
POST   /api/admin/plugins/:id/enable
POST   /api/admin/plugins/:id/disable
DELETE /api/admin/plugins/:id                uninstall
GET    /api/admin/deployments                runner deployments
PATCH  /api/admin/deployments/:id
GET    /api/admin/hardware                   detected hardware profile (§5.3)
POST   /api/admin/hardware/rescan
GET    /api/admin/principals                 trust table
POST   /api/admin/principals/:id/trust       set/change trust level
POST   /api/admin/principals/:id/identifiers verify a new Controller identifier
GET    /api/admin/alerts                     active + recent
POST   /api/admin/alerts/:id/ack
POST   /api/admin/alerts/:id/resolve
GET    /api/admin/logs                       filtered log viewer
GET    /api/admin/audit                      state_events queries
POST   /api/admin/replay/:conversation_id    rebuild past-turn context for forensics
```

**WebSocket (AsyncAPI spec published):**

```
WS /api/stream     unified live event stream:
  chat.message_inbound / chat.message_outbound / chat.token_delta
  chat.voice.* (user_utterance, agent_utterance, interrupt, rescinded)
  agent.thinking / agent.tool_use / agent.tool_result / agent.wakeup_scheduled
  research.session_started / research.progress / research.complete
  conversation.presence_changed / conversation.trust_changed
  plugin.status / plugin.installed / plugin.uninstalled
  alert.fired / alert.acked / alert.resolved
  container.lifecycle
  hardware.rescan_complete
```

The WebSocket replaces the 60s polling selfhosted-claw did on the dashboard; everything live. Voice sessions are just filtered subscriptions by `conversation_id`.

### 8.5 Port strategy for existing features

| Feature | Path in new UI | Port effort |
|---|---|---|
| Dashboard metrics | Admin → Dashboard | Low (rebuild REST queries) |
| Plugins list + detail | Admin → Plugins | Medium — new manifest shape, ZIP-upload install flow |
| Routines (scheduled / recurring automations, formerly "Scheduled tasks") | Sidebar → Routines | Low |
| Personality | Admin → Personality | Low |
| Policy | Admin → Policy (trust + capability editor) | Medium — new engine |
| Tools | Admin → Tools (read-only view of registered tools) | Low |
| Skills | Admin → Skills | Low |
| Audit (event log) | Admin → Audit | Medium |
| Logs | Admin → Logs | Low (same JSONL+SQLite pattern as selfhosted-claw) |
| Approvals | Badge on top bar + inline in chat | Medium |
| Setup wizard | First-run overlay; per-plugin setup inside plugin's UI panel | Medium |
| Phone voice tester | Collapsed into conversation "Start voice" button | Low |
| Contacts / Identities | Admin → Identities (trust table) | Low |
| Files / Artifacts | Admin → Artifacts (research PDFs, attachments) | Low |
| My Identities (multi-channel controller) | Admin → Settings → My Identities | **New** — not in selfhosted-claw |
| Alerts / Notifications | Bell dropdown + dedicated page | Medium — replaces 60s poll with WS |
| Hardware + Deployments | Admin → Hardware | **New** — from Phase 0

---

## 9. Voice Parity

Voice is a single first-class modality — a streaming STT → LLM → TTS pipeline with VAD-driven barge-in and backchannel rescind, modeled after Pipecat / LiveKit Agents patterns adapted to Rust. Architectural detail is in §2.13; this section is the shipping strategy.

**Default component stack (all self-hosted, all OSS):**

| Stage | Component | Default on the hybrid dev target |
|---|---|---|
| VAD | Silero VAD ONNX | CPU (~1ms per 30ms frame) |
| Echo cancellation | WebRTC AEC3 | CPU (mandatory when using speakers) |
| STT | Whisper (small.en int8) via `service-whisper` | Intel Arc (OpenVINO GenAI WhisperPipeline) — or CUDA (faster-whisper) on nvidia-only hosts |
| Turn detector (semantic EoT) | LiveKit turn-detector (Qwen2.5-0.5B ONNX, Apache-2.0) | CPU |
| LLM | `runner-local` against configured inference backend | nvidia (vLLM + `QuantTrio/Qwen3.5-27B-AWQ`) |
| TTS | **Kokoro-82M** via `service-kokoro` | Intel Arc (OpenVINO 2025.2 with ISTFT-GPU) or nvidia (ONNX+CUDA) — both supported, both first-class |
| TTS fallback | Piper via `service-piper` | CPU-first; low-latency airbag |

**Key design decisions:**
- **One voice path.** No separate "realtime" modality. Cascaded STT → LLM → TTS is the self-hosted ceiling in 2026; we accept the ~700-1100ms EoS-to-first-audio latency (vs. ~300-500ms for cloud native-audio) in exchange for portability, privacy, Apache-licensed components, and dual-GPU support. See §2.13.6 for the honest delta.
- **Distributed pipeline across GPUs.** On hybrid nvidia+Intel, the voice-peripheral stack (STT, TTS) runs on Intel Arc via OpenVINO; the LLM runs on nvidia via vLLM; CPU handles VAD, turn-detector, AEC. The hardware gets fully used.
- **TTS = Kokoro, not F5TTS.** Replaces the F5TTS default from selfhosted-claw because Kokoro streams natively, is Apache-2.0 (vs F5TTS CC-BY-NC which blocks commercial), ports cleanly to both OpenVINO and CUDA (OpenVINO 2025.2 shipped the ISTFT-GPU operator specifically for Kokoro), has a maintained Rust port, and is half the size.
- **STT = Whisper retained**, with two backends (faster-whisper on CUDA, OpenVINO GenAI on Intel) behind one trait.
- **Warm pipeline pinned per active call.** Cold-starting any stage is fatal for voice UX.
- **Barge-in with rescind.** Silero VAD + system-lane Interruption frames + 120ms backchannel-rescind window (§2.13.3).
- **Planner/executor preserved for untrusted voice.** STT transcripts get spotlighting; executor has no tools; Rule of Two applies per turn.
- **Sub-agent escalation** (§2.9 case 3) works identically in voice — primary emits a filler utterance while the deep runner (larger local model on nvidia) grinds.

**Shared semantics with text:**
- A voice session is a conversation with `modality = Voice`. Same persistence, same conversation types, same policy, same audit trail, same forensic replay.
- Start a voice call on any text conversation; modality flips; transcripts stay in `state_events`. End the call; modality flips back to text seamlessly.

This kills the split-brain in selfhosted-claw where `voice-runner/` had its own container pipeline — and keeps the voice path honest about what the self-hosted OSS ecosystem can actually deliver.

---

## 10. Alerting and Operational Health

Selfhosted-claw had no coherent alert surface — expired OAuth tokens, unreachable providers, plugin crashes, and outbox failures tended to either fail silently, spam logs, or surface only after the controller went looking. execlaw makes this first-class.

Design draws on proven patterns:
- **Alertmanager** — fingerprint-based dedup, severity routing, inhibition, silencing
- **Sentry** — issue grouping via stable fingerprint, event-under-issue aggregation
- **PagerDuty / OpsGenie** — explicit lifecycle (firing → acknowledged → resolved)
- **Prometheus** — health probes with for-duration thresholds to avoid flaps

Simplified to execlaw's scale (single controller, self-hosted, one ops surface).

### 10.1 What is an alert (vs. an event, vs. an approval)

| Concept | Scope | Required action |
|---|---|---|
| **Event** (`state_events`) | Forensic log of everything | None — log only |
| **Alert** (`state_alerts`) | Operational condition the controller should know about | Acknowledge / resolve / act, non-blocking |
| **Approval** (§2.11, §2.14) | Agent is paused pending a controller decision | **Blocks** until controller responds |

Alerts and approvals share infrastructure — sideband notification, UI surfacing, signed action tokens — but differ in semantics: alerts inform, approvals block.

### 10.2 Data model

```sql
CREATE TABLE state_alerts (
    id              TEXT PRIMARY KEY,              -- UUID
    fingerprint     TEXT NOT NULL,                  -- dedup key (see §10.3)
    severity        TEXT NOT NULL,                  -- Critical | Error | Warning | Info
    source          TEXT NOT NULL,                  -- plugin.<id> | core.<subsystem>
    title           TEXT NOT NULL,                  -- one-line human summary
    detail          TEXT,                           -- Markdown-capable longer description
    context_json    BLOB,                           -- structured data (expiry date, URL, counts)
    status          TEXT NOT NULL,                  -- Firing | Acked | Resolved | Snoozed
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    occurrence_count INTEGER NOT NULL DEFAULT 1,
    resolved_at     INTEGER,
    resolved_by     TEXT,                           -- 'auto' | principal_id
    ack_at          INTEGER,
    ack_by          TEXT,
    snooze_until    INTEGER,
    incident_id     TEXT,                           -- optional grouping (see §10.3)
    actions_json    BLOB,                           -- suggested remediation actions
    UNIQUE (fingerprint, status) ON CONFLICT IGNORE -- dedup: identical firing alert re-uses row
);

CREATE INDEX idx_alerts_status_severity ON state_alerts(status, severity, last_seen_at);
CREATE INDEX idx_alerts_source ON state_alerts(source, last_seen_at);

CREATE TABLE state_incidents (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    first_seen_at   INTEGER NOT NULL,
    resolved_at     INTEGER,
    alert_count     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE state_alert_silences (
    id              TEXT PRIMARY KEY,
    matcher_json    BLOB NOT NULL,                  -- e.g. {"source": "plugin.signal", "severity": "Warning"}
    expires_at      INTEGER,                        -- NULL = indefinite (rare; discouraged)
    created_by      TEXT NOT NULL,
    reason          TEXT
);
```

State transitions append to `state_events` with `kind` in `{alert_fired, alert_renotified, alert_acked, alert_resolved, alert_snoozed, incident_opened, incident_closed}`. Full history is replayable.

### 10.3 Fingerprint and grouping

Every alert has a **fingerprint** — a stable identifier such that the "same problem happening again" produces the same fingerprint. Shape: `<source>:<kind>:<specifier>`.

Examples:
- `plugin.google-calendar:oauth_expired:controller_account`
- `core.container-manager:service_unhealthy:service-openarc`
- `core.outbox:delivery_failed:idempotency=abc123`
- `plugin.signal:rate_limit_hit:group_jid=...`

Dedup rule: if a `firing` alert with the same fingerprint already exists, increment `occurrence_count`, update `last_seen_at`, emit an `alert_renotified` event. Do not create a new row, do not re-notify the controller until a cooldown has elapsed (§10.5).

**Incident grouping.** Optionally, related alerts fold under a parent `incident`. Example: if `plugin.google-calendar:oauth_expired` fires, every subsequent `plugin.google-calendar:tool_failed:<tool>` caused by the missing token auto-groups under the OAuth incident. Incident heuristics:
- Alerts from the same source within N minutes, when one has severity ≥ the other's cause
- Explicit `caused_by` link when the emitter knows (e.g., tool-failed alerts can reference the OAuth alert id)

### 10.4 Severities

| Severity | Example | Default routing |
|---|---|---|
| **Critical** | All runners failing; DB quota exceeded; vault unreachable; every transport offline | Immediate sideband (§10.6), page-like |
| **Error** | One plugin unhealthy; OAuth expired and cannot auto-refresh; outbox DLQ > threshold | Sideband within 5-15 min if not acked |
| **Warning** | OAuth expires in 3 days (auto-refresh not available); disk 80%; rate limit close to limit | Daily digest; UI badge |
| **Info** | New plugin version available; backup completed; model update pending | UI only |

Operator-configurable in `config_alert_routing`.

### 10.5 Lifecycle

```
           ┌──────────────────────────────────────────┐
           │                                           │
  firing ──┼──▶ Active ──ack─▶ Acked                   │
           │      │              │                     │
           │      │              └──snooze─▶ Snoozed ──┤
           │      │                                    │ auto-resolve
           │      └───────────resolve──▶ Resolved ◀────┘ (condition cleared)
           │                                 │
           └─────────────renotify────────────┘
                (if re-fires while Active/Snoozed
                 the counter ticks; renotify on cooldown)
```

- **Auto-resolve.** When the underlying condition clears (OAuth refreshed, service healthy again, rate-limit window passed), the emitter reports `clear`; the alert moves to `Resolved` with `resolved_by='auto'`.
- **Manual resolve.** Controller can mark resolved from the UI or via signed sideband response. Future re-firing produces a *new* alert (fresh row, fresh first_seen_at), since the controller believes the condition is fixed.
- **Ack.** Suppresses re-notification but keeps the alert active in the UI. Used when the controller sees the alert and is working on it.
- **Snooze.** Time-bounded suppression ("remind me in 4 hours"). Defaults: 1h / 4h / 1d / until-resolved.
- **Renotify cooldown.** Default: renotify no more than once per hour for Error, once per 15min for Critical, once per day for Warning. Configurable.

### 10.6 Sources (who emits)

Every emitter calls `Alerter::emit(AlertSpec)` in Rust. Sources:

1. **Plugins**, via the plugin host's `report_alert` capability (declared in manifest). Plugins emit their own operational problems: OAuth failure, provider error, rate limit, bad config.
2. **Health checks**, plugin-declared. Manifest example:
   ```toml
   [[health_checks]]
   name = "google_calendar_reachable"
   interval = "5m"
   timeout = "10s"
   failure_threshold = 3       # N consecutive failures before firing
   recovery_threshold = 2      # N consecutive successes to resolve
   probe = { kind = "http", url = "https://www.googleapis.com/...", expect_status = 200 }
   on_fail_severity = "Error"
   ```
   The control plane runs these; state transitions emit / resolve alerts automatically.
3. **OAuth monitor** (core): for each OAuth-using plugin, track token expiry. Warning at 7d/3d/1d out when auto-refresh is not possible; Error when expired or revoked. Emits with `[Reconnect]` action.
4. **Container manager**: service unhealthy after M retries, OOM kill, runner crash loop (N crashes within window).
5. **Outbox relay**: delivery failure after max retries → move to DLQ + Error alert; DLQ depth > threshold → Warning.
6. **Policy engine**: repeated Rule-of-Two denials from the same conversation → Warning; capability-violation bursts → Error.
7. **Agent core**: runner spawn failures, session corruption detected, replay mismatch.
8. **Storage watchdog**: SQLite size approaching configured limit; WAL checkpoint lag; disk pressure.

### 10.7 Remediation actions

Every alert can carry suggested actions. Stored as `actions_json`; rendered in UI as buttons and in sideband as reply verbs.

```json
{
  "actions": [
    {
      "id": "reconnect",
      "label": "Reconnect Google",
      "verb": "reconnect_oauth",
      "capability": "plugin.google-calendar.oauth.reconfigure",
      "payload": { "plugin_id": "google-calendar", "account_id": "..." }
    },
    {
      "id": "disable",
      "label": "Disable plugin",
      "verb": "plugin_disable",
      "capability": "plugin.lifecycle",
      "payload": { "plugin_id": "google-calendar" }
    },
    {
      "id": "view_logs",
      "label": "View logs",
      "verb": "navigate",
      "payload": { "url": "/admin/logs?plugin=google-calendar" }
    }
  ]
}
```

Action execution is capability-gated like any other tool call (§7.2). One-click from the UI; for sideband the controller replies with a signed verb the transport plugin translates into the same capability check.

### 10.8 Delivery — UI dropdown primary, Signal fallback

Keep it simple, mirroring selfhosted-claw's pattern:

- **Primary: UI notification dropdown** in the chat-first SPA. Live-updating via the WebSocket event stream (`plugin.status`, `alert.fired`, `alert.resolved` events). Unread-count badge on a bell icon in the top bar; click opens a dropdown listing active alerts with inline action buttons. An Alerts page exposes full filtering (severity, source, status) and history.
- **Fallback: Signal DM to controller** when (a) the alert is `Critical` and the UI hasn't acked it within 5 minutes, or (b) the operator explicitly enables sideband for a given alert source. The Signal fallback reuses the sideband approval mechanism (§2.11) so the controller can approve/resolve from their phone. If the Signal plugin itself is the alert source, the alert is UI-only (no point alerting about Signal via Signal).
- **No per-severity routing rules, no per-source overrides, no digest emails.** A single operator watching a single UI does not need the routing complexity of Alertmanager. Keep the knobs minimal: one "enable Signal fallback for Critical alerts" toggle, one "unack-timeout-before-Signal" duration.

### 10.9 Silencing and maintenance mode

- **Silences** (`state_alert_silences`) match alerts by source/severity/fingerprint-pattern and suppress notification for a bounded window. UI-creatable; mandatory expiry (discouraging "silence forever" footguns; the only way to silence indefinitely is a "disable this alert class" setting, separate from silences).
- **Maintenance mode** (global) — operator explicitly marks "I'm upgrading the system"; non-critical alerts queue quietly and deliver a single summary when maintenance ends. Auto-expires after a configurable max window.
- **Plugin-scope mute** — "I know `plugin.foo` is broken, stop pinging me" — equivalent to a silence on `source = plugin.foo` with a default 24h expiry.

### 10.10 OAuth-specific flow (concrete worked example)

This is the user's motivating example. OAuth-using plugins declare:

```toml
[[oauth_accounts]]
name = "controller"
scopes = ["calendar.readonly", "calendar.events"]
provider = "google"
token_store = "vault://google-calendar/controller"
proactive_refresh_window = "10m"   # refresh this far ahead of expiry
warn_before_expiry = ["7d", "3d", "1d"]   # emit Warning alerts at each threshold if refresh isn't working
```

Control-plane OAuth monitor behavior:
1. Tracks `access_token` and `refresh_token` expiries per account.
2. Proactively refreshes within the configured window. Success → no alert.
3. On refresh failure with retryable error → retry with exponential backoff; after N attempts with no success → emit `Error` alert `plugin.<id>:oauth_refresh_failing:<account>`.
4. On refresh failure with non-retryable error (revoked, invalid_grant) → emit `Error` alert `plugin.<id>:oauth_expired:<account>` with a `[Reconnect]` action.
5. When refresh-token itself is near expiry and can't be auto-renewed → emit `Warning` per the `warn_before_expiry` schedule. Escalates to `Error` when the window closes.
6. Downstream tool calls that need this OAuth fail gracefully with a structured error that *references the alert id*; the agent's error-handling prompt surface includes "Tool unavailable — alert §ALERT-ID is active"; the agent can tell the user "I can't check your calendar right now, the controller needs to reconnect Google."
7. On successful reconnect (controller uses the action button), the plugin emits `oauth_restored` → all related alerts auto-resolve; incident closes; agent resumes using the tool.

Every step appends events; the full OAuth incident is replayable from the log.

### 10.11 UI surface

- **Bell icon** in the top bar with unread count; clicking opens a panel of active alerts.
- **Alerts page** under Admin → Alerts (replaces selfhosted-claw's ad-hoc "Notifications"): filter by severity / source / status; per-alert detail view with history and action buttons.
- **Inline badges** on the relevant admin pages (e.g., a Plugins page shows a warning triangle on google-calendar when it has an active alert).
- **Chat-first integration**: critical alerts can surface as a system message in the ControllerDM conversation, with one-click actions inline — so the same interface that handles conversations handles ops.

### 10.12 Observability of the alert system itself

The alert system needs its own reliability posture:
- `core.alerts:emit_failed` — if emit can't write to SQLite (disk full) — emitted with a disk-bypass sideband path.
- `core.alerts:sideband_delivery_failed` — if a notification can't be delivered; escalates via alternate sidebands.
- Metrics: emit-to-notify latency (SLO: Critical < 30s), alerts-open count, alerts-resolved-last-24h.

### 10.13 Where this lands in phases

- **Phase 1** — alert table + emit API + UI badge (alert-count only, list deferred). Core agent can emit from this phase forward.
- **Phase 2** — plugin host exposes `report_alert` capability; health-check manifest fields wired up for the first plugins (`transport-signal`).
- **Phase 3** — sideband routing reuses the approval-flow mechanism; silences; OAuth monitor (for any OAuth-using plugin added in Phase 3+).
- **Phase 5** — full alerts UI, digest delivery, per-source routing rules, metrics for the alert system itself.

---

## 11. Migration Phases

Each phase ends with an executable milestone. No phase is "just plumbing" — every one lands a demoable slice.

### Phase 0 — Foundation + local inference + GPU-aware deployment (3-4 weeks)

Self-hosted inference is core, not a Phase 7 extra. The hardware profile and runner-deployment machinery land here so every subsequent phase develops against a real local model on real hardware.

- Rust workspace scaffolding (`execlaw-core`)
- **`Dockerfile.control-plane`** producing a minimal Linux image (Rust binary + shared libs + CA certs + embedded PCI ID DB, per axiom #12) — the deployment artifact
- **`docker-compose.yml`** orchestrating the control-plane container + Docker socket mount + sysfs bind-mounts + volumes for `execlaw.db` and the keyring-agent socket — the operator-facing entrypoint, matching selfhosted-claw's `docker-compose.control-plane.yml` pattern
- **`execlaw up` CLI** = thin wrapper over `docker compose up` with setup-wizard first-run flow
- SQLite + migration tool (`refinery` or `sqlx-migrate`), `rusqlite` + `bundled-sqlcipher-vendored-openssl` wired up
- OS-keyring integration with passphrase-file fallback (keyring accessed via host-socket bind-mount, not in-container keyring)
- `bollard` client + `container-manager` crate with spawn/stop/logs, event bus, and **tiered hardware-profile detection** (§5.3): Tier 1 sysfs reads with embedded PCI ID DB, Tier 2 `docker info` runtime check, Tier 3 one-shot probe containers (`probe-nvidia`, `probe-intel`), Tier 4 manual override. No vendor GPU tooling in the control-plane image (axiom #12).
- `inference-api` crate — OpenAI-compatible client (streaming SSE, function calling); the single internal LLM contract
- **Runner deployment registry** (§5.4) with `config_runner_deployments` SQLite table + `execlaw deployments` CLI subcommand for list/set/start/stop
- Admin UI `/api/admin/hardware` endpoint + readonly hardware view
- Axum server with `/api/health` and WS `/api/stream` echo
- `tracing` with JSON layer → `~/.execlaw/logs/*.jsonl` + SQLite `log_entries` table (selfhosted-claw pattern)
- OS-keyring integration with **passphrase-file fallback** at `~/.execlaw/master.key` (headless / CI deploys + first-run wizard write the file)
- CLI (`execlaw up`, `execlaw doctor`, `execlaw db migrate`, `execlaw hw rescan`, `execlaw deployments list/set/start`, `execlaw vault rekey`, `execlaw plugin install <zip>`)
- **API specs:** `utoipa`-annotated Axum routes produce a live **OpenAPI 3.x (Swagger) spec** at `/api/openapi.json`; a **hand-maintained AsyncAPI 3.x spec** at `/api/asyncapi.json` documents the `/api/stream` WebSocket event vocabulary. Both specs are served at `/api/docs` as Swagger UI + AsyncAPI viewer.

**Moved out by the 2026-04-24 refactor (anything external-service-dependent → Phase 8; anything UI → Phase 6):**

- **`service-vllm` / `service-openarc` service plugins** → Phase 8. Both wrap external runtime software (vLLM, OpenArc) the control plane doesn't bundle; per the refactor any plugin needing external services lives in Phase 8.
- **Setup wizard** (interactive HardwareProfile → propose deployments → operator accepts) → Phase 6 (chat-first UI). The CLI exposes `execlaw deployments` for headless setup; the wizard is the UI surface on top of it.
- **`execlaw up` real-hardware demo** (detects nvidia + Intel Arc, services boot, inference routes a request, etc.) → Phase 8 acceptance, since it depends on the moved-out service plugins.

- **Phase 0 demo (internal):** `execlaw db migrate` brings the schema up; `execlaw hw rescan` returns the sysfs-detected `HardwareProfile` JSON; `execlaw deployments list` reads the registry; `execlaw plugin install <zip>` installs the in-tree reference plugin and `GET /api/admin/plugins/tools` shows its tool; `GET /api/admin/hardware` returns the same profile the CLI does; `/api/docs` renders both specs; the JSONL log file lands at `~/.execlaw/logs/execlaw.jsonl` with one row per tracing event AND a matching row appears in `log_entries`.

### Phase 1 — Agent core with one transport (3-4 weeks)

The **hard, load-bearing phase** — this is where the agent model lives or dies. Runs against the local LLM from Phase 0.

- **Event log schema** (`state_events`, `state_conversations`, `state_outbox`, `state_inbox`) + replay
- **Snapshotting** every ~50 events + constant-time resume
- **Turn-as-transaction** commit semantics with the `tool_use`/`tool_result` pairing invariant (cancellation result synthesis on failure)
- **Outbox relay** as a separate async task with framework-minted idempotency keys and exponential backoff
- **Wakeup scheduler** (priority queue + `tokio::sync::Notify`, sub-second precision) + `schedule_wakeup` agent tool that turns into a wakeup-table row + drives the scheduler's notify
- **`runner-local` crate** — in-tree runner that speaks OpenAI-compatible API to whichever inference backend is configured. Implements session hydration from `state_events`, function-call-based memory tool exposure, streaming, tool use, interrupt/resume, compaction. **No cloud SDK.**
- **Memory-tool shim** mapping function-call `read_memory`/`write_memory` to scoped `memory_entries` rows with trust-class enforcement at the shim layer
- One transport: **web chat** (no auth, local dev, WS bidirectional)
- **Crash tests** as acceptance: (a) kill control plane mid-turn → restart → turn resumes correctly, no dangling `tool_use`, no double-send on the outbox; (b) agent calls `schedule_wakeup(30s, note)` and resumes within ≤1s of target; (c) kill runner mid-tool-call → cancellation result committed, next turn proceeds cleanly.

**Moved out by the 2026-04-24 refactor:**

- **Minimal chat UI** (send/receive widget) → Phase 6 chat-first UI. The REST + WS APIs are stable; the React SPA work is its own track.

- **Phase 1 demo (internal):** Chat with the agent against the local inference backend (set `EXECLAW_INFERENCE_URL`). Trigger crash tests (a) and (c) via the integration test suite (`cargo test --workspace --test plugin_lifecycle --test approval_flow`). Trigger (b) by directly inserting a `schedule_wakeup` row and observing the `Wakeup` event committed within ~1 s of the target. Airplane-mode: disconnect network, everything still works (run the test suite with no network).

### Phase 2 — Plugin framework (2-3 weeks)

**Scope-pinning (2026-04-24 refactor):** Phase 2 is **framework only** — the
machinery for installing, isolating, and invoking plugins. The actual
plugin ports (`plugin-signal`, `plugin-google-calendar`, `plugin-voice`,
all the research/search plugins, etc.) live in **Phase 8 — External
integrations**, since every one of them needs credentials, external
services, or third-party SDKs that can't be built against in an
airplane-mode dev loop. Earlier drafts conflated the two and made Phase
2 "done" depend on external-service work landing; that's fixed by
separating them.

What lands in Phase 2:

- `plugin-sdk` Rust traits + manifest schema (hook points per §4.2) with
  the `[runtime]` table declaring isolation tier + entrypoint.
- `plugin-host`: `HookRegistry` (per-hook lookup maps, all-or-nothing
  `enable` with conflict detection) + `SubprocessPlugin` tier (tier 2
  per §4.4 — lowest barrier for porting Node integrations).
- `PluginHost` lifecycle: install / enable / disable / uninstall /
  hydrate, with SQLite persistence so installs survive restart.
- ZIP-upload install flow end-to-end (`POST /api/admin/plugins/install`)
  with zip-slip defense + manifest validation.
- Native function-call tools exposed by plugins (not MCP — see §4.3).
- **Capability-enforced dispatch bridge**: the runner's `ToolDispatch`
  chains built-ins → plugins, rejecting calls that lack the required
  capability before the subprocess sees any args.
- Capability-token issuance for runners (per-turn-bound, EdDSA-signed)
  — ported up from Phase 1 (already lands there).
- HMAC-signed event log (tamper evidence on audit trail) — also
  shipped in Phase 1.
- Plugin inventory enumeration document (`docs/plugin-inventory.md`):
  every `src/integrations/*.ts` in selfhosted-claw classified into
  bucket A (port as plugin in Phase 8), B (fold into core), or C
  (retire). Drives Phase 8's queue.

What's **out of scope** for Phase 2:

- Porting any real selfhosted-claw integration (moved to Phase 8).
- Anything that requires a Signal CLI, Google OAuth creds, a search
  API key, a cloud service, or a third-party SDK.

- **Demo:** Install an in-tree reference plugin via
  `POST /api/admin/plugins/install`; the tool it declares shows up in
  `GET /api/admin/plugins/tools`; a turn can call it; `disable` drops
  it from the registry + kills the subprocess; server restart +
  `PluginHost::hydrate` brings enabled plugins back automatically.
  A plugin declaring a tool with `required_capabilities = ["admin"]`
  is rejected at dispatch time when the caller lacks that capability.

### Phase 3 — Participants, trust, policy engine, Rule of Two (3-4 weeks)

Where the participant-aware security model lands end-to-end.

The security model itself is **internal**: it runs against whatever
transports are installed. Phase 3 lands the full trust ladder + policy
engine + cold-contact flow, verifying each invariant against the
web-chat transport (always available) and an in-tree reference identity
provider. The **cross-transport** demos (sideband approvals that hop
from one transport to another, Signal cold-contacts, Google-Contacts
auto-trust) prove themselves in Phase 8 once the real transport/identity
plugins land — at which point the same policy engine from Phase 3 is
exercised end-to-end without any code change.

- **Trust ladder** (§2.6) in the `principals` table with the full enum (`Controller` / `Delegated` / `KnownTrusted` / `KnownLimited` / `UnknownPending` / `Blocked`)
- **Derived conversation kinds** — including the `MixedTrust` case
- **Controller identity** bound to a cryptographic key; first-run key ceremony
- **Identity-provider plugin contract** (§2.14) + in-tree reference plugin `identity-local-address-book` (UI-managed contact list, no external deps). Third-party identity providers (`identity-signal-safety-numbers`, `identity-google-contacts`) ship in Phase 8.
- **Cold-contact escalation flow** (§2.14) end-to-end: holding auto-reply → sideband notification → controller verb (`Trust` / `TrustLimited` / `Deny` / `IgnoreOnce`) → persistence → conversation resumes. Verified against web-chat + a second in-tree mock transport for the sideband hop; the Signal-over-email variant demonstrates in Phase 8.
- **`config_trust_policy` table** with all documented defaults, UI-editable
- **Policy engine** evaluates per-turn on `sender_trust` / `addressee_trust` / `effective_trust` / `broadcast_min_trust` / `conversation_kind`
- **Rule of Two enforcement** per turn
- **Planner/executor split** for any turn with `effective_trust < KnownTrusted`
- **Spotlighting** on ingress for all content from non-trusted senders
- **`ask_controller` sideband flow** over a different transport than the originating one, signed approval tokens (shares mechanism with cold-contact approval)
- **Trust-class-scoped long-term memory** (retrieval key includes trust class)
- **Demos (all internal, no external services required):**
  - (a) A simulated "unknown sender" inbound via web chat triggers the holding auto-reply; the controller gets a sideband notification via the in-tree mock transport; controller approves; conversation resumes with the original message.
  - (b) Install `identity-local-address-book`, add a contact, have that contact message the agent through web chat — auto-trusted without prompting.
  - (c) Adversarial: injection attempt in an outsider conversation fails to pull a Controller-scoped memory (already tested at the unit level; Phase 3 runs it through the live HTTP + policy stack).
  - (d) Revoke trust on an existing contact — mid-conversation turn finishes, conversation archives, future messages dropped.
  - (e) Rule of Two breach triggers an approval request that the controller must resolve before the turn commits.
  - Signal / cross-transport / group-conversation demos land in Phase 8 once `plugin-signal` ships.

### Phase 4 — Voice pipeline primitives (3-4 weeks)

Pure-Rust voice stack: the two-lane Tokio graph, punctuation-aware
endpointer, barge-in rescind logic, and the streaming-frame vocabulary.
These primitives are **internal** and already partially landed in the
current foundation (§2.13 primitives + tests).

Voice acceptance against a real mic/speaker — the ≤1.1s EoS-to-first-
audio demo, Kokoro TTS on the actual GPU path — requires
`service-whisper` and `service-kokoro` container images, which are
external-integration work and ship in Phase 8. Phase 4 therefore
verifies everything *up to* the I/O boundary: VAD decisions against
recorded audio fixtures, endpointer classification, barge-in decision
tables, two-lane preemption ordering, event-schema wiring, spotlighting
on simulated transcripts, and sub-agent escalation logic.

- **`voice-pipeline` crate** — two-lane Tokio graph (system + data lanes), Pipecat-style frame vocabulary, warm-pipeline pinning per active call
- **Silero VAD ONNX** integration wired into the pipeline (threshold 0.6, min_speech 150ms, min_silence 300-500ms). VAD runs in-process against the audio stream; no external service needed.
- **Audio I/O trait** (`AudioIn` / `AudioOut`) with a mock implementation for tests. The real `transport-voice` plugin that owns device access lands in Phase 8.
- **Endpointer** (punctuation-aware, shipped) + **barge-in decision logic** (shipped) + **backchannel rescind** (shipped)
- **Voice event schema** (`voice.session_started`, `audio.in_chunk`, `vad.speech_started/ended`, `stt.partial/final`, `turn.user_ended`, `llm.token`, `llm.cancelled`, `tts.first_audio`, `tts.audio_chunk`, `tts.ended`, `interrupt.started/rescinded/confirmed`) wired to `state_events`; PCM blobs in separate `blobs` table
- Modality-adaptive behavior: extended thinking off, response-length budget, tool-call budget (`latency: low` only), context slice
- Planner/executor split preserved for `ExternalWithOutsider` voice (STT-transcript spotlighting)
- Sub-agent escalation (§2.9 case 3) — primary emits filler while deep runner grinds — logic verified against mock runners
- **Acceptance (internal):**
  - Two-lane preemption: system-lane `Interruption` arrives at every stage before any queued data-lane frame, verified under data-lane backlog.
  - Endpointer classifies terminal vs mid-thought tail punctuation correctly.
  - Barge-in + backchannel decision table covers Wait / Rescind / Confirm.
  - Event-schema round-trips through the log with HMAC signing intact.
  - Spotlighting strips smuggled delimiters from simulated STT transcripts.
  - Crash mid-turn still commits paired cancellation results.
- Real-audio acceptance (≤1.1s EoS → first audio, Kokoro on Intel Arc + nvidia, WebRTC AEC3, live barge-in over a mic) lands in Phase 8 with `service-whisper` + `service-kokoro` + `transport-voice`.

### Phase 5 — Observability, evaluation, and the replay CLI (2 weeks)

New phase, promoted out of "Hardening" because it pays off immediately for debugging the earlier phases' issues in production.

**Scope boundary (2026-04-24 refactor):** Phase 5 is **infrastructure
only** — the data layer + HTTP routes + CLI commands that surface the
observability and replay primitives. Every UI component (log viewer,
trace viewer, eval dashboard, regression-flag chip) lands in Phase 6
when the React SPA is built; Phase 5 stops at "the data is queryable
through stable APIs."

What lands in Phase 5:

- **Tracing → SQLite `log_entries`** layer: a custom
  `tracing_subscriber::Layer` impl that converts each event into a
  `LogRow` and writes via `LogStore`. Closes the Phase-0 deferral
  (file-only logging) so the admin UI in Phase 6 has a queryable
  data source.
- **`GET /api/admin/logs`** with `level` / `plugin_id` /
  `conversation_id` / `since` query params + pagination. The data
  surface the Phase-6 log viewer renders.
- **`execlaw replay <conversation_id> --at <seq>`** CLI command:
  hydrates the conversation up to the target seq, runs
  `policy::evaluate_turn` with the recorded sender trust, prints
  the exact prompt the model saw (system + history + user_msg),
  the `TurnPolicyDecision` (capabilities, planner_executor,
  spotlighting, latency_band), and the events `commit_turn`
  produced for that turn.
- **`eval_flagged` table** (migration 0004) + `EvalFlaggedStore` in
  `execlaw-core` with insert/list/by_label.
- **`execlaw eval flag <conv> --range <a..b> --label <name>`** CLI to
  tag event ranges. Companion `execlaw eval list [--label X]`.
- **`GET /api/admin/eval/flags`** for the Phase-6 eval-dashboard
  data feed.
- **LLM-judge harness scaffolding**: a small Rust binary that runs
  rubric prompts against a local OpenAI-compatible endpoint (the
  configured Qwen). Rubric files live in `evals/rubrics/`. The
  harness is invoked manually or by a nightly job (no built-in
  scheduler — that's the operator's `cron`). CI runs the harness
  against a mock endpoint so we don't need a live model.

What's out of scope for Phase 5:

- Any React component, page, or chart — Phase 6.
- Chat-route surfacing of replay results — Phase 6.
- Eval-dashboard rendering — Phase 6.
- Setup-wizard UI — Phase 6 (already moved out by the prior
  refactor).

- **Phase 5 demo (internal):** `execlaw replay <conv> --at 47` prints
  the exact prompt the model saw at turn 47 + the policy decision +
  the committed events; `execlaw eval flag <conv> --range 12..48
  --label "regression-2026-04-22"` records the flag; `curl
  /api/admin/logs?level=warn&since=...` returns paginated JSON;
  `cargo run --bin eval-harness -- --rubric evals/rubrics/trust-class.toml`
  runs the rubric against a mock-or-real OpenAI endpoint and prints
  pass/fail per case. Operator-visible UI surfaces follow in Phase 6.

### Phase 6 — UI port, chat-first landing (3-4 weeks per sub-phase)

The UI is built once as an **encapsulated SPA** and ships across multiple
compile targets so the operator can use it from a browser today, a
desktop app shortly after, and a mobile app later. The SPA only ever
talks to the Rust server over **JWT-authenticated REST + WebSocket** —
no shared state, no shared filesystem, no native API surface — so the
same bundle ships everywhere.

#### Stack (as shipped)

- **React** (plain DOM, no react-native-web — see audit note below).
- **react-bootstrap** + **Bootstrap CSS** + **Bootstrap Icons** for the
  visual layer + responsive breakpoints. iOS / Android native targets
  swap this for a parallel component layer (Phase 9b).
- **Vite** bundler.
- **GSAP** (`gsap` + `@gsap/react`) for screen transitions
  (login → chat handoff, sign-out fade, etc.). Replaced an earlier
  Reanimated 4 attempt that didn't composite cleanly with Vite +
  react-native-web.
- **Plugins are trusted** — UI panels load via dynamic ESM `import()`
  with no sandboxing per the 2026-04-25 decision. The plugin-install
  flow is gated by the controller's auth so anything that lands has
  been opted-in.
- **Dark mode by default**; light/dark/system toggle in settings.

#### Compile targets (this phase ships the web SPA only)

| Target | Path | Phase |
|---|---|---|
| Web SPA | React + Vite bundle served by the Rust server (rust-embed planned for 10a wrapper) | 6a — done |
| Tauri Desktop | same React bundle in a Tauri webview + OS notifications | 10a — last phase |
| iOS / Android native | React Native + parallel component layer (Tamagui or similar) | 10b — last phase |

#### UX decisions (locked in 2026-04-25)

- **Chat-first landing**: the conversation is the front door. Admin
  pages live behind sidebar nav.
- **Sidebar layout** (OpenWebUI-shaped):
  ```
    ✏  New chat
   ───────────────
    🕒 Routines
    👥 Contacts
    ⋯  More       ← Tools / Skills / plugin UI panels
   ───────────────
    🧵 Threads     [⏷ external]   ← toggle filters non-controller threads
    📌 Control thread             ← always pinned, always visible
    📨 Marge — Q4 plans  ✉
    💬 Standup           web
    ...
   ───────────────
    ⚙ justin@example.com
  ```
- **Controller thread merge**: every message the controller sends or
  receives — across web, voice, Signal, email, etc. — collapses into
  ONE pinned thread named "Control thread". Each message renders a
  subtle channel-origin icon (web, signal, email, voice…). The
  controller has exactly one DM with the agent regardless of channel.
- **Thread-list status icon** (left of each thread name):
  - **empty grey dot** — default / read state
  - **blue filled dot** — agent has replied since the user last viewed
  - **animated loader** — agent currently processing this thread
  - For external (group / outsider) threads the channel icon
    (Signal / email / voice) replaces the dot — origin matters more
    than read-state for those.
- **Thread names**:
  - "Control thread" — the pinned controller thread (hard-coded)
  - For external groups — the truncated transport-supplied group name
    + channel icon
  - For internal new threads — a 3-word LLM-generated summary written
    via the agent tool `set_thread_name(name)` after enough context
    accumulates. Persisted to `state_conversations.display_name`.
- **External-channel filter toggle**: a switch above the thread list
  hides every external thread except the pinned Control thread, so the
  controller can focus on personal chat threads.
- **Approvals UX**: a ChatGPT-style card slides in from above the main
  text input when an approval is pending in the active thread. Cards
  for cold-contact, Rule-of-Two breach, and sensitive-tool-call
  approvals share the same shape. Top-bar pending-approvals chip is a
  later iteration.
- **Streaming tokens**: rendered as they arrive (typing-cursor
  effect). Sentence buffering can land later if the flicker becomes
  bothersome.
- **Truncation**: long inbound messages (long emails, copy-pasted
  documents) render with a fixed-height clamp and a "Read more…"
  affordance. Outbound (agent) messages do the same when they're
  longer than ~12 lines.
- **Setup detection**: on UI boot the SPA hits `GET /api/ping` which
  returns plain text `pong` (controller user exists, normal mode) or
  `setup` (first-run; SPA routes to the wizard). The wizard collects
  `admin_password`, `display_name`, and an optional `email`, then
  POSTs to `/api/setup` which writes one row to the `users` table
  (migration 0005, landed as Phase-6 prep) and returns the access +
  refresh JWTs. `GET /api/admin/me` returns the logged-in user's
  profile so the SPA can render the bottom-of-sidebar `⚙ user@email`
  affordance.

  Today execlaw is **single-user-controller** by design: the `users`
  table holds exactly one row with `role = "controller"` and the
  Controller principal carries `["*"]` capabilities. Phase 7
  hardening adds invite + role-scoped operators (`role = "operator"`
  / `"viewer"`) without schema changes — the columns are already
  there.
- **Incognito threads**: a toggle in the new-thread modal marks the
  thread `is_ephemeral = 1` with a default 1-hour expiry. Events ARE
  persisted during the conversation (so crash recovery works) but
  the `EphemeralSweeper` task DELETEs every event row whose parent
  conversation has `is_ephemeral = 1` and has passed
  `ephemeral_expires_at`. Audit posture: the conversation row stays
  with `last_seq = 0` after purge, so reports can show "N incognito
  threads existed but their content was purged".

#### New API routes

| Route | Purpose |
|---|---|
| `GET /api/ping` | Plain-text `pong` or `setup`. UI's first request. |
| `GET /api/admin/plugins/ui_panels` | List installed UI panel manifests (mount path, entry, icon, label) for sidebar nav. |
| `PATCH /api/chats/:id` | Update thread metadata (`display_name`, `is_ephemeral`, `is_pinned`). |
| Agent tool `set_thread_name(name)` | Lets the agent set the 3-word thread summary. Writes `state_conversations.display_name`. |

#### Sub-phasing

| Sub-phase | Scope | Estimated time | Status |
|---|---|---|---|
| **6a** | Vite scaffold + React + react-bootstrap + Bootstrap Icons + JWT auth + WS event bus + chat view + inline approval card + Control-thread merge + setup-wizard route | 1-2 wk | ✅ done |
| **6b** | Admin read-views: plugins, principals, hardware, eval flags, logs, audit (deployment editor moves to Phase 7 once the deployments backend lands) | 1-2 wk | ✅ done |
| **6c** | Writes: plugin install upload, approval verbs, trust revoke, incognito toggle, thread rename (deployment editor → Phase 7) | 1-2 wk | ✅ done |

Note: the original Phase-6 stack called for `react-native-web` + Reanimated to share a codebase with iOS/Android. Phase 6a tried that and the RN-on-Vite plumbing produced a stream of CJS/ESM and runtime crashes; the SPA was rebuilt on plain React + GSAP. Native iOS / Android targets are **deferred** to a Phase 9+ port that will add a parallel component layer (Tamagui / similar) — they're not blocking 6.

#### Out of scope for Phase 6

- Voice UI (push-to-talk recorder + audio playback) — Phase 9 with the real audio plugins, plus the native-target half in Phase 10b.
- Tauri Desktop wrapper — moved to **Phase 10a** (final-phase surface port). The web bundle ships first; Tauri is one webview around it.
- Native iOS / Android — Phase 10b, requires a parallel component layer (Tamagui or similar) to replace react-bootstrap.

- **Phase 6 demo (web only):** First-run hits `/api/ping → setup`,
  routes to the wizard, controller sets a password + sees the hardware
  profile, lands on the chat view. Sends a message; tokens stream in.
  Opens a new incognito thread, has a conversation, closes it; events
  purged within ~5 min. An external sender (simulated cold contact)
  triggers the approval card; controller approves, original message
  appears in the Control thread tagged with the channel icon.

### Phase 7 — Hardening (ongoing)

Phase 7 was originally an undifferentiated punch list of "everything left
that touches an invariant." After the first two waves shipped, the
remaining items split cleanly into three sub-phases each large enough to
deserve its own scope acknowledgement, design pass, and test plan. They
are listed here so they don't get smuggled into a "wave 3" without
deliberate planning.

**Wave 1 (✅ shipped 2026-04-25):**
- **Deployment editor** (UI + backend) — `config_runner_deployments`
  CRUD with GPU pinning, model-spec JSON validation, audit-logged via
  `AuditStore`, SPA Settings → Deployments page with create/edit/delete
  modal. 16 server tests + 4 SPA tests + 3 Criterion benches.
- **Event-log HMAC key rotation** — `KeyRing` primitive (`crates/core/src/event_hmac.rs`). Old rows verify under their original `key_id`; new rows pick up the rotated current key. Migration 0004a adds `key_id INTEGER`. CLI: `execlaw rotate-event-key`. 6 unit tests covering rotation, mid-rotation appends, signature mismatch on tamper.
- **Log retention sweeper** — `LogRetentionSweeper` purges `log_entries` past 30d. Runs alongside the existing `EphemeralSweeper` in the server's tokio runtime. Configurable per-deployment via `config_log_retention_days`. 5 unit tests.

**Wave 2 (✅ shipped 2026-04-25):**
- **Background back-fill verifier** for legacy NULL-tagged `state_events` rows. `EventLog::backfill_null_tags()` requires a `KeyRing`, scans NULL-tag rows, signs them under the current key, writes `tag + key_id` back. Idempotent. CLI: `execlaw backfill-events`. 4 unit tests including the adversarial path that rejects when no key is attached. Once the operator has run the back-fill on a given DB, migration 0004b will flip `tag` to `NOT NULL`.
- **Backup + restore** with DR runbook. SQLite `VACUUM INTO` for atomic encrypted-at-rest snapshot; restore validates the snapshot is a real execlaw DB (schema_version table present + row count > 0) before atomic file rename. CLI: `execlaw backup --to <path>` and `execlaw restore --from <path>`. Inherits SQLCipher encryption posture from the source DB. Integration test covers full round-trip including key-ring continuity.
- **Multi-controller users** — list / invite / delete with `count_by_role` invariant guarding "cannot remove the last controller" + "cannot self-delete". Audit-logged on every mutation. SPA Settings → Users page with role badges, invite form (Controller-only), per-row delete with confirm. 7 server tests + 5 SPA tests + 3 core tests.

**Wave 3 — sub-phases:**

| Sub-phase | Scope | Notes |
|---|---|---|
| **7e** | **WebAuthn controller auth** — second factor after JWT password, optional per-user. `webauthn-rs` 0.5 (passkey API) for the relying-party side, `state_webauthn_credentials` table, registration ceremony from Settings → Profile, challenge state held server-side between begin/finish, login challenge wired into `/api/login`, fallback to password if no credentials registered. | ✅ shipped 2026-04-25. |
| **JWT plumbing** | Persistent refresh-token store + `/api/logout/all` + SPA silent retry on 401 + background refresh + sweeper. | ✅ shipped 2026-04-25 (post-audit gap fill). |
| **~~7d~~** | ~~**WASM plugin tier**~~ | **Dropped 2026-04-25.** Considered and explicitly de-scoped: every plugin in `docs/plugin-inventory.md` wraps a native binary or speaks HTTPS, so none compile to wasm; the threat model is self-hosted single-operator, so the deny-by-default story doesn't pay for the `wasmtime`/Cranelift dep weight. Subprocess tier carries Phase 2-8. Reconsider only if a plugin port surfaces a real need. |
| **~~7f~~** | ~~**Advanced subagents**~~ | **Removed from Phase 7 scope 2026-04-25.** The wave-2 audit observed 7f is greenfield (no planner/executor split, no subagent lifecycle, ~95% new code) and is fundamentally a feature, not hardening. Tracked separately as a feature roadmap item; subagents stay default-on in their current shape per the 2026-04-23 locked decision. |

**Phase 7 is functionally complete** with 7e + JWT plumbing shipped. Remaining items are either dropped (above) or correctly classified as fluff / not warranted for a self-hosted single-operator appliance: rate-limiting on `/api/login` + `/api/token/refresh` (not internet-facing), HttpOnly cookies + CSRF tokens (same-origin SPA, no third-party JS), DR runbook doc (CLI + tests cover the actual work), PII redaction on log entries (retention sweeper IS the privacy control), and an active-sessions UI (the `count_for_user` primitive exists; surfacing it is Phase 8+ polish).

### Phase 8 — MCP client integration (in progress; inserted 2026-04-25)

Lets the operator point execlaw at one or more existing MCP servers
(github-mcp, slack-mcp, anything they already run) and have those
tools surface in the runner's tool array alongside builtins + plugin
tools. Unblocks several Phase-9 plugin ports because anything with an
existing MCP server in the wild no longer needs a custom execlaw
plugin.

**Locked decisions (2026-04-25):**
- Sampling (`sampling/createMessage`): refuse all. Treated as a
  capability execlaw never grants. Reconsidered if a real use case
  appears.
- Resources + Prompts: tools + resources in scope. Resources surface
  through the existing `memory` machinery so the runner can read
  them as ordinary context. Prompts deferred.
- Stdio sandboxing: trust the operator. Adding an stdio MCP server
  is `execve` on operator-supplied input — same trust model as
  installing a plugin.
- HTTP auth: bearer + custom header, value resolved from the vault.
  OAuth deferred until a port needs it.
- Tool naming: prefix `mcp:<server_id>:<tool_name>` in the registry
  AND in what the model sees. Disambiguates collisions across servers.
- New tool default: inherits the server-level `default_allowed_classes`
  policy on first appearance; per-tool overrides via Settings → Tools.
- Trust-class semantics: the per-tool allowlist applies to ALL tools
  in the registry, not just MCP. Generalised in Phase 8a.

**Sub-phases:**

| Sub-phase | Scope | Notes |
|---|---|---|
| **8a** | **Per-tool trust-class allowlist (foundation)** — `config_tool_access` table + `ToolAccessStore` + `ChainedToolDispatch` access gate + Settings → Tools page. | ✅ shipped 2026-04-25. |
| **8b** | **`crates/mcp-client`** — minimal MCP client speaking JSON-RPC 2.0 over stdio + Streamable HTTP. Implements `initialize`, `tools/list`, `tools/call`, `resources/list`, `resources/read`, and the `notifications/tools/list_changed` handler. Refuses every `sampling/createMessage` request. | next |
| **8c** | **`config_mcp_servers` + connection manager.** Migration adds the table; per-server tokio actor maintains a persistent connection with exponential-backoff reconnect; `tools/list` results reflected into `config_tool_access` rows with `mcp:<server>:<tool>` names; per-server `default_allowed_classes` honoured on first-sight. Vault-resolved auth secrets. | queued |
| **8d** | **`McpDispatch` + Settings UI + e2e.** Third tier in `ChainedToolDispatch` routes prefixed tool names to the connection manager; Settings → MCP page CRUD-s servers with status indicator + test-connection; resources surface as memory-readable entries. End-to-end test against a mock MCP server. | queued |

### Phase 9 — External plugin ports (open-ended; runs in parallel once Phase 2-8 foundations hold)

Every remaining port of a selfhosted-claw integration lives here. This
phase exists because each of these needs something execlaw's core
doesn't provide on its own — a third-party service account, an OAuth
client, a vendor SDK, a specific daemon — and bundling that work into
the phase that built the *framework* (Phase 2) or the phases that
built the *invariants* (Phase 3-4) blurred what "done" meant.

Each port is its own small PR landing against a stable framework. The
phase has no single end state; it's a queue drained in priority order,
with each port unlocking the Phase-3/4 cross-transport demos that were
deferred.

**Bucket A — port as execlaw plugins** (full inventory + blockers
tracked in [`docs/plugin-inventory.md`](docs/plugin-inventory.md)):

| selfhosted-claw source | execlaw plugin | Hook attachments | External dep |
|---|---|---|---|
| `src/channels/signal.ts` | `plugin-signal` | transport, identity_provider, alert_sources, health_checks | `signal-cli` subprocess |
| `src/integrations/phone-voice*.ts` + `src/voice-runner/` | `plugin-voice` + `service-whisper` + `service-kokoro` + `service-piper` containers | transport, chat_components | Whisper / Kokoro / Piper model weights + OpenVINO / ONNX runtimes |
| `src/integrations/deep-research.ts` + `src/research/orchestrator.ts` | `plugin-research-orchestrator` | tools, services, alert_sources | depends on the search/vision/pdf plugins below |
| `src/integrations/search-exa.ts` | `plugin-search-exa` | tools, oauth_accounts | Exa API key |
| `src/integrations/search-brave.ts` | `plugin-search-brave` | tools, oauth_accounts | Brave Search API key |
| `src/integrations/search-duckduckgo.ts` | `plugin-search-duckduckgo` | tools | none (keyless) |
| `src/integrations/url-fetch.ts` | `plugin-url-fetch` | tools | none |
| `src/research/vision.ts` | `plugin-research-vision` | tools | local vision model (`service-vision`) |
| `src/research/pdf.ts` | `plugin-research-pdf` | tools | pdfium/MuPDF in a probe container |
| `src/integrations/google-calendar.ts` | `plugin-google-calendar` | tools, oauth_accounts, ui_panels, health_checks | Google OAuth client creds |
| `src/integrations/google-contacts.ts` | `plugin-google-contacts` | identity_provider, tools, oauth_accounts | Google OAuth client creds |
| `src/control-actions.ts` (per-action) | `plugin-control-<action>` (one per action) | tools, ui_panels, alert_sources | none |
| `src/tools/*.ts` (overflow beyond `runner-local` built-ins) | `plugin-core-tools` | tools | none |

**Port priority order:**

1. `plugin-url-fetch` + `plugin-search-duckduckgo` — no external creds; fastest smoke-test of the full install → tool-call path with real HTTP I/O.
2. `plugin-signal` — unlocks the Phase-3 cross-transport sideband demos and Phase-4 voice approvals over Signal.
3. `plugin-google-calendar` + `plugin-google-contacts` — the canonical `oauth_accounts` + `identity_provider` story.
4. `plugin-voice` + `service-whisper` + `service-kokoro` — the Phase-4 real-audio acceptance demos (≤1.1s EoS → first audio on the hybrid-GPU path).
5. `plugin-research-*` bundle — once the primitives work, the orchestrator composes them.
6. `plugin-core-tools` + `plugin-control-*` — the long tail.

**Per-port checklist** (every port follows this):

- [ ] Manifest (`plugin.toml`) with `[runtime]` declaring tier + entrypoint.
- [ ] Subprocess implementation speaking the plugin JSON-RPC protocol.
- [ ] Transport-tier plugins call `ConversationResolver::resolve_or_mint(plugin_id, transport_handle, principal_id, idle_timeout_ms)` on every inbound message (see §2.6). The Controller-thread short-circuit collapses every controller-origin message into the pinned Control thread regardless of channel.
- [ ] Transport-tier plugins set `idle_timeout_ms` in their `plugin.toml` per the table in §2.6 (Signal 24h, email none, voice ~5min, SMS ~4h).
- [ ] Every event payload the plugin commits includes a `channel_origin` field (e.g. `"signal"`, `"email"`) so the SPA can render the per-message channel icon in the Control thread.
- [ ] Tests that run the plugin against a sandboxed mock of the external service (so CI doesn't need live creds).
- [ ] One end-to-end manual test against the real service with creds loaded from the vault.
- [ ] `docs/plugin-inventory.md` checklist entry flipped to `[x]`.
- [ ] Any Phase-3 or Phase-4 demo that depended on this port now runs end-to-end; close the cross-reference.

**Bucket B — fold into core** (already landed in Phase 2):

| selfhosted-claw source | execlaw home |
|---|---|
| `src/mount-security.ts` + `config-examples/mount-allowlist.json` | `execlaw-container-manager` |
| `src/inbound-guard.ts` + `scripts/inbound-message-guard.mjs` | `execlaw-policy::input_guard` |
| `src/control-store.ts` HMAC primitives | `execlaw-core::event_hmac` |

**Bucket C — retire** (see §12 "What We're Not Porting"): anything
bound to cloud LLMs (violates axiom #1), MCP client shims (replaced by
native OpenAI function-calling per §4.3), the `isMain` boolean auth
(replaced by the trust ladder §2.6), and JID-shaped routing (replaced
by the participant-aware model §2.6).

- **Demo:** each port ships its own demo from the per-port checklist.
  Phase 8 has no single "complete" state — it closes when the
  inventory spreadsheet is fully green, and any post-launch
  third-party integration lands here too.

### Phase 10 — Surface ports & native targets (last phase, open-ended)

The shipping surface today is the React SPA served by the Rust
control plane. Phase 9 wraps that same SPA in alternative shells +
ports it to native UI runtimes. Each surface is independent and can
land out of order.

| Sub-phase | Scope | Notes |
|---|---|---|
| **10a** | **Tauri Desktop wrapper** — wrap the existing React + GSAP bundle in a Tauri webview, add OS notifications for cold-contact alerts + critical alert pop-ups. Same SPA, no parallel component layer. | Needs the Rust `tauri` toolchain installed and a `src-tauri/` crate; otherwise it just packages the existing `web/dist/` output. |
| **10b** | **iOS / Android native** — the React SPA is web-only; native targets need a parallel component layer (Tamagui or similar) to replace react-bootstrap. Voice UI lives here too once the Phase-8 audio plugins are stable. | Largest scope of the phase. Requires choosing the cross-platform component lib + porting every chat / settings / approval-card view. |

Phase 10 has no single end state; each surface ships when it's ready
and the previous-phase backend / SPA work continues to drive every
target.

---

## 11.Z Built-but-Unwired Scaffolds — Cleanup Backlog (TODO)

The 2026-04-28 dead-code audit found several crates / modules that
compile, have tests, and are reachable from `Cargo.toml`, but are
**not on the live chat path today**. They're not dead enough to delete
outright — each was built deliberately for a future phase — but they
*are* drift risk: an operator reading the code can't tell from the
shape of the workspace which features are real.

Each entry below is a "decide before next ship" TODO. Disposition is
one of: **wire it** (turn the scaffold into a live runtime path),
**retire it** (delete the crate + remove from `Cargo.toml`), or
**re-scope it** (rewrite to match the current architecture). Until
disposition lands, treat the listed code as load-bearing-for-tests-
only and DO NOT extend it without a phase plan.

| Scaffold | Crate / Path | Purpose at design time | Live runtime hook? | Disposition TBD |
|---|---|---|---|---|
| **Plugin host (ZIP-upload, hooks)** | `crates/plugin-host`, `crates/plugin-sdk` | Phase 4 plugin framework: ZIP upload → manifest validation → hook dispatch (transports, tools, identity providers, services). Locked decision §4.3. | Compiled into `AppState.plugin_host`. `chats.rs` reads `plugin_host.registry().all_tools()` to gate the tool-capable path, but no hooks are *fired* — there's no inbound transport, no tool plugin uploaded, no service plugin lifecycle. | Phase 4 implementation OR explicit "plugins are stubs until Phase 4" doc note + remove the `plugins.rs` admin route from the SPA so operators don't think it works. |
| **MCP client / host** | `crates/mcp-client`, `crates/server/src/mcp_host.rs`, `crates/server/src/mcp_admin.rs` | Phase 8c: stdio MCP servers configured via `config_mcp_servers`, tools reflected into `config_tool_access` so the runner can call `mcp:<id>:<name>`. | `mcp_host.reconcile()` runs at boot; admin CRUD route exists; tool dispatch path **exists** in `tool_dispatch.rs`. *But*: locked decision 2026-04-23 says "MCP later" — there's no SPA UI past the admin CRUD, and no server has been demonstrated end-to-end. | Confirm "MCP later" still holds → add a feature flag `EXECLAW_MCP_ENABLED` defaulting off, hide the SPA admin page, OR commit to a Phase 8c integration test that exercises a real stdio MCP server. |
| **Voice pipeline (STT/TTS/runtime)** | `crates/voice-pipeline`, `crates/server/src/voice_*` | Phase 13.B/C: WS audio session → jitter buffer → Whisper STT → agent loop → Kokoro TTS streaming back. | `voice_sessions` + `voice_runtime` + `voice_clients` are all wired into `AppState`; routes for `/api/voice/*` exist; one round-trip integration test (`voice_ws_round_trip.rs`) passes against mocks. *But*: the SPA has no voice UI; the chat path doesn't dispatch voice modality; no operator has used voice end-to-end. | Either build the SPA voice surface (Phase 9) or quarantine the voice crates behind a `voice` cargo feature so they don't add 60s to a `cargo build` for operators not using voice. |
| **Outbox / approval flow** | `crates/server/src/approvals.rs`, `crates/server/tests/approval_flow.rs` | Axiom #4 (effects through outbox) + sideband HITL (§2.11): runner produces intent → outbox queue → approval card → controller approves/denies → relay produces effect. | Approval routes exist; `approval_flow.rs` test passes. *But*: nothing in the chat / runner path *enqueues* approvals today — every effect goes inline. The outbox table exists in migrations but is never written to. | Wire one end-to-end effect (e.g. a tool that triggers a real Signal send) through the outbox + approval card OR retire the approvals.rs surface until §2.4 lands. Currently misleading: the SPA shows an Approvals page that's always empty. |
| **Subagent escalation** | Referenced in `docs/voice-design.md`, agent-model research (§2.9) | Voice "small-model runner + warm pool" with sub-agent escalation to a bigger model for hard problems. Default-on subagents per locked decision 2026-04-23. | **No code path exists.** The supervisor has one runner per group; there's no fan-out, no dispatch-to-bigger-model, no result merge. | Phase 9 / voice closure. Either add a §9.x sub-phase that ships subagent dispatch OR remove the locked-decision claim from `MEMORY.md` until it's real. |
| **Capability tokens (signed JWTs)** | *Retired 2026-04-28* | Per-turn signed bearer the runner echoes on every tool call so the server verifies `(group_id, turn_id, capability_set)` before dispatching. §7.2. | ~~Module + protocol field + benches existed but nothing verified.~~ Pruned. The in-process tool dispatcher gates on `policy.capability_set` directly. | Resurrect when the runner-container path supports tools (see "runner-local TurnExecutor" item below) and the cross-process boundary needs a forgery-resistant bearer. |
| **runner-local `TurnExecutor` tool path** | `crates/runner-local/src/lib.rs` (`TurnExecutor`), live caller `crates/server/src/chats.rs:1139` | Tool-capable agent loop: model emits `tool_use` → server dispatches via `ChainedToolDispatch` → result → next round, until the model finishes. | **Live and load-bearing**: every chat turn that has plugin/MCP tools available routes through `TurnExecutor::new()` in the in-process server, NOT through the runner container. The supervisor path bypasses tools today. | **Address now (2026-04-28).** Either teach the runner WS protocol to forward `tool_use` to the supervisor (so the runner container can use tools) OR formalize the split: runner = no-tools text turns; in-process executor = tool-capable turns. Until decided, the SPA has two parallel agent loops with different bug surfaces. See "Agent loop tool execution" tracking note at the top of `crates/runner-local/src/lib.rs`. |

**Process note.** When picking up any of these, the *first* PR must
be a phase-plan section (§11.x) that defines: scope, demo, owner crate
boundaries, test budget. No "I'll just wire it up" PRs — those are how
scaffolds turn into permanent half-features.

---

## 12. What We're *Not* Porting

- **`.env` files and process-env-based config.** Replaced by SQLite config + OS-keyring vault + CLI bootstrap flags. See §6.
- **UI-editable config files mounted into containers / consumed by other processes.** The UI writes to SQLite; the control plane hands fresh snapshots to consumers at read time. No shared-file races.
- **Raw `fetch()` prompt assembly** ([src/index.ts:969](../selfhosted-claw/src/index.ts)). Replaced by the in-tree `runner-local` crate speaking OpenAI-compatible API to locally-hosted inference services.
- **Cloud LLM paths, period.** No Anthropic, OpenAI, Gemini, or any other cloud inference provider — not in core, not as plugins, not as opt-in bridges, not at all. Models must be hosted locally (§0 axiom #1).
- **Inference-bridge plugins.** Removed from the plugin framework. If one appears in a PR, reject on sight.
- **OpenTelemetry, OpenInference, Langfuse, Arize Phoenix.** Bloat. Log with `tracing` to JSONL + SQLite like selfhosted-claw.
- **WebAuthn for Controller auth (Phase 1).** JWT + admin password is the shipping auth; WebAuthn is a future option if requested.
- **`git`-URL plugin install, git-branch plugin distribution.** Plugins install via ZIP upload only.
- **Cold-contact holding auto-reply.** Silent hold; controller decides.
- **Per-severity alert routing with digests.** UI notification dropdown primary, Signal fallback for Critical. That's it.
- **LiveKit turn-detector model (Phase 1 voice).** Punctuation + dynamic-silence heuristic is sufficient; model-based endpointer is a future option if false-interrupt rate is a problem.
- **Separate Reasoning deployment / QwQ model.** One `QuantTrio/Qwen3.5-27B-AWQ` serves all LLM purposes.
- **F5TTS.** Replaced by Kokoro-82M (Apache-2.0, cross-GPU, streaming).
- **Hardcoded model/GPU pairings.** Every runner-purpose → model → GPU mapping is a row in `config_runner_deployments`, editable in the UI (§5.4).
- **In-memory dedup sets.** Everything persistent; framework-minted idempotency keys.
- **Advance-cursor-then-queue pattern.** Replaced by turn-as-transaction with event log + outbox (§2.4).
- **Cursor-based message state.** Replaced by the event log as source of truth (§2.3).
- **`isMain` boolean.** Replaced by principal + policy + trust class.
- **Direct LLM calls to external APIs.** All effects go through the outbox (§2.4).
- **Regex/model-based "prompt injection detection" as primary defense.** Replaced by architectural containment: planner/executor split, Rule of Two, spotlighting, capability scoping (§2.5, §7).
- **`docker run` shell-outs scattered across files.** Replaced by bollard client in one crate.
- **Stdout-marker parsing for agent output.** Replaced by structured IPC over the capability token channel.
- **60-second scheduler polling.** Replaced by priority queue + condvar; wakeups are resume events.
- **Compose files for services** (`scripts/phone-voice-stt/docker-compose.yml`). Replaced by service-plugin manifests.
- **Legacy `signal_*` IPC task types.** Every transport operation goes through the plugin interface.
- **JSON config files in `~/.config/self-hosted-claw/integrations/*.json`.** Replaced by `plugin_settings` rows scoped per plugin.
- **Separate voice agent loop.** Voice flows through the same event log, same runner model (§2.2, Phase 4).
- **Multi-agent by default.** Default is single-threaded; subagents exist only as guardrails and opt-in research fan-out in controller conversations (§2.9).

---

## 13. Open Questions for the User

Decided so far (locked in for Phase 0):
- ~~**Database**~~: SQLite-first, WAL mode, config + state + vault all SQLite. (§6)
- ~~**Single vs split DB**~~: **Single** `execlaw.db` with table-prefix namespacing (`config_*`, `state_*`, `vault_*`, `log_*`). Split logs later if volume warrants. (§6.3)
- ~~**Secrets at rest**~~: **SQLCipher** via `rusqlite` `bundled-sqlcipher-vendored-openssl`. Statically linked, self-contained. Master key in Linux Secret Service with a passphrase-file fallback. (§6.4)
- ~~**Host target**~~: **Linux container image** (Dockerfile + docker-compose, matching selfhosted-claw's pattern), developed on WSL2 Docker Desktop, deployable to bare Linux Docker/Podman. No native-Windows build. GPU passthrough via `nvidia-container-toolkit` or Intel `/dev/dri` bind-mount, identical on both. (§3.2, §0 axiom #11)
- ~~**Self-hosted grounding**~~: No mandatory external services; offline-capable first run. (§0)
- ~~**Agent core shape**~~: Event-sourced state machine on SQLite; turn-as-transaction with outbox; framework-minted idempotency keys; runners stateless against the log. (§2)
- ~~**Injection defense posture**~~: Architectural containment (planner/executor split + Rule of Two + spotlighting + capability scoping), not model-level detection. Accept injection succeeds sometimes; contain blast radius. (§2.5, §7.4)
- ~~**MCP vs native tools for plugins**~~: Native in-process function-call tools for execlaw plugins; external MCP reserved for third-party SaaS reach or pre-existing user MCP servers. (§4.3)
- ~~**Multi-agent default**~~: Off. Subagents only for guardrails and explicit research fan-out in `ControllerDM`. (§2.9)
- ~~**Session storage**~~: Custom `SessionStore` adapter backed by the `state_events` table; transcript persists synchronously after each runner turn. (§2.8)
- ~~**Memory architecture**~~: Four layers (transcript, scratchpad, compaction summaries, trust-class-scoped long-term); surfaced to the LLM as ordinary OpenAI function-call tools implemented in `runner-local`, backed by SQLite tables. No vendor SDK. (§2.7)

**Recently decided (all batch on 2026-04-23):**
- ~~**Plugin distribution**~~: ZIP-upload-with-manifest. No git-branch method, no hosted registry. (§4.5)
- ~~**Controller identity bootstrap**~~: First-time UI sets admin password; JWT auth; SPA stores + refreshes. WebAuthn deferred. (§7.1, §8.3)
- ~~**Phase 1 chat transport scope**~~: Real browser chat UI based on OpenWebUI's look (not code). This is the primary surface. (§8)
- ~~**selfhosted-claw continuity**~~: Hard cutover. No compatibility bridge.
- ~~**WSL2 GPU test**~~: Not relevant — docker handles it; don't inflate the control plane with GPU deps.
- ~~**Observability sink**~~: JSONL + SQLite, same as selfhosted-claw. No OTEL.
- ~~**Extended thinking policy**~~: Drop the knob. Use model's native reasoning if available.
- ~~**Exact Qwen build**~~: `QuantTrio/Qwen3.5-27B-AWQ` for everything.
- ~~**Reasoning model split**~~: Single model.
- ~~**Trust hint threshold**~~: `Contact` auto-trusts.
- ~~**Plugin inventory**~~: Port every selfhosted-claw integration. Google-contacts shape is the template. (§Phase 2)
- ~~**Audio retention**~~: Transcripts only.
- ~~**Kokoro voices**~~: `bf_emma` female, `am_michael` male (operator-editable).
- ~~**Turn-detector**~~: Drop the LiveKit model; use punctuation + dynamic silence heuristic.
- ~~**Alerts**~~: UI notification dropdown. Signal as fallback only. No digests.
- ~~**Cold-contact response**~~: Silent hold. No auto-reply. No auto-deny threshold.
- ~~**Sideband fallback**~~: Signal. UI is primary.
- ~~**Plugin framework framing**~~: Every plugin is a plugin; hooks declared in manifest (§4.2).
- ~~**Runner isolation**~~: Per-conversation hot runner container (learn + improve on selfhosted-claw's HotRunnerPool). (§2.8)
- ~~**Subagents**~~: Default on; deep research is the flagship. (§2.9)
- ~~**No cloud LLMs ever**~~: Strict (§0 axiom #1).
- ~~**API docs**~~: Swagger/OpenAPI for REST + AsyncAPI for WebSocket. (§8.4, Phase 0 deliverable)

**Remaining — Phase-4-adjacent, can decide during Phase 4 kickoff:**

1. **Kokoro on Intel Arc validation run** — hands-on test on your actual Arc SKU confirming OpenVINO 2025.2 ISTFT-GPU path works. If not, Kokoro on CPU is the fallback.
2. **Operator validation of `QuantTrio/Qwen3.5-27B-AWQ` native reasoning support** — confirm the specific build's capabilities during Phase 0 setup. Doesn't block anything; just determines whether the reasoning mode flag gets wired.

That's it.

---

## 14. Appendix: Evidence links into selfhosted-claw

For each claim above, the source of truth in the predecessor project:

- Agent loop: [`selfhosted-claw/src/index.ts:804-928`](../selfhosted-claw/src/index.ts)
- Dedup set: [`selfhosted-claw/src/channels/signal.ts:149`](../selfhosted-claw/src/channels/signal.ts)
- Group queue retry/cooldown: [`selfhosted-claw/src/group-queue.ts`](../selfhosted-claw/src/group-queue.ts)
- Docker scatter: [`selfhosted-claw/src/container-runner.ts`](../selfhosted-claw/src/container-runner.ts), [`selfhosted-claw/src/container-runtime.ts`](../selfhosted-claw/src/container-runtime.ts), [`selfhosted-claw/src/runner/common/hot-runner-pool.ts`](../selfhosted-claw/src/runner/common/hot-runner-pool.ts), [`selfhosted-claw/src/integrations/service-manager.ts`](../selfhosted-claw/src/integrations/service-manager.ts)
- Integration registry (current "plugin" surface): [`selfhosted-claw/src/integrations/registry.ts`](../selfhosted-claw/src/integrations/registry.ts)
- Voice runner separation: [`selfhosted-claw/src/voice-runner/`](../selfhosted-claw/src/voice-runner/), [`selfhosted-claw/src/integrations/phone-voice-ws.ts`](../selfhosted-claw/src/integrations/phone-voice-ws.ts)
- Admin server (to be replaced): [`selfhosted-claw/src/admin-server.ts`](../selfhosted-claw/src/admin-server.ts)
- Inbound guard pattern: [`selfhosted-claw/src/inbound-guard.ts`](../selfhosted-claw/src/inbound-guard.ts)
- Existing mount allowlist security: [`selfhosted-claw/src/mount-security.ts`](../selfhosted-claw/src/mount-security.ts)
