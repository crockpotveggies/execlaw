#!/usr/bin/env bash
# execlaw dev server — Rust hot-reload via cargo-watch.
#
# Run from the repo root:
#   bash scripts/dev-server.sh
#
# What it does:
#   * Watches every Rust source file under `crates/`.
#   * On any change, runs `cargo run -p execlaw -- serve …` which
#     incremental-compiles + restarts the binary in place.
#   * The Vite dev server (npm run dev) proxies /api → this server,
#     so SPA changes hot-reload via Vite HMR and Rust changes
#     hot-reload via this script — together you get a full-stack
#     restart-free workflow.
#
# Bind port: 127.0.0.1:3031 is the default everywhere — `execlaw
# serve`, `SERVICE_BIND` in crates/cli/src/service.rs, and the Vite
# proxy default in web/vite.config.ts all agree. Override via
# EXECLAW_DEV_BIND if you need a different port for one-off testing.
#
# Tweakables (all overridable via env):
#   EXECLAW_DEV_BIND       — server bind addr        (127.0.0.1:3031)
#   EXECLAW_DEV_DB         — SQLite path             (~/.execlaw/execlaw.db)
#   EXECLAW_WATCH_FEATURES — extra cargo features    ("")
#
# Stop with Ctrl+C; cargo-watch tears down the child binary cleanly.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# --- bootstrap cargo on PATH ----------------------------------------
# Git Bash on Windows often spawns with a stripped PATH that doesn't
# include `~/.cargo/bin`, so a bare `cargo` invocation fails with
# "command not found" even though PowerShell finds it fine. We try
# several candidate locations because $HOME isn't always set the way
# we expect (Git Bash sometimes points it at `/h/` or a roaming
# profile, MSYS2 may use `/home/<user>`, etc.).
#
# We use `-f` rather than `-x` because Git Bash + Windows doesn't
# always set the executable bit on `.exe` files even when they ARE
# runnable.
_candidates=(
    "${HOME:-}/.cargo/bin"
    "${USERPROFILE:-}/.cargo/bin"
    "/c/Users/${USERNAME:-$USER}/.cargo/bin"
    "/c/Users/${USER:-}/.cargo/bin"
)
# Also normalise Windows-style USERPROFILE (`C:\Users\foo`) to the
# bash form (`/c/Users/foo`) so the check works whether HOME or
# USERPROFILE was the one set.
if [ -n "${USERPROFILE:-}" ]; then
    _norm_userprofile=$(echo "$USERPROFILE" | sed 's|\\|/|g; s|^\([A-Za-z]\):|/\L\1|')
    _candidates+=("${_norm_userprofile}/.cargo/bin")
fi

# Final fallback: glob every Windows user profile for a cargo
# install. Catches the common Windows-truncated username mismatch
# (Windows truncates "justin" → "justi" on profile creation, so
# `/c/Users/justin/.cargo/bin` doesn't exist but
# `/c/Users/justi/.cargo/bin` does). WSL paths under `/mnt/c/`
# get the same treatment for users running a real WSL bash.
_glob_candidates=()
for _root in "/c/Users" "/mnt/c/Users"; do
    if [ -d "$_root" ]; then
        for _profile in "$_root"/*/.cargo/bin; do
            [ -f "$_profile/cargo.exe" ] && _glob_candidates+=("$_profile")
        done
    fi
done
# `${arr[@]+"${arr[@]}"}` expands to nothing when the array is
# empty — without the guard, `set -u` trips on an unbound
# reference under older bash builds.
_candidates+=("${_glob_candidates[@]+${_glob_candidates[@]}}")

# Source the rustup env file too — it's the canonical way and works
# even on layouts our path-guessing doesn't cover.
for _envfile in "${HOME:-}/.cargo/env" "${USERPROFILE:-}/.cargo/env"; do
    if [ -n "${_envfile}" ] && [ -f "${_envfile}" ]; then
        # shellcheck disable=SC1090
        . "${_envfile}"
        break
    fi
done

# Resolve a callable cargo binary. The wrinkle: WSL2 bash can READ
# Windows-side `.exe` files via interop but does NOT auto-append
# `.exe` to PATH lookups, so a bare `cargo` invocation fails even
# when `cargo.exe` is on PATH. We therefore record the FULL binary
# path (including `.exe` when applicable) and call it directly
# below.
CARGO_BIN=""
if cargo --version >/dev/null 2>&1; then
    CARGO_BIN="cargo"
fi
if [ -z "$CARGO_BIN" ]; then
    for _bin in "${_candidates[@]}"; do
        if [ -z "$_bin" ]; then
            continue
        fi
        if [ -f "$_bin/cargo.exe" ]; then
            CARGO_BIN="$_bin/cargo.exe"
            export PATH="$_bin:$PATH"
            break
        fi
        if [ -f "$_bin/cargo" ]; then
            CARGO_BIN="$_bin/cargo"
            export PATH="$_bin:$PATH"
            break
        fi
    done
fi
if [ -z "$CARGO_BIN" ] && [ -n "${EXECLAW_CARGO_BIN:-}" ]; then
    if [ -f "${EXECLAW_CARGO_BIN}/cargo.exe" ]; then
        CARGO_BIN="${EXECLAW_CARGO_BIN}/cargo.exe"
    elif [ -f "${EXECLAW_CARGO_BIN}/cargo" ]; then
        CARGO_BIN="${EXECLAW_CARGO_BIN}/cargo"
    fi
    [ -n "$CARGO_BIN" ] && export PATH="${EXECLAW_CARGO_BIN}:$PATH"
fi

if [ -z "$CARGO_BIN" ] || ! "$CARGO_BIN" --version >/dev/null 2>&1; then
    echo "error: 'cargo' not found on PATH or in any ~/.cargo/bin candidate." >&2
    echo "       Searched:" >&2
    for _bin in "${_candidates[@]}"; do
        [ -n "$_bin" ] && echo "         - $_bin" >&2
    done
    echo "       HOME='${HOME:-<unset>}' USERPROFILE='${USERPROFILE:-<unset>}'" >&2
    echo "       Override with EXECLAW_CARGO_BIN=/path/to/.cargo/bin and re-run." >&2
    exit 127
fi

# Locate cargo-watch in the same directory as cargo (rustup
# installs it alongside). Fall back to letting cargo find it on
# its own PATH (which works under Git Bash / native Linux but
# requires the `.exe` workaround on WSL2).
CARGO_DIR=$(dirname "$CARGO_BIN")
if [ -f "${CARGO_DIR}/cargo-watch.exe" ]; then
    : # cargo will find cargo-watch.exe via the prepended PATH
elif [ -f "${CARGO_DIR}/cargo-watch" ]; then
    :
else
    if ! "$CARGO_BIN" watch --version >/dev/null 2>&1; then
        echo "error: 'cargo-watch' not installed. Run:" >&2
        echo "       $CARGO_BIN install cargo-watch --locked" >&2
        exit 127
    fi
fi

"$CARGO_BIN" --version 2>/dev/null | sed "s|^|dev-server: ($CARGO_BIN) |" >&2

BIND="${EXECLAW_DEV_BIND:-127.0.0.1:3031}"
DB_FLAG=()
if [ -n "${EXECLAW_DEV_DB:-}" ]; then
    DB_FLAG=(--db "$EXECLAW_DEV_DB")
fi

FEATURES_FLAG=()
if [ -n "${EXECLAW_WATCH_FEATURES:-}" ]; then
    FEATURES_FLAG=(--features "$EXECLAW_WATCH_FEATURES")
fi

# `cargo-watch` flags:
#   --why            print which file triggered the rebuild
#   --watch crates   only watch the workspace crates dir (skip target/)
#   --watch Cargo.toml + Cargo.lock — pick up dependency edits
#   --clear          clear the terminal between runs (TTY only — set
#                    EXECLAW_DEV_CLEAR=0 to disable for background /
#                    pipe-redirected runs where ANSI escapes fail with
#                    "The handle is invalid" on Windows).
#   -x "run …"       run cargo subcommand on each change
#
# `--no-encrypt` is dev-only; production runs SQLCipher.
WATCH_FLAGS=(--why --watch crates --watch Cargo.toml --watch Cargo.lock)
if [ "${EXECLAW_DEV_CLEAR:-1}" = "1" ] && [ -t 1 ]; then
    WATCH_FLAGS+=(--clear)
fi

exec "$CARGO_BIN" watch \
    "${WATCH_FLAGS[@]}" \
    -x "run -p execlaw ${FEATURES_FLAG[*]:-} -- serve --bind $BIND --no-encrypt ${DB_FLAG[*]:-}"
