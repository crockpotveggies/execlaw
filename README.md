# execlaw

Self-hosted Rust agent framework with persistent memory, hook-based plugins,
tools, and skills. **No cloud LLMs, ever.** All inference runs on operator
hardware.

execlaw is a from-scratch rebuild of
[`selfhosted-claw`](https://github.com/justinelgenlong/selfhosted-claw),
driven by a Rust control plane, a WordPress-style plugin framework, a
unified container manager, a chat-first UI, and a participant-aware
agent model.

## Source of truth

- **[`MIGRATION_PLAN.md`](MIGRATION_PLAN.md)** — the target architecture
  and phased plan. Every design decision in the codebase cites a section
  there. §0 "Grounding Principles" is the non-negotiable list: axiom #1
  (no cloud LLMs), axiom #11 (container deployment), and axiom #12
  (minimal container images) are absolute.
- **[`STATUS.md`](STATUS.md)** — live progress log. Read this first if
  you want to see what works today.

## Quick start (when ready)

```bash
# Build and start the control-plane container.
docker compose up -d

# Liveness probe.
curl http://localhost:3030/api/health   # → {"status":"ok"}

# First-run setup (admin password).
curl -X POST http://localhost:3030/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"pick-something-longer"}'

# Browse the API docs (Swagger + AsyncAPI).
open http://localhost:3030/api/docs
```

## Workspace layout

See [`MIGRATION_PLAN.md` §3.1](MIGRATION_PLAN.md) for the full rationale.

| Crate | Purpose |
|---|---|
| `crates/core` | Event log, FSM, migrations, SQLCipher-encrypted storage |
| `crates/session` | Per-conversation pipeline composition (text vs voice) |
| `crates/inference-api` | OpenAI-compatible LLM client. **No cloud SDKs.** |
| `crates/runner-local` | The one runner; Phase 1 fills in the agent loop |
| `crates/voice-pipeline` | STT → LLM → TTS two-lane Tokio graph |
| `crates/plugin-sdk` | `plugin.toml` manifest parser + ZIP staging |
| `crates/plugin-host` | Plugin registry + lifecycle |
| `crates/container-manager` | bollard client + tiered hardware detection |
| `crates/policy` | Rule of Two, capability tokens, input guards |
| `crates/vault` | OS-keyring master key + Argon2id admin password |
| `crates/transport-api` | Trait a transport plugin implements |
| `crates/identity-api` | Trait an identity-provider plugin implements |
| `crates/outbox` | Outbox relay primitives |
| `crates/server` | Axum HTTP + WebSocket surface |
| `crates/cli` | `execlaw` binary (up, doctor, db migrate, serve, ...) |

## Building from source (developer)

```bash
# Plaintext SQLite path (fast; skips OpenSSL vendoring).
cargo test --workspace
cargo run -p execlaw -- doctor

# Full SQLCipher path (production build — the Docker image does this).
cargo test --workspace --no-default-features -F execlaw-core/sqlcipher
```

Requires Rust 1.85+ (edition 2024). The production target is
`x86_64-unknown-linux-gnu` inside the container image — see
`Dockerfile.control-plane`.

## License

AGPL-3.0-or-later.
