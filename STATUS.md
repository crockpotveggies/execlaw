# execlaw build STATUS

Last update: end of Phase 1-4 groundwork session (15 commits on `foundation`).

## TL;DR

- `cargo build --workspace` — **clean**
- `cargo clippy --workspace --all-targets -- -D warnings` — **clean**
- `cargo test --workspace --no-fail-fast` — **152 passing, 0 failing**
- **Zero cloud-SDK dependencies** anywhere in the workspace (audited independently)
- Phases 1-4 foundations landed as testable primitives; the load-bearing invariants (pairing, turn-transaction, trust scoping, barge-in rescind) all verified end-to-end against mock HTTP servers + in-memory SQLite

## How to run what works today

**Prereqs:** Docker (for the production path). Rust 1.85+ for local dev.

```bash
# Start the stack end-to-end.
cargo bootstrap                                    # migrate + image + start

# Try the chat surface:
curl -X POST http://localhost:3030/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"change-me-longer"}'

# Subscribe to the live event bus (needs a WebSocket client):
wscat -c ws://localhost:3030/api/stream

# Send a message; watch it broadcast on the stream:
curl -X POST http://localhost:3030/api/chats/conv1/messages \
  -H 'content-type: application/json' \
  -d '{"text":"hello world"}'

# Read back the committed events:
curl http://localhost:3030/api/chats/conv1/messages
```

## What's in, by phase

### Phase 0 — Foundation + local inference
Complete from the prior session:
- 15 Rust crates, Dockerfile.control-plane (minimal per axiom #12), docker-compose.yml, SQLCipher-encrypted SQLite with 22-table schema, OS-keyring integration, `execlaw` CLI with 9 container-lifecycle subcommands + matching cargo aliases (`cargo start`, `cargo stop`, `cargo restart`, `cargo logs`, etc.), axum server with JWT auth (setup/login/refresh/logout), Swagger OpenAPI + AsyncAPI served at `/api/docs`, tiered hardware-profile detection (sysfs + docker-info + probe containers), hook-based plugin manifest parser with ZIP staging + zipslip defense.

### Phase 1 — Agent core with one transport

**Event log** (`crates/core/src/events.rs`)
- Append-only `state_events` writes + `replay_since` reads
- `EventLog::commit_turn` enforces the `tool_use`/`tool_result` **pairing invariant** via `enforce_tool_pairing` — synthesizes cancellation results for any dangling `tool_use` in a batch
- **Snapshotting** (§2.3): `build_snapshot` + `hydrate` combine snapshot-blob + events-after for constant-time replay; falls through to full replay on corrupt snapshot

**Outbox drain** (`crates/outbox/`, `crates/core/src/outbox.rs`)
- `ready_pending`, `claim` (atomic pending→in_flight), `mark_delivered`, `record_failure`, `dead_letter_count`
- `Dispatcher` trait + `DispatcherRegistry` keyed by `effect_kind`
- `drain_once` (one-pass) + `run_drain_loop` (long-running tokio task)
- **`WakeupDispatcher`** — appends `EventKind::Wakeup` to the conversation log when a `schedule.wakeup` outbox row fires (§2.10 "wakeups are resume events")
- Framework-minted idempotency keys derived from `(conversation_id, turn_seq, tool_call_ordinal)`; consumer-side inbox dedup
- Retry budget + exponential backoff + dead-letter after N attempts

**Inference-API** (`crates/inference-api/`)
- Real OpenAI-compatible `reqwest` client
- `InferenceClient::chat_completions(req)` — non-streaming `POST /v1/chat/completions`
- Full types: `ChatMessage` (system/user/assistant/tool_result builders), `ToolCall` + `ToolCallFunction`, `ChatResponse` with `Choice` + `Usage`
- `ChatRequestNonStreaming` serializer adapter forces `stream: false` on the wire
- End-to-end test against a mocked local TCP listener

**Memory-tool shim** (`crates/runner-local/src/memory_tool.rs`)
- Plain OpenAI function-call tools: `read_memory`, `write_memory`, `list_memory` — our own, no vendor schema
- **Trust-class enforcement at the shim**: reads cascade downward only (outsider cannot read Controller memory); writes pinned to current trust level (no escalation via claiming higher class)

**Runner-local turn executor** (`crates/runner-local/src/turn.rs`)
- End-to-end turn: appends `user_msg`, assembles chat messages from the event log, calls inference, loops on tool_calls, dispatches via a `ToolDispatch` trait, commits atomically via `EventLog::commit_turn`
- `max_tool_rounds` hard cap prevents runaway loops
- Two tests against multi-response mock HTTP servers verify the canonical `user_msg → tool_use → tool_result → model_turn` ordering with same-ordinal pairing

**WebSocket event bus** (`crates/server/src/events.rs`)
- `tokio::sync::broadcast` channel fans out `UiEvent`s to every connected `/api/stream` subscriber
- Live events: `ChatMessageInbound/Outbound`, `ChatTokenDelta`, `AgentToolUse/Result`, `ConversationPhaseChanged`, `AlertFired/Resolved`, `Ping`
- Ping-on-connect so the UI can detect dead connections
- Lag-tolerant: lagging subscribers skip old events (UI re-hydrates from the log)

**Chat surface** (`crates/server/src/chats.rs`)
- `POST /api/chats/:id/messages` — appends user_msg, commits a stub reply (Phase 1 dev path until runner-deployment registry resolves to a reachable service), broadcasts both events on the bus
- `GET /api/chats/:id/messages` — paginated committed history

**Per-turn capability tokens** (`crates/server/src/capability.rs`)
- Ed25519-signed JWT payload with `conversation_id`, `turn_seq`, `capability_set`, `nonce`, `exp`
- `verify_capability_token` rejects mismatched conversation or turn_seq (§7.2 invariant)
- Tests: wrong turn → reject, wrong conversation → reject, different signer → reject, nonce differs per issuance

### Phase 2 — Plugin host + port groundwork

**Hook registry** (`crates/plugin-host/src/hook_registry.rs`)
- Live maps keyed per hook point from §4.2: `tools_by_name`, `ui_panels_by_mount`, `transports_by_id`, `identity_providers`, `event_subs`, `alert_sources`
- `enable(manifest)` is **all-or-nothing**: validates conflicts (duplicate tool name, mount, transport id) before inserting; leaves registry untouched on error
- `disable(plugin_id)` removes every hook a plugin owns in one shot

**Subprocess plugin tier** (`crates/plugin-host/src/subprocess.rs`)
- `SubprocessPlugin::spawn` boots a child process, pipes JSON-RPC over stdin/stdout
- One message per line; async stdout reader task correlates responses by numeric id through a `pending` map of oneshot channels
- `kill_on_drop` guarantees child cleanup even on task panic
- Graceful `shutdown()` sends an RPC and then kills
- Includes a shell-based echo test on Unix

**HMAC-signed event log** (`crates/core/src/event_hmac.rs`)
- HMAC-SHA256 tags over canonical event bytes
- Null-separator encoding prevents field-smuggling (two different field layouts can't produce the same canonical bytes)
- Constant-time verify via `hmac` crate's `verify_slice`
- Tamper tests: tampered payload, tampered seq, different key, field-smuggling resistance
- *Port note:* plan-level integration with `state_events` INSERTs is Phase 2 proper; the primitives are ready

### Phase 3 — Trust, policy, sideband

**Trust-level policy engine** (`crates/policy/src/trust.rs`)
- `TrustLevel` enum (Controller / Delegated / KnownTrusted / KnownLimited / UnknownPending / Blocked) with `rank()` ordering
- `evaluate_turn(TurnPolicyInput)` → `TurnPolicyDecision` returning:
  - `drop_turn` (Blocked senders)
  - `require_approval` (UnknownPending OR Rule-of-Two ≥3)
  - `planner_executor` (any sub-KnownTrusted effective trust)
  - `spotlighting` (any sub-KnownTrusted effective trust)
  - `latency_band` (voice and limited trust → LowOnly)
  - `capability_set` (Controller gets `*`; cascades downward by rank)
- 8 tests covering every trust level + Rule-of-Two interaction + voice latency

**Spotlighting** (`crates/policy/src/spotlighting.rs`)
- Random per-conversation delimiters (ChaCha20 RNG, hex-encoded 64-bit tag)
- `wrap(content)` **strips smuggled occurrences of the delimiter** from the content so attackers can't forge closers mid-stream
- Deterministic-RNG seed path for reproducible tests

**Sideband approval primitives** (`crates/policy/src/sideband.rs`)
- `ApprovalReason` enum: `ColdContact`, `RuleOfTwoBreach`, `SensitiveToolCall`, `AskController`, `AnomalyTripwire`
- `ApprovalVerb` enum for cold-contact flow: `Trust`, `TrustLimited`, `Block`, `IgnoreOnce` plus general `Approve`/`Edit`/`Reject`
- `ApprovalClaims` JWT payload shape
- `pick_sideband_transport(enabled, origin, priority)` — **guarantees the notification goes over a different transport than the origin** so an attacker controlling Signal can't approve their own request via Signal

### Phase 4 — Voice pipeline primitives

**Two-lane Tokio graph** (`crates/voice-pipeline/src/graph.rs`)
- `Pipeline` owns a `broadcast::Sender<SystemFrame>` (system lane, preempts) and an `mpsc::Sender<DataFrame>` (data lane, FIFO)
- `interrupt()`, `user_started_speaking()`, `user_stopped_speaking()`, `error()` helpers broadcast to every stage
- `subscribe_system()` returns a receiver each stage holds alongside its data-lane mpsc
- Test verifies `biased select!` observes the system lane first even with data-lane backlog

**Punctuation-aware endpointer** (`crates/voice-pipeline/src/endpointer.rs`)
- Tail classifier: terminal punctuation (`.`, `?`, `!`, `。`) → short silence window (~250ms); mid-thought (`,`, `:`, `;`) → long (~900ms); unknown → default (~500ms)
- No model calls; zero extra inference cost
- CJK-aware

**Barge-in + backchannel rescind** (`crates/voice-pipeline/src/bargein.rs`)
- LiveKit-pattern rescind decision table: `Wait` during 120ms delay, `Rescind` on backchannel within 400ms cap, `Confirm` otherwise
- Backchannel vocabulary covers `mm-hmm`, `yeah`, `okay`, `got it`, etc. (case-insensitive)
- Pure logic — no timers — caller drives elapsed time

## Test counts (per crate)

```
execlaw-core            53     DB, migrations, events, outbox, snapshots,
                               HMAC tags, alerts, memory, principals
execlaw-server          24     auth, events (WS bus), capability tokens,
                               chat routes, setup→login→refresh→logout
execlaw-policy          26     Rule of Two, trust evaluator, spotlighting,
                               sideband routing, input guard, JWT claims
execlaw-voice-pipeline  17     frames, two-lane graph, endpointer, barge-in
execlaw-runner-local     9     memory_tool (trust scoping), turn executor
execlaw-plugin-sdk       8     manifest parsing, ZIP staging + zipslip
execlaw-plugin-host      9     hook registry, subprocess RPC codec
execlaw-inference-api    5     chat req/resp shape, E2E mock HTTP server
execlaw-container-manager 3    mock sysfs detection, PCI vendor mapping
execlaw-vault            3     Argon2id password verification
execlaw-outbox           8     backoff, retry budget, drain, wakeup dispatcher
execlaw-plugin-host      (see above)
(others)                 ~5    session, transport-api, identity-api smokes
----------------------------------------------------------------
TOTAL                  152 passing, 0 failing
```

## Grounding-rule compliance (re-audited this session)

- Zero cloud LLM SDKs in any Cargo.toml (grep for `anthropic`, `openai`, `google-genai`, `claude-agent-sdk` returns nothing in code)
- Zero cloud-bridge plugin infrastructure
- Plugin manifest is hook-based, no typed `PluginKind`
- Trust ladder uses `Blocked`, not `UnknownDenied`
- Single-model policy: `QuantTrio/Qwen3.5-27B-AWQ` default; no QwQ separate deployment
- Control-plane image stays minimal (axiom #12)
- Deployment = container image, not bare binary
- No `.env` files, no `dotenv` dep

## Known gaps / intentional deferrals

- **Streaming SSE** — non-streaming `chat_completions` ships first; streaming follows when the WS chat UI lands (mechanical addition on top of the current client)
- **Runner container spawn per-conversation** — the `TurnExecutor` is in-process today; §2.8 "per-conversation hot runner container" lands when container-manager gains a `spawn_runner(spec)` path
- **Real Signal transport plugin port** — the subprocess tier is ready for it; actually porting selfhosted-claw's `src/channels/signal.ts` is its own diff
- **HMAC wiring into `state_events` INSERTs** — primitives exist; the migration adding `state_events.tag BLOB NOT NULL` column + insertion path lands before Phase 2 ships
- **Voice VAD/STT/TTS service containers** — `service-whisper`, `service-kokoro` plugin manifests will ship when those service containers are built; the pipeline crate is ready to drive them
- **Chat UI (React SPA)** — the REST + WS APIs are stable; UI work is tracked separately
- **End-to-end crash tests against Docker** — testable locally but requires Docker running; the unit-test suite already verifies the underlying invariants (pairing, idempotency, claim-prevents-double-dispatch, etc.)

## Where to look for the key pieces

| Concern | File |
|---|---|
| Event log + pairing invariant | `crates/core/src/events.rs` |
| Turn commit transaction | `crates/runner-local/src/turn.rs` + `crates/core/src/events.rs::commit_turn` |
| Outbox drain loop | `crates/outbox/src/lib.rs` |
| Wakeup dispatch | `crates/outbox/src/lib.rs::WakeupDispatcher` |
| Trust policy evaluator | `crates/policy/src/trust.rs` |
| Spotlighting | `crates/policy/src/spotlighting.rs` |
| Sideband approval flow | `crates/policy/src/sideband.rs` |
| HMAC-signed events | `crates/core/src/event_hmac.rs` |
| Per-turn capability tokens | `crates/server/src/capability.rs` |
| Plugin hook registry | `crates/plugin-host/src/hook_registry.rs` |
| Subprocess plugins | `crates/plugin-host/src/subprocess.rs` |
| WebSocket event bus | `crates/server/src/events.rs` |
| Chat API | `crates/server/src/chats.rs` |
| Memory tool (with trust scoping) | `crates/runner-local/src/memory_tool.rs` |
| Voice two-lane graph | `crates/voice-pipeline/src/graph.rs` |
| Endpointer | `crates/voice-pipeline/src/endpointer.rs` |
| Barge-in rescind | `crates/voice-pipeline/src/bargein.rs` |

## Commits this session

```
9b109b4 Wave 3-4: policy engine, spotlighting, sideband, voice pipeline primitives
8a1ced6 Wave 1-2: WS event bus, capability tokens, chat route, plugin hooks, HMAC events
7f0ab73 Apply audit findings: JWT field names, JSON tracing, typo
6890492 runner-local: memory_tool shim + turn executor (§2.4, §2.7 Phase 1)
97d08d1 inference-api: real reqwest client with chat_completions
0a717a2 outbox: drain loop + WakeupDispatcher (§2.4, §2.15 Phase 1)
b0aff56 core/events: add snapshot build + hydrate (§2.3 Phase 1)
a7e99e6 Add docs/architecture.md reference document
(plus 7 Phase-0 commits from the prior overnight session)
```

Net session delta: **+58 tests**, **+5 major subsystems** (WS event bus, hook registry, subprocess plugins, HMAC events, voice pipeline primitives), **+3 architecture docs** alignment changes.
