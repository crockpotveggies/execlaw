# Contributing to execlaw

Thanks for considering a contribution. execlaw is a self-hosted,
single-operator agent platform; the contribution model reflects that —
small, focused changes, no marketing-shaped features, no cloud-LLM
dependencies, ever.

If you've never opened the codebase before, read these in order:

1. [`README.md`](README.md) — quick start + dev mode.
2. [`docs/architecture.md`](docs/architecture.md) — the *what*.
3. [`docs/agent-model.md`](docs/agent-model.md) — the *how* of one turn.
4. [`docs/plugins.md`](docs/plugins.md) — plugin author reference.
5. [`AGENTS.md`](AGENTS.md) — onboarding for AI coding agents
   (humans benefit from reading it too — the conventions and
   non-negotiable rules apply to everyone).

That's the canonical orientation path. Skipping step 5 will cost you
more than reading it.

---

## Ground rules

These are the same rules listed in [`AGENTS.md` §2](AGENTS.md). If you
push back on any of them, expect a re-do.

1. **No cloud LLMs.** Anthropic, OpenAI, Gemini, Mistral cloud — none of
   them, on any code path, ever. Inference happens against a local
   OpenAI-compatible endpoint.
2. **Plugins, not hardcoded built-ins.** Host crates do not match on
   plugin IDs by name in production paths. Tests and doc-comment
   examples are fine.
3. **SQLite is the source of truth.** No `.env`, no `/etc/execlaw.toml`,
   no environment-variable configuration of operator-editable values.
   Operator config lives in `config_*` tables; secrets live in the
   SQLCipher vault.
4. **Effects go through the outbox.** The LLM never makes external HTTP
   calls directly. Tool calls become outbox rows; the relay drains
   them with framework-minted idempotency keys.
5. **`tool_use` and `tool_result` always pair in the same commit.**
   Enforced by `EventLog::commit_turn::enforce_tool_pairing()`.
6. **Tests are mandatory.** Per axiom #13, every non-trivial function
   has tests; security-critical code has adversarial tests.
   `cargo test --workspace` must pass before any PR is mergeable.
7. **Performance regressions are blocked by Criterion benchmarks.** Per
   axiom #14, hot paths have explicit budgets. Don't claim a speedup
   without numbers.

---

## Workflow

The active development branch is **`foundation`**. There is no `main`
branch yet; tagged releases will branch off `foundation` once a 1.0
ships.

### Filing an issue

Use the issue templates if/when they land in `.github/`. Until then:

- **Bug report**: include OS, Rust version (`rustc --version`),
  control-plane version (commit SHA from `git rev-parse HEAD` or the
  `version` field of `cargo run -p execlaw -- --version`), the
  installed-plugin list (`curl -s http://127.0.0.1:3031/api/admin/plugins | jq`),
  the relevant log excerpt (`~/.execlaw/logs/execlaw.jsonl.<date>`),
  and reproduction steps.
- **Feature request**: describe the operator pain you're solving and
  cite the architecture-doc section you'd amend. Features that don't
  fit the design principles in [`docs/architecture.md` §2](docs/architecture.md)
  will be redirected, not rejected — tell us what you're trying to
  accomplish, not your proposed implementation.
- **Security issue**: do **not** open a public GitHub issue. See
  [`docs/security.md`](docs/security.md) for the disclosure path.

### Opening a pull request

1. Fork (or branch from `foundation` if you have push access).
2. Make focused commits — one logical change per commit, present-tense
   imperative subject under ~70 chars. Body wraps at 72 columns and
   explains the *why*, not the *what*. Cite file paths + commit SHAs
   when the change is non-obvious. Examples in `git log`.
3. Run the local test/lint/format gauntlet:
   ```bash
   cargo fmt --all
   cargo clippy --workspace --all-targets
   cargo test --workspace
   cd web && npm test && npm run lint && npm run build
   ```
4. Push your branch and open a PR against `foundation`. Title format
   is the same as commit subjects.
5. Address review comments by adding new commits (don't force-push
   over review history) and squash at merge time if the reviewer
   asks.
6. CI on every push will run the four-target build matrix
   (`.github/workflows/ci.yml`). A red CI lights blocks merge unless
   it's a known flaky test (in which case, fix the flake in the same
   PR or a follow-up).

### Commit message conventions

```
<scope>: <imperative present-tense subject under ~70 chars>

Body wraps at 72 columns. Explains the why, not the what.
Reference commit hashes (8c8b31b) when relevant. Cite file paths
and line numbers in the body when the change is non-obvious.

Co-Authored-By: <name> <email>
```

Common scopes: `whatsapp`, `signal`, `slack`, `sms-socket`, `core`,
`server`, `script`, `policy`, `plugin-host`, `runner`, `voice`,
`docs`, `web`, `ci`. New scopes are fine if they describe a coherent
slice of the system.

---

## Code conventions

Same as [`AGENTS.md` §5](AGENTS.md). The high-impact ones:

- **Rust edition 2024.** MSRV 1.85.
- **Comments explain *why*, not *what*.** Inline a context comment
  whenever a piece of logic addresses a specific bug, sidecar quirk,
  or external-API field-name detail. The codebase is full of these —
  they're the navigation system.
- **No emoji** in code, comments, or commit messages unless explicitly
  asked. The codebase is intentionally plain-text.
- **`tracing::info!` / `warn!` / `error!`** for logs, never `println!`
  except in CLI tooling. Include `plugin_id`, `conversation_id`, or
  similar scope key.
- **Tests live next to the code.** `#[cfg(test)] mod tests` at the
  bottom of the file. Cross-crate integration tests live under
  `crates/server/tests/`.
- **No `unsafe`** unless there's a documented FFI or perf-critical
  reason. Prefer `expect("...")` with a context string over `unwrap()`
  in production code.
- **Migrations are append-only.** Add a new
  `crates/core/migrations/00NN_<change>.sql` — never edit existing
  ones.

---

## Authoring a plugin

[`docs/plugins.md`](docs/plugins.md) is the full reference. The short
version:

- Most plugins use the **script tier** (`tier = "script"`, single
  `main.rhai` file). Reach for the **subprocess tier** only when you
  need a native binary (audio decoding, ONNX, native crypto).
- Closest cognate when you start a new plugin:
  - new transport with a Go/Java sidecar → copy `plugins/whatsapp/`
    (webhook flavour) or `plugins/signal/` (WS flavour).
  - new HTTP-only OAuth integration → copy `plugins/google-calendar/`.
  - new API-key HTTP integration → copy `plugins/google-places/`.
  - new identity provider → copy `plugins/google-contacts/`.
  - new subprocess plugin → copy `plugins/hello/`.
- Plugin IDs and tool names are stable contracts. Renames are breaking
  changes for operators who already have the plugin installed.
- Webhook routes (`[[webhook_routes]]`) are unauthenticated. The
  plugin handler **must** validate caller identity before doing
  anything stateful — see WhatsApp's `on_webhook_event` for the
  canonical pattern (constant-time compare against a vault-stored
  shared secret).
- Read `[docs/plugins.md` §12 "Common pitfalls"][docs/plugins.md] before
  shipping. The list is short and every entry is something the
  in-tree plugins got wrong at least once.

[docs/plugins.md]: docs/plugins.md

---

## Licensing

execlaw is licensed under the [Apache License, Version 2.0](LICENSE).
The repository's [`NOTICE`](NOTICE) file carries the project copyright.

Contributions are accepted under the same license. By submitting a
pull request you certify that:

1. You have the right to license your contribution under
   Apache-2.0 (you're the author, or your employer has signed off,
   or the upstream code you're vendoring is itself
   Apache-2.0-compatible).
2. You agree your contribution is licensed under Apache-2.0 with
   no additional restrictions.

We don't currently require a separate Contributor License Agreement
or DCO sign-off line — the act of opening a PR against this
repository is the certification. If your employer requires a CLA
arrangement before you can contribute, open an issue and we'll work
something out.

Apache-2.0 includes a patent grant (§3) and an explicit
contribution-back clause (§5). If you contribute proprietary patches
through a fork without merging upstream, those terms still apply to
the patches you publish, but Apache-2.0 imposes no
network-use-as-distribution clause — you can deploy modified versions
without disclosing source.

---

## A note on AI-assisted contributions

Using Claude Code, Cursor, Aider, or similar tools to draft PRs is
fine — the codebase has been built with significant AI assistance and
[`AGENTS.md`](AGENTS.md) is written *for* those tools. Two
expectations:

1. **You're still the author.** Read every line your tool generated.
   You're responsible for it. "The model wrote it" is not a defense
   for a bug or a license violation.
2. **Verify cited claims.** AI tools confidently cite file paths and
   line numbers that don't exist. Before submitting, grep for every
   path you cite in commit messages or doc updates and confirm it's
   real.

---

## Code of conduct

Be civil. Stay technical. Disagreement is fine; personal attacks
aren't. The maintainers reserve the right to close issues / PRs and
ban users who violate that — without warning, without appeal, without
debate. We're a small project; we don't have time for moderation
drama.
