# Changelog

All notable changes to execlaw are documented here. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/) once
a 1.0 ships. Until then every change lives in `Unreleased` and the only
release artifact is the latest `foundation` commit.

This file is hand-curated. The full per-commit log is in `git log`;
this changelog is for operators and plugin authors who want to see the
"what changed for me" view without combing through 345+ commits.

## [Unreleased]

### Fixed

- **Factory reset now produces an actual factory state.** The previous
  `wipe_all_user_tables` `DELETE`d every non-`schema_version` row but
  only re-seeded `config_general`. Every other migration-seeded
  singleton (`config_research`, `config_personality`,
  `config_search_providers`, `config_skills`, …) was left empty.
  Downstream code reading those tables via `query_row(...)?` (no
  `.optional()` fallback) blew up minutes later with
  `sqlite error: Query returned no rows`. Symptom: deep-research jobs
  stalled / failed immediately after a fresh-account setup that
  followed a factory reset.

  Rewrote `factory_reset.rs::wipe_and_remigrate` to DROP every user
  table including `schema_version`, then re-run
  `MigrationRunner::apply_all()`. The migration set's `CREATE TABLE`
  + `INSERT OR IGNORE` statements take care of re-creating the schema
  AND re-seeding every singleton. The response body now also reports
  `migrations_reapplied`. Three new regression tests pin: (a) every
  migration-seeded singleton is populated after reset, (b)
  `ResearchConfigStore::get()` succeeds post-reset, (c) the
  `tables_wiped` + `migrations_reapplied` counts both surface in the
  response JSON.

- **`ResearchConfigStore::get()` honors its docstring.** The
  docstring promised "returns the defaults on a fresh DB rather than
  `None`" but the implementation propagated `QueryReturnedNoRows`
  through `?` whenever the singleton row was missing. Fixed with
  `.optional()` + fall-through to `ResearchConfig::default()`. A new
  unit test pins the contract by DELETEing the row then asserting
  `get()` still returns defaults.

### Added

- **GitHub Actions CI** (`.github/workflows/ci.yml`) — four-target
  Rust matrix (Linux x86_64, macOS x86_64, macOS arm64, Windows MSVC)
  plus a separate SPA job. Runs on every push and PR.
- **`docs/plugins.md`** — comprehensive plugin-author reference:
  manifest schema, runtime tiers, sidecar model, Rhai primitives,
  step-by-step walkthroughs, common pitfalls.
- **`AGENTS.md`** — onboarding doc for AI coding agents (and humans).
  Non-negotiable rules, repo orientation, code conventions, common
  task patterns, end-of-task checklist.
- **`docs/screenshots/`** directory + convention doc for SPA shots
  embedded in README.
- **WhatsApp transport plugin** (`plugins/whatsapp/`) via the wuzapi
  sidecar — full inbound (webhook-driven) + outbound, QR pairing,
  privacy-mode JID handling, RFC3339 timestamp parsing, read
  receipts via `/chat/markread`, msg-ID dedupe.
- **Slack transport plugin** (`plugins/slack/`) via Socket Mode —
  multi-workspace OAuth, sidecar-free.
- **SMS transport plugin** (`plugins/sms-socket/`) via a local
  Android-side WebSocket gateway. Includes the rehydrate-cursor
  recovery protocol for handling gateway downtime.
- **Pushover plugin** (`plugins/pushover/`) — one-way outbound
  notifications.
- **Google Places plugin** (`plugins/google-places/`) — text search,
  nearby search, place details via the Places (New) API. API-key
  auth (no OAuth).
- **`[[webhook_routes]]` plugin manifest section** — public,
  unauthenticated HTTP routes mounted at
  `/api/webhooks/{plugin_id}{path}`. The plugin handler validates
  caller identity (typically against a vault-stored shared secret).
- **`host_route_inbound_spawn` script primitive** — fire-and-forget
  variant of `host_route_inbound` for HTTP-webhook handlers where
  upstream timeouts (wuzapi: 30s) require sub-second acks.
- **`parse_rfc3339_ms` script primitive** — RFC3339 string →
  millisecond epoch i64.
- **MCP server install + dispatch** — operator-installable MCP
  servers (`config_mcp_servers` table), agent-callable tools
  surfaced alongside plugin tools.
- **Memory tier lifecycle** (migration 0035) — hot / warm / cold
  tiers on `memory_entries`, promotion proposals, reflection log.
- **Search-provider plumbing** — multiple providers (SerpAPI,
  Perplexity, SearchAPI, Websurfx self-hosted), rotating wrapper
  with global pacing, per-provider enable toggles.
- **Group-awareness** in the agent classifier — the agent knows
  when it's in a group and is biased toward silence unless
  explicitly addressed.
- **Auto-bridge** of agent text replies — when the model produces
  a `model_turn` text without explicitly calling
  `<channel>.send_message`, the host dispatches it via the inbound
  transport.

### Changed

- **Project license relicensed from AGPL-3.0-or-later to Apache-2.0.**
  `LICENSE` file replaced with the canonical Apache-2.0 text. New
  `NOTICE` file at the repo root carries the project copyright
  (Copyright © 2026 Justin Long). All 11 plugin manifests + the
  workspace `Cargo.toml` flipped their `license` SPDX identifier.
  CONTRIBUTING.md `## Licensing` section rewritten — the
  AGPL-network-clause language is dropped; Apache-2.0's patent
  grant + contribution-back clause are summarized in its place.
  No CLA / DCO requirement is being added; opening a PR remains
  the certification.
- **Default bind port aligned to `127.0.0.1:3031` everywhere** —
  production service, dev-server, Vite proxy, and tests. Previous
  state had the production service on 3030 and the dev-server on
  3031 to dodge Docker Desktop's vpnkit squat on Windows; fixing
  the port mismatch is now a one-default story. Operators on
  existing 3030 installs stay on 3030 (DB row wins) until they
  edit Settings → General.
- **Control plane is no longer containerized** — the architecture
  doc previously claimed "the control plane is a single Docker
  container." It's now a per-OS native binary registered as a host
  service via the `service-manager` crate (systemd / launchd /
  Windows SCM). Containers are still used for the per-conversation
  runner, plugin sidecars, and managed-mode inference backends.
- **Webhook dispatcher accepts both `application/json` and
  `application/x-www-form-urlencoded` bodies** — wuzapi posts the
  latter; everything else posts the former. Plugins see a uniform
  Map shape regardless.
- **WhatsApp wuzapi retry envelope extended to ~8.5h** —
  `WEBHOOK_RETRY_COUNT=10`. Covers operator downtime longer than
  wuzapi's 15-min default.
- **Signal plugin typing-indicator** uses PUT (not POST) per the
  upstream contract.
- **Agent tool dispatch caps bumped** + every dispatch is logged
  for audit visibility.
- **`MIGRATION_PLAN.md` retired** — its content was distributed
  across `docs/architecture.md`, `docs/agent-model.md`,
  `docs/plugins.md`, `docs/sidecar-supervisor-design.md`,
  `docs/runner-design.md`. Inline source comments still cite
  `MIGRATION_PLAN.md §X` as historical citations.
- **Plugin webhook handlers must use `host_route_inbound_spawn`**
  for routes that route to the agent. The synchronous
  `host_route_inbound` blocks until the agent finishes the turn,
  which exceeds upstream HTTP-webhook timeouts and triggers
  duplicate retries.

### Fixed

- **WhatsApp duplicate-reply bug** — wuzapi was retrying webhook
  POSTs that took >30s (the agent reply path commonly does), and
  each retry was running a fresh agent turn. Fix: spawn dispatch
  + dedupe by `Info.ID` in the plugin layer.
- **WhatsApp inbound pipeline (multiple bugs)** — wuzapi field
  names (`webhookurl`/`events` not `webhook`/`subscribe`),
  jsonData envelope unwrap, RFC3339 timestamp parsing,
  privacy-mode `SenderAlt` JID resolution.
- **WhatsApp `/session/connect` 500 'already connected'** —
  swallowed as the desired state instead of bubbled as an error.
- **Auto-bridge double-send for SMS / WhatsApp / Slack** — the
  bridge was firing alongside the model's explicit
  `<channel>.send_message` call. Now backs off when the agent
  already invoked the explicit tool in the same turn.
- **Hardcoded transport-name matches in host code** replaced with
  dynamic registry lookups.
- **`is_send_tool_for_channel`** derived from convention
  (`<channel>.send_message`) instead of a hardcoded table.
- **sms-socket WebSocket reconnect** — server-ping pongs sent
  (fixes keepalive timeout), duplicate subscription prevented,
  rehydrate cursor anchored on every reconnect, post-close
  reconnect tightened.
- **Migration checksum tolerates line-ending differences** + new
  `repair-checksum` subcommand for fixing drift.
- **Pushover Rhai const-in-fn-body NameError** — module-level
  `const` is invisible inside `fn` bodies in Rhai; literals
  inlined at call sites.
- **`ws_subscribe_bidi` closes the previous active subscription**
  on replace, preventing leaked WS handles when a plugin
  re-subscribes.

### Security

- **HMAC-chained event log** — every committed `state_events` row
  is signed with the server's HMAC key. The chain is
  `tag_n = HMAC(key, prev_tag || payload)`, making the log
  tamper-evident.
- **Signed approval-token JWT** — cold-contact responses include
  an `approval_token` whose `jti` matches the `approval_id`. The
  respond endpoint verifies the JWT before honoring any verb so
  an attacker who guesses the id can't forge a response.
- **Capability-token gating** on every tool dispatch — Ed25519-
  signed JWT scoped to one conversation + one turn.
- **Trust-floor enforcement** at the host layer before plugin
  dispatch — tools with `trust_floor = "Controller"` reject
  callers below that rank.
- **Read-down memory cascade** enforced at the storage shim, not
  at the tool layer — a `KnownTrusted` caller cannot read
  `Controller`-scoped memory entries even if they pass the right
  scope+key.

### Removed

- **`MIGRATION_PLAN.md`** at the repo root. (Content distributed
  to `docs/`; inline source citations preserved.)
- **`docs/plugin-inventory.md`** — Phase-8 port queue, no longer
  meaningful.
- **`web/scripts/dev-3031.mjs` + the `dev:3031` npm script** —
  redundant once `:3031` became the default everywhere.

---

## Format & maintenance notes

- Date the section heading when a tagged release ships
  (e.g. `## [0.1.0] — 2026-MM-DD`).
- Group entries by **Added / Changed / Fixed / Security / Removed**
  in that order. Skip empty groups.
- Keep entries terse — one line per change, leading verb, no
  marketing voice.
- Operator-facing detail wins over implementation detail. "Fixed
  duplicate-reply bug on WhatsApp" beats "refactored
  `dispatch_handler::body_value`".
- Update this file *in the same commit* as the change it describes
  — same rule the codebase enforces for doc updates that ride
  alongside code.
