# execlaw on macOS (Apple Silicon)

This page covers first-run setup on an Apple Silicon Mac (M1 / M2 / M3 / M4). **Intel Macs are not supported** — the only macOS-specific code path that justifies a dedicated build is Metal-accelerated inference via Ollama, which requires an Apple GPU. Run execlaw on Linux or Windows if you're on an Intel Mac.

## Why Apple Silicon is special

execlaw spawns its inference engine in a Docker container on every other platform. On Apple Silicon, that path is unusable:

- **Docker Desktop on macOS** runs containers inside a Linux microVM. The VM has zero Metal access — the M-series GPU is invisible to anything inside the container, so an Ollama / llama.cpp container running on a Mac would fall back to CPU and lose the entire point of an Apple-GPU host.
- **Apple's `container` CLI** (built on Virtualization.framework, WWDC 2025) has the same limitation.
- **vLLM** has no Metal kernels at all — even running the vLLM binary natively on a Mac wouldn't help, because there's no GPU code path.

So on Apple Silicon, execlaw's control plane spawns **Ollama as a native macOS subprocess** instead of a Docker container. The control plane itself is already a per-OS native binary (`aarch64-apple-darwin`), so it supervises Ollama directly via `tokio::process` — same lifecycle contract (spawn, health-probe, restart-backoff) the Docker path uses, just without bollard in the middle.

## Prerequisites

1. **Homebrew** — install from [brew.sh](https://brew.sh).
2. **Ollama** — `brew install ollama`. The control plane discovers the binary in this order:
   1. `$OLLAMA_BINARY` env var (operator override).
   2. `PATH` lookup.
   3. `/opt/homebrew/bin/ollama` (Apple Silicon brew prefix).

   The Intel-Mac brew prefix (`/usr/local/bin/`) is intentionally NOT probed — Intel Macs aren't supported. If none of the above resolve, the wizard surfaces an actionable error verbatim: `ollama binary not found — install with brew install ollama, or set OLLAMA_BINARY to an absolute path`.

3. **Disk space.** Pick a model that fits in unified memory **and** has room on disk for the GGUF blob:
   - `qwen2.5:7b-instruct-q4_K_M` — ~4.4 GB on disk, ~5 GB RAM.
   - `qwen2.5:14b-instruct-q4_K_M` — ~9 GB on disk, ~10 GB RAM.
   - `qwen2.5:32b-instruct-q4_K_M` — ~20 GB on disk, ~22 GB RAM.

   The wizard's "Detected hardware" badge shows `Apple Silicon (Metal) · NN GB` where the byte count is `sysctl hw.memsize × 2/3` — that's the conservative GPU-accessible budget on Apple Silicon and matches macOS's `iogpu.wired_limit_mb` default.

## First-run flow

1. Install + start execlaw (`execlaw install` then the launchd plist starts automatically).
2. Open the SPA. The wizard detects the M-series GPU and pre-selects the **Ollama (Apple Silicon)** preset for the Standard backend.
3. Pick the model that fits your machine.
4. **Pull the model.** Until the active-pull supervisor integration ships (tracked as a known gap, see below), run this once from a terminal:
   ```bash
   ollama pull qwen2.5:32b-instruct-q4_K_M
   ```
   …substituting whichever model you chose in the wizard. This downloads the GGUF weights into Ollama's cache so the first chat completion finds them locally.
5. Submit the wizard. The supervisor launches `ollama serve` as a subprocess; once `GET /api/tags` returns 200, the SPA writes `http://127.0.0.1:8101/v1` to the backend row and the chat path goes live.

## Known gap: active model pull

The supervisor's vLLM path runs a pre-spawn HF download with progress reported into the SPA ("Downloading qwen2.5-7b · 47%…"). The Ollama path doesn't have an equivalent yet — the daemon spawns instantly and reports `Healthy` against `/api/tags` long before any model is in the cache, so the first chat completion will 404 with `model 'X' not found` if you skip step 4 above.

This is a deliberate v1 punt; the follow-up wires `POST /api/pull`'s streaming response into the existing `download_task` machinery so the same `DownloadingModel` → `LoadingModel` → `Healthy` transitions surface in the SPA's status pill. See the corresponding audit gap in `fancy-launching-lecun.md` (the plan file).

## Troubleshooting

**"ollama binary not found"** — `brew install ollama`, or set `OLLAMA_BINARY=/path/to/ollama` and restart execlaw.

**"OLLAMA_BINARY points to '…' but that file does not exist"** — typo in the env var; the discovery error names the offending path so you can copy/paste into a `ls` to debug.

**Chat 404s with "model 'qwen2.5:…' not found"** — run `ollama pull <model>` manually. This is the known-gap workaround above.

**Brand indicator pulsing blue** — at least one backend is in the install / warm-up phase. Click the icon to jump to `/settings/backends` and see the per-row status.

**Detected hardware shows the wrong memory** — execlaw applies a 2/3-of-RAM cap to match macOS's `iogpu.wired_limit_mb` default. If you've raised that limit (`sudo sysctl iogpu.wired_limit_mb=...`), the wizard will undercount. Override via the advanced disclosure if needed; runtime hardware override lives under Settings → Hardware.

## Multiple execlaw instances on one Mac

The supervisor injects `OLLAMA_HOST=127.0.0.1:{host_port}` per spawn (where `host_port` is the per-purpose pool — 8101 for Standard, 8102 for Small, etc.) so two execlaw instances on one host don't fight over Ollama's default port (11434). The model cache (`~/.ollama/models`) is shared across instances by default; set the `OLLAMA_MODELS` env var in execlaw's launchd plist if you need per-instance isolation.

## Future engines

The `binary_hint` indirection in `model_spec_json` keeps the door open for additional Apple-native engines:

- **MLX** (`mlx_lm.server`) — Apple's native framework; ~10–30% faster than llama.cpp Metal for supported quants, at the cost of a Python venv + `pip install mlx-lm` first-run dance.
- **llama.cpp `llama-server`** — Ollama's underlying engine, exposed directly. More configuration knobs, less curation.

Neither ships in v1; both slot in by adding a `discover_<engine>` function + a match arm in `NativeServiceController::discover_for_hint` + a preset row. The wizard, supervisor, and brand indicator are runtime-agnostic from there.
