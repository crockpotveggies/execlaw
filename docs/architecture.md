# execlaw — Architecture

Reference document for the execlaw agent model. This is the mental model a new contributor needs in 30 minutes before they read a line of code.

Relationship to other docs:

- [`MIGRATION_PLAN.md`](../MIGRATION_PLAN.md) is the design rationale, section-by-section, with research citations and trade-off discussion. Read it when you need to understand *why*.
- This document is the *what*: the structure, the invariants, the flows. Read it when you need to understand *how things fit together*.
- [`STATUS.md`](../STATUS.md) is the live progress log: what works today vs. what's scaffolded.

---

## 1. One-paragraph pitch

**execlaw is a deterministic state machine over an append-only SQLite event log that occasionally calls an LLM.** The LLM is the interesting but *replaceable* part. The durability, policy, and isolation around it are the product. Everything runs on the operator's hardware (no cloud LLMs, ever). The control plane is a single Docker container; the runner is a per-conversation Docker container; inference backends (vLLM, Whisper, Kokoro) are separate local service containers. Plugins extend the system via a WordPress-style hook framework loaded from ZIP uploads.

---

## 2. Design principles (referenced everywhere)

From `MIGRATION_PLAN.md` §0 — restated here so this doc stands alone:

1. **Self-hosted only.** No cloud LLM providers on any code path. Strict.
2. **SQLite is the source of truth** for configuration and state.
3. **The event log is the source of truth** for conversations.
4. **Effects go through an outbox**; the LLM never calls external APIs directly.
5. **Every `tool_use` pairs with a `tool_result`** in the same commit.
6. **Plugins, not hardcoded built-ins** — every extension is a plugin.
7. **One control plane, one container manager.**
8. **Participant-aware, trust-class-scoped.**
9. **Rule of Two** for untrusted turns.
10. **Sideband HITL** via a different transport than the one that introduced untrusted content.
11. **Portable control-plane container** — deployment artifact is a container image.
12. **Minimal containers** — every image ships only what its single job requires.

The rest of this document is these principles made concrete.

---

## 3. System topology

```
                            operator's machine
┌─────────────────────────────────────────────────────────────────────────────┐
│                                                                             │
│   ┌──────────────┐   HTTP+WS    ┌───────────────────────────────────────┐   │
│   │              │ ◀──────────▶ │  control-plane container              │   │
│   │   Chat UI    │   (JWT)      │  (Rust binary + CA certs + pci.ids)   │   │
│   │   (SPA)      │              │                                        │   │
│   │              │              │  axum server  event log  scheduler    │   │
│   └──────────────┘              │  ┌──────────┐ ┌─────────┐ ┌─────────┐ │   │
│                                 │  │ REST/WS  │ │ SQLite  │ │ wakeup  │ │   │
│                                 │  │ + OpenAPI│ │ SQLCipher│ │ queue  │ │   │
│                                 │  │ + AsyncAPI│ └─────────┘ └─────────┘ │   │
│                                 │  └──────────┘                          │   │
│                                 │  container-mgr   outbox-relay          │   │
│                                 │  ┌──────────┐   ┌─────────────┐        │   │
│                                 │  │ bollard  │   │ retry+inbox │        │   │
│                                 │  │ + hw-prof│   │ dedup       │        │   │
│                                 │  └──────────┘   └─────────────┘        │   │
│                                 │      │                    │            │   │
│                                 │      │ docker.sock        │ tool calls │   │
│                                 └──────┼────────────────────┼────────────┘   │
│                                        │                    │                │
│                    ┌───────────────────┼────────────────────┼──────────────┐ │
│                    │                   ▼                    ▼              │ │
│                    │  ┌────────────────────────┐  ┌───────────────────┐    │ │
│                    │  │  runner-local (per     │  │ transport plugins │    │ │
│                    │  │  active conversation)  │  │                   │    │ │
│                    │  │                        │  │  plugin-signal    │    │ │
│                    │  │  stateless, Ed25519    │  │  plugin-voice     │    │ │
│                    │  │  capability token      │  │  plugin-webchat   │    │ │
│                    │  └───────────┬────────────┘  │  plugin-email     │    │ │
│                    │              │               └────────┬──────────┘    │ │
│                    │              │ OpenAI API             │               │ │
│                    │              ▼                        │               │ │
│                    │  ┌────────────────────────┐           │               │ │
│                    │  │ local inference services│          │               │ │
│                    │  │                        │          │               │ │
│                    │  │  service-vllm          │          │               │ │
│                    │  │    (Qwen3.5-27B-AWQ)   │          │               │ │
│                    │  │  service-whisper       │          │               │ │
│                    │  │  service-kokoro        │          │               │ │
│                    │  │  service-openarc       │          │               │ │
│                    │  └────────────────────────┘          │               │ │
│                    │                                       │               │ │
│                    │  nvidia GPU  +  Intel Arc GPU         │               │ │
│                    └───────────────────────────────────────┼───────────────┘ │
│                                                            │                 │
└────────────────────────────────────────────────────────────┼─────────────────┘
                                                             │
                                                             ▼
                                              Signal / SMS / phone / email
                                              (external world)
```

Every arrow is local IPC or loopback HTTP. Nothing in the default path reaches the public internet except the transport plugins talking to their external surfaces (Signal server, email host, etc.) and the occasional `plugin-url-fetch` during research.

---

## 4. Actors and responsibilities

### 4.1 Control plane (one container, Rust binary)

The coordinator. Owns:

- **Event log** — `state_events` + related tables in SQLite (SQLCipher in production).
- **Scheduler** — priority queue for wakeups, sub-second precision.
- **Policy engine** — capability tokens, trust resolution, Rule of Two, input guards.
- **Plugin host** — manifest parsing, ZIP install, hook registry.
- **Container manager** — bollard client wrapping all Docker operations; hardware profile.
- **Outbox relay** — drains `state_outbox` to transport plugins with idempotency.
- **Axum server** — REST + WebSocket surface for UI and plugins.
- **Vault** — SQLCipher-encrypted secrets; master key from OS keyring.

Minimal image: Rust binary + `libssl` + CA certs + embedded `pci.ids` database. No CUDA, no OpenVINO, no Python, no vendor SDKs. Runs as `nobody`, read-only root filesystem.

### 4.2 Runner (one container *per active conversation*)

Thin Rust binary (`runner-local`). Speaks OpenAI-compatible API to whichever local inference backend is configured. Stateless against the event log: on spawn, hydrates context from SQLite via an authenticated RPC to the control plane; runs one turn; writes output back; exits (or stays warm for the next turn).

**Why per-conversation isolation?** Ported the HotRunnerPool pattern from selfhosted-claw. A runner compromised by prompt injection in conversation A can't touch conversation B's data — its capability token scopes it to one `conversation_id`.

### 4.3 Inference services (separate containers)

`service-vllm` (nvidia), `service-openarc` (Intel), `service-whisper`, `service-kokoro`, etc. Each serves an OpenAI-compatible or protocol-matched endpoint. Control plane calls them via `inference-api` client. These are the containers that carry the heavy vendor runtimes — keeping the control plane minimal (axiom #12).

### 4.4 Transport plugins

Signal, web chat, phone voice, email, Matrix. Each implements the `Transport` trait: receive inbound events and push them to the control plane's event log with stable `(plugin_id, source_event_id)` identifiers; drain outbox rows and deliver to the external surface.

### 4.5 Outbox relay

A separate async task in the control plane, explicitly *not* invoked by the runner. Reads `state_outbox`, delivers via transport plugins with the framework-minted idempotency key, handles retries (5 attempts + exponential backoff + dead-letter), and commits `effect_committed` events on success. The LLM never calls an external API directly; this is the only path out.

---

## 5. Data model

Full schema is in [`crates/core/migrations/0001_initial_schema.sql`](../crates/core/migrations/0001_initial_schema.sql) (22 tables). The load-bearing ones:

### 5.1 `state_events` — the source of truth

```sql
CREATE TABLE state_events (
    conversation_id TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    payload         BLOB NOT NULL,     -- MessagePack
    committed_at    INTEGER NOT NULL,
    actor           TEXT,
    PRIMARY KEY (conversation_id, seq)
);
```

Append-only. Monotonic `seq` per `conversation_id`. Every action in the system is a row here. Replay reconstructs state deterministically.

**Event kinds** (from [`crates/core/src/events.rs`](../crates/core/src/events.rs)):

| Category | Kinds |
|---|---|
| **Conversation** | `user_msg`, `model_turn`, `tool_use`, `tool_result`, `interrupt`, `resume`, `approval`, `effect_committed`, `wakeup` |
| **Trust & identity** | `cold_contact_arrived`, `identity_resolution_conflict`, `trust_changed` |
| **Alerts** | `alert_fired`, `alert_renotified`, `alert_acked`, `alert_resolved`, `alert_snoozed`, `incident_opened`, `incident_closed` |
| **Voice (finer-grained)** | `voice.session_started`, `voice.session_ended`, `vad.speech_started`, `vad.speech_ended`, `stt.partial`, `stt.final`, `turn.user_ended`, `llm.token`, `llm.response_final`, `llm.cancelled`, `tts.first_audio`, `tts.audio_chunk`, `tts.ended`, `interrupt.started`, `interrupt.rescinded`, `interrupt.confirmed` |
| **Research** | `research_progress_updated` |
| **Escape hatch** | `other` (for forward-compat with future additions) |

### 5.2 `state_conversations` — the materialized view

```sql
CREATE TABLE state_conversations (
    conversation_id TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,       -- ControllerDM | GroupWith... | Blocked | ...
    last_seq        INTEGER NOT NULL,
    phase           TEXT NOT NULL,       -- FSM state (§7)
    controller_id   TEXT,
    trust_class     TEXT NOT NULL,       -- effective trust for this conversation
    modality        TEXT NOT NULL,       -- Text | Voice
    snapshot_blob   BLOB,                -- MessagePack, built every ~50 events
    snapshot_seq    INTEGER,
    lease_owner     TEXT,                -- worker id; NULL = idle
    lease_expires   INTEGER               -- crash recovery
);
```

A single row per conversation carrying the fast-resume snapshot and the lease that enforces per-conversation serialization.

### 5.3 `state_outbox` / `state_inbox` — effect plumbing

```sql
CREATE TABLE state_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,   -- framework-minted
    conversation_id TEXT NOT NULL,
    effect_kind     TEXT NOT NULL,
    payload         BLOB NOT NULL,
    status          TEXT NOT NULL,          -- pending | in_flight | delivered | failed
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    last_error      TEXT,
    enqueued_seq    INTEGER NOT NULL
);

CREATE TABLE state_inbox (
    idempotency_key TEXT PRIMARY KEY,
    received_at     INTEGER NOT NULL
);
```

Idempotency keys are derived from `(conversation_id, turn_seq, tool_call_ordinal)` — framework-minted, **never** LLM-derived. Combined with consumer-side inbox dedup at the transport, this gives effectively exactly-once delivery.

### 5.4 `principals` — the trust table

```sql
CREATE TABLE principals (
    id              TEXT PRIMARY KEY,
    identifiers     BLOB NOT NULL,       -- JSON array of (transport, handle) pairs
    trust_level     BLOB NOT NULL,       -- serialized TrustLevel enum
    resolved_by     BLOB NOT NULL,       -- which identity-provider plugins matched
    metadata        BLOB NOT NULL,
    first_seen      INTEGER NOT NULL,
    last_seen       INTEGER,
    controller_notes TEXT
);
```

**Trust ladder** ([`crates/core/src/principal.rs`](../crates/core/src/principal.rs)):

```
Controller   — admin, cryptographically bound, full capabilities
Delegated    — explicit, time-bounded grant from controller
KnownTrusted — identity-provider matched + controller approved
KnownLimited — identity-provider matched, topic/tool-scoped
UnknownPending — first-time contact, awaiting controller
Blocked      — controller rejected (universal state: applies to unknown
               AND previously-trusted principals)
```

The `Blocked` state is the reason we renamed `UnknownDenied` — you can block anyone, not just strangers.

### 5.5 `memory_entries` — long-term memory, trust-scoped

```sql
CREATE TABLE memory_entries (
    scope       TEXT NOT NULL,
    trust_class TEXT NOT NULL,     -- enforced at the tool shim
    key         TEXT NOT NULL,
    value_blob  BLOB NOT NULL,
    ttl         INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (scope, trust_class, key)
);
```

`trust_class` in the composite key is what prevents an untrusted conversation from reading `Controller`-scoped memories.

### 5.6 Other tables (pointer-level)

- `state_alerts`, `state_incidents`, `state_alert_silences` — operational alerting (§10 of plan).
- `state_attachments`, `state_artifacts` — blob references for inbound images and research PDFs.
- `config_runner_deployments` — GPU + model + backend mapping per `RunnerPurpose`.
- `config_trust_policy`, `config_alert_routing`, `config_research_quota`, `config_runtime_settings` — operator-editable settings.
- `research_jobs` — background research sessions (§2.9.1 of plan).
- `vault_secrets` — SQLCipher-encrypted secret store; references are opaque to plugins.
- `log_entries` — SQLite half of the JSONL+SQLite dual log sink.
- `transport_cursors` — per-transport resume point (what `source_event_id` was last processed).

---

## 6. The conversation FSM

`state_conversations.phase` is a finite state machine. Transitions are driven by events in `state_events`; illegal transitions are rejected.

```
                      ┌────────────────┐
                      │      Idle      │◀──────────┐
                      └────────┬───────┘            │
                               │                    │
              user_msg arrives │                    │  turn commits
                               │                    │  (no wakeup, no approval needed)
                               ▼                    │
                      ┌────────────────┐             │
                      │    Thinking    │─────────────┤
                      └────────┬───────┘             │
                               │                     │
                 model requests│tool use             │
                               │                     │
                               ▼                     │
                      ┌────────────────┐             │
                      │ AwaitingTool   │─────────────┤
                      └────────┬───────┘             │
                               │                     │
                     policy says│"need approval"     │
                               │                     │
                               ▼                     │
                      ┌──────────────────┐           │
                      │ AwaitingApproval │───approve─┤
                      └────────┬─────────┘           │
                               │  reject             │
                               │                     │
                               ▼                     │
                      ┌────────────────┐             │
                      │ Thinking       │─────────────┘
                      └────────────────┘

      orthogonal phases:
        AwaitingWakeup      agent called schedule_wakeup; scheduler will fire
        AwaitingReconnect   transport dropped; wait N minutes then give up
        AwaitingTrustDecision  cold contact, controller hasn't decided yet
        TrustRevoked        terminal; conversation is archived
```

A worker holds a lease on the conversation (`state_conversations.lease_owner`) while the phase is anything but `Idle`. Leases have an expiry; if a worker crashes, the lease expires and another worker picks up the conversation.

**Phase transitions always commit as events** (`interrupt`, `resume`, `approval`, `wakeup`), so the FSM is replayable from the log.

---

## 7. What a turn is — the anatomy

A **turn** is a commit unit — the smallest span that commits atomically. It is *not* a request/response boundary. Text turns are typically one user-message / one model-response. Voice turns are bounded by utterance / tool call / approval. The invariants below hold identically in both.

### 7.1 Text turn sequence

```
  Transport        Control Plane         Runner          Inference        Outbox
     │                  │                  │                 │              │
     │ inbound event    │                  │                 │              │
     │─────────────────▶│                  │                 │              │
     │                  │ dedupe (inbox)   │                 │              │
     │                  │ append user_msg  │                 │              │
     │                  │ to state_events  │                 │              │
     │                  │                  │                 │              │
     │                  │ acquire lease    │                 │              │
     │                  │ phase → Thinking │                 │              │
     │                  │                  │                 │              │
     │                  │ spawn/reuse ────▶│                 │              │
     │                  │   runner         │ hydrate context │              │
     │                  │                  │ from snapshot   │              │
     │                  │                  │  + events       │              │
     │                  │                  │                 │              │
     │                  │                  │ POST /v1/chat/  │              │
     │                  │                  │  completions ──▶│              │
     │                  │                  │                 │              │
     │                  │                  │ stream tokens ◀─┤              │
     │                  │                  │ assemble turn   │              │
     │                  │                  │                 │              │
     │                  │ tool_use(args) ◀─┤                 │              │
     │                  │                  │                 │              │
     │                  │ policy check     │                 │              │
     │                  │   ├─ capability  │                 │              │
     │                  │   ├─ Rule of Two │                 │              │
     │                  │   └─ taint       │                 │              │
     │                  │                  │                 │              │
     │                  │ if local:        │                 │              │
     │                  │   execute tool   │                 │              │
     │                  │ if external:     │                 │              │
     │                  │   enqueue ──────────────────────────────────────▶│
     │                  │                  │                 │              │
     │                  │ tool_result ────▶│                 │              │
     │                  │                  │ continue turn   │              │
     │                  │                  │                 │              │
     │                  │ (repeat tool_use/tool_result until  │              │
     │                  │  model finishes)                   │              │
     │                  │                  │                 │              │
     │                  │ COMMIT TURN (one SQLite tx):       │              │
     │                  │   ├─ model_turn event              │              │
     │                  │   ├─ paired tool_use+tool_result   │              │
     │                  │   ├─ state_outbox rows             │              │
     │                  │   ├─ state_conversations update    │              │
     │                  │   └─ snapshot refresh (every 50)   │              │
     │                  │                  │                 │              │
     │                  │ phase → Idle     │                 │              │
     │                  │ release lease    │                 │              │
     │                  │                  │                 │              │
     │                  │                  │                 │              │ drain, deliver, retry,
     │                  │                  │                 │              │ inbox-dedup, commit
     │                  │                  │                 │              │ effect_committed
     │                  │                  │                 │              │
     │ outbound msg ◀──────────────────────────────────────────────────────┤
     │                  │ effect_committed event on success  │              │
```

### 7.2 Load-bearing invariants (every turn)

1. **`tool_use`/`tool_result` pairing.** Every `tool_use` event must have a matching `tool_result` event committed in the same transaction. If the turn fails, a cancellation `tool_result` is synthesized. Enforced in `EventLog::commit_turn` via `enforce_tool_pairing()` in [`crates/core/src/events.rs`](../crates/core/src/events.rs). *The single most violated invariant in production agent systems — Claude Code itself has an open bug on this.*

2. **Turn-as-transaction.** The entire turn (model_turn event + every tool_use/tool_result pair + every outbox row + the conversation-state update) commits in one SQLite transaction. Either the whole turn is in the log or none of it is. *This is what selfhosted-claw got wrong — it advanced the cursor before the container confirmed.*

3. **Framework-minted idempotency keys.** Derived from `(conversation_id, turn_seq, tool_call_ordinal)`. Never from LLM output (that's a subtle bug: model rephrases rationale, collision check silently fails). Enables at-least-once delivery + consumer-side dedup = effectively exactly-once.

4. **Per-conversation serialization.** Lease on `state_conversations.lease_owner` means one worker per conversation at a time. Different conversations run in parallel, bounded by container pool size.

5. **Runner is stateless against the log.** Everything the runner needs for a turn comes from hydrating `state_events` on spawn. Nothing durable lives in the runner filesystem, process memory, or any non-event-log store.

---

## 8. Effects and the outbox

The LLM emits tool calls. The control plane decides whether they run. If they do run and they have external effect (sending a message, creating a calendar event, making an HTTP call), they are **never executed by the runner**.

```
  turn commit (one tx):
    ┌─────────────────────────────────────┐
    │ state_events: model_turn            │
    │ state_events: tool_use (ord=0)      │
    │ state_events: tool_result (ord=0)   │
    │ state_outbox: {                     │
    │   id: 42,                           │
    │   idempotency_key: hash(conv_123,   │
    │                         turn=47,    │
    │                         ord=0),     │
    │   effect_kind: "transport.send",    │
    │   payload: { to: ..., body: ...},   │
    │   status: "pending"                 │
    │ }                                   │
    │ state_conversations: last_seq=N     │
    └─────────────────────────────────────┘
              ↓
      (transaction commits)
              ↓
      outbox relay (separate task, polls state_outbox)
              ↓
      dispatch via transport plugin with idempotency_key
              ↓
      transport plugin → external surface (Signal, email, …)
              ↓
      on success: status→"delivered", commit effect_committed event
      on failure: status→"pending", backoff, retry (max 5)
      after retry budget: status→"failed", move to dead-letter,
                         fire Error alert
```

If the same idempotency key retries (control plane crashed between send-attempt and status-update), the transport plugin's inbox returns the already-stored delivery ID and we mark success without double-sending.

---

## 9. Participant-aware policy

Every turn carries four trust-related inputs to the policy engine:

- **`sender_trust`** — trust level of the principal whose message triggered the turn.
- **`addressee_trust`** — who the reply is aimed at (default: the sender; for broadcasts: minimum in the room).
- **`effective_trust`** — minimum trust across readers (policy floor).
- **`conversation_kind`** — derived from participants: `ControllerDM`, `GroupWithControllerPresent`, `GroupWithControllerAbsent`, `MixedTrust`, `ExternalWithOutsider`.

### 9.1 Rule of Two (per turn)

```
  for each turn:
    count = 0
    if this turn ingests untrusted input:           count += 1
    if this turn accesses sensitive data:           count += 1
    if this turn produces an external effect:       count += 1

    if count > 2:
      phase → AwaitingApproval
      sideband notify controller (via a DIFFERENT transport
        than the one that introduced the untrusted content)
      no tool_use executes until approved
```

This is Meta's "Agents Rule of Two" pattern. It's the honest posture given that prompt injection at the model level is unsolved.

### 9.2 Planner/executor split for untrusted turns

For `ExternalWithOutsider` turns and any turn ingesting untrusted content:

```
  ┌──────────────┐                       ┌──────────────┐
  │   PLANNER    │                       │   EXECUTOR   │
  │              │                       │              │
  │ sees:        │                       │ sees:        │
  │  trusted     │                       │  UNTRUSTED   │
  │  metadata +  │                       │  content     │
  │  structured  │                       │  (spotlit)   │
  │  summaries   │                       │              │
  │              │                       │              │
  │ has:         │                       │ has:         │
  │  all tools   │                       │  NO tools    │
  │              │                       │              │
  │ produces:    │                       │ produces:    │
  │  tool calls  │───── placeholders ───▶│  values to   │
  │  with holes  │                       │  fill holes  │
  │              │◀─── tainted values ───│              │
  └──────────────┘                       └──────────────┘
                │
                ▼
    policy engine rejects tainted values entering
    sensitive sinks (cross-conversation send, vault read,
    shared-state write) unless explicitly trusted
```

This is CaMeL (DeepMind 2503.18813) restated for execlaw's trust classes. It doesn't prevent injection; it contains blast radius.

### 9.3 Cold-contact escalation (Phase 3)

An inbound message from a sender the control plane has never seen
before doesn't reach the model. Instead:

```
  POST /api/chats/:id/messages         controller's UI
  sender_principal_id = "stranger-42"         ▲
          │                                    │ sideband
          ▼                                    │ notification
  ┌───────────────────────────────────┐   ┌────┴───────┐
  │  resolve_sender()                  │   │ AlertFired │
  │  → PrincipalStore::find_by_identifier│  │ on WS bus  │
  │  → not found                       │   │ (source =   │
  │  → persist as UnknownPending       │   │  core.cold_ │
  │  → return to chat route            │   │  contact)   │
  └─────────┬─────────────────────────┘   └─────────────┘
            │                                    ▲
            ▼                                    │
  ┌───────────────────────────────────┐           │
  │  policy.evaluate_turn()            │           │
  │  sender_trust = UnknownPending     │           │
  │  → drop_turn = false               │           │
  │  → require_approval = true         │           │
  │  → spotlighting = true             │           │
  └─────────┬─────────────────────────┘           │
            │                                      │
            ▼                                      │
  ┌─────────────────────────────────────┐          │
  │  handle_cold_contact()               │          │
  │  1. commit ColdContactArrived event  │          │
  │  2. phase → AwaitingTrustDecision    │──────────┘
  │  3. publish UiEvent::AlertFired      │
  │  4. return 202 { approval_id }       │
  └─────────┬──────────────────────────┘
            │
            │  controller reads the alert,
            │  decides a verb (Trust / TrustLimited /
            │  Block / IgnoreOnce)
            ▼
  POST /api/admin/approvals/:id/respond
  { "verb": "trust" }
            │
            ▼
  ┌─────────────────────────────────────┐
  │  PrincipalStore::set_trust(...)      │
  │  commit TrustChanged event           │
  │  replay original message on WS bus   │
  │  phase → Idle                        │
  └─────────────────────────────────────┘
            │
            ▼
  Normal turn path resumes; original text is now
  processed as a KnownTrusted/KnownLimited message.
```

Note that **the model sees nothing during the cold-contact window** —
no prompt is assembled, no tool is called. An injection attempt hidden
inside the first message from a stranger simply parks the conversation
until the controller intervenes. This is the architectural containment
for the "first contact" attack vector.

### 9.4 Capability tokens

Every runner gets an Ed25519-signed JWT at spawn. Exact field names match `crates/server/src/auth.rs`:

```json
{
  "sub": "pri_controller",
  "principal_id": "pri_controller",
  "conversation_id": "conv_abc123",
  "turn_seq": 47,
  "session_id": "sess_a3f91",
  "capability_set": ["tools.*", "memory.read", "memory.write"],
  "iat": 1714230000,
  "exp": 1714233600,
  "nonce": "a3f91..."
}
```

Bound to a specific conversation and a specific turn. Cross-conversation reads/writes are rejected by the policy engine. A runner compromised by prompt injection is bounded — it can only affect the conversation it's serving.

---

## 10. Memory layers

Four layers, all implemented as OpenAI function-call tools (no vendor SDK).

| Layer | Storage | Agent-facing tools |
|---|---|---|
| **Transcript** | `state_events` | (implicit — hydrated into context) |
| **Scratchpad** | `state_conversations.scratchpad_blob` | `read_scratchpad()`, `write_scratchpad(content)` |
| **Compaction summaries** | Rust-side pass writes into scratchpad before trimming prompt | `compaction_note(content)` (internal) |
| **Long-term memory** | `memory_entries(scope, trust_class, key, value)` | `read_memory(scope, key)`, `write_memory(scope, key, value)`, `list_memory(scope, prefix)` |

The long-term memory tool shim **enforces trust-class scoping before returning any value**. An `ExternalWithOutsider` turn calling `read_memory("controller", "personal_calendar")` gets back a policy denial — the data never leaves the database.

---

## 11. Sub-agents (default on)

The primary runner can spawn background sub-agents for:

1. **Guardrails** — one-shot parallel classifiers (input risk, output policy check).
2. **Research fan-out** — deep research via `plugin-research-orchestrator` (Phase 2 port of selfhosted-claw's `DeepResearchExecutor`).
3. **Deep reasoning** — escalation to the same model in reasoning mode, or a more deliberate-prompt invocation, for hard problems in voice/chat.

Sub-runner invocation:

```
  primary agent                  control plane                  sub-runner
       │                               │                             │
       │ tool: spawn_research(         │                             │
       │         questions=[...],      │                             │
       │         budget={...},         │                             │
       │         return_mode="async")  │                             │
       │──────────────────────────────▶│                             │
       │                               │ create research_job row     │
       │                               │ spawn sub-runner with       │
       │                               │   capability token capped   │
       │                               │   at KnownTrusted           │
       │                               │────────────────────────────▶│
       │ ack_text, research_id ◀───────│                             │
       │                               │                             │
       │ (primary turn commits         │                             │
       │  immediately; phase→Idle)     │ SCOPE → SEARCH → FETCH →    │
       │                               │ SUMMARIZE → (images) →      │
       │                               │ DRAFT → EXEC_SUMMARY → PDF  │
       │                               │                             │
       │                               │◀────────── events ──────────│
       │                               │  (research_progress_updated)│
       │                               │                             │
       │                               │                             │
       │                               │ sub-runner completes;       │
       │                               │ control plane appends       │
       │                               │ synthetic user msg          │
       │                               │ "[Research <id> complete]"  │
       │                               │ to the conversation         │
       │                               │                             │
       │ new turn triggers.            │                             │
       │ agent calls read_research(id);│                             │
       │ digest is in context; agent   │                             │
       │ writes response to user.      │                             │
```

Sub-runners have a narrower tool set (`search_web`, `fetch_url`, `read_pdf`, `describe_image`, `write_research_note`). They cannot touch the parent conversation's memory or send external messages.

`ExternalWithOutsider` turns spawn only guardrail sub-agents — no research fan-out, no deep escalation (peer-agent privilege escalation is a documented jailbreak vector).

---

## 12. Runner isolation — per-conversation hot containers

```
         control plane (container manager)
                       │
                       │ bollard (Docker API)
                       │
          ┌────────────┼────────────┐──────────────────┐
          │            │            │                  │
          ▼            ▼            ▼                  ▼
    ┌──────────┐ ┌──────────┐ ┌──────────┐      ┌──────────┐
    │ runner   │ │ runner   │ │ runner   │ ...  │ runner   │
    │ conv_A   │ │ conv_B   │ │ conv_C   │      │ conv_N   │
    │          │ │          │ │          │      │          │
    │ token    │ │ token    │ │ token    │      │ token    │
    │ scoped   │ │ scoped   │ │ scoped   │      │ scoped   │
    │ to A     │ │ to B     │ │ to C     │      │ to N     │
    └──────────┘ └──────────┘ └──────────┘      └──────────┘
         │            │            │                  │
         └────────────┴────────────┴──────────────────┘
                              │
                  OpenAI-compatible inference
                              │
                              ▼
           ┌──────────────────────────────────┐
           │  service-vllm  (Qwen3.5-27B-AWQ) │
           │  service-whisper  service-kokoro │
           └──────────────────────────────────┘
```

**Lifecycle:**

- Spawn on demand when a conversation has pending work.
- Stay warm per-conversation for a configurable idle window (default ~10 min).
- Reap on idle or memory pressure; respawn if the conversation wakes again.
- On crash (OOM, panic, kill), the container manager detects via bollard events, commits cancellation `tool_result`s for any open `tool_use`, and respawns. Work is never lost; work is never double-executed.

**Minimal image per axiom #12:** Rust binary + shared-lib deps + CA certs. No vendor SDKs, no model weights, no Python. All heavy runtime lives in the `service-*` backends.

---

## 13. Recovery — what happens on every kind of interruption

| Interruption | What happens | What prevents loss / loops / duplicates |
|---|---|---|
| **User cancels mid-turn** | Runner closes SSE stream; cancellation `tool_result`s committed for open `tool_use`s; phase → `Idle` | Pairing invariant enforcement |
| **Runner crash** (OOM, panic, docker kill) | Bollard event detected; cancellation results committed; new runner spawns and hydrates | Pairing invariant + stateless runners + respawn |
| **Control plane restart** (compose down/up, upgrade) | Scan `state_conversations` for stale leases; cancellation for dangling `tool_use`; phase → `Idle`; scheduler picks up pending wakeups | Lease expiry + pairing invariant + event log as source of truth |
| **Host power loss** | SQLite WAL replay restores last committed state; same flow as control-plane restart. Outbox rows in `in_flight` retry on startup | SQLite atomicity + outbox idempotency + inbox dedup |
| **Transport drop** | Transport plugin reconnects, resumes inbound poll from `transport_cursors.cursor_value`; inbox dedup absorbs duplicates | Stable `(plugin_id, source_event_id)` dedup at ingress |
| **External API timeout** (we don't know if send landed) | Outbox row stays `in_flight`; retry uses same idempotency key; consumer-side inbox returns stored delivery ID; mark success | Framework-minted idempotency + consumer inbox |
| **Infinite-retry risk** (tool keeps failing) | Per-effect retry budget: 5 attempts + exp backoff + dead-letter; Error alert fires; turn continues with `retry_budget_exhausted` error fed back to agent | Hard retry caps + dead-letter queue + alerting |
| **Infinite-wakeup risk** (agent keeps scheduling wakeups) | Rate limit (12/hr/conversation); exceeding suspends further wakeups + fires Error alert for controller review | Wakeup rate limit + alert |
| **Controller device loss** | Vault backup (`execlaw vault export`) produces an encrypted bundle; restore rebuilds Controller identity; re-bind channel identifiers | Vault portability |

---

## 14. Observability

Two logging streams, both local, both mirroring selfhosted-claw's pattern:

```
  anywhere in the Rust code:
     tracing::info!(field = value, "message");
                         │
                         ▼
    tracing-subscriber (JSON layer)
                         │
             ┌───────────┴───────────┐
             ▼                       ▼
    ~/.execlaw/logs/*.jsonl   SQLite log_entries table
    (rolling, tailable)       (queryable, filterable in UI)
```

No OpenTelemetry, no Langfuse, no Arize Phoenix — that's bloat for a single-operator system. The `state_events` table IS the forensic audit trail; logs are the operational view on top.

Admin UI has a log viewer with filters by level / plugin / conversation / time window. `execlaw replay <conversation_id> --at <seq>` rebuilds the exact prompt + capability set + policy decisions for a specific turn.

---

## 15. Voice adaptations (pointer)

The voice modality uses the same event log, runner, policy, memory, and outbox. What differs:

- **Pipeline**: streaming STT (Whisper) → LLM (Qwen) → streaming TTS (Kokoro) orchestrated by the in-tree `voice-pipeline` crate — a two-lane Tokio graph (system lane for interrupts, data lane for audio/text).
- **Endpoint detection**: punctuation + dynamic silence heuristic (not a separate model).
- **Barge-in**: Silero VAD + 120ms backchannel-rescind window (LiveKit pattern).
- **Event kinds** are finer-grained (`stt.partial`, `tts.audio_chunk`, etc.) because commits happen per utterance / tool call / approval rather than per turn.
- **Runner deployments**: STT can run on Intel Arc via OpenVINO while LLM runs on nvidia via vLLM — the voice pipeline composes them.

Full detail in [`MIGRATION_PLAN.md` §2.13](../MIGRATION_PLAN.md).

---

## 16. Key source files

For the reader who wants to jump into code:

| File | What's there |
|---|---|
| [`crates/core/migrations/0001_initial_schema.sql`](../crates/core/migrations/0001_initial_schema.sql) | All 22 tables |
| [`crates/core/migrations/0002_event_hmac_tag.sql`](../crates/core/migrations/0002_event_hmac_tag.sql) | HMAC `tag` + `key_id` on `state_events` |
| [`crates/core/migrations/0003_state_plugins.sql`](../crates/core/migrations/0003_state_plugins.sql) | Plugin install persistence |
| [`crates/core/src/events.rs`](../crates/core/src/events.rs) | Event-log primitives, `commit_turn`, `enforce_tool_pairing`, HMAC sign/verify |
| [`crates/core/src/event_hmac.rs`](../crates/core/src/event_hmac.rs) | HMAC-SHA256 canonical bytes + constant-time verify |
| [`crates/core/src/principal.rs`](../crates/core/src/principal.rs) | Trust ladder + `PrincipalStore` persistence (Phase 3) |
| [`crates/core/src/outbox.rs`](../crates/core/src/outbox.rs) | Outbox enqueue / inbox dedup |
| [`crates/policy/src/trust.rs`](../crates/policy/src/trust.rs) | `evaluate_turn` + capability tiers + Rule of Two |
| [`crates/policy/src/spotlighting.rs`](../crates/policy/src/spotlighting.rs) | Per-conversation random delimiters |
| [`crates/policy/src/sideband.rs`](../crates/policy/src/sideband.rs) | Sideband transport picker + `ApprovalVerb` |
| [`crates/policy/src/input_guard.rs`](../crates/policy/src/input_guard.rs) | Zero-width / bidi / homoglyph strip |
| [`crates/plugin-sdk/src/manifest.rs`](../crates/plugin-sdk/src/manifest.rs) | Hook-based manifest parser + `[runtime]` |
| [`crates/plugin-host/src/host.rs`](../crates/plugin-host/src/host.rs) | `PluginHost` lifecycle (Phase 2) |
| [`crates/plugin-host/src/hook_registry.rs`](../crates/plugin-host/src/hook_registry.rs) | Tool/transport/identity-provider lookup maps |
| [`crates/plugin-host/src/subprocess.rs`](../crates/plugin-host/src/subprocess.rs) | Subprocess plugin tier (JSON-RPC over stdio) |
| [`crates/container-manager/src/hardware.rs`](../crates/container-manager/src/hardware.rs) | Tier-1 sysfs GPU detection |
| [`crates/server/src/routes.rs`](../crates/server/src/routes.rs) | REST surface (auth, OpenAPI) |
| [`crates/server/src/chats.rs`](../crates/server/src/chats.rs) | Chat surface — policy + capability + cold-contact + streaming |
| [`crates/server/src/approvals.rs`](../crates/server/src/approvals.rs) | `POST /api/admin/approvals/:id/respond` (Phase 3) |
| [`crates/server/src/plugins.rs`](../crates/server/src/plugins.rs) | `POST /api/admin/plugins/install` + lifecycle (Phase 2) |
| [`crates/server/src/tool_dispatch.rs`](../crates/server/src/tool_dispatch.rs) | `ChainedToolDispatch` — built-ins → plugins with capability check |
| [`crates/server/src/capability.rs`](../crates/server/src/capability.rs) | Per-turn capability token issue + verify |
| [`crates/runner-local/src/turn.rs`](../crates/runner-local/src/turn.rs) | TurnExecutor — full tool-loop turn path |
| [`crates/runner-local/src/memory_tool.rs`](../crates/runner-local/src/memory_tool.rs) | `read_memory` / `write_memory` with trust-class scoping |
| [`crates/inference-api/src/lib.rs`](../crates/inference-api/src/lib.rs) | OpenAI-compatible client + streaming SSE |
| [`crates/voice-pipeline/src/`](../crates/voice-pipeline/src/) | Two-lane graph, endpointer, bargein (Phase 4) |
| [`spec/asyncapi.yaml`](../spec/asyncapi.yaml) | WebSocket event vocabulary |
| [`plugins/hello/`](../plugins/hello/) | In-tree reference subprocess plugin |
| [`docs/plugin-inventory.md`](./plugin-inventory.md) | Phase 8 port queue |
| [`crates/cli/src/main.rs`](../crates/cli/src/main.rs) | `execlaw` CLI (+ lifecycle subcommands) |

---

## 17. Non-goals (what execlaw deliberately does not do)

These are *not* oversights — they are chosen constraints:

- **Cloud LLMs.** Not as default, not as opt-in, not ever. No Anthropic, OpenAI, Gemini, or equivalent on any code path. Models must be hosted locally.
- **Native-audio full-duplex** (GPT-4o Realtime-style). The OSS ecosystem hasn't shipped something portable across nvidia + Intel with acceptable quality. Cascaded STT→LLM→TTS with aggressive barge-in is the self-hosted ceiling; we accept the latency delta.
- **Vendor agent SDKs.** The Claude Agent SDK, OpenAI Assistants API, and equivalents are not used. We implement sessions, memory, streaming, tool use, compaction, and reasoning-on-demand ourselves in Rust against a local OpenAI-compatible inference endpoint. Research findings from those SDKs inform design; they do not define dependencies.
- **Multi-agent by default — with exception for research.** Default is single-threaded. Sub-agents are endorsed for guardrails, research fan-out, and deep escalation; never for untrusted conversations.
- **Hosted plugin registries.** Plugins install via ZIP upload. No central index, no `cargo install`-style package manager for plugins.
- **Complex observability stack.** No OpenTelemetry, Langfuse, Phoenix. JSONL + SQLite, same as selfhosted-claw.
- **Distributed operation.** Single host. SQLite is enough; the whole thing runs in one `docker compose up`.

---

## 18. What's built vs. what's next

Per [`STATUS.md`](../STATUS.md) as of 2026-04-24.

**Phase 0 — Foundation + local inference + GPU-aware deployment.** Complete.

**Phase 1 — Agent core with one transport (web chat).** Complete.
- Event-log primitives with pairing-invariant enforcement
- HMAC-signed event log (§7.8): migration 0002 + sign-on-append + verify-on-replay
- TurnExecutor wired into `POST /api/chats/:id/messages`
- Policy + per-turn capability token on the turn path
- Streaming SSE (`chat_completions_stream`) + `ChatTokenDelta` on the WS bus
- Crash-safety tests (kill mid-turn, replay-after-restart, post-commit tamper)

**Phase 2 — Plugin framework.** Complete (ports moved to Phase 8).
- `PluginHost` lifecycle (install/enable/disable/uninstall/hydrate) with SQLite persistence via migration 0003
- `POST /api/admin/plugins/install` + list / enable / disable / uninstall / tools routes
- Manifest `[runtime]` table for subprocess-tier entrypoint declaration
- Capability-enforced `ChainedToolDispatch` — built-ins → plugins → error
- In-tree reference plugin at `plugins/hello/`
- Tool-capable chat path that lights up when tools are registered

**Phase 3 — Participants, trust, policy engine, Rule of Two.** Complete (in-tree demos).
- `PrincipalStore` persists the full rich `TrustLevel` variant via JSON
- Identity resolution in the chat route: unknown senders → `UnknownPending` + cold-contact flow
- Cold-contact escalation: `ColdContactArrived` event + `AwaitingTrustDecision` phase + `AlertFired` sideband broadcast
- `POST /api/admin/approvals/:id/respond` with every `ApprovalVerb` branch: `Trust` → KnownTrusted, `TrustLimited` → KnownLimited with allowed_topics, `Block` → Blocked (future messages 403), `IgnoreOnce` → clear park without trust change
- `TrustChanged` event committed on every transition (audit trail)
- Original message replayed on the WS bus when the controller approves
- Spotlighting applied to prompt assembly when `policy.spotlighting` fires (untrusted sender sees delimiter-wrapped content; the log still holds unwrapped text)
- Trust-class-scoped memory reads (via `MemoryStore` + `memory_tool` shim from Phase 1)

**Phase 3 deferrals** (land in Phase 8 or the next Phase 3 iteration):
- Identity-provider plugin contract dispatch: the hook registry tracks `identity_providers`, but the chat route doesn't yet iterate them on inbound. Adding `PluginHost::resolve_identity` parallel to `call_tool` unblocks the reference `identity-local-address-book` plugin.
- Planner/executor split in `TurnExecutor`: `policy.planner_executor` flag is plumbed through, but the executor itself still runs one model call per turn.
- `config_trust_policy` UI-editable defaults: SQLite table exists; UI surfacing lands with Phase 6.
- Cross-transport sideband delivery (controller approves via Signal): waits on `plugin-signal` in Phase 8.

**What's next — Phase 4 (voice pipeline primitives):** See `MIGRATION_PLAN.md` §11.
