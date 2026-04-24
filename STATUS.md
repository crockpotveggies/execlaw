# execlaw build STATUS

Last update: 2026-04-24, after Phase 3 closeout + Phase 4 voice primitives.

## TL;DR

- `cargo build --workspace` — **clean**
- `cargo clippy --workspace --all-targets -- -D warnings` — **clean**
- `cargo test --workspace --no-fail-fast` — **293 passing, 0 failing**
- `cargo bench --workspace --no-run` — **clean** (35+ benches across 8 crates)
- **Zero cloud-SDK dependencies** anywhere in the workspace
- Phases 0–4 complete (internal); Phase 5 observability/replay next

## Migration-plan phase structure (post-2026-04-24 refactor)

Phase 2 used to conflate "plugin framework" with "port every selfhosted-claw integration". That meant Phase 2 couldn't be "done" until external-service work (Signal CLI, Google OAuth, search API keys, Whisper/Kokoro models) had landed, which tangled every downstream phase. The refactor split them:

| Phase | Scope | Status |
|---|---|---|
| 0 — Foundation + local inference + GPU-aware deployment | foundation primitives | ✅ done |
| 1 — Agent core with one transport (web chat) | HMAC event log, TurnExecutor, policy+capability on turn path, streaming SSE, crash invariants | ✅ done |
| 2 — **Plugin framework** (framework only — ports moved to Phase 8) | hook registry, subprocess tier, install route, lifecycle, dispatch bridge, capability enforcement | ✅ done |
| 3 — Participants, trust, policy engine, Rule of Two | `PrincipalStore`, identity resolution (+ plugin dispatch), cold-contact flow, approval endpoint with every verb, spotlighting, planner/executor tool-strip, trust-class memory scoping | ✅ done |
| 4 — Voice pipeline primitives (pure Rust; real-audio demos move to Phase 8) | two-lane graph, Vad/Audio/Stt/Tts traits + mocks, `VoiceSession` orchestrator, voice event schema wired to state_events, endpointer, barge-in | ✅ done |
| 5 — Observability, evaluation, replay CLI | log viewer, `execlaw replay`, eval harness against local Qwen | pending |
| 6 — UI port, chat-first landing | React SPA on the new APIs | pending |
| 7 — Hardening | WASM tier, WebAuthn, key rotation, multi-controller | ongoing |
| 8 — **External plugin ports** (new, open-ended) | every plugin that needs creds/external-services — see [plugin-inventory.md](docs/plugin-inventory.md) | queue; no ports started |

## How to run what works today

**Prereqs:** Docker (for the production path). Rust 1.85+ for local dev.

```bash
# Start the stack end-to-end.
cargo bootstrap                                    # migrate + image + start

# First-run admin setup:
curl -X POST http://localhost:3030/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"change-me-longer"}'

# Subscribe to the live event bus:
wscat -c ws://localhost:3030/api/stream

# Send a message; watch it broadcast on the stream:
curl -X POST http://localhost:3030/api/chats/conv1/messages \
  -H 'content-type: application/json' \
  -d '{"text":"hello world"}'

# Install a plugin (ZIP upload):
curl -X POST http://localhost:3030/api/admin/plugins/install \
  -H 'content-type: application/zip' \
  --data-binary @my-plugin.zip

# List installed plugins + their tools:
curl http://localhost:3030/api/admin/plugins
curl http://localhost:3030/api/admin/plugins/tools

# Streaming replies (set EXECLAW_INFERENCE_URL before `cargo start`):
EXECLAW_INFERENCE_URL=http://127.0.0.1:8000/v1 cargo start
```

## What's in, by phase

### Phase 0 — foundation + local inference
- 15 Rust crates, `Dockerfile.control-plane` (minimal per axiom #12), `docker-compose.yml`, SQLCipher-encrypted SQLite, OS-keyring integration, `execlaw` CLI with 9 container-lifecycle subcommands, axum server with JWT + refresh auth, Swagger + AsyncAPI specs at `/api/docs`, tiered hardware-profile detection, hook-based plugin manifest parser with ZIP staging + zipslip defense.

### Phase 1 — agent core
- **HMAC-signed event log** (§7.8): migration 0002 adds `tag BLOB` + `key_id`. `EventLog` signs on append/commit_turn, verifies on replay. Post-commit SQL UPDATE fails the next `GET /messages` with 500 instead of serving forged rows.
- **Turn executor wired into chat route**: `POST /api/chats/:id/messages` runs the full turn when `EXECLAW_INFERENCE_URL` is set; falls back to a stub echo when offline. Stub path commits user_msg + model_turn atomically via one `commit_turn`.
- **Policy engine on every turn**: `evaluate_turn` runs before the model call. Blocked senders → 403. UnknownPending → 202 (Phase 3 cold-contact hook). Others proceed with the matching static capability tier.
- **Per-turn capability token** minted bound to `(conversation_id, turn_seq, capability_set)` — in-process today, becomes the header runner containers present in Phase 7+ (container tier).
- **Streaming SSE**: `chat_completions_stream` in `execlaw-inference-api` with minimal SSE parser. Each content delta broadcasts as `UiEvent::ChatTokenDelta` on the WS bus. Per-chunk decode ~390 ns.
- **Crash-safety tests**: kill-mid-turn synthesizes paired `tool_result` so the log is never asymmetric; replay-after-restart reconstructs history; post-commit tamper fails reads.

### Phase 2 — plugin framework (framework only; ports ➜ Phase 8)
- **`PluginHost` lifecycle** (`crates/plugin-host/src/host.rs`): install / enable / disable / uninstall / hydrate, all-or-nothing atomicity on hook conflicts, subprocess spawn/kill, SQLite persistence via migration 0003 `state_plugins`.
- **HTTP routes** (`crates/server/src/plugins.rs`):
  - `POST   /api/admin/plugins/install` — ZIP body, zip-slip defense, manifest validation, hook registration, subprocess spawn, persist.
  - `GET    /api/admin/plugins` — list installed plugins with status.
  - `GET    /api/admin/plugins/tools` — union of every tool in the registry (for the UI tool picker).
  - `POST   /api/admin/plugins/:id/enable` — re-register hooks + respawn subprocess.
  - `POST   /api/admin/plugins/:id/disable` — un-register + kill child.
  - `DELETE /api/admin/plugins/:id` — full uninstall.
- **Manifest `[runtime]` table**: tier + executable + args + env. Phase 2 supports tier = "subprocess"; others rejected with 400.
- **Capability-enforced dispatch bridge** (`crates/server/src/tool_dispatch.rs`): `ChainedToolDispatch` routes tool calls built-ins → plugins → error. Capability check runs BEFORE the subprocess sees args; wildcard `"*"` short-circuits for Controller turns.
- **Inventory doc** at `docs/plugin-inventory.md` classifies every selfhosted-claw integration into bucket A (port in Phase 8), B (folded into core), or C (retire).

### Phase 2 deferrals (closed by Phase 8)
- Actual plugin ports (Signal, Google Calendar/Contacts, search/research, voice). None started; queue prioritized in the inventory doc.
- Real Signal cold-contact + cross-transport sideband demos from Phase 3 — tested internally with web chat + a mock second transport; prove against Signal in Phase 8.
- Real-audio voice acceptance (≤1.1s EoS → first audio, Kokoro on GPU) — primitives tested in Phase 4 against fixtures; real audio in Phase 8 with `plugin-voice` + `service-whisper` + `service-kokoro`.

## Test counts (per crate)

```
execlaw-core              74     DB, events (+HMAC sign/verify + atomicity),
                                 outbox (+claim/record_failure/ready_pending),
                                 alerts, memory, principals (+PrincipalStore
                                 round-trip, find_by_identifier, set_trust),
                                 idempotency keys, snapshots, HMAC
                                 tamper-tests, migrations
execlaw-server            47     auth, events (WS bus), capability tokens,
                                 chat routes (streaming, policy, crash tests,
                                 cold-contact adversarial, identity-match
                                 classifier), tool_dispatch
execlaw-server (integ)    18     plugin_lifecycle (11) + approval_flow (7)
execlaw-policy            43     rule_of_two, trust evaluator, spotlighting,
                                 sideband, input_guard, JWT claims
execlaw-voice-pipeline    34     frames, two-lane graph, endpointer, bargein,
                                 traits (MockAudioIn/Out, MockVad, MockStt,
                                 MockTts), VoiceSession (speech→LLM→TTS,
                                 barge-in rescind/confirm, sentence splitter)
execlaw-runner-local      16     memory_tool (trust scoping + adversarial),
                                 turn executor
execlaw-plugin-host       15     hook registry, subprocess RPC
execlaw-plugin-sdk         8     manifest parsing, ZIP staging + zipslip
execlaw-inference-api      7     chat req/resp, streaming SSE parse
execlaw-container-manager  3     mock sysfs detection, PCI vendor mapping
execlaw-vault              3     Argon2id password verification
execlaw-outbox             8     backoff, retry budget, drain
execlaw-session            1     modality binding
--------------------------------------------------------------------
TOTAL                    293 passing, 0 failing
```

## Benchmarks (cargo bench --workspace)

| Path | Time | Budget | Notes |
|---|---|---|---|
| `event_hmac/sign_event` | 215 ns | ≤10 µs | SHA-256 core |
| `event_hmac/verify_event` | 332 ns | ≤10 µs | constant-time compare |
| `idempotency_key_mint` | 78 ns | — | format! String alloc |
| `commit_turn/1` | 24 µs | ≤2 ms | SQLite INSERT dominates |
| `commit_turn/10` | 62 µs | ≤2 ms | scales ~6 µs/event |
| `replay_since_0_of_500` (keyless) | 138 µs | — | 275 ns/event |
| `replay_since_0_of_500` (HMAC-verified) | 337 µs | — | 675 ns/event (2.4× keyless; invariant worth the cost) |
| `outbox/claim` | 24 µs | — | transaction overhead |
| `evaluate_turn/controller` | 2.5 ns | ≤1 µs | static-slice caps (-99% after axiom #14 optimization) |
| `spotlight_wrap/tiny` | 39 ns | — | fast-path skip of double-replace (-92%) |
| `pick_sideband_transport` | 42 ns | — | |
| `classify_tail_terminal` | 1.5 ns | ≤1 µs | |
| `decide_rescind` | 30 ns | ≤1 µs | |
| `decide_confirm` | 0.6 ns | — | short-circuits |
| `issue_capability_token` | 35 µs | ≤500 µs | Ed25519 sign intrinsic |
| `verify_capability_token` | 36 µs | ≤200 µs | Ed25519 verify intrinsic |
| `stream_chunk_decode/content` | 389 ns | ≤5 µs | serde_json parse |
| `hook_registry_tool_lookup/hit` | 24 ns | ≤200 ns | after `Arc<RegisteredTool>` optimization (-92%) |
| `manifest_parse/realistic` | 6 µs | ≤1 ms | TOML parse + hook validation |

## Grounding-rule compliance (re-audited this session)

- Zero cloud LLM SDKs in any Cargo.toml
- Zero cloud-bridge plugin infrastructure
- Plugin manifest is hook-based, no typed `PluginKind`
- Trust ladder uses `Blocked`, not `UnknownDenied`
- Single-model policy: `QuantTrio/Qwen3.5-27B-AWQ` default
- Control-plane image stays minimal (axiom #12)
- Deployment = container image, not bare binary
- No `.env` files, no `dotenv` dep
- Axiom #13 (extensive testing): every invariant + public API has tests, security code has adversarial tests
- Axiom #14 (performance): every hot path has a Criterion bench; optimizations justified by before/after numbers

## Where to look for the key pieces

| Concern | File |
|---|---|
| Event log + pairing invariant + HMAC signing | `crates/core/src/events.rs` |
| Turn commit transaction | `crates/runner-local/src/turn.rs` + `crates/core/src/events.rs::commit_turn` |
| Outbox drain loop | `crates/outbox/src/lib.rs` |
| Wakeup dispatch | `crates/outbox/src/lib.rs::WakeupDispatcher` |
| Trust policy evaluator | `crates/policy/src/trust.rs` |
| Spotlighting | `crates/policy/src/spotlighting.rs` |
| Sideband approval flow | `crates/policy/src/sideband.rs` |
| HMAC primitives | `crates/core/src/event_hmac.rs` |
| Per-turn capability tokens | `crates/server/src/capability.rs` |
| Plugin hook registry | `crates/plugin-host/src/hook_registry.rs` |
| Plugin lifecycle (Phase 2) | `crates/plugin-host/src/host.rs` |
| Subprocess plugins | `crates/plugin-host/src/subprocess.rs` |
| Plugin HTTP routes | `crates/server/src/plugins.rs` |
| Tool dispatch chain | `crates/server/src/tool_dispatch.rs` |
| WebSocket event bus | `crates/server/src/events.rs` |
| Chat API (streaming + policy + capability) | `crates/server/src/chats.rs` |
| Memory tool (with trust scoping) | `crates/runner-local/src/memory_tool.rs` |
| Voice two-lane graph | `crates/voice-pipeline/src/graph.rs` |
| Endpointer | `crates/voice-pipeline/src/endpointer.rs` |
| Barge-in rescind | `crates/voice-pipeline/src/bargein.rs` |
| Streaming inference client | `crates/inference-api/src/lib.rs` |

## Recent commit history (foundation branch)

```
106c78f Phase 2 framework: plugin lifecycle + install route + capability-enforced dispatch bridge
f9cae6b Phase 1 complete: HMAC event log + TurnExecutor wired to chat route + streaming SSE
30d9d50 Grounding axioms #13 (testing) + #14 (perf): backfill adversarial tests, wire Criterion, optimize hot paths
f3b8ed4 STATUS.md: Phase 1-4 groundwork session — 152 tests, 15 commits, 5 new subsystems
9b109b4 Wave 3-4: policy engine, spotlighting, sideband, voice pipeline primitives
8a1ced6 Wave 1-2: WS event bus, capability tokens, chat route, plugin hooks, HMAC events
```
