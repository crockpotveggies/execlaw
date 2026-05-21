# python-sandbox:fast — alpine, multi-stage build.
#
# Design: one shared container running jupyter_kernel_gateway,
# kernel-per-conversation managed via the gateway's HTTP+WS API.
# The sidecar supervisor keeps THIS container alive; per-conversation
# kernels are managed by the host's `python_sandbox` module.
#
# Every dep in the `:fast` stack ships musllinux wheels EXCEPT
# duckdb, which we build from source in the builder stage. Audited
# 2026-05-18: source-built musl duckdb performs within noise of
# glibc duckdb on our typical groupby/filter/aggregate workload
# (4 ms vs 3 ms on a 2M-row reference query). DuckDB's docs warn of
# up to 5× slowdowns on "some workloads" — applies to specific
# analytical paths, not the agent-driven flow we care about. If
# duckdb ships a musllinux wheel upstream, this Dockerfile picks it
# up automatically and the source-build layer goes from ~4 min to
# seconds.
#
# Sizes (audited 2026-05-18 against the smoke suite):
#   transfer (docker pull):  ~174 MB
#   on-disk uncompressed:    ~830 MB
# Build wall time (cold cache): ~5 min — the source-build of duckdb
# is the long pole; subsequent rebuilds without dep changes are
# seconds thanks to Docker's layer cache.

# =====================================================================
# Stage 1 — builder
# =====================================================================
FROM python:3.12-alpine AS builder

# Build deps for duckdb (C++ via CMake) + Python C extension headers.
# linux-headers is needed by transitives that probe ucontext.
RUN apk add --no-cache \
      build-base \
      cmake \
      ninja \
      python3-dev \
      linux-headers \
      git

COPY requirements.txt /tmp/requirements.txt

# Install everything into a venv we copy wholesale into the runtime
# stage. duckdb has no musllinux wheel so pip falls back to the
# sdist and compiles — this is the slow part. The post-install prune
# drops:
#   * jedi    (~33 MB on disk) — IPython's tab-completer. The agent
#     doesn't use tab-completion; completer falls back to readline.
#   * debugpy (~18 MB)         — ipykernel's debug-adapter bridge.
#     We don't expose a debugger.
#   * __pycache__, bundled test suites — dead weight in a runtime image.
RUN python -m venv /opt/venv \
 && /opt/venv/bin/pip install --no-cache-dir --upgrade pip \
 && /opt/venv/bin/pip install --no-cache-dir -r /tmp/requirements.txt \
 && /opt/venv/bin/pip uninstall --yes jedi debugpy \
 && find /opt/venv -type d -name __pycache__ -exec rm -rf {} + \
 && find /opt/venv -type d -name tests -exec rm -rf {} + \
 && find /opt/venv -type d -name test -exec rm -rf {} +

# =====================================================================
# Stage 2 — runtime
# =====================================================================
FROM python:3.12-alpine

# Runtime deps of duckdb / pyarrow / numpy / pillow's transitive image
# libs. Without libstdc++ + libgomp the compiled .so files load but
# fail immediately with "Error loading shared library libstdc++.so.6".
RUN apk add --no-cache \
      libstdc++ \
      libgomp \
      libjpeg-turbo \
      zlib

# Bring across the venv from the builder, including the freshly-built
# duckdb wheel. The venv is relocatable — it's installed into
# /opt/venv and uses /opt/venv/bin/python as the shebang.
COPY --from=builder /opt/venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"

# Kernel gateway config — idle culling, max kernels, no-auth. See
# kernel_gateway_config.py for the knobs.
COPY kernel_gateway_config.py /etc/jupyter/kernel_gateway_config.py

# Home + workspace. A dedicated /home/sandbox exists so libraries
# that probe Path.home() (jupyter runtime sockets, matplotlib font
# cache, pip's metadata cache) find a writable directory.
# Without it, HOME defaults to "/" for UID 1000 and Jupyter dies on
# startup trying to mkdir /.local/share/jupyter — caught by the
# Phase 1 audit run before this image ever reached the supervisor.
#
# The supervisor populates per-conversation subdirs at
# /work/<conversation_id>/{uploads,outputs}/ from the host side; the
# kernel sees them at the same paths through the bind mount.
RUN mkdir -p /home/sandbox /work \
 && chown -R 1000:1000 /home/sandbox /work
ENV HOME=/home/sandbox

# Non-root by default. The container's outer boundary is the real
# sandbox (--cap-drop ALL, --read-only on root, --network none, set
# by the supervisor at run time); dropping root inside is cheap
# defense-in-depth.
USER 1000:1000
WORKDIR /work

# Single port: 8888. Supervisor maps to a host port in the 8501+ pool.
EXPOSE 8888

# CMD (not ENTRYPOINT) so an operator or test runner can override
# with `docker run IMAGE other-command` for diagnostics. The
# supervisor always runs the default form in production.
CMD ["jupyter", "kernelgateway", \
     "--config=/etc/jupyter/kernel_gateway_config.py"]
