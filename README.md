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

## Quick start

### One-shot (via cargo aliases — recommended)

```bash
cargo bootstrap          # migrate + build image + start stack
cargo ps                 # verify container is running
curl http://localhost:3030/api/health    # → {"status":"ok"}
open http://localhost:3030/api/docs      # Swagger + AsyncAPI
```

### Container lifecycle

Cargo aliases defined in `.cargo/config.toml` — each delegates to the
`execlaw` CLI, which wraps `docker compose` for you:

| Cargo command | What it does | Underlying call |
|---|---|---|
| `cargo image` | Build the control-plane docker image | `docker build -f Dockerfile.control-plane ...` |
| `cargo bootstrap` | First-run install (migrate + image + start) | `db migrate` → `build` → `up -d` |
| `cargo start` | Start the stack | `docker compose up -d` |
| `cargo restart` | Restart the stack | `docker compose restart` |
| `cargo stop` | Stop the stack | `docker compose down` |
| `cargo ps` | Show container status | `docker compose ps` |
| `cargo logs` | Tail logs (last 200 lines) | `docker compose logs --tail 200` |
| `cargo tail` | Follow logs live (`-f`) | `docker compose logs --tail 200 -f` |
| `cargo doctor` | Preflight env checks | docker + sqlcipher + keyring |

Notes:

- `cargo build` and `cargo install` retain their native Rust meanings
  (Rust compilation and binary install). The container image is
  `cargo image`; the first-run install is `cargo bootstrap`.
- All aliases forward extra args — `cargo image -- --no-cache`,
  `cargo logs -- --tail 1000`, `cargo start -- --compose-file other.yml`.
- The equivalent direct invocations are `execlaw build`, `execlaw install`,
  `execlaw start`, `execlaw restart`, `execlaw stop`, `execlaw status`,
  `execlaw logs`. Run `execlaw --help` for the full list.

### Manual (without cargo aliases)

```bash
docker compose up -d
curl http://localhost:3030/api/health
curl -X POST http://localhost:3030/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"pick-something-longer"}'
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
