# Python sandbox container image

This directory holds the Docker image + supporting files for the
**python-sandbox native feature** (see `crates/server/src/python_sandbox/`).

The image runs a [Jupyter Kernel Gateway](https://github.com/jupyter-server/kernel_gateway)
that the host's `python.execute` / `python.reset` / `python.interrupt` /
`python.list_files` tools dispatch to via HTTP + WebSocket. One container
per host; kernel-per-conversation managed by the host's `KernelPool`.

## Files

- `Dockerfile` — image build. Tagged as `execlaw/python-sandbox-fast:0.1.0`.
- `kernel_gateway_config.py` — Tornado/JKG config. Binds 0.0.0.0:8888,
  enables CORS for the supervisor's RPC probe, disables auth (the
  port is bind-mounted to localhost only).
- `requirements.txt` — pre-installed Python packages: pandas, polars,
  duckdb, pyarrow, numpy, openpyxl, ipython, httpx. **Intentionally
  no matplotlib** — charting goes through the host's `chart.render`
  with Vega-Lite specs.
- `smoke_execute.py` — manual smoke test the operator can run via
  `docker exec` to verify a kernel spawns + executes correctly.

## Build

The image is built once per release and tagged locally:

```
docker build -t execlaw/python-sandbox-fast:0.1.0 docker/python-sandbox/
```

The supervisor's `inspect_image` short-circuit (see
`crates/container-manager/src/service.rs`) detects the locally-built
image and skips the registry pull, so operators can build once and
re-spawn freely.

## Why this isn't a plugin

Python-sandbox shipped briefly as a plugin (Phase 8 — `plugins/python-sandbox/`)
but the implementation is tightly coupled to the host crate
(5,685 LOC of Rust for kernel pool, Jupyter WS protocol, output
watcher, etc.) and the plugin SDK doesn't ship Rust modules.
The migration to native (commit landed 2026-05-20) moved:

- Tool dispatch + service code → stays in `crates/server/src/python_sandbox/`
  (was already there during the plugin era; now formally native).
- Sidecar registration → native call from `cli/main.rs` at boot,
  gated on the new `config_python_sandbox.enabled` flag.
- Config storage → new `config_python_sandbox` table (was
  per-plugin vault rows).
- Config UI → native Settings page at `/settings/python-sandbox`
  (was `[[ui_panels]]` `DynamicPluginPanel`).
- Sidecar artifact (this dir) → moved from `plugins/python-sandbox/`.

See `crates/core/migrations/0011_python_sandbox_native.sql` for
the DB migration that drops the legacy plugin install state.
