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

execlaw runs as a host service on bare metal — systemd on Linux,
launchd on macOS, the Service Control Manager on Windows. Docker is
optional and only needed for managed-mode inference backends (Phase
12); the control plane itself is a single Rust binary.

### One-shot install

```bash
cargo install --path crates/cli   # or `cargo build --release` and copy the binary
execlaw install                   # migrate DB → register service → start it
curl http://127.0.0.1:3030/api/health    # → {"status":"ok"}
open http://127.0.0.1:3030/api/docs      # Swagger + AsyncAPI
```

`execlaw install` registers a per-user service by default. Add
`--system` for a system-wide install (root / Administrator). On
Windows the Service Control Manager always runs system-level, so
`--system` is implied.

### Service lifecycle

| Command | What it does |
|---|---|
| `execlaw install` | First-run: migrate + register + start |
| `execlaw service install` | Register (without starting) |
| `execlaw service start` | Start the service |
| `execlaw service restart` | Stop + start |
| `execlaw service stop` | Stop the service |
| `execlaw service status` | Print install state + per-OS log commands |
| `execlaw service uninstall` | Deregister |
| `execlaw doctor` | Preflight checks (DB, vault, optional Docker) |
| `execlaw serve` | Run in the foreground (dev / debug) |

`cargo bootstrap`, `cargo start`, `cargo stop`, `cargo restart`,
`cargo svc-status`, and `cargo doctor` are convenience aliases that
forward to the equivalent `execlaw …` invocations
(see `.cargo/config.toml`).

### Live logs

| OS | Command |
|---|---|
| Linux (user) | `journalctl --user -u execlaw -f` |
| Linux (system) | `journalctl -u execlaw -f` |
| macOS | `log stream --predicate 'process == "execlaw"'` |
| Windows | `Get-EventLog -Source execlaw -LogName Application` |

`execlaw service status` prints the right command for your platform.

### First-run setup

```bash
curl -X POST http://127.0.0.1:3030/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"pick-something-longer"}'
```

The SPA at `http://127.0.0.1:3030/` will guide you through the rest
(backend wizard, plugin install, personality, etc.).

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
| `crates/cli` | `execlaw` binary (install, service install/start/stop, doctor, serve, ...) |

## Building from source (developer)

```bash
# Plaintext SQLite path (fast; skips OpenSSL vendoring).
cargo test --workspace
cargo run -p execlaw -- doctor

# Full SQLCipher path (production build).
cargo test --workspace --no-default-features -F execlaw-core/sqlcipher
```

Requires Rust 1.85+ (edition 2024). Bare-metal targets:
`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. Service registration
on each is handled by the
[`service-manager`](https://crates.io/crates/service-manager) crate.

## License

AGPL-3.0-or-later.
