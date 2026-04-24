# Phase 2 Plugin Inventory

Kickoff deliverable from MIGRATION_PLAN §11 Phase 2: every `src/integrations/*.ts` (and sibling) in selfhosted-claw gets assigned to one of three buckets — **port** as a plugin, **fold** into core, or **retire**.

This document is the checklist that drives the rest of Phase 2. As each plugin lands, mark it off here.

## Status legend

- [ ] not started
- [~] framework scaffold in place, port pending
- [x] fully ported + tested + feature-equivalent

## Bucket A — port as execlaw plugins

Each of these becomes a ZIP-installable plugin under `crates/plugins/<name>/` (or a submodule repo). The plugin declares its hooks in `plugin.toml` per §4.2.

| selfhosted-claw source | execlaw plugin id | Hooks declared | Blocker |
|---|---|---|---|
| `src/channels/signal.ts` | `plugin-signal` | `transport`, `identity_provider`, `alert_sources`, `health_checks` | signal-cli subprocess available |
| `src/integrations/phone-voice*.ts` + `src/voice-runner/` | `plugin-voice` | `transport`, `chat_components` | §4 voice pipeline primitives + `service-whisper`, `service-kokoro` containers |
| `src/integrations/deep-research.ts` + `src/research/orchestrator.ts` | `plugin-research-orchestrator` | `tools`, `services`, `alert_sources` | Phase 5 evaluation scaffolding |
| `src/integrations/search-exa.ts` | `plugin-search-exa` | `tools`, `oauth_accounts` | Exa API key (optional — Brave / DDG fallbacks below) |
| `src/integrations/search-brave.ts` | `plugin-search-brave` | `tools`, `oauth_accounts` | Brave Search API key |
| `src/integrations/search-duckduckgo.ts` | `plugin-search-duckduckgo` | `tools` | none (DDG Instant Answer is keyless) |
| `src/integrations/url-fetch.ts` | `plugin-url-fetch` | `tools` | none |
| `src/research/vision.ts` | `plugin-research-vision` | `tools` | local vision model (`service-vision` — rolled into §4 voice-LLM for now) |
| `src/research/pdf.ts` | `plugin-research-pdf` | `tools` | pdfium or MuPDF vendored in a probe container |
| `src/integrations/google-calendar.ts` | `plugin-google-calendar` | `tools`, `oauth_accounts`, `ui_panels`, `health_checks` | Google OAuth2 client creds |
| `src/integrations/google-contacts.ts` | `plugin-google-contacts` | `identity_provider`, `tools`, `oauth_accounts` | Google OAuth2 client creds |
| `src/control-actions.ts` (per-action) | `plugin-control-<action>` (one per action) | `tools`, `ui_panels`, `alert_sources` | none |
| `src/tools/*.ts` overflow (the long tail not folded into `runner-local` built-ins) | `plugin-core-tools` | `tools` | none |

### Order of port

Priority for the first wave (§11 phase 2 demo milestone):
1. `plugin-signal` — proves the transport hook path end-to-end, unblocks the Signal-over-web-chat demo.
2. `plugin-google-calendar` — proves the `oauth_accounts` hook path, unblocks the OAuth-expiry-alert demo.
3. `plugin-url-fetch` — the simplest tool plugin, smoke-test for the tool-dispatch bridge.
4. `plugin-core-tools` — ports the long tail of small `src/tools/*.ts` utilities as one bundle.

The research/search plugins and google-contacts port after the framework + first-wave plugins stabilize. The voice plugin waits on §4 voice-pipeline + `service-whisper` / `service-kokoro` containers, per the MIGRATION_PLAN Phase-4 placement.

## Bucket B — fold into core

Not every `src/integrations/*.ts` becomes a plugin. The security-baseline ones are folded into `execlaw-container-manager` or `execlaw-policy` so they can't be disabled or swapped:

| selfhosted-claw source | execlaw home | Rationale |
|---|---|---|
| `src/mount-security.ts` + `config-examples/mount-allowlist.json` | `execlaw-container-manager` | Mount allowlist is a security baseline, not a user-configurable plugin. |
| `src/inbound-guard.ts` + `scripts/inbound-message-guard.mjs` | `execlaw-policy::input_guard` | Zero-width / bidi / homoglyph strip runs on every inbound transport event; non-optional. |
| Controller-identity + session primitives (scattered) | `execlaw-server::auth` + `execlaw-policy::trust` | Trust ladder is the security spine. |
| `src/control-store.ts` HMAC primitives | `execlaw-core::event_hmac` | Landed Phase 2. |

## Bucket C — retire

Things we're explicitly not porting. Per MIGRATION_PLAN §12 "What We're Not Porting":

| selfhosted-claw source | Why |
|---|---|
| `src/integrations/anthropic*.ts` | Violates axiom #1 (no cloud LLMs). |
| `src/integrations/openai*.ts` | Violates axiom #1. |
| `src/integrations/google-gemini*.ts` | Violates axiom #1. |
| Any hosted-registry / phone-home telemetry | Violates axiom #1. |
| MCP client shims | Replaced by native OpenAI function-calling per §4.3. |
| The `isMain` boolean auth | Replaced by the trust ladder (§2.6). |
| JID-shaped routing (`signal:user:*`, `signal:group:*`) | Replaced by the participant-aware model (§2.6). |

## Where the framework is at the start of Phase 2

| Primitive | Status | File |
|---|---|---|
| Plugin manifest parse (`plugin.toml` with hooks, per §4.2) | done | `crates/plugin-sdk/src/manifest.rs` |
| ZIP staging + zipslip defense | done | `crates/plugin-sdk/src/zip_stage.rs` |
| Hook registry (tools, transports, identity providers, panels, event subs, alert sources) | done | `crates/plugin-host/src/hook_registry.rs` |
| Subprocess plugin tier (JSON-RPC over stdin/stdout) | done | `crates/plugin-host/src/subprocess.rs` |
| Capability-token issuance | done (Phase 1) | `crates/server/src/capability.rs` |
| HMAC-signed event log | done (Phase 1) | `crates/core/src/event_hmac.rs` + `events.rs` |
| **`PluginHost` lifecycle (install/enable/disable/uninstall, persisted)** | **Phase 2 scope** | *this diff* |
| **`POST /api/admin/plugins/install` + list/enable/disable/uninstall routes** | **Phase 2 scope** | *this diff* |
| **Hook-registry → `TurnExecutor::tool_dispatch` bridge** | **Phase 2 scope** | *this diff* |
| **Capability enforcement at tool-dispatch time** | **Phase 2 scope** | *this diff* |
| **Reference in-tree plugin for E2E test** | **Phase 2 scope** | *this diff* |
| Actual integration ports (signal, calendar, research, etc.) | **follow-up sessions** | external-service-dependent |
