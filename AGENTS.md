# AGENTS.md

Guide for AI coding agents (Claude Code, Cursor, Aider, etc.) working on
this repo. The product itself is also a coding-capable agent platform —
this doc is about agents *editing* execlaw, not agents *running on*
execlaw.

If you are an agent and you have not yet read [`docs/architecture.md`](docs/architecture.md),
do that first. The rest of this file assumes you've internalised the
12 design principles in §2 of that doc.

---

## 1. What this codebase is

execlaw is a self-hosted Rust agent framework. Single-operator, runs on
the operator's hardware, no cloud LLMs ever. It's built around three
load-bearing abstractions:

1. **An append-only event log** (SQLite, optionally SQLCipher-encrypted).
   Every action is a row in `state_events`; replay reconstructs state.
2. **A plugin framework** that loads ZIP bundles at runtime — manifest
   declares tools, sidecars, transports, identity providers, OAuth
   clients, webhook routes. Two runtime tiers: script (Rhai) and
   subprocess (JSON-RPC).
3. **A trust ladder + Rule of Two policy gate** that decides which tools
   the agent can call given the principal's trust class and the turn's
   sensitivity.

The Rust workspace lives in `crates/`. The SPA lives in `web/`.
First-party plugins live in `plugins/`. Documentation lives in `docs/`.
Design rationale lives in `MIGRATION_PLAN.md`.

---

## 2. Non-negotiable rules

These are axioms. Violating any of them will require a re-do.

1. **No cloud LLMs.** Anthropic, OpenAI, Gemini, Mistral cloud — none of
   them, on any code path, ever. Inference happens against a local
   OpenAI-compatible endpoint (vLLM / OpenArc / similar). If a feature
   seems to require a cloud LLM, the answer is "design it differently
   or don't ship it."
2. **Plugins, not hardcoded built-ins.** If a host crate references a
   specific plugin id by name in production code (e.g. `if plugin_id ==
   "signal"`), that's a leak. The host knows about plugins only through
   manifest-declared surfaces (registry lookups, capability gates).
   Test fixtures and doc-comment examples are fine.
3. **SQLite is the source of truth.** No environment variables for
   configuration, no `.env` files, no shared TOML/YAML in `/etc`.
   Operator-editable config lives in `config_*` tables; secrets live in
   the SQLCipher vault keyed by an OS-keyring master key.
4. **Effects go through the outbox.** The LLM never makes external HTTP
   calls directly. It emits a `tool_use` event; the host enqueues a
   `state_outbox` row with a framework-minted idempotency key; a
   separate relay drains it.
5. **`tool_use` and `tool_result` always pair in the same commit.**
   Enforced by `EventLog::commit_turn::enforce_tool_pairing()`. If a
   turn fails mid-tool, a synthetic cancellation `tool_result` must be
   committed alongside.
6. **Tests are mandatory for non-trivial code.** Per the project's
   axiom #13, every non-trivial function, every invariant, every public
   API has tests. Security-critical code has adversarial tests.
   `cargo test --workspace` must pass before any commit.
7. **Performance regressions are blocked by Criterion benchmarks.** Per
   axiom #14, hot paths have benchmarks with explicit budgets. Don't
   claim a speedup without numbers.
8. **Never create commits without explicit human approval.** A user
   asking you to fix a bug is not the same as authorising a commit.
   Wait for "commit", "push", or equivalent before invoking git.
9. **Never push to `main`/`master`.** The active branch is `foundation`.
   Force-pushing to anything other than your own ad-hoc branch requires
   explicit user permission.

---

## 3. Repository orientation

```
execlaw/
├── crates/                Rust workspace (~25 crates)
│   ├── core/              Event log, FSM, migrations, principal store, memory
│   ├── server/            Axum surface — chats, approvals, plugins, sidecar supervisor
│   ├── plugin-host/       Manifest parsing, install, hook registry
│   ├── plugin-sdk/        Manifest schema (the source of truth for plugin TOML shape)
│   ├── script/            Embedded Rhai engine + primitives
│   ├── policy/            Trust gating, capability tokens, input guards
│   ├── runner-local/      TurnExecutor — the agent's tool loop
│   ├── inference-api/     OpenAI-compatible client
│   ├── voice-pipeline/    STT → LLM → TTS graph
│   ├── outbox/            Outbox relay primitives
│   ├── container-manager/ bollard wrapper + GPU detection
│   ├── vault/             SQLCipher-encrypted secret store
│   ├── mcp-client/        MCP server tool dispatch
│   ├── cli/               `execlaw` binary
│   └── eval-harness/      LLM-judge harness
├── plugins/               In-tree reference plugins (10 of them)
│   ├── signal/, whatsapp/, slack/, sms-socket/    Transports
│   ├── google-calendar/, google-contacts/, google-places/
│   ├── pushover/, hello/, identity-local-address-book/
├── web/                   SPA (React + react-bootstrap + Vite)
├── docs/                  Architecture, agent-model, plugins, screenshots
├── scripts/               dev-server.sh / dev-server.ps1
├── evals/                 Rubric TOML files
├── spec/                  OpenAPI + AsyncAPI specs
├── MIGRATION_PLAN.md      Section-by-section design rationale
├── README.md              Quick start + dev mode
└── AGENTS.md              You are here
```

When asked to find something:

| Question | Where to look |
|---|---|
| How is X persisted? | `crates/core/migrations/00*.sql` for the schema; `crates/core/src/<feature>.rs` for the store layer. |
| How is the agent's tool list built? | `crates/runner-local/src/turn.rs` (TurnExecutor) → registry queries against plugin-host. |
| What primitives can a Rhai plugin call? | `crates/script/src/primitives.rs` — every `engine.register_fn(...)` is a public surface. |
| What does the manifest accept? | `crates/plugin-sdk/src/manifest.rs`. |
| Why does the auto-bridge exist? | `crates/server/src/chats.rs::bridge_text_reply_to_originating_transport`, plus `docs/agent-model.md` §10. |
| How does inbound from a transport reach the agent? | Transport plugin's WS or webhook handler → `host_route_inbound` (sync) or `host_route_inbound_spawn` (async, for HTTP webhooks) → host's principal admit → `chats.rs` → TurnExecutor. |
| Where do I add a new admin endpoint? | If host-wide: `crates/server/src/`. If plugin-scoped: `[[admin_routes]]` in `plugin.toml` + Rhai handler. |

---

## 4. Working on the codebase

### Build / test / lint

```bash
# Compile everything (use plaintext SQLite path — fast).
cargo build --workspace

# Run all tests. Default features are fine for most edits.
cargo test --workspace

# Production SQLCipher path — slow because of OpenSSL vendoring;
# use only when changing crypto / vault / migration code.
cargo test --workspace --no-default-features -F execlaw-core/sqlcipher

# Fmt + clippy on everything you touched.
cargo fmt
cargo clippy --workspace -- -D warnings

# SPA tests + type-check.
cd web && npm test && npm run lint
```

Before claiming a change works, run the relevant tests. The repo's CI
budget assumes you've already run them locally.

### Dev server

Two-terminal hot-reload workflow lives in `README.md` "Dev mode". The
short version:

```bash
# Terminal 1: Rust hot-reload (cargo-watch + cargo run).
bash scripts/dev-server.sh
# pwsh on Windows: pwsh scripts/dev-server.ps1

# Terminal 2: SPA hot-reload (Vite HMR).
cd web && npm run dev
```

Server lands on `:3031` (avoiding Docker Desktop's `:3030` squat). SPA
proxies `/api → :3031` and serves at `:5173`.

### Database / vault / state

User data lives at `~/.execlaw/`. Wiping `execlaw.db` resets all state;
wiping `master.key` is destructive (encrypted vault rows become
unreadable). Don't `rm -rf ~/.execlaw/` without explicit user
permission.

```bash
# Direct sqlite for read-only inspection (plaintext path only):
sqlite3 ~/.execlaw/execlaw.db
```

The migrations directory is append-only. To change the schema, add a
new `crates/core/migrations/00NN_<change>.sql` — never edit existing
ones.

### Plugin development workflow

When iterating on a plugin's Rhai source:

```bash
# Build a fresh ZIP from plugins/<id>/.
cd plugins/<id>
powershell -Command "Compress-Archive -Path main.rhai,plugin.toml,schemas \
  -DestinationPath ../../dist/<id>-<version>.zip -Force"
# (or `zip` on POSIX)

# Reinstall through the API. JWT lives in browser localStorage during
# dev — grab from the SPA's network panel. Or use `execlaw replay` /
# the admin CLI.
JWT="<paste from localStorage>"
curl -X DELETE "http://127.0.0.1:3031/api/admin/plugins/<id>" \
  -H "Authorization: Bearer $JWT"
curl -X POST   "http://127.0.0.1:3031/api/admin/plugins/install" \
  -H "Authorization: Bearer $JWT" \
  -F "file=@dist/<id>-<version>.zip"
curl -X POST   "http://127.0.0.1:3031/api/admin/plugins/<id>/enable" \
  -H "Authorization: Bearer $JWT"
```

Full plugin-author guide in [`docs/plugins.md`](docs/plugins.md). Read
that before designing any new plugin.

---

## 5. Code conventions

- **Rust edition 2024.** MSRV 1.85.
- **Comments explain *why*, not *what*.** The code is the *what*.
  Inline a context comment whenever a piece of logic addresses a
  specific bug or external constraint (sidecar quirk, third-party API
  field name, etc.) — see existing plugin code for examples.
- **No emoji** in code, comments, or commit messages unless explicitly
  asked. The codebase is intentionally plain-text.
- **Errors are typed.** Return `Result<T, ConcreteError>`; reach for
  `anyhow::Error` only at edge boundaries (CLI, axum route handlers).
- **`tracing::info!` / `warn!` / `error!`** for logs, never `println!`
  except in CLI tooling. Include a `plugin_id`, `conversation_id`, or
  similar key wherever the log refers to a scoped resource.
- **Public API gets doc comments.** `pub fn`, `pub struct`, modules.
  Internal helpers can skip them.
- **Tests live next to the code.** `#[cfg(test)] mod tests` at the
  bottom of the file. Cross-crate integration tests live under
  `crates/server/tests/`. Plugin end-to-end tests live there too,
  installing the real ZIP through the host.
- **No `unsafe`** unless there's a documented FFI / perf-critical
  reason. If you find yourself reaching for it, ask the user first.
- **`unwrap()` is a smell.** Permitted in tests; in production code,
  `expect("...")` with a context string or a proper `Result` return.

### Commit messages

```
<scope>: <imperative present-tense subject under ~70 chars>

Body wraps at 72 columns. Explains the why, not the what.
Reference commit hashes (8c8b31b) when relevant. Cite file paths
and line numbers in the body when the change is non-obvious.

Co-Authored-By: <agent-name> <noreply@example.com>
```

Examples in `git log`. Common scopes: `whatsapp`, `signal`, `core`,
`server`, `script`, `policy`, `plugin-host`, `docs`, `web`.

---

## 6. Common task patterns

### "Fix this bug"

1. Read the failing log / reproduction steps. Find the *exact* file
   path and line that's wrong.
2. Read enough surrounding code to understand the contract being
   broken. Don't trust the bug report's framing — verify.
3. Write the smallest correct change. Add or update a test that would
   have caught it.
4. Run `cargo test -p <affected-crate>` first, then `cargo test
   --workspace`.
5. Report the change. Don't commit unless asked.

### "Add a new tool to plugin X"

1. Add `[[tools]]` block to `plugins/X/plugin.toml`. Set `trust_floor`,
   `latency`, optional `schema` path.
2. Implement the dispatch arm in `plugins/X/main.rhai`'s `tool_call`
   function.
3. If the tool needs a JSON Schema, add `plugins/X/schemas/<tool>.json`.
4. Bump `version` in plugin.toml.
5. Build the ZIP, reinstall, exercise via the SPA or curl.

### "Add a new transport plugin"

Read `docs/plugins.md` §11 — the closest cognate is your fastest path.
Most likely you want to copy `plugins/whatsapp/` (webhook flavour) or
`plugins/signal/` (WS flavour) and adapt.

### "Update docs"

The user-facing docs are:
- `README.md` — quick start, dev mode, doc-pointers, screenshots.
- `docs/architecture.md` — what the system is.
- `docs/agent-model.md` — how a turn executes.
- `docs/plugins.md` — plugin author reference.
- `MIGRATION_PLAN.md` — design rationale.
- `AGENTS.md` — this file.

Match the existing tone (terse, technical, citation-heavy). Cite file
paths and migration numbers. Don't invent things — ground every claim
in code you've actually read.

### "Investigate something complex"

Use the Explore agent for read-only investigations. Use the Plan agent
when designing implementation strategy. The general-purpose agent is
fine for hands-on multi-step work but expensive for pure search.

When delegating, write self-contained prompts: file paths, line
numbers, the exact question. Don't push synthesis onto the sub-agent
("based on your findings, fix the bug" is bad — you should be the one
synthesising).

---

## 7. Things that look broken but aren't

A few patterns recur in this codebase that look wrong at first glance.
Don't "fix" them without asking.

- **Plugin webhooks return 200 even on token mismatch.** That's
  intentional — third-party services interpret non-200 as a delivery
  failure and retry. The handler logs and ignores; we ack 200 to make
  the retry storm stop. See `plugins/whatsapp/main.rhai`.
- **`host_route_inbound_spawn` ignores its return value.** Fire-and-
  forget by design. The synchronous variant `host_route_inbound`
  exists for WS-driven inbound where the consumer is already a
  background task.
- **Some Rhai functions look duplicated.** Rhai's module-level `const`
  is invisible inside `fn` bodies, so we inline literals at call sites
  instead of factoring them.
- **`crates/server/tests/google_*_e2e.rs` lives in host code.** That's
  correct: integration tests for the public plugin contract belong
  host-side. The audit rule (no leaks) is about *production* paths.
- **Migrations 0001–0035 plus a "30+ migrations" claim in docs.**
  Some migrations have been merged or renumbered over time; the
  current count is `ls crates/core/migrations/ | wc -l`. Trust the
  filesystem, not the doc.

---

## 8. When to push back on the user

Sometimes the user's request conflicts with the architecture. Push
back, briefly, and offer the principled alternative. Examples:

- "Add a cloud-LLM fallback when the local model is down." → No.
  Axiom #1. Suggest: scaling the local model, queueing turns, returning
  a typed error.
- "Hardcode plugin X to always run on inbound from transport Y." → No.
  Axiom #6 (plugins not built-ins). Suggest: declare the binding as a
  manifest entry and let the registry route it.
- "Skip the test for now." → If it's a real test, no. Either fix the
  test, fix the code, or document why the test was wrong.
- "Disable the HMAC chain temporarily." → No. The tamper-evidence
  property is load-bearing; if it's failing, the bug is upstream.
- "Just commit my work without me asking." → Never. Wait for explicit
  instruction.

When you push back, do it concisely. One line of refusal + one line of
the better path is usually enough.

---

## 9. End-of-task checklist

Before reporting a task done:

- [ ] All tests for affected crates pass (`cargo test -p <crate>`).
- [ ] `cargo clippy` clean on the touched files.
- [ ] `cargo fmt` applied.
- [ ] If the SPA changed: `npm test && npm run lint` in `web/`.
- [ ] If you added a public API: doc comment with at least one example.
- [ ] If you added a migration: it's the next sequential number, doesn't
      edit any existing migration, and the test suite passes.
- [ ] If a doc claim could change: `README.md` / `docs/*.md` reflects
      the new reality.
- [ ] No emoji in any new code or commit message.
- [ ] No cloud-LLM API on any code path (re-grep your diff).
- [ ] No hardcoded plugin id in `crates/` outside of tests / fixtures
      / doc-comment examples.
- [ ] You did NOT commit unless explicitly asked.

If you're confident, summarise what changed in one paragraph + cite
the affected file paths. The user will read the diff for the rest.
