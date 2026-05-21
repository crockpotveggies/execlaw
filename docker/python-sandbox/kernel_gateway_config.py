# Jupyter Kernel Gateway config for python-sandbox:fast.
#
# Loaded inside the container at /etc/jupyter/kernel_gateway_config.py.
# Knobs picked to match the host-side python_sandbox lifecycle policy
# (15-min idle eviction, 32-kernel ceiling) so the gateway agrees with
# the supervisor on what "alive" means.

c = get_config()  # noqa: F821 — provided by jupyter traitlets

# --- Binding -------------------------------------------------------
# 0.0.0.0:8888 inside the container. Supervisor publishes this to
# 127.0.0.1:<host_port_from_pool> on the host.
c.KernelGatewayApp.ip = "0.0.0.0"
c.KernelGatewayApp.port = 8888

# --- API mode ------------------------------------------------------
# `jupyter_websocket` = REST for kernel lifecycle + WebSocket per kernel
# for execute_request / iopub. This is what our Rust client speaks.
# (The alternative `notebook-http` mode maps HTTP routes to cells —
# not what we want.)
c.KernelGatewayApp.api = "kernel_gateway.jupyter_websocket"

# --- Auth ----------------------------------------------------------
# No token. The gateway sits on a private docker network reachable
# only from the control plane (the host's python_sandbox module).
# If the SPA ever dials directly, add a token and pass it via the
# supervisor's env-to-secret bridge.
c.KernelGatewayApp.auth_token = ""

# CORS — locked down. The control plane is the only caller.
c.KernelGatewayApp.allow_origin = ""
c.KernelGatewayApp.allow_credentials = "false"

# Enable GET /api/kernels so the supervisor's rpc_health_path probe
# returns 200 instead of 403, AND so the host-side python_sandbox
# module can reconcile its conversation→kernel map against the
# gateway's view after a host restart. Default is False (the gateway
# treats kernel enumeration as a sensitive operation); flipping it on
# is safe here because the gateway is reachable only from the
# control plane via the docker network, not from the SPA or any
# external caller.
#
# NOTE — the trait lives on `JupyterWebsocketPersonality` in
# kernel_gateway 3.x, NOT on `KernelGatewayApp` as some 2.x docs
# suggest. The audit caught this: setting c.KernelGatewayApp.list_kernels
# is silently ignored and GET /api/kernels keeps 403'ing.
c.JupyterWebsocketPersonality.list_kernels = True

# --- Kernel lifecycle ---------------------------------------------
# Cull idle kernels after 15 minutes. Matches the host-side eviction
# policy; both sides will converge so a kernel evicted host-side
# won't linger gateway-side and vice versa.
c.MappingKernelManager.cull_idle_timeout = 15 * 60  # seconds
c.MappingKernelManager.cull_interval = 60           # check every minute
# Don't kill kernels mid-execute even if they cross the idle deadline
# while running. The host enforces per-execute timeouts separately.
c.MappingKernelManager.cull_busy = False
# Cap concurrent kernels per container — bounds memory pressure if
# something spawns conversations faster than expected. The host's
# ensure_for logic will respect this by surfacing 503 to the agent
# when the gateway refuses.
c.MappingKernelManager.cull_connected = True

# Default kernel name. ipykernel registers "python3" out of the box.
c.KernelGatewayApp.default_kernel_name = "python3"

# Max kernels — hard ceiling that catches runaway spawning before
# OOM. 32 is well above any realistic self-hosted concurrent-convo
# count; cranking higher should be a deliberate operator decision.
c.KernelGatewayApp.max_kernels = 32

# --- Disable iopub rate limiting ----------------------------------
# ipykernel ships with an iopub_data_rate_limit (default ~1 MB/s)
# that **silently drops** stream output once a cell exceeds the
# budget — empirically (Phase 2 audit): a `print('x' * 60_000_000)`
# yields ZERO stream messages on the wire, just busy→idle.
#
# We'd rather fail loudly than truncate silently. Our Rust client
# enforces its own 50 MB per-execute cap (MAX_OUTPUT_BYTES) that
# emits a clear `OutputTooLarge` status. Disable the kernel-side
# limit so all output reaches us, and our cap is the single source
# of truth for "output too large" behavior.
#
# The trait lives on `ZMQChannelsWebsocketConnection` — that's the
# WS handler that actually enforces it. Phase 2 audit caught this:
# setting `c.ServerApp.iopub_data_rate_limit` got us a warning
# echoed back in the truncation message, but didn't override the
# active value (still 1 MB/s when we measured). The deprecation
# shim on ServerApp doesn't propagate into the websocket handler;
# you have to set it where the limit is actually checked.
c.ZMQChannelsWebsocketConnection.iopub_data_rate_limit = 1e12
c.ZMQChannelsWebsocketConnection.iopub_msg_rate_limit = 1e6
c.ZMQChannelsWebsocketConnection.rate_limit_window = 3.0
