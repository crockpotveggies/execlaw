# execlaw — Architecture

Reference document for the execlaw agent model. This is the mental model a new contributor needs in 30 minutes before they read a line of code.

Relationship to other docs:

- [`MIGRATION_PLAN.md`](../MIGRATION_PLAN.md) is the design rationale, section-by-section, with research citations and trade-off discussion. Read it when you need to understand *why*.
- This document is the *what*: the structure, the invariants, the flows. Read it when you need to understand *how things fit together*.
- [`agent-model.md`](agent-model.md) is the *how* of one turn — TurnExecutor, memory layers, reflection loop, planner/executor split.
- [`plugins.md`](plugins.md) is the plugin-author reference — manifest schema, runtime tiers, sidecar model, primitives, and a step-by-step guide for writing a custom plugin.
- [`sidecar-supervisor-design.md`](sidecar-supervisor-design.md) deep-dives the supervised-container layer plugins compose against.

---

## 1. One-paragraph pitch

**execlaw is a deterministic state machine over an append-only SQLite event log that occasionally calls an LLM.** The LLM is the interesting but *replaceable* part. The durability, policy, and isolation around it are the product. Everything runs on the operator's hardware (no cloud LLMs, ever). The control plane is a single native binary that registers as a host service (systemd / launchd / Windows SCM); the per-conversation runner is a Docker container the control plane spawns; inference backends (vLLM, Whisper, Kokoro) are separate local service containers. Plugins extend the system via a WordPress-style hook framework loaded from ZIP uploads.

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
11. **Native control-plane binary** — deployment artifact is a per-OS native binary registered as a host service via the `service-manager` crate (systemd on Linux, launchd on macOS, Service Control Manager on Windows). No Docker image for the control plane.
12. **Minimal containers** — every container image execlaw spawns (per-conversation runner, inference backends, plugin sidecars) ships only what its single job requires.

The rest of this document is these principles made concrete.

---

## 3. System topology

```
                            operator's machine
┌─────────────────────────────────────────────────────────────────────────────┐
│                                                                             │
│   ┌──────────────┐   HTTP+WS    ┌───────────────────────────────────────┐   │
│   │              │ ◀──────────▶ │  execlaw control plane                │   │
│   │   Chat UI    │   (JWT)      │  (native binary registered as a       │   │
│   │   (SPA)      │              │   host service via service-manager:   │   │
│   │              │              │   systemd / launchd / Windows SCM)    │   │
│   └──────────────┘              │                                        │   │
│                                 │  axum server  event log  scheduler    │   │
│                                 │  ┌──────────┐ ┌─────────┐ ┌─────────┐ │   │
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
│                    │  │  runner-local (per     │  │ plugins (ZIP-     │    │ │
│                    │  │  active conversation)  │  │ installed)        │    │ │
│                    │  │                        │  │                   │    │ │
│                    │  │  stateless, Ed25519    │  │  signal, whatsapp │    │ │
│                    │  │  capability token      │  │  slack, sms-socket│    │ │
│                    │  └───────────┬────────────┘  │  google-* trio    │    │ │
│                    │              │               │  pushover, hello, │    │ │
│                    │              │               │  identity-local…  │    │ │
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

### 4.1 Control plane (single native binary, host service)

The coordinator. Owns:

- **Event log** — `state_events` + related tables in SQLite (SQLCipher in production).
- **Scheduler** — priority queue for wakeups, sub-second precision.
- **Policy engine** — capability tokens, trust resolution, Rule of Two, input guards.
- **Plugin host** — manifest parsing, ZIP install, hook registry.
- **Container manager** — bollard client wrapping all Docker operations the control plane delegates *out* (per-conversation runner spawns, plugin sidecars, inference services).
- **Outbox relay** — drains `state_outbox` to transport plugins with idempotency.
- **Axum server** — REST + WebSocket surface for UI and plugins.
- **Vault** — SQLCipher-encrypted secrets; master key from OS keyring.

Deployed as a per-OS native binary, one of `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, or `aarch64-apple-darwin`. Intel Macs (`x86_64-apple-darwin`) are intentionally out of scope — the only macOS-specific code path that justifies a dedicated build is Metal-accelerated inference, which doesn't exist on Intel hardware. The `service-manager` crate registers the binary as a host service — systemd unit on Linux, launchd plist on macOS, Service Control Manager entry on Windows. State lives at `~/.execlaw/` (SQLite DB, master key, per-plugin sidecar volumes). No Docker image for the control plane itself; `execlaw install` migrates the DB, registers the service, and starts it.

### 4.2 Runner (one container *per active conversation*)

Thin Rust binary (`runner-local`). Speaks OpenAI-compatible API to whichever local inference backend is configured. Stateless against the event log: on spawn, hydrates context from SQLite via an authenticated RPC to the control plane; runs one turn; writes output back; exits (or stays warm for the next turn).

**Why per-conversation isolation?** Ported the HotRunnerPool pattern from selfhosted-claw. A runner compromised by prompt injection in conversation A can't touch conversation B's data — its capability token scopes it to one `conversation_id`.

### 4.3 Inference services (separate containers — or native subprocesses on Apple Silicon)

`service-vllm` (nvidia), `service-openarc` (Intel), `service-whisper`, `service-kokoro`, etc. Each serves an OpenAI-compatible or protocol-matched endpoint. Control plane calls them via `inference-api` client. These are the containers that carry the heavy vendor runtimes — keeping the control plane minimal (axiom #12).

**Apple Silicon carve-out:** `service-ollama` runs as a host-native subprocess, not a Docker container. Docker Desktop on macOS executes containers inside a Linux microVM with no Metal passthrough — every container-bound inference engine on Mac falls back to CPU and loses the entire point of an Apple-GPU host. The same constraint affects every Metal-backed engine (llama.cpp Metal, Whisper.cpp Metal, MLX), so the control plane manages them as native subprocesses via `NativeServiceController` instead. vLLM is intentionally **not** supported on Apple Silicon — it has no Metal kernels and the CPU build is unusable for any LLM larger than a few billion parameters. See [`setup-mac.md`](setup-mac.md) for first-run setup. The "minimal containers" axiom (#12) still holds — it's the same principle expressed as "minimal native dependencies" because Apple Silicon doesn't offer a container-passthrough surface for the GPU.

| Host class | Standard inference | Process model |
|---|---|---|
| Linux + NVIDIA | vLLM | Docker container, `--gpus` passthrough |
| Linux + Intel Arc | vLLM-CPU / OpenVINO | Docker container, `/dev/dri` bind |
| Windows + NVIDIA | vLLM (Docker Desktop) | Docker container, `--gpus` passthrough |
| **macOS + Apple Silicon** | **Ollama** | **Native `ollama serve` subprocess** |
| Any host, GPU-less | vLLM-CPU | Docker container, CPU-only |

### 4.4 Plugins (ZIP-installed extensions)

Plugins are how every non-core capability lights up — transports, third-party integrations, identity providers, OAuth-using HTTP bridges, sidecar-backed services. Operator uploads a ZIP via the SPA; the host parses `plugin.toml`, registers all declared hooks atomically, and from that moment the plugin's tools appear in the agent's catalog (subject to capability + trust gating). Two runtime tiers: **script** (Rhai source loaded into an embedded interpreter — the dominant tier; used by signal, whatsapp, slack, discord, sms-socket, google-apps, google-places, open-meteo, pushover) and **subprocess** (native binary, JSON-RPC over stdio — used by hello reference plugin and identity-local-address-book). Transport-class plugins additionally implement the conversation-routing contract: receive inbound events, push them to the event log with stable `(plugin_id, source_event_id)` identifiers, drain outbox rows, deliver to the external surface. Full reference in [`plugins.md`](plugins.md).

### 4.5 Outbox relay

A separate async task in the control plane, explicitly *not* invoked by the runner. Reads `state_outbox`, delivers via transport plugins with the framework-minted idempotency key, handles retries (5 attempts + exponential backoff + dead-letter), and commits `effect_committed` events on success. The LLM never calls an external API directly; this is the only path out.

---

## 5. Data model

Full schema is the union of every file in [`crates/core/migrations/`](../crates/core/migrations/) — initial schema in `0001_initial_schema.sql` plus 30+ incremental migrations as the system has grown (HMAC-tag column, plugin install table, eval flags, users + WebAuthn, principal groups, OAuth accounts, skills, transport bindings, search providers, memory lifecycle, …). The load-bearing tables:

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
- `config_trust_policy`, `config_alert_routing`, `config_research_quota`, `config_runtime_settings`, `config_general` — operator-editable settings.
- `config_tool_access` (migration 0009) — per-trust-class capability grants for tool dispatch.
- `config_mcp_servers` (migration 0010) — operator-supplied MCP server registrations; tools surface dynamically alongside plugin tools.
- `config_routines` — cron-shaped recurring tasks fired through the wakeup channel.
- `research_jobs` (migration 0027) — background research sessions (§2.9.1 of plan).
- `vault_secrets` — SQLCipher-encrypted secret store; references are opaque to plugins.
- `log_entries` — SQLite half of the JSONL+SQLite dual log sink.
- `transport_cursors` — per-transport resume point (what `source_event_id` was last processed).
- `transport_conversations` (migration 0006) — `(plugin_id, transport_handle, principal_id) → conversation_id` mapping that the `ConversationResolver` uses on inbound to decide whether a new message continues an existing thread or rotates to a new one. The Controller principal short-circuits: every controller message — across web, voice, Signal, WhatsApp, SMS, Slack, email — collapses into one fixed `controller-thread` ConversationId so the SPA can render a single pinned **Control thread**.
- `transport_bindings` (migration 0032) — `(transport, foreign_id) → principal_id` map that drives auto-bridge transport selection (`bridge_text_reply_to_originating_transport`).
- `principal_groups` (migration 0024) — `principal_group_id ↔ conversation_id` mapping; lets multi-channel principals share one conversation thread.
- `eval_flagged` (migration 0004) — operator-tagged regression-target event ranges.
- `state_plugins` (migration 0003) — persisted plugin installs; re-hydrated on every server boot.
- `state_oauth_clients`, `state_oauth_tokens` (migration 0028) — OAuth client metadata + access/refresh tokens for plugins that declare `[[oauth_accounts]]`. Plugins never see refresh tokens or client secrets.
- `users`, `state_webauthn_credentials`, `state_refresh_tokens` (migrations 0005/0007/0008) — operator account + auth state for the SPA.
- `state_skills`, `state_skill_proposals`, `config_skills` (migrations 0029–0031) — operator-authored skill markdown registry; plugins ship skills via `[[skills]]` manifest entries.
- `search_providers` (migration 0033) — pluggable search backend registrations for the research subsystem.

### 5.7 Threads, controller-thread merge, and incognito

A **thread** is the user-facing name for a `ConversationId`. The word *session* is reserved for JWT auth state.

UI channels mint a fresh thread on "new chat" — explicit. Non-UI channels (Signal, email, voice) call `ConversationResolver::resolve_or_mint(plugin_id, transport_handle, principal_id, idle_timeout_ms)` on every inbound message. The resolver:

1. **Controller short-circuit** — if the resolved principal is Controller, ALWAYS return `controller-thread:<controller_principal_id>`. One DM, every channel.
2. Otherwise: look up the `is_current = 1` row for the triple. If present and `now - last_message_at < idle_timeout_ms`, return its `conversation_id`.
3. Else: mark old as `is_current = 0`, mint new, insert, return new.

Default `idle_timeout_ms` per transport: web/UI = explicit (resolver not called), Signal = 24 h, email = none (every reply continues), voice = 5 min, SMS = 4 h.

**Per-message `channel_origin`** field on event payloads lets the SPA render channel icons in the Control thread without losing the unified-DM UX.

**Incognito threads** (`is_ephemeral = 1` on `state_conversations`) persist events during the conversation (so crash recovery works) but the `EphemeralSweeper` task DELETEs every event row whose parent is past `ephemeral_expires_at`. The conversation row stays with `last_seq = 0` after purge so audit reports can show "N incognito threads existed but their content was purged." `execlaw replay` skips purged ephemerals.

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
| **Control plane restart** (`execlaw service restart`, OS reboot, deploy) | Scan `state_conversations` for stale leases; cancellation for dangling `tool_use`; phase → `Idle`; scheduler picks up pending wakeups | Lease expiry + pairing invariant + event log as source of truth |
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
| [`crates/plugin-sdk/src/manifest.rs`](../crates/plugin-sdk/src/manifest.rs) | Hook-based manifest parser. Source of truth for every section: `[plugin]`, `[runtime]`, `[[tools]]`, `[transport]`, `[identity_provider]`, `[[services]]` + `[services.sidecar]`, `[[admin_routes]]`, `[[webhook_routes]]`, `[[oauth_accounts]]`, `[[ui_panels]]`, `[[skills]]`, `[[health_checks]]`, `[[event_subscriptions]]`, `[[alert_sources]]`. |
| [`crates/plugin-host/src/host.rs`](../crates/plugin-host/src/host.rs) | `PluginHost` install/upgrade/enable/disable/hydrate lifecycle |
| [`crates/plugin-host/src/hook_registry.rs`](../crates/plugin-host/src/hook_registry.rs) | Tool / transport / identity-provider / admin-route / webhook-route lookup maps |
| [`crates/plugin-host/src/subprocess.rs`](../crates/plugin-host/src/subprocess.rs) | Subprocess plugin tier (JSON-RPC over stdio) |
| [`crates/script/src/primitives.rs`](../crates/script/src/primitives.rs) | Rhai script-tier primitive registration: HTTP, sidecar, vault, OAuth, WS, `host_route_inbound` / `host_route_inbound_spawn`, JSON, time, logging |
| [`crates/server/src/sidecar_supervisor.rs`](../crates/server/src/sidecar_supervisor.rs) | Supervised-container reconcile loop, health probe, crash-loop guard. See [`docs/sidecar-supervisor-design.md`](sidecar-supervisor-design.md). |
| [`crates/server/src/plugin_admin_routes.rs`](../crates/server/src/plugin_admin_routes.rs) | Authenticated dispatcher at `/api/admin/plugins/{plugin_id}{path}` |
| [`crates/server/src/plugin_webhook_routes.rs`](../crates/server/src/plugin_webhook_routes.rs) | Unauthenticated dispatcher at `/api/webhooks/{plugin_id}{path}`; supports both `application/json` and `application/x-www-form-urlencoded` bodies |
| [`crates/container-manager/src/hardware.rs`](../crates/container-manager/src/hardware.rs) | Cross-platform GPU detection — Linux sysfs (Tier 1) + `hardware-query` (WMI on Windows) + `system_profiler SPDisplaysDataType -json` parse on macOS (the upstream crate's macOS GPU path is currently stubbed). Apple Silicon SoCs surface as `GpuVendor::Apple` with a unified-memory budget derived from `sysctl hw.memsize × 2/3` (matches macOS's `iogpu.wired_limit` default). |
| [`crates/container-manager/src/service.rs`](../crates/container-manager/src/service.rs) | `ServiceController` trait + `BollardServiceController` (Docker) + `NativeServiceController` (host subprocess) + `MultiplexedServiceController` (per-spec dispatch). `ServiceSpec::runtime: ServiceRuntime` (Docker / Native) drives which one spawns; default is Docker for backwards-compat. Native is gated on `binary_hint` (`"ollama"` in v1) so future native engines (llama-server, MLX) slot in by adding match arms in `discover_for_hint`. |
| [`crates/server/src/routes.rs`](../crates/server/src/routes.rs) | REST surface (auth, OpenAPI) |
| [`crates/server/src/chats.rs`](../crates/server/src/chats.rs) | Chat surface — policy + capability + cold-contact + streaming |
| [`crates/server/src/approvals.rs`](../crates/server/src/approvals.rs) | `POST /api/admin/approvals/:id/respond` (Phase 3) |
| [`crates/server/src/plugins.rs`](../crates/server/src/plugins.rs) | `POST /api/admin/plugins/install` + lifecycle (Phase 2) |
| [`crates/server/src/tool_dispatch.rs`](../crates/server/src/tool_dispatch.rs) | `ChainedToolDispatch` — built-ins → plugins with capability check |
| [`crates/server/src/capability.rs`](../crates/server/src/capability.rs) | Per-turn capability token issue + verify |
| [`crates/runner-local/src/turn.rs`](../crates/runner-local/src/turn.rs) | TurnExecutor — full tool-loop turn path |
| [`crates/core/src/builtin_tools.rs`](../crates/core/src/builtin_tools.rs) | Built-in tool implementations including `read_memory` / `write_memory` / `list_memory` |
| [`crates/core/src/tool_apis.rs`](../crates/core/src/tool_apis.rs) | `DbMemoryApi` — trust-class read-down cascade enforced at the storage shim |
| [`crates/core/src/memory_lifecycle.rs`](../crates/core/src/memory_lifecycle.rs) | `PromotionStore` + `ReflectionStore` (memory hot/warm/cold lifecycle) |
| [`crates/inference-api/src/lib.rs`](../crates/inference-api/src/lib.rs) | OpenAI-compatible client + streaming SSE |
| [`crates/voice-pipeline/src/graph.rs`](../crates/voice-pipeline/src/graph.rs) | Two-lane Tokio graph (system lane preempts data lane) |
| [`crates/voice-pipeline/src/traits.rs`](../crates/voice-pipeline/src/traits.rs) | `AudioIn`/`AudioOut`/`Vad`/`SttClient`/`TtsClient` + mocks |
| [`crates/voice-pipeline/src/session.rs`](../crates/voice-pipeline/src/session.rs) | `VoiceSession` orchestrator + voice-event log wiring (Phase 4) |
| [`crates/voice-pipeline/src/endpointer.rs`](../crates/voice-pipeline/src/endpointer.rs) | Punctuation-aware endpointer |
| [`crates/voice-pipeline/src/bargein.rs`](../crates/voice-pipeline/src/bargein.rs) | Barge-in / backchannel-rescind decision |
| [`spec/asyncapi.yaml`](../spec/asyncapi.yaml) | WebSocket event vocabulary |
| [`plugins/hello/`](../plugins/hello/) | In-tree reference subprocess plugin |
| [`plugins/signal/`, `plugins/whatsapp/`, `plugins/slack/`, `plugins/sms-socket/`](../plugins/) | Transport plugins — sidecar / webhook / WS variants |
| [`plugins/google-apps/`, `plugins/google-places/`](../plugins/) | OAuth + API-key reference HTTP plugins |
| [`docs/plugins.md`](plugins.md) | Plugin-author reference: when to use plugins vs MCP, manifest schema, sidecar model, primitives, walkthroughs |
| [`crates/core/src/eval.rs`](../crates/core/src/eval.rs) | `EvalFlaggedStore` for regression-target event ranges (Phase 5) |
| [`crates/server/src/observability.rs`](../crates/server/src/observability.rs) | `GET /api/admin/logs` + `GET /api/admin/eval/flags` (Phase 5) |
| [`crates/server/src/tracing_layer.rs`](../crates/server/src/tracing_layer.rs) | `SqliteLogLayer` — mirrors tracing events into `log_entries` (Phase 5) |
| [`crates/eval-harness/src/main.rs`](../crates/eval-harness/src/main.rs) | LLM-judge harness against local Qwen (Phase 5) |
| [`evals/rubrics/`](../evals/rubrics/) | Rubric TOML files |
| [`crates/cli/src/main.rs`](../crates/cli/src/main.rs) | `execlaw` CLI (+ replay + eval flag/list subcommands) |

---

## 17. Non-goals (what execlaw deliberately does not do)

These are *not* oversights — they are chosen constraints:

- **Cloud LLMs.** Not as default, not as opt-in, not ever. No Anthropic, OpenAI, Gemini, or equivalent on any code path. Models must be hosted locally.
- **Native-audio full-duplex** (GPT-4o Realtime-style). The OSS ecosystem hasn't shipped something portable across nvidia + Intel with acceptable quality. Cascaded STT→LLM→TTS with aggressive barge-in is the self-hosted ceiling; we accept the latency delta.
- **Vendor agent SDKs.** The Claude Agent SDK, OpenAI Assistants API, and equivalents are not used. We implement sessions, memory, streaming, tool use, compaction, and reasoning-on-demand ourselves in Rust against a local OpenAI-compatible inference endpoint. Research findings from those SDKs inform design; they do not define dependencies.
- **Multi-agent by default — with exception for research.** Default is single-threaded. Sub-agents are endorsed for guardrails, research fan-out, and deep escalation; never for untrusted conversations.
- **Hosted plugin registries.** Plugins install via ZIP upload. No central index, no `cargo install`-style package manager for plugins.
- **Complex observability stack.** No OpenTelemetry, Langfuse, Phoenix. JSONL + SQLite, same as selfhosted-claw.
- **Distributed operation.** Single host. SQLite is enough; the control plane runs as one host service, the runner + inference + plugin sidecars are local containers it spawns over the host's Docker socket.

---

## 18. What's built vs. what's next

Last refreshed: 2026-05-08. The phase tags below reflect implementation milestones; for the live-progress feed, look at `git log` on `foundation` and the per-plugin manifests under `plugins/`.

**Phase 0 — Foundation + local inference + GPU-aware deployment.** Complete.

**Phase 1 — Agent core with one transport (web chat).** Complete.
- Event-log primitives with pairing-invariant enforcement
- HMAC-signed event log (§7.8): migration 0002 + sign-on-append + verify-on-replay
- TurnExecutor wired into `POST /api/chats/:id/messages`
- Policy + per-turn capability token on the turn path
- Streaming SSE (`chat_completions_stream`) + `ChatTokenDelta` on the WS bus
- Crash-safety tests (kill mid-turn, replay-after-restart, post-commit tamper)

**Phase 2 — Plugin framework.** Complete and exercised in production by 12 in-tree plugins.
- `PluginHost` lifecycle (install/enable/disable/uninstall/hydrate) with SQLite persistence via migration 0003
- `POST /api/admin/plugins/install` + list / enable / disable / uninstall / tools routes
- Manifest schema: `[plugin]`, `[runtime]` (script + subprocess tiers), `[[tools]]` (with `host_internal`, `trust_floor`, `latency`), `[transport]`, `[identity_provider]`, `[[services]]` + `[services.sidecar]`, `[[admin_routes]]`, `[[webhook_routes]]` (unauthenticated, plugin validates), `[[oauth_accounts]]`, `[[ui_panels]]`, `[[skills]]`, `[[health_checks]]`, `[[event_subscriptions]]`, `[[alert_sources]]`
- Capability-enforced `ChainedToolDispatch` — built-ins → plugins → MCP → error
- Script-tier engine (`crates/script/src/`) — embedded Rhai with primitives for HTTP, sidecar HTTP (SSRF-aware), WebSocket subscribe / bidi, vault, OAuth-token injection, JSON, time (incl. `parse_rfc3339_ms`), routing (`host_route_inbound` synchronous + `host_route_inbound_spawn` fire-and-forget for HTTP-webhook handlers)
- Subprocess-tier engine — JSON-RPC over stdio; reference at `plugins/hello/`
- Authenticated admin router at `/api/admin/plugins/{id}/...`; unauthenticated webhook router at `/api/webhooks/{id}/...` accepting both `application/json` and `application/x-www-form-urlencoded` bodies
- Sidecar supervisor (`crates/server/src/sidecar_supervisor.rs`) with 5 s reconcile, health probes, crash-loop guard, stable per-`(plugin_id, sidecar_name)` host port allocation
- Shipped plugins: `signal` (Signal-Messenger transport, supervised `signal-cli` sidecar), `whatsapp` (WhatsApp transport, supervised `wuzapi` sidecar, webhook inbound, `markread` read receipts), `slack` (multi-workspace OAuth transport), `discord` (multi-guild gateway-WS transport), `sms-socket` (Android-gateway WS transport), `google-apps` (consolidated Gmail/Calendar/Contacts/Tasks/Drive OAuth integration; also an identity provider — replaced separate google-calendar + google-contacts plugins 2026-05-14), `google-places` (Places API key-only HTTP integration), `open-meteo` (key-less weather/marine/air-quality/seasonal/ensemble/flood/climate/geocoding/elevation tools + chart renderer), `pushover` (one-way notifier), `hello` (subprocess reference), `identity-local-address-book` (subprocess identity provider)
- Plugin-author reference: [`docs/plugins.md`](plugins.md)

**Phase 3 — Participants, trust, policy engine, Rule of Two.** Complete.
- `PrincipalStore` persists the full rich `TrustLevel` variant via JSON
- `ConversationKind::derive` derives the kind (ControllerDM / GroupWithControllerPresent / GroupWithControllerAbsent / ExternalWithOutsider / MixedTrust) from a slice of participant trust-class tags. Chat route refreshes the kind on every inbound message.
- Identity resolution in the chat route: unknown senders → identity-provider dispatch → UnknownPending + cold-contact OR auto-admit as KnownTrusted when a provider vouches
- `PluginHost::resolve_identity` iterates installed `identity_provider` hooks via JSON-RPC `identity.resolve`
- In-tree reference plugin `identity-local-address-book` (in `plugins/identity-local-address-book/`) — JSON-file contact list, exposes the `identity_provider` hook
- `classify_identity_matches` — pure decision function mapping provider matches (highest-confidence wins, `Unknown` hint rejected) to a `TrustLevel`
- Cold-contact escalation: `ColdContactArrived` event + `AwaitingTrustDecision` phase + `AlertFired` sideband broadcast
- **Signed approval-token JWT** (§2.11): cold-contact response includes a `approval_token` whose `jti` matches the `approval_id`. The respond endpoint verifies the JWT before honoring any verb so an attacker who guesses the id can't forge a response.
- `POST /api/admin/approvals/:id/respond` with every `ApprovalVerb` branch
- `POST /api/admin/principals/:id/revoke` for direct trust revocation
- `TrustChanged` event committed on every transition (audit trail)
- Spotlighting applied to prompt assembly when `policy.spotlighting` fires
- **Planner/executor containment** — when `policy.planner_executor` is true (effective_trust < KnownTrusted), the tool-capable chat path is disabled. A prompt-injected executor can't exfiltrate via tool_use args because there are no tool_use slots. Full placeholder-passing choreography lands as a later refinement.
- Trust-class-scoped memory reads (from Phase 1)

**Phase 3 deferrals**:
- `config_trust_policy` UI-editable defaults: SQLite table exists; UI surfacing lands with Phase 6.
- Cross-transport sideband delivery is wired now that signal/whatsapp/slack/sms-socket transports ship; remaining work is the controller-pick-transport policy table.
- Rule-of-Two breach approval flow for non-cold-contact (currently 202 awaiting_approval; the ApprovalVerb::Approve / Edit / Reject path lands when there's a sensitive-tool-call to gate).
- Group-awareness in agent classifier: shipped — agent now knows when it's in a group and is biased toward silence.

**Phase 4 — Voice pipeline primitives.** Complete (internal, with mocks).
- `traits.rs`: `AudioIn` / `AudioOut` / `Vad` / `SttClient` / `TtsClient` — the full contract between the pipeline and stage backends, plus `MockAudioIn` / `MockAudioOut` / `MockVad` / `MockStt` / `MockTts` for deterministic testing.
- `session.rs::VoiceSession`: the orchestrator. Owns the two-lane `Pipeline`, the stage clients, and the event-log handle (with optional HMAC key). Drives the full state machine: `Listening → UserSpeaking → AwaitingLlm → AgentSpeaking ↔ BargeInDecision → …`.
- Voice event schema wired to `state_events`: every stage transition commits a `voice.*` / `vad.*` / `stt.*` / `llm.*` / `tts.*` / `interrupt.*` row via `EventLog::append`. Timestamp (`t_ms`) stored on every row so EoS→first-audio latency can be reconstructed from the log.
- Sentence splitter (`chunk_at_sentence_boundaries`) feeds TTS chunk-by-chunk so first-audio latency can be minimized.
- Barge-in resolution: `resolve_bargein(user_still_speaking)` applies the existing `bargein::decide` rule table to the session state; on Confirm, cancels TTS + fires an `Interruption` on the system lane + commits `InterruptConfirmed`.
- **HMAC-signed voice events** verified end-to-end: tampering with a committed voice row trips `DbError::TamperDetected` on next replay (just like text events).
- **Crash invariant**: a mid-`speak` panic leaves a partially-committed log that still verifies under HMAC. No half-signed rows; the partial state faithfully records what happened without a misleading `TtsEnded`.
- **STT-transcript spotlighting** verified: a delimiter-smuggling attempt in a simulated STT transcript produces a wrap with exactly one outer open + one outer close — no escape.
- **Modality-adaptive helpers** (`VoiceTurnBudget`): voice turns get max_response_tokens=80, max_tool_rounds=1, low-latency-only tools, suppressed extended thinking. The chat route reads these values when running a voice turn.

**Phase 4 deferrals → Phase 8 (real-audio acceptance):**
- Silero VAD ONNX integration — `Vad` trait is ready; `MockVad` covers the decision logic; ONNX runtime binding lands as a feature-gated impl.
- `service-whisper` / `service-kokoro` / `service-piper` sidecar containers — `SttClient` / `TtsClient` traits are ready; the wrappers are subprocess plugins the plugin-host manages.
- `transport-voice` plugin for mic/speaker I/O + WebRTC AEC3 — `AudioIn` / `AudioOut` traits are ready.
- ≤1.1 s EoS → first-audio latency acceptance: can be measured once the real backends plug in (the `t_ms` field on every voice event exists precisely for this measurement).

**Phase 5 — Observability, evaluation, replay CLI (infra only).** Complete.
- Migration 0004: `eval_flagged` table for tagging regression-target event ranges
- `EvalFlaggedStore` (insert / list_all / list_by_label) with adversarial test (inverted range rejected)
- `LogStore::query` with level / plugin_id / conversation_id / since_ms filters + limit
- `SqliteLogLayer` — `tracing_subscriber::Layer` impl that mirrors every tracing event into `log_entries`. Best-effort writes (DB lock failures don't break the process).
- `GET /api/admin/logs` and `GET /api/admin/eval/flags` HTTP routes — pure data feeds for the Phase-6 React UI
- `execlaw replay <conversation_id> --at <seq>` CLI — reconstructs the prompt history + sender trust + policy decision (capabilities / planner_executor / spotlighting / latency_band) + the events that turn committed
- `execlaw eval flag <conv> --range a..b --label X --tags ... --notes ...` and `execlaw eval list [--label]` CLI commands
- `execlaw-eval-harness` Rust binary — runs rubric TOML against a local OpenAI-compatible endpoint (the same Qwen the agent uses; no cloud judge). `--mock` mode skips the network call so CI exercises the orchestration without a live model.
- Reference rubric at `evals/rubrics/trust-class.toml` covering: outsider can't pull Controller memory, Rule-of-Two breach blocked, tool_use/tool_result pairing.

**Phase 5 deferrals → Phase 6 (UI):**
- Log viewer React component
- Eval-flag dashboard
- Trace viewer embedded in the chat UI

**Phase 6 — UI port (chat-first SPA + Tauri Desktop).** Sub-phases 6a–6d landed; full incognito-thread UI polish and voice UI still pending.
- Chat-first SPA scaffolding under `web/` is live: pinned Control thread, thread list, inbound messages from external transports collapse into the controller thread per the `ConversationResolver` rule, OpenAI-style streaming token render.
- Settings → Plugins page drives install / enable / disable / uninstall + per-plugin admin panels (each plugin's `[[ui_panels]]` mounts a SPA route).
- Research subsystem (C3–C6) shipped: deep-research plan/gather/synthesize pipeline, retention policy, `/research` page, every-phase event flow.
- Approval queue infrastructure: cold-contact alerts, sensitive-tool approvals, OAuth-grant proposals all flow through one SPA dropdown.
- **6d Tauri Desktop wrapper** shipped 2026-05-15: `desktop-macos/src-tauri/` produces `execlaw.app` for Apple Silicon. Menu bar app with no Dock icon (NSApplication `Accessory` activation policy), registers the bundled LaunchAgent via Apple's `SMAppService` (macOS 13+) so drag-to-Trash auto-disables the service. SPA embedded in the server binary via `rust-embed`; the webview navigates to `http://127.0.0.1:3031` for same-origin API + SPA. Build via `scripts/build-mac.sh`; release via tag push (see `.github/workflows/macos-bundle.yml`).
- Pending: full incognito-thread UI polish, voice UI (lands with Phase 8 audio plugins).

Stack (locked in 2026-04-25):
- **React Native** + **react-native-web** as the cross-platform component layer.
- **react-bootstrap** + **Bootstrap CSS** + **Bootstrap Icons** (subtle, monochrome, theme-tinted) — works on react-native-web's DOM output and Tauri's webview; iOS/Android need a parallel native layer when those targets land.
- **Vite** (web) / **Metro** (native).
- **TanStack Query** for REST, **Zustand** for the WS event store.
- **Plugins are trusted** — UI panels load via dynamic ESM `import()`; no sandboxing.
- Built static assets embedded in the Rust binary via `rust-embed` (shipped 2026-05-15 alongside Phase 6d) so the production artifact stays a single binary serving SPA + API on one origin — both the Docker image and the Tauri `.app` bundle rely on this.

UX (locked):
- Chat-first landing; OpenWebUI-shaped sidebar with `New chat`, nav (Routines / Contacts / More → Tools, Skills, plugin panels), thread list, settings + user at the bottom.
- **Pinned Control thread** at the top of the thread list — every controller message regardless of channel collapses here, with subtle per-message channel icons.
- **Thread-list status icons**: empty grey dot (default), blue filled dot (agent replied unseen), animated loader (agent processing). External threads show their channel icon instead of the dot.
- **Thread names**: "Control thread" for the pinned controller thread; truncated transport-supplied name for external groups; LLM-generated 3-word summary (via `set_thread_name(name)` agent tool) for new internal threads.
- External-channel filter toggle above the thread list (Control thread always visible).
- ChatGPT-style approval card slides in from above the input.
- Tokens render as they arrive.
- Long messages truncate with "Read more…".
- `GET /api/ping` returns `pong` or `setup`; SPA routes to wizard on `setup`.
- **Incognito threads** — toggle in the new-thread modal; default 1h expiry; `EphemeralSweeper` purges events.
- Dark default with light/dark/system toggle.
- Voice UI deferred to Phase 8 with the real audio plugins.
- Native iOS / Android deferred to post-Phase-6.

Sub-phases: **6a** (scaffold + chat view + auth + WS bus + approval card + Control-thread merge), **6b** (admin read-views), **6c** (writes — setup wizard, plugin upload, approval verbs, trust revoke, incognito toggle, thread rename), **6d** (Tauri Desktop wrapper).

**What's next — Phase 6 (UI port + chat-first landing):** See `MIGRATION_PLAN.md` §11.
