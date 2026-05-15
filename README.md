# execlaw

[![CI](https://github.com/crockpotveggies/execlaw/actions/workflows/ci.yml/badge.svg?branch=foundation)](https://github.com/crockpotveggies/execlaw/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/crockpotveggies/execlaw/branch/foundation/graph/badge.svg)](https://codecov.io/gh/crockpotveggies/execlaw)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Self-hosted Rust agent framework with persistent memory, hook-based plugins,
tools, and skills. **No cloud LLMs, ever.** All inference runs on operator
hardware.

<p align="center">
  <img src="docs/screenshots/skills-screenshot.png" alt="execlaw — Skills page" width="48%">
  <img src="docs/screenshots/deep-research-screenshot.png" alt="execlaw — Deep research session" width="48%">
</p>

## Documentation

| Doc | What it covers |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | System topology, design principles, FSM, data model, recovery, observability — the **what**. |
| [`docs/agent-model.md`](docs/agent-model.md) | TurnExecutor, memory layers, reflection loop, planner/executor split — the **how** of one turn. |
| [`docs/plugins.md`](docs/plugins.md) | Plugin manifest schema, runtime tiers, sidecar model, Rhai primitives, and a step-by-step guide for writing a custom plugin. |
| [`docs/setup-walkthroughs.md`](docs/setup-walkthroughs.md) | Operator-facing pairing flows for Signal QR, WhatsApp wuzapi, Slack OAuth, Google OAuth + API-key. |
| [`docs/setup-mac.md`](docs/setup-mac.md) | Apple Silicon first-run notes — native Ollama subprocess, model sizing, brand indicator. |
| [`desktop-macos/README.md`](desktop-macos/README.md) | macOS `.app` bundle internals — Tauri 2, SMAppService, build script. |
| [`docs/security.md`](docs/security.md) | Disclosure path, threat model, cryptography, trust assumptions, known limitations, hardening checklist. |
| [`docs/sidecar-supervisor-design.md`](docs/sidecar-supervisor-design.md) | Supervised-container layer plugins compose against. |
| [`docs/runner-design.md`](docs/runner-design.md) | Per-conversation runner container model. |
| [`docs/voice-followups.md`](docs/voice-followups.md) | Voice modality design notes. |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Workflow, code conventions, AGPL→Apache-2.0 licensing notes. |
| [`AGENTS.md`](AGENTS.md) | Onboarding for AI coding agents working on this repo. |

## What ships today

- **Control plane** (Rust binary): event log + scheduler + plugin host + container manager + outbox relay + axum server + SQLCipher vault.
- **Per-conversation runner containers**: stateless against the log, stateless OpenAI-compatible client to local inference (vLLM / OpenArc / Whisper / Kokoro).
- **Trust ladder + Rule of Two**: `Controller / Delegated / KnownTrusted / KnownLimited / UnknownPending / Blocked` with cold-contact escalation, signed approval-token JWTs, sideband HITL.
- **HMAC-chained event log**: every committed row is tamper-evident; replay rebuilds state deterministically.
- **Outbox + idempotency**: framework-minted `(conversation_id, turn_seq, tool_call_ordinal)` keys, retries with backoff, dead-letter queue.
- **Plugin framework** (12 in-tree plugins — see [Plugins shipped](#plugins-shipped)): script-tier (Rhai) + subprocess-tier (JSON-RPC), full manifest schema (tools / transports / identity providers / OAuth / sidecars / admin routes / webhook routes / UI panels / skills).
- **Five shipped transports**: Signal (signal-cli sidecar), WhatsApp (wuzapi sidecar), Slack (multi-workspace Socket Mode OAuth), Discord (multi-guild Gateway WebSocket), SMS (Android-gateway WebSocket).
- **HTTP integrations**: Google Apps (Gmail/Calendar/Contacts/Tasks/Drive in one OAuth), Google Places, Open-Meteo (key-less weather), Yahoo Finance (market data), Pushover.
- **Research subsystem**: deep-research plan/gather/synthesize pipeline with retention and per-phase event flow.
- **SPA**: chat-first sidebar, pinned Control thread (every controller-channel message collapses here), token streaming, approval queue, per-plugin admin panels, settings.
- **Native macOS app** (Apple Silicon): menu bar `.app` bundle, SMAppService-managed LaunchAgent, drag-to-Trash uninstall — see [Install on macOS](#install-on-macos-apple-silicon--menu-bar-app).

See [`docs/architecture.md` §18](docs/architecture.md) for the full milestone breakdown.

## Plugins shipped

All 12 in-tree plugins ship as ZIPs under [`dist/`](dist/) and install via the SPA's Settings → Plugins page (or `POST /api/admin/plugins/install`). Source under [`plugins/`](plugins/).

| Plugin | Version | Tier | Kind | What it does |
|---|---|---|---|---|
| [`signal`](plugins/signal/) | 0.5.0 | script | transport | Signal Messenger via a supervised [`signal-cli`](https://github.com/AsamK/signal-cli) sidecar. Inbound consumer + outbound + group ops + QR/number pairing. |
| [`whatsapp`](plugins/whatsapp/) | 0.2.0 | script | transport | WhatsApp Multi-Device via a supervised [wuzapi](https://github.com/asternic/wuzapi) (whatsmeow-backed) sidecar. QR pairing, group ops, attachments, read receipts. |
| [`slack`](plugins/slack/) | 0.3.2 | script | transport | Multi-workspace Slack via Socket Mode (no public URL). Sidecar-free — pure-Rhai over `http_post` + `ws_subscribe` + `ws_send`. |
| [`discord`](plugins/discord/) | 0.2.0 | script | transport | Discord bot via the Gateway WebSocket. Multi-guild from one bot token, sidecar-free, gateway heartbeats over `ws_set_keepalive`. |
| [`sms-socket`](plugins/sms-socket/) | 0.2.0 | script | transport | SMS / MMS via the [Android SMS Socket app](https://github.com/crockpotveggies/sms-socket-app) — WebSocket to the operator's phone on LAN. |
| [`google-apps`](plugins/google-apps/) | 0.3.0 | script | integration + identity | Gmail + Calendar + Contacts + Tasks + Drive in one OAuth grant. Per-module toggle. Identity provider for email/phone via the People API. |
| [`google-places`](plugins/google-places/) | 0.2.0 | script | integration | Google Places (New) API — text search, nearby search, place details. API-key only, no OAuth. |
| [`open-meteo`](plugins/open-meteo/) | 0.4.0 | script | integration | Key-less weather, marine, air-quality, seasonal, ensemble, flood, climate, geocoding, elevation via the public [Open-Meteo](https://open-meteo.com/) APIs. |
| [`finance-yahoo`](plugins/finance-yahoo/) | 0.1.0 | script | integration | Real-time + historical market data via Yahoo Finance's public quote / chart endpoints. No API key. |
| [`pushover`](plugins/pushover/) | 0.2.0 | script | notifier | One-way [Pushover](https://pushover.net/) push notifications to the operator's phone. |
| [`identity-local-address-book`](plugins/identity-local-address-book/) | 0.1.0 | subprocess | identity | Local JSON contact list at `~/.execlaw/contacts.json` — auto-trusts saved contacts as `KnownTrusted`. |
| [`hello`](plugins/hello/) | 0.1.0 | subprocess | reference | Echo tool exercising the subprocess JSON-RPC tier. Template for new plugin authors. |

Tools, host-side built-ins, and the manifest schema are documented in [`docs/plugins.md`](docs/plugins.md). Chart rendering (`chart.render`) is a host-side built-in as of 2026-05-15 — it was previously inside `open-meteo`.

---

## Minimum requirements

execlaw is **self-hosted by design** — there is no SaaS tier, no cloud
fallback, and no plan for one. Inference happens on the operator's own
hardware against a local OpenAI-compatible endpoint. The hardware
floor is set by the LLM you choose to run, not by execlaw itself.

### Operating system

| Platform | Status | Recommended install | Service backend |
|---|---|---|---|
| Linux x86_64 (Ubuntu 22.04+, Debian 12+, Fedora 39+, Arch, …) | Supported | `execlaw install` (CLI) | systemd |
| macOS arm64 (Apple Silicon, M1+) | Supported | **`execlaw.app` menu bar bundle** | launchd via SMAppService |
| macOS x86_64 (Intel) | Supported | `execlaw install` (CLI) | launchd |
| Windows 10 / 11 (x86_64, MSVC toolchain) | Supported | `execlaw install` (CLI) | Service Control Manager |

The CLI path uses the [`service-manager`](https://crates.io/crates/service-manager) crate. On Apple Silicon the recommended path is the [menu bar `.app`](#install-on-macos-apple-silicon--menu-bar-app), which registers the LaunchAgent via Apple's `SMAppService` API so dragging the app to Trash automatically cleans up the service. The CLI install still works for headless Macs.

### GPU / inference acceleration

You need a GPU capable of running the LLM you intend to use. The
in-tree default is **Qwen3.5-27B-AWQ** (~14 GB VRAM for weights + a
working KV cache budget for ~8K-token contexts). Two acceleration paths
are supported out-of-the-box:

| Path | Hardware | Backend container | Typical floor |
|---|---|---|---|
| **NVIDIA CUDA** | RTX 30-series or newer with **≥16 GB VRAM** | `service-vllm` (vLLM) | RTX 4090 / 3090 / A4000 |
| **Intel Arc / Xeon** | Arc A770 / B580, Battlemage, Xeon w/ AMX | `service-openarc` (OpenVINO) | Arc A770 16 GB |

CPU-only inference is technically possible via llama.cpp or similar
sidecars, but at 27B-AWQ the latency makes the agent loop unusable.
Smaller models (Qwen2.5-7B-AWQ at ~5 GB VRAM) work on consumer 8 GB
cards if you accept the quality drop — operators swap the model spec
in Settings → Backends.

The voice subsystem (Whisper STT, Kokoro TTS) runs alongside the LLM —
add ~1-2 GB VRAM headroom if you want both on the same card. Operators
with a second GPU (typical Intel-Arc-for-voice + NVIDIA-for-LLM split)
can pin each backend per-card via Settings → Runners.

### Memory + disk

| Resource | Floor | Comfortable |
|---|---|---|
| System RAM | 16 GB | 32 GB |
| Free disk for `~/.execlaw/` | 2 GB | 10 GB (DB + log retention + plugin sidecar volumes) |
| Free disk for Docker images | 30 GB | 80 GB+ (LLM weights dominate; vLLM + Whisper + Kokoro + plugin sidecars) |

### Required runtime dependencies

- **[Docker](https://docs.docker.com/engine/install/)** — required for
  per-conversation runner containers, plugin sidecars (signal-cli,
  wuzapi, …), and managed-mode inference backends. The control plane
  talks to the local Docker daemon via the standard socket
  (`/var/run/docker.sock` on Linux/macOS, `\\.\pipe\docker_engine` on
  Windows). Docker Desktop is fine on macOS/Windows; Docker Engine or
  Podman-with-the-docker-socket-shim works on Linux. **Without
  Docker the agent loop runs text-only with the runner in-process;
  sidecars and managed inference are unavailable** — usable for plain
  chat but not for the bridged-transport plugins.
  *Apple Silicon exception:* Docker Desktop on a Mac runs Linux in a
  microVM with no Metal access, so containerised inference on M-series
  GPUs falls back to CPU and is unusable. execlaw spawns **Ollama as
  a native subprocess** on Apple Silicon instead — see
  [`docs/setup-mac.md`](docs/setup-mac.md). Docker is still needed for
  the bridged-transport sidecars (signal-cli, wuzapi).
- **An NVIDIA or Intel GPU driver stack** matching the inference path
  you choose — CUDA 12+ runtime for NVIDIA, the OpenVINO drivers for
  Intel. Both are normally installed alongside the GPU; `execlaw doctor`
  prints what's missing.
- **An OS keyring backend** for vault master-key storage — Keychain
  on macOS, Credential Manager on Windows, Secret Service / KWallet
  on Linux. The vault falls back to `~/.execlaw/master.key` if the
  keyring is unavailable; the file fallback is also the durable sink
  on Windows where Credential Manager has documented drift issues
  (see [`docs/security.md`](docs/security.md) §5).

### Build-from-source dependencies

Only required if you're compiling rather than installing a release
binary:

- **Rust 1.85+** (edition 2024). MSRV documented at the workspace root;
  CI runs against current stable.
- **Node.js 20+** for the SPA build (`web/`).
- **A C toolchain** for the SQLite bundling: `gcc`/`clang` on
  Linux/macOS, MSVC on Windows.
- **Strawberry Perl 5.32+** on Windows *only* if you build the
  production `sqlcipher` feature (vendored OpenSSL needs Perl). Not
  required for default `bundled-sqlite-plain` dev builds.

`execlaw doctor` runs preflight checks for all of the above and prints
remediation pointers per platform.

---

## Quick start (production)

execlaw's control plane runs as a host service on bare metal —
systemd on Linux, launchd on macOS, the Service Control Manager on
Windows. The control plane itself is a single native binary; Docker
is required only for the things the control plane spawns *out* (per-
conversation runner containers, plugin sidecars like signal-cli /
wuzapi, managed-mode inference backends). On a host without Docker
the agent loop still works text-only with the runner running
in-process; sidecars and managed inference are unavailable.

### One-shot install

```bash
cargo install --path crates/cli   # or `cargo build --release` and copy the binary
execlaw install                   # migrate DB → register service → start it
curl http://127.0.0.1:3031/api/health    # → {"status":"ok"}
open  http://127.0.0.1:3031/api/docs     # Swagger + AsyncAPI
```

`execlaw install` registers a per-user service by default. Add
`--system` for a system-wide install (root / Administrator). On
Windows the Service Control Manager always runs system-level, so
`--system` is implied.

### Install on macOS (Apple Silicon) — menu bar app

For desktop Macs the recommended path is the menu bar `.app`. It
bundles the same server binary as the CLI install above, but
registers the LaunchAgent through Apple's modern `SMAppService`
API — which means **dragging the `.app` to the Trash
automatically removes the background service.** No leftover plist
in `~/Library/LaunchAgents/`, no manual cleanup.

1. Download the latest `execlaw_<version>_aarch64.dmg` from
   [Releases](https://github.com/justinelgenlong/execlaw/releases).
2. Open the `.dmg` and drag **execlaw** to `/Applications`.
3. **First launch** — because the build is unsigned, Gatekeeper
   refuses a double-click. Right-click execlaw → *Open* → confirm
   in the dialog. macOS remembers the exception for subsequent
   launches.
4. The menu bar icon appears. The first time, macOS may surface
   *Background Items Added* — that's `SMAppService` registering
   the LaunchAgent. Approve in *System Settings → General →
   Login Items & Extensions → Allow in Background* if prompted
   (the tray's status row links you there).
5. Click the menu bar icon → *Open execlaw*. The SPA loads from
   the local server on `127.0.0.1:3031` and walks you through
   first-run setup.

The menu bar also exposes *Restart service*, *Open data folder*,
and *Uninstall execlaw…* (the latter deregisters the LaunchAgent
and optionally wipes `~/.execlaw/` before you drag the `.app` to
Trash).

**Build it yourself** — on a macOS 13+ host with Xcode CLT, Rust,
Node 20, and `cargo install tauri-cli --version "^2.0"`:

```bash
./scripts/build-mac.sh
# .app → desktop-macos/src-tauri/target/aarch64-apple-darwin/release/bundle/macos/
# .dmg → desktop-macos/src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/
```

The .dmg opens to a Finder window showing the `execlaw` app on
the left and an `Applications` symlink on the right — drag one
to the other to install.

See [desktop-macos/README.md](desktop-macos/README.md) for build
details, [Phase 6d](docs/architecture.md) for the broader desktop
wrapper design, and [CONTRIBUTING.md → Cutting a release](CONTRIBUTING.md)
for the tag → GitHub Release flow.

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
curl -X POST http://127.0.0.1:3031/api/setup \
  -H 'content-type: application/json' \
  -d '{"admin_password":"pick-something-longer"}'
```

The SPA at `http://127.0.0.1:3031/` will guide you through the rest
(backend wizard, plugin install, personality, etc.).

---

## Dev mode (hot-reload, full stack)

Two long-running processes give you a restart-free edit cycle for both
the Rust server and the SPA.

### One-time setup

```bash
# Rust file-watcher.
cargo install cargo-watch --locked

# SPA dependencies.
cd web && npm install
```

### Run both terminals

```bash
# Terminal 1 — Rust hot-reload. cargo-watch rebuilds + restarts the
# binary on every .rs save. Wraps `cargo run -p execlaw -- serve`.
bash scripts/dev-server.sh         # POSIX / WSL / Git Bash on Windows
# or:
pwsh scripts/dev-server.ps1        # Windows PowerShell
# or, from inside web/:
cd web && npm run dev:server       # alias for the bash script

# Terminal 2 — SPA hot-reload. Vite HMR; proxies /api → :3031.
cd web && npm run dev
```

Open <http://127.0.0.1:5173/> — the SPA hits the Vite dev server, which
proxies API calls to the cargo-watch'd Rust binary on `:3031`. Editing
a `.tsx` file triggers a Vite HMR push; editing a `.rs` file triggers
a `cargo build` + binary restart and the next API call hits the new
code (typically <5s for incremental edits).

The dev server, the installed production service, and the Vite proxy
all default to `127.0.0.1:3031` — there's no port-swizzling between
modes. Override for one-off testing:

```bash
EXECLAW_DEV_BIND=127.0.0.1:9000 bash scripts/dev-server.sh
VITE_API_TARGET=http://127.0.0.1:9000 npm run dev
```

### Useful npm scripts (in `web/`)

| Script | What it does |
|---|---|
| `npm run dev` | Vite dev server with HMR on `:5173`. Proxies `/api → :3031`. |
| `npm run dev:server` | Forwards to `bash ../scripts/dev-server.sh` so you can launch the Rust server from inside `web/`. |
| `npm run build` | Production SPA bundle (`web/dist/`). |
| `npm run preview` | Serve the built bundle locally. |
| `npm test` / `npm run test:watch` | Vitest. |
| `npm run lint` | `tsc --noEmit`. |
| `npm run size` | Print bundle-size budget snapshot. |

### Rust dev cheatsheet

```bash
# Plaintext SQLite path (fast; skips OpenSSL vendoring).
cargo test --workspace
cargo run -p execlaw -- doctor

# Full SQLCipher path (production build).
cargo test --workspace --no-default-features -F execlaw-core/sqlcipher

# Replay a turn — reconstructs the exact prompt, capability set,
# policy decision, and committed events for one conversation/seq.
cargo run -p execlaw -- replay <conversation_id> --at <seq>
```

Requires Rust 1.85+ (edition 2024). Bare-metal targets:
`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`. Service registration
on each is handled by the
[`service-manager`](https://crates.io/crates/service-manager) crate.

### Disk-space note

The Rust workspace's `target/` directory grows quickly (40+ GB on a
warm dev box). If `cargo-watch` rebuilds start failing with
`No space left on device`, run `cargo clean` to reclaim.

---

## Workspace layout

| Path | Purpose |
|---|---|
| `crates/core/` | Event log, FSM, migrations (flattened baseline + incremental), SQLCipher-encrypted storage, principal store, memory lifecycle. |
| `crates/session/` | Per-conversation pipeline composition (text vs voice). |
| `crates/inference-api/` | OpenAI-compatible LLM client. **No cloud SDKs.** |
| `crates/model-adapter/` | Provider-specific prompt + tool-call shape adapters (Qwen, Llama, OpenAI-compatible variants). |
| `crates/runner-local/` | TurnExecutor — full tool-loop turn path. |
| `crates/runner-protocol/` | Wire types for the per-conversation runner-container RPC. |
| `crates/runner-binary/` | Static-musl `execlaw-runner` binary baked into `Dockerfile.runner`. |
| `crates/voice-pipeline/` | STT → LLM → TTS two-lane Tokio graph. |
| `crates/plugin-sdk/` | `plugin.toml` manifest parser + ZIP staging. |
| `crates/plugin-host/` | Plugin registry + lifecycle (install / enable / disable / hydrate / purge). |
| `crates/script/` | Embedded Rhai engine + primitive bindings (HTTP, sidecar, vault, OAuth, WS, routing, JSON, time). |
| `crates/skills/` | Skills runtime (capture, retrieve, surface in prompt). |
| `crates/charting/` | Server-side chart rendering for the `chart.render` host built-in. |
| `crates/container-manager/` | bollard client + tiered hardware detection. |
| `crates/policy/` | Rule of Two, capability tokens, input guards, spotlighting. |
| `crates/vault/` | OS-keyring master key + Argon2id admin password. |
| `crates/transport-api/` | Trait a transport plugin implements. |
| `crates/identity-api/` | Trait an identity-provider plugin implements. |
| `crates/outbox/` | Outbox relay primitives (idempotency, retry, dead-letter). |
| `crates/server/` | Axum HTTP + WebSocket surface, sidecar supervisor, admin/webhook routers, chat path, SPA-embed via `rust-embed`. |
| `crates/mcp-client/` | MCP server registration + tool dispatch (alternative to plugin tools). |
| `crates/cli/` | `execlaw` binary (install, service, doctor, serve, replay, eval, …). |
| `crates/eval-harness/` | LLM-judge harness against local Qwen. |
| `plugins/` | In-tree reference + first-party plugins (see [Plugins shipped](#plugins-shipped)). |
| `web/` | React + react-bootstrap SPA. Vite + Vitest. |
| `desktop-macos/` | Tauri 2 menu bar app for Apple Silicon. SMAppService LaunchAgent + WebView. Out-of-workspace cargo crate. |
| `scripts/` | `dev-server.{sh,ps1}` (cargo-watch wrappers), `build-mac.sh` (Tauri release), `trace-turn.{sh,ps1}` (turn replay). |
| `docs/` | Architecture + agent-model + plugins + setup walkthroughs + screenshots. |
| `evals/` | Rubric TOML files for the LLM-judge harness. |
| `spec/` | OpenAPI + AsyncAPI specs. |
| `dist/` | Built plugin install ZIPs (one per plugin / version). |
| `.github/workflows/` | CI (per-push), `macos-bundle.yml` (tag-driven `.app` + `.dmg` → GitHub Releases). |

## License

Apache License, Version 2.0 — see [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE).

Copyright (c) 2026 Justin Long.
