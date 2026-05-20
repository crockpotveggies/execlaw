# execlaw

[![CI](https://github.com/crockpotveggies/execlaw/actions/workflows/ci.yml/badge.svg?branch=foundation)](https://github.com/crockpotveggies/execlaw/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/crockpotveggies/execlaw/branch/foundation/graph/badge.svg)](https://codecov.io/gh/crockpotveggies/execlaw)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Self-hosted Rust agent framework with persistent memory, hook-based plugins,
tools, and skills. **Bare metal best metal.** All inference runs on operator
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
| [`docs/desktop-installations.md`](docs/desktop-installations.md) | Cross-OS reference for the three desktop bundles — `.app`/`.dmg`, NSIS `.exe`, `.deb`. Tray architecture, service-manager mapping, install + uninstall flows, build scripts. |
| [`docs/ollama.md`](docs/ollama.md) | Pre-installed Ollama support across macOS / Linux / Windows. How discovery works, when to pick Ollama over Docker, the wizard's serving dropdown. |
| [`docs/setup-mac.md`](docs/setup-mac.md) | Apple Silicon first-run notes — native Ollama subprocess, model sizing, brand indicator. |
| [`desktop-macos/README.md`](desktop-macos/README.md) | macOS `.app` bundle internals — Tauri 2, SMAppService, build script. |
| [`desktop-windows/README.md`](desktop-windows/README.md) | Windows NSIS `.exe` bundle internals — Tauri 2, SCM service, build script. |
| [`desktop-linux/README.md`](desktop-linux/README.md) | Linux `.deb` bundle internals — Tauri 2, systemd `--user` unit, build script. |
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
- **Native desktop bundles** for all three desktops — `.app`/`.dmg` on macOS (Apple Silicon, SMAppService LaunchAgent), NSIS `.exe` on Windows (SCM service), `.deb` on Linux (systemd `--user` unit). Each ships a tray icon, the bundled control plane, and the same SPA on `127.0.0.1:3031`. See [Desktop installations](#desktop-installations) below and [`docs/desktop-installations.md`](docs/desktop-installations.md) for the full cross-OS reference.

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
| Linux x86_64 (Ubuntu 22.04+, Debian 12+, Mint 21+, Pop_OS! 22.04+) | Supported | **`execlaw_<v>_amd64.deb`** (Debian-family desktop) or `execlaw install` (CLI / non-Debian) | systemd `--user` (`.deb`) / systemd (CLI) |
| macOS arm64 (Apple Silicon, M1+) | Supported | **`execlaw.app` menu bar bundle** | launchd via SMAppService |
| macOS x86_64 (Intel) | Supported | `execlaw install` (CLI) | launchd |
| Windows 10 / 11 (x86_64, MSVC toolchain) | Supported | **`execlaw_<v>_x64-setup.exe`** (NSIS) or `execlaw install` (CLI / headless) | Service Control Manager |

The CLI path uses the [`service-manager`](https://crates.io/crates/service-manager) crate. For desktop installs the recommended path is the OS-native bundle — `.app` on Apple Silicon, NSIS `.exe` on Windows, `.deb` on Debian-family Linux — each registers the background service through that OS's native API (`SMAppService` / SCM / systemd `--user`) so install + uninstall stay self-contained. See [Desktop installations](#desktop-installations). CLI install still works on headless servers (and is the only path on non-Debian Linux and Intel Macs).

### GPU / inference acceleration

You need a GPU capable of running the LLM you intend to use. The
in-tree default is **Qwen3.5-27B-AWQ** (~14 GB VRAM for weights + a
working KV cache budget for ~8K-token contexts). Two acceleration paths
are supported out-of-the-box:

| Path | Hardware | Backend | Typical floor |
|---|---|---|---|
| **NVIDIA CUDA** | RTX 30-series or newer with **≥16 GB VRAM** | `service-vllm` (vLLM, Docker) or native Ollama | RTX 4090 / 3090 / A4000 |
| **Intel Arc / Xeon** | Arc A770 / B580, Battlemage, Xeon w/ AMX | `service-openarc` (OpenVINO, Docker) or native Ollama | Arc A770 16 GB |
| **Apple Silicon** | M1 / M2 / M3 / M4 with 16+ GB unified memory | native Ollama subprocess (Metal) | M2 / M3 base 16 GB |

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
  *Cross-OS Ollama support:* the native-subprocess path also works on
  Linux and Windows when `ollama` is installed on the host. The setup
  wizard discovers it automatically and offers it as an alternative
  serving method alongside vLLM / OpenVINO. See
  [`docs/ollama.md`](docs/ollama.md) for when to pick which.
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

### Desktop installations

For desktop hosts the recommended path is the OS-native bundle.
Each one ships a tray / menu-bar icon plus the same bundled control
plane, and each registers the background service through that OS's
native API so install + uninstall stay self-contained. Full
cross-OS reference: [`docs/desktop-installations.md`](docs/desktop-installations.md).

#### macOS (Apple Silicon) — menu bar `.app`

Registers a LaunchAgent through Apple's modern `SMAppService` API,
so **dragging the `.app` to the Trash automatically removes the
background service** — no leftover plist in `~/Library/LaunchAgents/`.

1. Download `execlaw_<version>_aarch64.dmg` from
   [Releases](https://github.com/justinelgenlong/execlaw/releases).
2. Open `.dmg` → drag **execlaw** to `/Applications`.
3. First launch: right-click execlaw → *Open* (the build is
   unsigned, so a plain double-click hits Gatekeeper). macOS
   remembers the exception.
4. macOS surfaces *Background Items Added* the first time —
   that's `SMAppService` registering the LaunchAgent. Approve in
   *System Settings → General → Login Items & Extensions* if
   prompted (the tray's status row links you there).
5. Menu bar icon → *Open execlaw* → SPA loads on
   `http://127.0.0.1:3031/`. First-run wizard takes it from there.

The menu bar also exposes *Restart service*, *Open data folder*,
*View logs (log stream)…*, and *Uninstall execlaw…* (the latter
deregisters the LaunchAgent and optionally wipes `~/.execlaw/`
before you drag the `.app` to Trash).

#### Windows 10 / 11 — NSIS `.exe` installer

Registers a Service Control Manager service running as
`LocalSystem` so the control plane starts at boot.

1. Download `execlaw_<version>_x64-setup.exe` from
   [Releases](https://github.com/justinelgenlong/execlaw/releases).
2. Run the installer; UAC fires (the SCM service install needs
   admin). NSIS's post-install hook calls
   `execlaw.exe service install --system` + `service start --system`.
3. SmartScreen warns "Windows protected your PC" on first run
   (unsigned installer) → *More info → Run anyway*.
4. Notification-area icon appears. *Open execlaw* opens a WebView2
   window on `http://127.0.0.1:3031/`.

Uninstall via *Settings → Apps → execlaw → Uninstall* (NSIS's
pre-uninstall hook stops + deregisters the service) or from the
tray's *Uninstall execlaw…* (UAC → `service uninstall`).

#### Linux (Debian / Ubuntu / Mint / Pop_OS!) — `.deb`

Registers a `systemd --user` unit on first tray-app launch. **No
service registration happens at `apt install` time** — apt's
`postinst` runs as root, but `systemd --user` units must live in
the operator's HOME to start under their UID.

1. Download `execlaw_<version>_amd64.deb` from
   [Releases](https://github.com/justinelgenlong/execlaw/releases).
2. `sudo apt install ./execlaw_<version>_amd64.deb`.
3. Launch `execlaw-tray` from the application menu (or
   `/usr/bin/execlaw-tray` from a shell). The tray calls
   `execlaw service install --user` then `service start --user`.
4. SNI tray icon appears (Just Works on KDE Plasma, XFCE, MATE,
   Cinnamon, elementary OS; vanilla GNOME needs the *AppIndicator
   and KStatusNotifierItem Support* extension — Ubuntu bundles it
   since 22.04).
5. *Open execlaw* → webkit2gtk-4.1 window on
   `http://127.0.0.1:3031/`.

For boot-time start without an interactive login, run
`loginctl enable-linger $USER` once. To uninstall cleanly: tray
*Uninstall execlaw…* first (deregisters the user unit), then
`sudo apt remove execlaw` for the program files.

#### Building from source

| OS | Command | Toolchain ref |
|---|---|---|
| macOS | `./scripts/build-mac.sh` | [`desktop-macos/README.md`](desktop-macos/README.md) |
| Windows | `./scripts/build-windows.ps1` | [`desktop-windows/README.md`](desktop-windows/README.md) |
| Linux | `./scripts/build-linux.sh` | [`desktop-linux/README.md`](desktop-linux/README.md) |

See [`docs/desktop-installations.md`](docs/desktop-installations.md)
for the cross-OS architecture reference,
[`docs/architecture.md`](docs/architecture.md) for the broader
desktop-wrapper design, and
[`CONTRIBUTING.md` → Cutting a release](CONTRIBUTING.md) for the
tag → GitHub Release flow.

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
`aarch64-apple-darwin`. Intel Macs (`x86_64-apple-darwin`) are
explicitly **not** supported — the only macOS-specific code path
that matters is Metal-accelerated inference via Ollama, which lives
on Apple Silicon. Service registration on each supported target is
handled by the
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
| `desktop-macos/` | Tauri 2 menu bar app for Apple Silicon. SMAppService LaunchAgent + WKWebView. Out-of-workspace cargo crate. |
| `desktop-windows/` | Tauri 2 tray app for x86_64 Windows. NSIS installer + SCM service + WebView2. Out-of-workspace cargo crate. |
| `desktop-linux/` | Tauri 2 tray app for x86_64 Debian-family Linux. `.deb` installer + systemd `--user` unit + webkit2gtk-4.1. Out-of-workspace cargo crate. |
| `scripts/` | `dev-server.{sh,ps1}` (cargo-watch wrappers), `build-mac.sh` / `build-windows.ps1` / `build-linux.sh` (Tauri releases), `trace-turn.{sh,ps1}` (turn replay). |
| `docs/` | Architecture + agent-model + plugins + setup walkthroughs + desktop-installations + ollama + screenshots. |
| `evals/` | Rubric TOML files for the LLM-judge harness. |
| `spec/` | OpenAPI + AsyncAPI specs. |
| `dist/` | Built plugin install ZIPs (one per plugin / version). |
| `.github/workflows/` | CI (per-push), `macos-bundle.yml` / `windows-bundle.yml` / `linux-bundle.yml` (tag-driven `.app`+`.dmg` / NSIS `.exe` / `.deb` → GitHub Releases). |

## License

Apache License, Version 2.0 — see [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE).

Copyright (c) 2026 Justin Long.
