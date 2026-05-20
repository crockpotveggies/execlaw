# Pre-installed Ollama support

execlaw will use a host-native [Ollama](https://ollama.com/) install as
a managed inference backend on **every** supported OS — not just
Apple Silicon. If you already have `ollama` on your PATH, the setup
wizard discovers it and lets you pick it from the serving-method
dropdown alongside the Docker-backed engines (vLLM, OpenVINO,
OpenArc).

This page covers what "Ollama is detected" actually means, how the
discovery probe works, and when you'd want to pick the native
subprocess over a Docker container.

For Apple-Silicon-specific notes (model sizing, unified-memory
budget, the `iogpu.wired_limit_mb` math), see
[`docs/setup-mac.md`](setup-mac.md) — that doc is still the
authoritative reference for first-run setup on a Mac.

## Where Ollama fits in the inference matrix

execlaw's control plane supervises inference backends through one of
two `ServiceController` implementations:

- **`BollardServiceController`** — talks to the Docker daemon via
  `bollard`. Used for vLLM, OpenArc, Whisper, Kokoro, and every other
  containerized backend.
- **`NativeServiceController`** — supervises a host-native subprocess
  directly via `tokio::process`. Same lifecycle contract (spawn,
  health-probe, restart-backoff, graceful-shutdown) as the Docker
  path, just without bollard in the middle.

A backend preset's `model_spec_json.runtime` field picks which one
spawns it: `"docker"` (default) or `"native"`. The native path
additionally sets `binary_hint` to select the discoverer —
`"ollama"` is the only hint shipped in v1; `"llama-server"` and
`"mlx"` slot in by adding match arms in `discover_for_hint`.

Per-host:

| Host class | Default serving choice | Ollama is offered as |
|---|---|---|
| macOS + Apple Silicon | **Ollama (native)** — the only option | The only managed inference path (Docker Desktop has no Metal passthrough) |
| Linux + NVIDIA | vLLM (Docker) | Alternative — surfaced in the dropdown when `ollama` is on PATH |
| Linux + Intel Arc | OpenArc / OpenVINO (Docker) | Alternative — surfaced in the dropdown when `ollama` is on PATH |
| Windows + NVIDIA | vLLM (Docker Desktop) | Alternative — surfaced in the dropdown when `ollama` is on PATH |
| Any host, GPU-less | vLLM-CPU (Docker) | Alternative — surfaced in the dropdown when `ollama` is on PATH |

The serving-method dropdown logic lives in
[`web/src/settings/UnifiedBackendForm.tsx::servingMethodsFor`](../web/src/settings/UnifiedBackendForm.tsx).
Apple GPUs always get `["ollama"]`; NVIDIA / Intel get the vendor's
Docker engines, with `"ollama"` appended when the host has the
binary installed.

## When to pick Ollama over Docker

On NVIDIA + Intel + CPU-only hosts the Docker-backed engines (vLLM,
OpenVINO, OpenArc) are the default because they're the
production-grade path — vLLM in particular is what selfhosted
production deployments run. Ollama makes sense as the serving choice
when:

- **You already have Ollama installed and a model cache populated.**
  No reason to re-download weights into the HuggingFace cache the
  vLLM container reads from.
- **Docker is misbehaving.** Docker Desktop hung on a Windows host,
  `nvidia-container-toolkit` isn't installed on a Linux box, WSL2
  GPU passthrough isn't cooperating. Ollama's native runtime
  side-steps every container-runtime issue.
- **You're on a small consumer GPU** (8 GB or less) where vLLM's
  KV-cache budget eats too much VRAM and a Q4-quantized GGUF
  through Ollama runs the same model with less memory pressure.
- **You want the install to be self-contained without Docker.**
  Ollama is a single binary; vLLM is a 7 GB container image.

On Apple Silicon, you don't pick — Ollama is the only managed path.
See [`docs/setup-mac.md`](setup-mac.md) for the full reasoning
(Metal passthrough doesn't exist in Docker Desktop's microVM).

## Discovery — how execlaw finds your Ollama install

The control plane's discovery probe lives in
[`crates/container-manager/src/service.rs::discover_ollama`](../crates/container-manager/src/service.rs).
It runs **on every host OS** and resolves in this order, first match
wins:

1. **`$OLLAMA_BINARY`** — operator override. Useful when you have
   multiple Ollama installs or the binary lives somewhere
   non-standard. If the variable is set but the file doesn't exist,
   the probe surfaces an actionable error verbatim (the wizard
   prints the path you gave it).
2. **`PATH` lookup** — `ollama` on Linux/macOS, `ollama.exe` on
   Windows. Same algorithm the shell uses.
3. **Well-known install locations**, in order:
   - `/opt/homebrew/bin/ollama` — Apple Silicon Homebrew prefix.
     (Intel-Mac brew at `/usr/local/bin/` is covered by the next
     entry.)
   - `/usr/local/bin/ollama` — the official Linux installer
     (`curl https://ollama.com/install.sh | sh`) writes here.
   - `/usr/bin/ollama` — distro packages on Debian/Ubuntu/Arch/
     Fedora drop here.
   - `C:\Users\Default\AppData\Local\Programs\Ollama\ollama.exe`
     — `winget install Ollama.Ollama` and the standalone `.exe`
     installer from ollama.com both write to the default profile.
   - `%USERPROFILE%\AppData\Local\Programs\Ollama\ollama.exe` —
     per-user Windows install fallback.

If none of those resolve, the probe returns a per-OS install hint:

| Host OS | Hint surfaced in the wizard |
|---|---|
| macOS | `install with brew install ollama` |
| Windows | `install with winget install Ollama.Ollama` (or download the installer from https://ollama.com/download/windows) |
| Linux | `install with curl https://ollama.com/install.sh \| sh` (or your distro's package manager) |

The wizard renders this in the `OllamaPreflightPanel` component
(`web/src/settings/UnifiedBackendForm.tsx`), with a *Recheck* button
that re-runs the probe after the operator installs.

## How the wizard surfaces it

In **Settings → Backends → Add/Edit** (or the first-run setup
wizard's backend step), the form does three things:

1. **Probes the backend** via `GET /api/setup/preflight/backend`,
   which calls `discover_ollama` server-side and returns
   `{ ollama: { available, version, path } }` alongside the GPU
   detection.
2. **Builds the serving-method dropdown** per detected GPU.
   - Apple GPUs: always show `["ollama"]`; if not installed, render
     the install panel and gate Save.
   - NVIDIA/Intel GPUs: show the vendor's Docker engines; append
     `"ollama"` when `available === true`.
3. **Shows the install panel** when the operator picks `"ollama"`
   from the dropdown but `available === false`. The install panel
   includes the per-OS install command (above) and a *Recheck*
   button that re-fires the preflight probe — no service restart
   needed.

When Ollama is detected, the wizard shows a confirmation badge:
`Ollama v0.1.43 detected · /opt/homebrew/bin/ollama` (version +
path help disambiguate on multi-install systems).

## Per-instance port isolation

The supervisor injects `OLLAMA_HOST=127.0.0.1:{host_port}` into the
spawned subprocess's environment, where `host_port` comes from
execlaw's per-purpose port pool (8101 for Standard, 8102 for Small,
etc.). This lets two execlaw instances on one host run Ollama
side-by-side without fighting over the default port 11434.

The model cache (`~/.ollama/models` on Linux/macOS,
`%USERPROFILE%\.ollama\models` on Windows) is **shared** across
instances by default. If you want per-instance isolation, set the
`OLLAMA_MODELS` env var in execlaw's service unit (the launchd
plist / SCM service environment / systemd `--user` unit) to a
distinct directory per instance.

## Pulling models — the known gap

Ollama's daemon spawns instantly and reports `Healthy` against
`/api/tags` long before any model is in its cache. execlaw's vLLM
path runs a pre-spawn HuggingFace download with progress surfaced
in the SPA ("Downloading qwen2.5-7b · 47%…"); the Ollama path
doesn't have an equivalent yet.

**Workaround until the active-pull supervisor integration ships:**
run `ollama pull <model>` once from a terminal before submitting
the wizard, e.g.:

```bash
ollama pull qwen2.5:32b-instruct-q4_K_M
```

…substituting whichever model you picked. Otherwise the first chat
completion 404s with `model 'X' not found`.

This is a deliberate v1 punt — the follow-up wires `POST /api/pull`'s
streaming response into the existing `download_task` state machine so
the same `DownloadingModel → LoadingModel → Healthy` transitions
surface in the SPA. Tracked in the audit gap inside
`fancy-launching-lecun.md` (the plan file).

## Troubleshooting

**"ollama binary not found"** — Install Ollama using the command
the wizard suggests for your OS, or set `OLLAMA_BINARY=/path/to/ollama`
and click *Recheck*. (On launchd / systemd, `PATH` is minimal — the
binary might be present in your shell but invisible to the service.
The well-known-locations probe usually covers this; set
`OLLAMA_BINARY` if not.)

**"OLLAMA_BINARY points to '…' but that file does not exist"** —
Typo in the env var. The discovery error names the offending path
so you can copy/paste it into a shell to debug.

**Chat 404s with "model 'X' not found"** — Run `ollama pull <model>`
manually. See "Pulling models — the known gap" above.

**Dropdown only shows the Docker engines, no Ollama option** —
The preflight probe didn't find Ollama. Install it (or set
`OLLAMA_BINARY`), then click *Recheck* in the wizard. If you just
installed and the probe still doesn't find it, the service's `PATH`
might not include the install location — `OLLAMA_BINARY=…` is the
safest fix.

**Two execlaw instances both try to spawn Ollama on the same port**
— Don't expect this to work without configuration: each instance
needs a distinct `host_port` in its backend spec. The supervisor's
per-purpose pool (8101, 8102, …) handles this for slots inside one
instance, but a second execlaw on the same machine needs you to
override the pool base via Settings → Hardware or by pre-staging
config rows with non-colliding `host_port` values.

**Apple Silicon: brand indicator pulsing blue** — at least one
backend is in install / warm-up. Click the indicator to jump to
`/settings/backends` and see the per-row status.

**Apple Silicon: detected memory looks wrong** — execlaw applies a
2/3-of-RAM cap to match macOS's `iogpu.wired_limit_mb` default. If
you've raised that limit (`sudo sysctl iogpu.wired_limit_mb=…`),
the wizard will undercount. Override in the advanced disclosure;
runtime hardware override lives under Settings → Hardware.

## Future engines

The `binary_hint` indirection in `model_spec_json.runtime` keeps the
door open for additional native engines:

- **MLX** (`mlx_lm.server`) on Apple Silicon — Apple's native
  framework; ~10–30% faster than llama.cpp Metal for supported
  quants, at the cost of a Python venv + `pip install mlx-lm`
  first-run dance.
- **llama.cpp `llama-server`** — Ollama's underlying engine, exposed
  directly. More configuration knobs, less curation.

Neither ships in v1; both slot in by adding a `discover_<engine>`
function + a match arm in `NativeServiceController::discover_for_hint`
+ a preset row. The wizard, supervisor, and brand indicator are
runtime-agnostic from there.
