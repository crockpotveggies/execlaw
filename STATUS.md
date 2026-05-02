# execlaw build STATUS

Last update: 2026-05-02 (Phase 9 — second Rhai plugin shipped + SPA settings shell hardened). Two Google plugins now live on the script tier (`plugin-google-contacts` for People API identity + tool surface, `plugin-google-calendar` for Calendar API tools). The SPA's per-plugin config UI is now a **shell-owned chrome + plugin-supplied middle slot** architecture: `PluginConfigShell` provides the back button, header, and an un-overridable Uninstall danger zone; plugin pages implement the `PluginConfigProps` contract for the middle. The shared `OauthClientConfig` component absorbs the entire OAuth lifecycle (form, Connect/Disconnect, token status display) — Google plugins are ~75-line wrappers around it instead of 350-line near-duplicates. New `unix_to_rfc3339` and `url_encode` host primitives in execlaw-script reuse cleanly across plugins. Backend plugin-row response now exposes `has_settings_ui` derived from manifest `[[oauth_accounts]]`, driving the gear-icon-on-row UX (toggle icon next to it, Uninstall moved to the config page's danger zone).

**Plugins shipped on script tier:** `plugin-google-contacts` (~180 lines, identity_provider + contacts.list, 13 plugin tests + e2e install) and `plugin-google-calendar` (~250 lines, list_calendars + list_events with read-only scope, 9 plugin tests + e2e install). Both operator-installable via Settings → Plugins ZIP upload, configured via the gear icon on the plugin row. Reusable OAuth machinery (migration 0028, vault stores, refresh sweeper, admin endpoints, `_oauth` injection) carries them both with no plugin-specific backend code.

## TL;DR

- `cargo build --workspace` — **clean** (stub mode; webauthn-rs gated behind `--features webauthn` for the Linux/Docker production build)
- `cargo clippy --workspace --all-targets` — **zero warnings**, zero errors
- `cargo test --workspace --lib` — **1122 passing, 0 failing**
- `cargo test --workspace --tests` — **1191 passing, 0 failing** across lib + integration + plugin bin tests (script_plugin_install + google_contacts_e2e + google_calendar_e2e end-to-end coverage)
- `cargo bench --workspace --no-run` — **clean** (~76 benches; `script_hot_paths` pins engine-build ~99 µs, google-contacts parse ~267 µs, google-calendar parse ~313 µs, per-call dispatch overhead ~12 µs)
- `cd web && npm test` — **332 passing** (+ plugin-config-shell tests pinning the un-overridable-uninstall invariant + GoogleContactsPage tests against the refactored OauthClientConfig)
- `cd web && npm run build` — **clean** (≈682 KB JS / 335 KB CSS)
- **Zero cloud-SDK dependencies** anywhere in the workspace
- **Phase 7 JWT hardening done.** Shipped: (a) **Migration 0008** + `state_refresh_tokens` table — refresh tokens now survive a server restart instead of silently signing every operator out; (b) **`RefreshTokenStore`** in core (issue / single-use consume / revoke_session / revoke_all_for_user / active_session_count / purge_expired + 8 unit tests including the persistence-survives-recreate invariant); (c) **`POST /api/logout/all`** endpoint + 2 server tests covering the multi-session revoke + the auth-required gate; (d) **SPA `apiFetch` silent auto-retry** — installs a `RefreshHook` on AuthContext mount that rotates tokens on a 401 and replays the original request once, with explicit guards against retry loops + caller-controlled tokens; (e) **Background refresh timer** — fires at 80% of the 15-min access-token TTL so the user never sees a 401-flash; (f) **"Sign out everywhere"** button on Settings → Profile that calls `/api/logout/all` + bounces to `/login`.
- **Phase 7 sub-phase 7e (WebAuthn) done.** Shipped: (a) **Migration 0007** + `state_webauthn_credentials` table with per-user 10-credential cap + ON DELETE CASCADE on `users`; (b) **`WebauthnStore`** in core (insert / count_for_user / list_for_user / get / update_counter / delete_owned + 8 unit tests including ownership + cascade); (c) **`WebauthnSvc`** in server (relying-party config, register + authenticate ceremonies, 5-minute ceremony TTL with `prune_expired`, deterministic user-handle UUID); (d) **HTTP routes** for register begin/finish, list, delete, login finish — all audit-logged; (e) **/api/login second-factor branch** — when `count_for_user > 0`, returns the new `LoginOutcome::WebauthnChallenge` instead of tokens, fail-closed if the svc is missing; (f) **SPA**: `coerceCreationOptions`/`coerceRequestOptions`/`serializeCredential` browser helpers (base64url ↔ ArrayBuffer), Login screen handles the challenge (auto-prompts authenticator + retry button), Settings → Profile credential management.
- **Build feature gate**: `webauthn-rs` pulls in `openssl-sys` which can't build out-of-the-box on Windows MSYS perl. The integration lives behind the `webauthn` feature on the server crate (default OFF) so the workspace `cargo build` works on every host. Production Docker build enables it explicitly. When the feature is OFF, every webauthn route returns 503 `webauthn_unconfigured`, registration is impossible, and the login branch never fires (because count_for_user always observes 0).
- **Phase 7 complete; Phase 8a (per-tool trust-class allowlist) shipped.** New `config_tool_access` table holds one row per tool the runner might dispatch — builtins, plugin-supplied, and (Phase 8b+) MCP-server tools. The dispatch chain consults the row before any source handles the call: tools with the caller's `TrustLevel` not in `allowed_classes`, or `enabled = false`, or `removed_at != NULL`, are denied with a structured error. Operator policy is durable across server restarts and survives source resyncs (re-install / reconnect). Settings → Tools page lists every registered tool with per-row `enabled` toggle + per-class checkboxes; PATCH `/api/admin/tools/{tool_name}` is Controller-only and audit-logged.

## Since 2026-04-25 (88 commits on `foundation`)

Major subsystems landed since the last STATUS update:

- **Cards primitive (C1)** — generic `Card{Opened,Progressed,Closed}` event surface every long-running operator-visible task plugs into. Event-sourced, transport-aware (TextOnly / Rich / Native channels with per-channel update policies), per-kind renderer registry on the SPA. Currently used by the deep-research subsystem; future shell-session and file-pipeline tools land on the same surface.
- **Synchronous subagents (C2)** — `delegate_task` tool + `SubagentApi` capability. In-turn child LLM call with context isolation; the parent's turn pauses for the duration. Used inside the research gather phase as the per-sub-query extractor.
- **Deep-research subsystem (C3–C6)** — flagship long-running agent feature. Plan / gather / synthesise pipeline with three phase-gate modes (`none` / `plan_only` / `every_phase`), workspace dirs at `~/.execlaw/research/<job_id>/`, retention sweeper (purges terminal rows + dirs past `history_retention_days`), `/research` SPA drill-down page, running-jobs badge above the chat composer, every-phase advance + cancel admin endpoints. 7 LLM-facing tools (`research_start`, `research_status`, `research_list`, `research_get_report`, `delegate_task`, etc.) + the admin REST surface. See `docs/handoffs/2026-04-29-research-subsystem.md` for the full closure trail.
- **Global retention setting (migration 0026)** — Settings → General dropdown drives a single `history_retention_days` knob (30 / 60 / 90 / 120 / Infinite, default 30). Every history sweeper (event log, log entries, routine runs, research jobs) reads `RetentionPolicy::load(&db)` per tick. Pinned + ephemeral conversations exempt; memory entries + audit log + vault rows are intentional carve-outs.
- **Voice mode end-to-end (Phase 13)** — STT + TTS adapters, two-lane voice graph, barge-in, modality-adaptive inference resolver, voice-session reaper. See `docs/voice-followups.md` for explicit deferrals (server-side AEC3, SPA Opus→PCM16 capture).
- **Runner supervisor (Phase 16)** — per-principal-group container runners with controller-pin policy, idle reaping, and turn-duration watchdog.

## Phase 8 — MCP client integration (in progress)
- **Zero cloud-SDK dependencies** anywhere in the workspace
- **Phase 7 JWT hardening done.** Shipped: (a) **Migration 0008** + `state_refresh_tokens` table — refresh tokens now survive a server restart instead of silently signing every operator out; (b) **`RefreshTokenStore`** in core (issue / single-use consume / revoke_session / revoke_all_for_user / active_session_count / purge_expired + 8 unit tests including the persistence-survives-recreate invariant); (c) **`POST /api/logout/all`** endpoint + 2 server tests covering the multi-session revoke + the auth-required gate; (d) **SPA `apiFetch` silent auto-retry** — installs a `RefreshHook` on AuthContext mount that rotates tokens on a 401 and replays the original request once, with explicit guards against retry loops + caller-controlled tokens; (e) **Background refresh timer** — fires at 80% of the 15-min access-token TTL so the user never sees a 401-flash; (f) **"Sign out everywhere"** button on Settings → Profile that calls `/api/logout/all` + bounces to `/login`.
- **Phase 7 sub-phase 7e (WebAuthn) done.** Shipped: (a) **Migration 0007** + `state_webauthn_credentials` table with per-user 10-credential cap + ON DELETE CASCADE on `users`; (b) **`WebauthnStore`** in core (insert / count_for_user / list_for_user / get / update_counter / delete_owned + 8 unit tests including ownership + cascade); (c) **`WebauthnSvc`** in server (relying-party config, register + authenticate ceremonies, 5-minute ceremony TTL with `prune_expired`, deterministic user-handle UUID); (d) **HTTP routes** for register begin/finish, list, delete, login finish — all audit-logged; (e) **/api/login second-factor branch** — when `count_for_user > 0`, returns the new `LoginOutcome::WebauthnChallenge` instead of tokens, fail-closed if the svc is missing; (f) **SPA**: `coerceCreationOptions`/`coerceRequestOptions`/`serializeCredential` browser helpers (base64url ↔ ArrayBuffer), Login screen handles the challenge (auto-prompts authenticator + retry button), Settings → Profile credential management.
- **Build feature gate**: `webauthn-rs` pulls in `openssl-sys` which can't build out-of-the-box on Windows MSYS perl. The integration lives behind the `webauthn` feature on the server crate (default OFF) so the workspace `cargo build` works on every host. Production Docker build enables it explicitly. When the feature is OFF, every webauthn route returns 503 `webauthn_unconfigured`, registration is impossible, and the login branch never fires (because count_for_user always observes 0).
- **Phase 7 complete; Phase 8a (per-tool trust-class allowlist) shipped.** New `config_tool_access` table holds one row per tool the runner might dispatch — builtins, plugin-supplied, and (Phase 8b+) MCP-server tools. The dispatch chain consults the row before any source handles the call: tools with the caller's `TrustLevel` not in `allowed_classes`, or `enabled = false`, or `removed_at != NULL`, are denied with a structured error. Operator policy is durable across server restarts and survives source resyncs (re-install / reconnect). Settings → Tools page lists every registered tool with per-row `enabled` toggle + per-class checkboxes; PATCH `/api/admin/tools/{tool_name}` is Controller-only and audit-logged.

## Phase 8 — MCP client integration (in progress)

| Sub-phase | Scope | Status |
|---|---|---|
| **8a** | Generalised per-tool trust-class allowlist (foundation) | ✅ shipped |
| **8b** | `mcp-client` crate: stdio transport, refuses sampling, list_tools / call_tool / list_resources / read_resource. Streamable HTTP transport deferred to 8c. | ✅ shipped |
| **8c** | `config_mcp_servers` + connection manager, reflects tools into `config_tool_access` with `mcp:<server>:<tool>` namespacing | ✅ shipped |
| **8d** | `mcp:`-prefixed names route through `ChainedToolDispatch` to `McpHost`, Settings → MCP CRUD page in SPA | ✅ shipped |

**Phase 8 complete.** End-to-end: configure an MCP server in Settings → MCP, see its connection status flip to `connected`, watch its tools appear (prefixed) on Settings → Tools where you can per-tool gate which trust classes may use them, and the runner picks them up alongside builtins/plugins.

## Phase 8.5 — Runner architecture surface (in progress)

Audit caught that the legacy "Deployments" CRUD page conflated two
distinct concepts:

1. **Backends** (model + GPU + endpoint per runner-purpose) — fixed
   set of five purposes (Standard / Reasoning / Guardrail / VoiceSTT
   / VoiceTTS); operator edits per slot, never adds/removes.
2. **Runners** (per-conversation hot containers the control plane
   spawns automatically) — view-only, controller's runner stays hot
   indefinitely, others reaped after 10 min idle.

Wave 8.5 ships the corrected vocabulary + the runner observability
surface. Container-managed runners themselves (real per-conversation
processes vs. today's in-process model) are tracked in
[`docs/runner-design.md`](docs/runner-design.md), which captures the
selfhosted-claw `HotRunnerPool` optimisations to preserve in the
Rust port.

* **Backends** — migration 0011 renames `config_runner_deployments`
  → `config_backends`, collapses to one row per purpose
  (`purpose TEXT PRIMARY KEY`, no synthetic id, no is_default
  toggle). New `BackendStore` (5 tests) + admin routes
  `GET /api/admin/backends`, `PUT /api/admin/backends/{purpose}`,
  `DELETE /api/admin/backends/{purpose}` (5 tests). SPA Settings →
  Backends page renders the five purposes as fixed rows; "+ New" is
  gone (5 SPA tests).
* **Runners** — new in-memory `RunnerRegistry` (7 tests) tracks
  one entry per conversation that has emitted a turn this process,
  with controller-always-hot policy: `register_turn_start` /
  `register_turn_end` lifecycle hooks called from the chat path,
  background reaper (60s cadence) drops idle non-controller entries
  after 10 min. Admin routes `GET /api/admin/runners`,
  `POST /api/admin/runners/{conversation_id}/restart` (4 tests).
  SPA Settings → Runners page polls every 5s, shows status badges
  (controller / in-flight / restart pending / modality), live idle
  countdown, role-gated Restart button (4 SPA tests).

## Migration-plan phase structure (post-2026-04-24 refactor)

Phase 2 used to conflate "plugin framework" with "port every selfhosted-claw integration". That meant Phase 2 couldn't be "done" until external-service work (Signal CLI, Google OAuth, search API keys, Whisper/Kokoro models) had landed, which tangled every downstream phase. The refactor split them:

| Phase | Scope | Status |
|---|---|---|
| 0 — Foundation + local inference + GPU-aware deployment | foundation primitives, GET /api/admin/hardware, tracing JSONL to `~/.execlaw/logs/`, vault passphrase-file fallback | ✅ done (service plugins moved to Phase 8; setup wizard moved to Phase 6) |
| 1 — Agent core with one transport (web chat) | HMAC event log, TurnExecutor, policy+capability on turn path, streaming SSE, `WakeupScheduler` (priority queue + Notify, sub-second), crash invariants | ✅ done (chat UI moved to Phase 6) |
| 2 — **Plugin framework** (framework only — ports moved to Phase 9) | hook registry, subprocess tier, install route, lifecycle, dispatch bridge, capability enforcement | ✅ done |
| 3 — Participants, trust, policy engine, Rule of Two | `PrincipalStore`, identity resolution (+ plugin dispatch), cold-contact flow, approval endpoint with every verb, spotlighting, planner/executor tool-strip, trust-class memory scoping | ✅ done |
| 4 — Voice pipeline primitives (pure Rust; real-audio demos move to Phase 9) | two-lane graph, Vad/Audio/Stt/Tts traits + mocks, `VoiceSession` orchestrator, voice event schema wired to state_events, endpointer, barge-in | ✅ done |
| 5 — Observability, evaluation, replay CLI (infra only) | tracing→SQLite layer, `GET /api/admin/logs`, `GET /api/admin/eval/flags`, `execlaw replay <conv> --at <seq>`, `execlaw eval flag/list`, eval-harness binary + rubric scaffolding | ✅ done (UI components for log viewer + dashboard land in Phase 6) |
| 6 — UI port, chat-first landing | React + GSAP SPA: setup → login → chat (sidebar, thread list, streaming + cursor, channel-origin icons, long-msg truncation, external-channel filter, plugin UI panels) → settings (plugins/principals/hardware/logs/eval/audit), inline approval card with verbs, thread rename, incognito toggle, plugin install, trust revoke | ✅ done |
| 7 — Hardening | wave 1 (deployment editor + key rotation + log retention) ✅, wave 2 (back-fill verifier + backup/restore + multi-controller users) ✅, wave 3 (WebAuthn second-factor + JWT plumbing) ✅. WASM tier and advanced subagents dropped from Phase 7 scope (see MIGRATION_PLAN.md). | complete |
| 8 — MCP client integration | 8a per-tool trust-class allowlist ✅; 8b mcp-client crate ✅; 8c connection manager + tool sync ✅; 8d dispatch + Settings UI ✅ | complete |
| 9 — **External plugin ports** (open-ended) | every plugin that needs creds/external-services — see [plugin-inventory.md](docs/plugin-inventory.md) | queue; bumped from 8 by MCP insertion |
| 10 — **Surface ports & native targets** (last phase) | 10a Tauri Desktop wrapper; 10b iOS / Android native | queue |
| 8 — **External plugin ports** (open-ended) | every plugin that needs creds/external-services — see [plugin-inventory.md](docs/plugin-inventory.md) | queue; no ports started |
| 9 — **Surface ports & native targets** (last phase) | 9a Tauri Desktop wrapper (same React bundle in a webview + OS notifications); 9b iOS / Android native (parallel component layer, Tamagui or similar) | queue |

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
execlaw-core             171     DB, events (+HMAC sign/verify + atomicity +
                                 NULL-tag backfill), outbox, alerts,
                                 memory, principals, idempotency keys,
                                 snapshots, HMAC tamper-tests, migrations
                                 (now 8), conv kind derivation, eval_flagged
                                 store, log query, UserStore, WebauthnStore
                                 (insert/list/count/update_counter/
                                 delete_owned + 10-cred cap + ON DELETE
                                 CASCADE), RefreshTokenStore (issue/
                                 single-use consume/revoke_session/
                                 revoke_all_for_user/active_session_count/
                                 purge_expired + persistence-survives-
                                 recreate invariant), thread metadata
                                 mutators, ConversationResolver,
                                 EphemeralSweeper, KeyRing rotation
execlaw-server            95     auth, events (WS bus), capability tokens,
                                 chat routes (streaming, policy, crash tests,
                                 cold-contact adversarial, identity-match
                                 classifier), tool_dispatch, tracing_layer,
                                 /api/ping setup-detection, /api/admin/me
                                 with JWT extractor, PATCH /api/chats/:id,
                                 GET /api/admin/plugins/ui_panels,
                                 GET /api/chats, deployments CRUD,
                                 multi-controller users, /api/login
                                 webauthn second-factor branch (no-creds
                                 issues tokens; with-creds never silently
                                 bypasses → fail-closed invariant),
                                 OpenAPI coverage guard for every route,
                                 login leak-prevention, username
                                 invalid-shape rejection, /api/logout/all
                                 (multi-session revoke + auth-required)
execlaw-server (integ)    22     plugin_lifecycle (11) + approval_flow (11)
execlaw-policy            43     rule_of_two, trust evaluator, spotlighting,
                                 sideband, input_guard, JWT claims
execlaw-voice-pipeline    41     frames, two-lane graph, endpointer, bargein,
                                 traits (MockAudioIn/Out, MockVad, MockStt,
                                 MockTts), VoiceSession (speech→LLM→TTS,
                                 barge-in rescind/confirm, sentence splitter)
execlaw-runner-local      24     memory_tool (trust scoping + adversarial),
                                 turn executor, thread_tool (set_thread_name
                                 with trim/length/multi-byte/idempotency
                                 + bad-args adversarial)
execlaw-plugin-host       15     hook registry, subprocess RPC
execlaw-plugin-sdk         8     manifest parsing, ZIP staging + zipslip
execlaw-inference-api      7     chat req/resp, streaming SSE parse
execlaw-container-manager  3     mock sysfs detection, PCI vendor mapping
execlaw-vault              9     Argon2id password verification, OS-keyring + passphrase-file fallback round-trip
execlaw-outbox            11     backoff, retry budget, drain, WakeupScheduler (heap ordering, hydrate-from-outbox, sub-second fire)
execlaw-session            1     modality binding
execlaw-eval-harness       2     rubric parse, mock-mode orchestration
--------------------------------------------------------------------
TOTAL                    454 passing, 0 failing

execlaw-web (vitest)     117     api/client (incl. silent-retry on 401:
                                 hook called once + retried with new token,
                                 hook null = no retry, no infinite loop on
                                 retry-401, explicit accessToken disables
                                 the retry) + endpoints + tokens + auth
                                 boot,
                                 SetupWizard form validation (incl. username),
                                 ScreenTransition smoke, AppBoot routing,
                                 chat store (idempotent append, pinned/unread
                                 flag preservation, streaming buffer toggles),
                                 WsClient dispatch + malformed-payload guards,
                                 Composer (Enter / Shift+Enter / disabled),
                                 Sidebar (empty, ordering, click activates,
                                 unread + thinking icon swap), settings shell
                                 (tabs route, active marker), admin pages
                                 (hardware/logs/eval/principals/audit),
                                 plugins page, deployments page (CRUD form +
                                 model_spec JSON validation), UsersPage
                                 (list, role badge + 'you' marker, invite
                                 form POST body, delete confirm flow,
                                 operator read-only view), WebAuthn helpers
                                 (base64url ↔ ArrayBuffer round-trip,
                                 CreationOptions + RequestOptions coercion,
                                 attestation + assertion serialisation
                                 wire-shape)
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
| `conversation_resolver/resolve_controller_short_circuit` | 53 ns | ≤500 ns | pure id-prefix concat, no DB |
| `conversation_resolver/resolve_continue_within_idle` | 9 µs | ≤50 µs | one SELECT + one UPDATE in tx |
| `ephemeral_sweeper/sweep_n_threads/10` | 288 µs | — | per-thread tx; ~13 µs/thread |
| `ephemeral_sweeper/sweep_n_threads/100` | 1.36 ms | ≤1 s for 1k threads | linear; budget headroom > 70× |
| `conversation_metadata/set_display_name` | 2.3 µs | ≤200 µs | one UPDATE behind PATCH route |
| `conversation_metadata/set_pinned` | 3.6 µs | ≤200 µs | |
| `conversation_metadata/mark_ephemeral_then_clear` | 3.9 µs | ≤200 µs | |
| `thread_tool/dispatch_set_thread_name_ok` | 2.4 µs | ≤200 µs | agent tool happy path |
| `thread_tool/dispatch_set_thread_name_too_long` | 126 ns | — | validation rejection, no DB |
| `list_thread_summaries/threads/10` | 6.6 µs | ≤5 ms for 1k | sidebar mount + state.changed event |
| `list_thread_summaries/threads/100` | 95 µs | ≤5 ms for 1k | linear |
| `list_thread_summaries/threads/1000` | 963 µs | ≤5 ms for 1k | ~1 µs/thread, comfortably under budget |
| `principal_store/list_all_100` | 117 µs | ≤5 ms for 1k | powers /api/admin/principals settings page |
| `deployment_store/list/4` | 2.7 µs | ≤1 ms for 64 rows | powers /api/admin/deployments settings page |
| `deployment_store/list/16` | 10.2 µs | ≤1 ms | linear |
| `deployment_store/list/64` | 40.8 µs | ≤1 ms | comfortably under budget |
| `webauthn_store/count_for_user/0` | 1.4 µs | ≤50 µs | runs on EVERY /api/login; gate for 2FA branch |
| `webauthn_store/count_for_user/3` | 1.5 µs | ≤50 µs | linear w/ creds, dominated by SQLite COUNT |
| `webauthn_store/list_for_user/1` | 3.6 µs | ≤200 µs | only on the assertion path |
| `webauthn_store/list_for_user/10` | 8.0 µs | ≤200 µs | hard cap at MAX_CREDENTIALS_PER_USER |
| `refresh_token_store/issue` | 4.9 µs | ≤200 µs | every login + every silent-retry rotation |
| `refresh_token_store/consume_hit` | 5.6 µs | ≤200 µs | atomic DELETE … RETURNING |
| `refresh_token_store/consume_miss` | 2.7 µs | ≤200 µs | replayed token fast-fails |
| `refresh_token_store/revoke_all_for_user/64` | 58 µs | ≤5 ms | "sign out everywhere"; linear in session count |

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
| Conversation FSM + thread metadata | `crates/core/src/conversation.rs` |
| Transport→thread routing + Controller short-circuit | `crates/core/src/transport_conversations.rs` |
| Incognito-thread sweeper | `crates/core/src/ephemeral_sweeper.rs` |
| Operator users + auth lookup | `crates/core/src/users.rs` |
| Multi-controller HTTP routes (list/invite/delete) | `crates/server/src/users.rs` |
| Event-log key ring + back-fill verifier | `crates/core/src/events.rs::backfill_null_tags` |
| Backup / restore CLI | `crates/cli/src/main.rs::cmd_backup` + `cmd_restore` |
| Settings → Users SPA page | `web/src/settings/UsersPage.tsx` |
| WebAuthn credential store + cap | `crates/core/src/webauthn.rs` |
| WebAuthn relying-party + ceremony state | `crates/server/src/webauthn.rs` |
| Login second-factor branch (fail-closed) | `crates/server/src/routes.rs::login` |
| Login screen passkey flow | `web/src/routes/Login.tsx` + `web/src/auth/webauthn.ts` |
| Settings → Profile passkey management | `web/src/settings/ProfilePage.tsx` |
| Persistent refresh tokens (SQLite) + sweeper | `crates/core/src/refresh_tokens.rs` |
| RefreshStore wrapper (server) | `crates/server/src/auth.rs::RefreshStore` |
| /api/logout/all (sign-out-everywhere) | `crates/server/src/routes.rs::logout_all` |
| SPA silent retry hook | `web/src/api/client.ts::setRefreshHook` |
| SPA background refresh + signOutEverywhere | `web/src/auth/AuthContext.tsx` |
| Thread metadata route (PATCH) | `crates/server/src/chats.rs::patch_thread` |
| UI-panel manifest route | `crates/server/src/plugins.rs::list_ui_panels_handler` |
| Thread-name agent tool | `crates/runner-local/src/thread_tool.rs` |
| SPA scaffold (Vite + Bootstrap) | `web/` |
| SPA boot probe + setup-state routing | `web/src/routes/AppBoot.tsx` |
| Setup wizard form | `web/src/routes/SetupWizard.tsx` |
| SPA auth context + token store | `web/src/auth/` |
| SPA API client | `web/src/api/` |
| Chat shell (sidebar + main + composer) | `web/src/routes/Chat.tsx` + `web/src/chat/` |
| WS event bus client | `web/src/api/ws.ts` |
| Chat state store | `web/src/chat/store.ts` |
| Reanimated 4 + RN-web shim | `web/src/anim/`, `web/src/shims/` |

## Recent commit history (foundation branch)

```
106c78f Phase 2 framework: plugin lifecycle + install route + capability-enforced dispatch bridge
f9cae6b Phase 1 complete: HMAC event log + TurnExecutor wired to chat route + streaming SSE
30d9d50 Grounding axioms #13 (testing) + #14 (perf): backfill adversarial tests, wire Criterion, optimize hot paths
f3b8ed4 STATUS.md: Phase 1-4 groundwork session — 152 tests, 15 commits, 5 new subsystems
9b109b4 Wave 3-4: policy engine, spotlighting, sideband, voice pipeline primitives
8a1ced6 Wave 1-2: WS event bus, capability tokens, chat route, plugin hooks, HMAC events
```
