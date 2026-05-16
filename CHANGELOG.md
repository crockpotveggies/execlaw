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

- **Hydrate auto-evicts zombie plugin rows.** When `PluginHost::hydrate`
  couldn't load a plugin at boot — stage directory missing,
  `main.rhai` gone, subprocess binary unexec'able, manifest TOML
  corrupted — it used to call `registry.disable(plugin_id)` and
  leave `state_plugins.enabled=1` in the DB. That left a "zombie"
  state where the SPA queried `state_plugins`, saw the plugin as
  installed, rendered its config page, and every admin route 404'd
  with the confusing
    `no [[admin_routes]] entry on plugin 'X' matching GET /…`
  error. Recovery required a manual uninstall + reinstall via the
  SPA.

  Symptom: the factory-reset orphan-directory sweep added in
  commit 3753e05 wipes `~/.execlaw/plugins/` as part of a clean
  slate; if a `state_plugins` row from before the reset somehow
  survived (or if the user ran a different code path that wiped
  the stage tree without nuking the DB), the next server restart
  hit this exact zombie state.

  New behavior: hydrate now calls a single `evict_unrecoverable_plugin`
  helper that disables the plugin in the registry AND deletes the
  `state_plugins` row. The SPA's install-list query then correctly
  reports the plugin as uninstalled, and the operator sees a clean
  "Install plugin" affordance instead of a broken config page.
  All four hydrate failure branches (manifest unparseable, missing
  runtime.executable, missing runtime.source, runtime spawn failure)
  go through the same eviction path so DB ↔ registry parity is the
  rule, not the exception.

  Two new regression tests pin the contract:
  `hydrate_auto_purges_zombie_row_when_stage_dir_is_missing` (the
  reported failure mode) and
  `hydrate_auto_purges_zombie_row_when_manifest_is_unparseable`
  (the corrupted-TOML cousin).

- **Sidecar host-port allocation is now dynamic and self-healing.** The
  prior allocator was a naive monotonic counter starting at port 8501;
  when any pool member was occupied externally (a stale Docker veth, a
  competing dev server, a prior crashed sidecar holding the port), the
  supervisor minted the busy port, the Docker spawn failed with
  `Bind for 127.0.0.1:<p> failed: port is already allocated`, the slot
  burned a restart attempt, and the supervisor pinned the same dead
  port on every retry — parking the sidecar `CrashLooping` after
  `MAX_RESTART_ATTEMPTS` for an entirely environmental, externally-
  fixable problem. (Observed in production 2026-05-14 with signal-cli
  blocked on port 8502.)

  Two new layers in `crates/server/src/sidecar_supervisor.rs` close
  the gap:

    * **`port_is_free` OS probe.** `allocate_port` now walks the pool
      from `next_host_port` toward `SIDECAR_PORT_POOL_END` and skips
      any candidate that fails a `TcpListener::bind` probe. Common-
      case fast path: the busy port is detected before Docker is
      even called.
    * **`is_port_conflict_error` spawn-failure classifier.** When a
      Docker spawn does fail with a port-conflict signature (the
      probe-vs-bind TOCTOU race that the probe can't catch), the
      supervisor releases `slot.host_port` so the next reconcile
      mints a fresh one, and crucially does **NOT** burn a restart
      attempt — port conflicts are environmental, not plugin bugs,
      and the retry is cheap. The supervisor also `kick()`s the
      reconcile loop so the recovery spawn happens immediately
      instead of waiting the full tick interval.

  Four new tests pin the behavior: external-occupant skip (real
  `TcpListener` binds a pool port → allocator must hand out a
  different one), the `port_is_free` probe round-trip, the
  `is_port_conflict_error` matcher across known Docker error
  variants, and the end-to-end conflict-recovery flow (spawn fails
  → port released → next spawn succeeds with no restart-attempt
  burn).

### Added

- **Conversation history sliding-window truncation.** Pre-fix every
  chat turn re-hydrated the entire `state_events` table from
  `seq = 0` into the prompt, growing linearly with conversation
  length and quadratically with operator usage. Observed 2026-05-14:
  a long-lived Signal thread with ~83 KB of stored events took
  ~24.5 s per turn vs ~657 ms on a fresh web chat with ~3 KB of
  events on the same backend, same model, same code path. The cost
  is entirely prompt-prefill on the larger history.

  New `execlaw_core::history_budget` module + migration 0003
  (`config_general.max_history_tokens`, default 8000). Both
  `run_real_turn` and `run_tool_capable_turn` route their hydrated
  history through `truncate_to_budget` before constructing
  `ChatMessage`s. Walks backward from most-recent, drops oldest
  past the budget, preserves pair coherence (Assistant always has
  its preceding User), and guarantees the last 4 messages survive
  any budget (anti-starvation). Char-to-token estimate is
  `chars/4` — no tokenizer dependency. 11 unit tests pin the
  invariants (monotone in budget, current-turn always lands at the
  model, dedupe-aware blob delete is unchanged, etc.).

- **Local principal cache + O(1) identifier lookup.** Pre-fix
  `PrincipalStore::find_by_identifier` did
  `list_all().into_iter().find(...)` — an O(N) scan over every
  principal row (each loaded + JSON-deserialised) just to answer
  "have we seen this (transport, handle) before?" Every external
  inbound (Signal, WhatsApp, future bridges) hits this check via
  `admit_external_principal`, and a 500-contact install paid a full
  principal-table read per inbound message.

  Migration 0004 adds `principal_identifiers (transport, handle,
  principal_id)` with composite PK + supporting indexes + CASCADE
  delete + a one-shot backfill from existing `identifiers_json`
  (`json_each` over a `CAST AS TEXT` of the BLOB column).
  `PrincipalStore::upsert` now resyncs the index inside a single
  transaction (delete-then-insert; identifier sets are 1-4 rows
  per principal, cheap). `find_by_identifier` becomes a PK probe;
  `find_all_by_identifier` (used by reconcile to enumerate every
  claimant of the same identifier) is a small filtered query.

  Policy unchanged but now documented in code: the
  `principal_admit::admit_external_principal` cache-first path
  returns ANY previously-seen principal regardless of trust class
  (Controller, KnownTrusted, KnownLimited, UnknownPending, Blocked)
  without re-running the identity-provider plugin fanout. Plugin
  fanout only runs for first-ever inbound from a `(transport,
  handle)` pair. Six new tests pin the index sync (population
  on upsert, resync on identifier-set change, CASCADE on principal
  delete, single-claimant lookup, multi-claimant lookup for the
  reconcile fixture, migration backfill).

- **Plugin-lifecycle `purge` orchestration** —
  `crates/server/src/plugin_lifecycle.rs` is a new module that chains
  every per-plugin teardown step in one load-bearing order:

    1. `PluginHost::disable` fires the plugin's `on_disable` Rhai hook
       while its OAuth tokens, vault secrets, and transport bindings are
       still readable — so a well-behaved plugin can revoke an upstream
       OAuth grant or send a "going offline" message on its transport.
    2. `SidecarSupervisor::remove_for_plugin` stops + `docker rm -f`'s
       every container the plugin owns AND recursively deletes its
       per-plugin state root at `~/.execlaw/sidecars/<plugin_id>/`. The
       state-dir delete is the gap that earlier `stop_all` left behind:
       a re-install silently inherited signal-cli's keystore /
       wuzapi's session DB.
    3. OAuth tokens + clients, plugin artifacts (with refcount-aware
       blob delete), and vault secrets for the plugin are deleted by
       their respective stores' new `delete_for_plugin` /
       `purge_artifacts_for_plugin` methods.
    4. `PluginHost::uninstall` archives plugin-shipped skills, deletes
       the `state_plugins` row, and removes the staged plugin dir.

  Two callers consume the orchestrator:

    * `DELETE /api/admin/plugins/{id}` — the SPA's "Uninstall plugin"
      button now produces a true clean slate instead of leaving orphan
      Docker containers, state dirs, OAuth grants, vault secrets, and
      artifact blobs behind. Response shape changed from `{"ok": true}`
      to a `PluginPurgeReport` JSON body with per-resource counts.
    * `POST /api/admin/factory-reset` — enumerates every installed
      plugin via `PluginHost::list_rows`, runs `purge_plugin` for each,
      then sweeps top-level orphan dirs (`~/.execlaw/sidecars`,
      `~/.execlaw/plugin_artifacts`, `~/.execlaw/plugins`,
      `~/.execlaw/research`) before the DB file is rebuilt. Response
      body grew `plugins_purged: Vec<PluginPurgeReport>` and
      `orphan_dirs_removed: Vec<String>`; the old opaque
      `plugins_torn_down` / `sidecars_stopped` counters are replaced.

- **`OauthClientStore::delete_for_plugin` + `OauthTokenStore::delete_for_plugin`**
  (`crates/core/src/oauth.rs`) — bulk delete every OAuth grant owned by
  a `plugin_id`. Tokens already cascade from clients via the FK; the
  explicit token-delete is a defensive safety-net for hand-edited DBs.
- **`AttachmentStore::purge_artifacts_for_plugin`**
  (`crates/core/src/attachments.rs`) — wipe every `state_artifacts`
  row for a plugin AND (refcount-aware) unlink the underlying blobs
  on disk. Two plugins emitting identical chart bytes share one blob;
  uninstalling one must not break the other — pinned by test.
- **`VaultRowStore::delete_for_plugin`**
  (`crates/core/src/vault_row.rs`) — drop a plugin's `vault_secrets`
  rows. Core-scope rows (`plugin_id IS NULL`) are never touched —
  pinned by test.
- **`SidecarSupervisor::remove_for_plugin`**
  (`crates/server/src/sidecar_supervisor.rs`) — per-plugin variant of
  `stop_all`. Stops + removes every container matching the plugin
  AND deletes `~/.execlaw/sidecars/<plugin_id>/`. Returns a
  `SidecarRemovalReport`. Other plugins' state is untouched.

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

  Rewrote `factory_reset.rs::wipe_and_remigrate` to blow away the
  on-disk database file entirely (`.db` + `-wal` + `-shm` +
  `-journal`) via a new `Database::rebuild_to_empty(&DbConfig)`
  method, then re-open at the same path with the same encryption
  posture and re-run `MigrationRunner::apply_all()`. The migration
  set's `CREATE TABLE` + `INSERT OR IGNORE` statements take care of
  re-creating the schema AND re-seeding every singleton. An earlier
  iteration tried DROP TABLE per user table, but that tripped on the
  FTS5 `skill_search` virtual table's destructor
  (`vtable constructor failed: skill_search`); file-level delete
  bypasses vtable lifecycle entirely. `AppState` gained a
  `db_config: Arc<DbConfig>` field so the reset path can re-open
  without round-tripping through the OS keyring. The response body
  now reports `migrations_reapplied`. Three new regression tests
  pin: (a) every migration-seeded singleton is populated after
  reset, (b) `ResearchConfigStore::get()` succeeds post-reset, (c)
  the `tables_wiped` + `migrations_reapplied` counts both surface in
  the response JSON.

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
