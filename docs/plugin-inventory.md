# Plugin Inventory (Phase 9 queue)

Classification of every `src/integrations/*.ts` (and sibling) in selfhosted-claw into one of four buckets — **port** as a plugin, **fold** into core (or already-folded into a builtin tool), **retire**, or **defer to MCP**.

Originally a Phase-2 kickoff deliverable. Re-bucketed 2026-05-01 after two scope shifts:

1. **MCP integration landed (Phase 8 complete).** Most "wrap-an-HTTP-tool" entries that selfhosted-claw shipped as plugins now have community MCP servers — operators install those instead of waiting for a port. Where MCP is the cleaner answer, the row is moved to a new "Bucket D — MCP."
2. **Native builtin tools subsumed several "plugin" categories.** `WebFetchTool`, `WebSearchTool`, the research subsystem, scheduled tasks, memory tools, chat history tools, etc. all live in `execlaw-core::builtin_tools` with proper capability gating + SSRF guards + event-log integration. Things selfhosted-claw treated as plugins because it lacked a builtin layer are now natives in execlaw — and the inventory reflects that. Re-implementing them as plugins would be a regression.
3. **Plugin tier pivoted to Rhai script (2026-05-01).** Subprocess-tier plugins are reserved for things that genuinely need native code (signal-cli, voice pipeline, ffmpeg, native vision/PDF libs). Everything else is a `.rhai` script — ~50 lines on disk, zero compile, hot-reloadable. Each row in Bucket A now declares which tier applies.

## Status legend

- [ ] not started
- [~] framework scaffold in place, port pending
- [x] fully ported + tested + feature-equivalent
- [native] subsumed by an `execlaw-core::builtin_tools` impl — do not re-port

## Bucket A — port as execlaw plugins

These are the genuine plugin candidates: things our codebase needs and doesn't already have natively. Each is **either** a Rhai script (HTTP-API wrappers, identity providers) **or** a subprocess-tier plugin (native deps).

| selfhosted-claw source | execlaw plugin id | Hooks | Tier | Status |
|---|---|---|---|---|
| `src/integrations/google-contacts.ts` | `plugin-google-contacts` | `identity_provider`, `tools`, `oauth_accounts` | **Rhai** | [ ] |
| `src/integrations/google-calendar.ts` | `plugin-google-calendar` | `tools`, `oauth_accounts`, `ui_panels`, `health_checks` | **Rhai** | [ ] |
| `src/channels/signal.ts` | `plugin-signal` | `transport`, `identity_provider`, `alert_sources`, `health_checks` | **subprocess** (signal-cli binary) | [ ] |
| `src/integrations/phone-voice*.ts` + `src/voice-runner/` | `plugin-voice` | `transport`, `chat_components` | **subprocess** (ffmpeg + WebRTC) | [ ] |
| `src/research/vision.ts` | `plugin-research-vision` | `tools` | **subprocess** (ONNX/local vision model) | [ ] |
| `src/research/pdf.ts` | `plugin-research-pdf` | `tools` | **subprocess** (pdfium / MuPDF) | [ ] |
| `src/control-actions.ts` (per-action) | `plugin-control-<action>` | `tools`, `ui_panels`, `alert_sources` | **Rhai** (most) / hardcoded (UI-only ones) | [ ] |

### Order of port (Rhai-first)

The pivot to Rhai changes the priority — the cheap script-tier plugins land first to validate the runtime against real OAuth + real APIs, before the heavyweight subprocess plugins.

1. **`plugin-google-contacts`** — first real Rhai plugin. Exercises every OAuth surface (vault, sweeper, `_oauth` injection) + the `identity_provider` hook end-to-end. ~50 lines.
2. **`plugin-google-calendar`** — second OAuth-using Rhai plugin. Validates the shape generalises to a tools-only plugin. ~80 lines.
3. **`plugin-signal`** — first real subprocess plugin port. Exercises the transport hook + the SubprocessPlugin shutdown fix from 2026-05-01.
4. **`plugin-voice`** — depends on §4 voice-pipeline + `service-whisper` / `service-kokoro` containers being production-ready.
5. **`plugin-research-vision`**, **`plugin-research-pdf`** — subprocess-tier with native deps; defer until model selection + library packaging are settled.
6. **`plugin-control-*`** — small Rhai scripts; punt to last because most of these are operator-UI affordances rather than agent-callable tools.

## Bucket B — already native (do NOT port as plugins)

The following selfhosted-claw integrations are **already shipped as `execlaw-core::builtin_tools` impls or sibling crate functionality**. They live in our trusted core (capability-gated, SSRF-guarded, event-log-integrated, no IPC roundtrip) rather than as plugins. Re-porting them as plugins would be a regression — the plugin tier's value is *gap-filling*, not duplicating natives.

| selfhosted-claw plugin | execlaw native equivalent | File |
|---|---|---|
| `plugin-url-fetch` | `WebFetchTool` (HTTP GET, SSRF-guarded, capability-gated) | [crates/core/src/builtin_tools.rs](../crates/core/src/builtin_tools.rs) |
| `plugin-search-duckduckgo` / `-brave` / `-exa` | `WebSearchTool` + provider backends | [crates/server/src/tool_apis_search.rs](../crates/server/src/tool_apis_search.rs) — DuckDuckGoSearchApi today; Brave/Exa land as additional backends, not plugins |
| `plugin-research-orchestrator` | Research subsystem (research_start / research_status / research_list / research_get_report) | [crates/core/src/research.rs](../crates/core/src/research.rs) + [crates/server/src/research/](../crates/server/src/research/) |
| `plugin-core-tools` (the long tail) | Most utilities live in builtin_tools.rs (memory, scheduled tasks, chat history, thread renaming, controller notify, etc.) | [crates/core/src/builtin_tools.rs](../crates/core/src/builtin_tools.rs) |

If a specific tool from `plugin-core-tools` turns out to be missing, **add it as a builtin** rather than as a plugin — the tighter integration is the right choice for tools without external service dependencies.

## Bucket C — fold into core (security baselines)

Not user-configurable. Same as the original list:

| selfhosted-claw source | execlaw home | Rationale |
|---|---|---|
| `src/mount-security.ts` + `config-examples/mount-allowlist.json` | `execlaw-container-manager` | Mount allowlist is a security baseline, not a user-configurable plugin. |
| `src/inbound-guard.ts` + `scripts/inbound-message-guard.mjs` | `execlaw-policy::input_guard` | Zero-width / bidi / homoglyph strip runs on every inbound transport event; non-optional. |
| Controller-identity + session primitives (scattered) | `execlaw-server::auth` + `execlaw-policy::trust` | Trust ladder is the security spine. |
| `src/control-store.ts` HMAC primitives | `execlaw-core::event_hmac` | Landed Phase 2. |

## Bucket D — defer to MCP

Operators install a community MCP server instead of waiting for an execlaw-side plugin. Phase 8 wired MCP end-to-end; the discovered tools land on `Settings → Tools` with per-trust-class allowlist gating. For wrap-an-HTTP-API tooling that doesn't need execlaw-specific hooks (identity_provider, transport), MCP is strictly cleaner — no port, no maintenance burden on us, operator picks from the existing ecosystem.

| Use case | Recommended MCP server (examples) |
|---|---|
| GitHub issues / PRs / repos | github-mcp (official) |
| Linear / Jira | community MCP servers exist |
| Slack | slack MCP server |
| Notion | notion MCP server |
| Generic SQL | mcp-server-sqlite, postgres-mcp |
| Filesystem | mcp-server-filesystem |

Anything in this bucket can be reconsidered as an execlaw-native plugin **if** the integration needs an execlaw-specific hook (identity_provider, transport) or substantially tighter security/event-log integration than MCP provides. The default is "use MCP."

## Bucket E — retire

Things we're explicitly not porting. Per MIGRATION_PLAN §12 "What We're Not Porting":

| selfhosted-claw source | Why |
|---|---|
| `src/integrations/anthropic*.ts` | Violates axiom #1 (no cloud LLMs). |
| `src/integrations/openai*.ts` | Violates axiom #1. |
| `src/integrations/google-gemini*.ts` | Violates axiom #1. |
| Any hosted-registry / phone-home telemetry | Violates axiom #1. |
| The `isMain` boolean auth | Replaced by the trust ladder (§2.6). |
| JID-shaped routing (`signal:user:*`, `signal:group:*`) | Replaced by the participant-aware model (§2.6). |

(The original "MCP client shims" retire-line was wrong — MCP is now first-class; see Bucket D.)

## Where the framework is at the start of Phase 9

| Primitive | Status | File |
|---|---|---|
| Plugin manifest parse (`plugin.toml` with hooks, per §4.2) | done | [crates/plugin-sdk/src/manifest.rs](../crates/plugin-sdk/src/manifest.rs) |
| ZIP staging + zipslip defense | done | [crates/plugin-sdk/src/zip_stage.rs](../crates/plugin-sdk/src/zip_stage.rs) |
| Hook registry (tools, transports, identity providers, panels, event subs, alert sources, oauth_accounts) | done | [crates/plugin-host/src/hook_registry.rs](../crates/plugin-host/src/hook_registry.rs) |
| Subprocess plugin tier (JSON-RPC over stdin/stdout) | done; shutdown deadlock fixed 2026-05-01 | [crates/plugin-host/src/subprocess.rs](../crates/plugin-host/src/subprocess.rs) |
| **Script plugin tier (Rhai engine + sandbox + primitives)** | **done 2026-05-01** | [crates/script/](../crates/script/) |
| **Plugin host dispatches to either tier** | **done 2026-05-01** | [crates/plugin-host/src/host.rs](../crates/plugin-host/src/host.rs) |
| **Generic OAuth machinery (vault / clients / tokens / pending / sweeper / admin endpoints / `_oauth` injection)** | **done 2026-04-30** | [crates/core/src/oauth.rs](../crates/core/src/oauth.rs), [crates/server/src/oauth_*](../crates/server/src/) |
| Capability-token issuance | done (Phase 1) | [crates/server/src/capability.rs](../crates/server/src/capability.rs) |
| HMAC-signed event log | done (Phase 1) | [crates/core/src/event_hmac.rs](../crates/core/src/event_hmac.rs) |
| MCP client + admin endpoints + tool registry sync | done (Phase 8) | [crates/server/src/mcp_admin.rs](../crates/server/src/mcp_admin.rs) |
| **First real Rhai plugin (`plugin-google-contacts`)** | **next up** | (this diff) |
