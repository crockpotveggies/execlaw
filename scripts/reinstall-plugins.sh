#!/usr/bin/env bash
#
# Rebuild every plugin ZIP and reinstall it into a running dev server.
#
# Phase E (Flows middleware redesign) added default flows to 9
# plugins. After bumping their versions + rebuilding the ZIPs the
# dev server still has the OLD versions installed — the new flow
# rows don't land until each plugin is reinstalled through the
# [[default_automations]] importer at install / upgrade time.
#
# This script automates that loop:
#
#   1. Run `scripts/package-plugins.sh` to rebuild every plugin's
#      UI panel + ZIP.
#   2. Mirror every `dist/*.zip` into the operator's
#      `~/.execlaw/bundled-plugins/` directory (the source the
#      `/api/admin/plugins/install-bundled` endpoint reads from).
#   3. Log in to the running dev server (`POST /api/login`) to obtain
#      a JWT.
#   4. For every ZIP, POST
#      /api/admin/plugins/install-bundled?file=<zip>&if_existing=upgrade
#      which routes through the same staging + hook-registration path
#      as a manual operator install.
#
# Requires:
#   * dev server running (default 127.0.0.1:3031)
#   * EXECLAW_OPERATOR_PASSWORD env var set
#   * curl + jq on PATH
#
# Env overrides:
#   EXECLAW_SERVER_URL          (default http://127.0.0.1:3031)
#   EXECLAW_OPERATOR_USERNAME   (default admin)
#   EXECLAW_DATA_DIR            (default $HOME/.execlaw)
#
# Flags:
#   --only-changed   reinstall only the 9 Phase-E plugins
#   --skip-build     skip the rebuild step (re-use the existing dist/)

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$REPO_ROOT"

ONLY_CHANGED=0
SKIP_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --only-changed) ONLY_CHANGED=1 ;;
        --skip-build)   SKIP_BUILD=1 ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

SERVER_URL="${EXECLAW_SERVER_URL:-http://127.0.0.1:3031}"
USERNAME="${EXECLAW_OPERATOR_USERNAME:-admin}"
DATA_DIR="${EXECLAW_DATA_DIR:-$HOME/.execlaw}"
BUNDLED_DIR="$DATA_DIR/bundled-plugins"

if [[ -z "${EXECLAW_OPERATOR_PASSWORD:-}" ]]; then
    echo "EXECLAW_OPERATOR_PASSWORD must be set." >&2
    exit 2
fi

# --- 1. Build every plugin's ZIP -----------------------------------------
if [[ "$SKIP_BUILD" -eq 0 ]]; then
    echo "==> Step 1: package all plugins (rebuilds UI panels + ZIPs)"
    bash scripts/package-plugins.sh
else
    echo "==> Step 1: skipping package step (--skip-build)"
fi

# --- 2. Mirror dist/*.zip into ~/.execlaw/bundled-plugins/ --------------
mkdir -p "$BUNDLED_DIR"
echo "==> Step 2: mirror dist/*.zip -> $BUNDLED_DIR"

# Phase-E plugins (the ones that gained default flows).
CHANGED_IDS=(signal whatsapp sms-socket discord slack google-apps google-places open-meteo finance-yahoo)

# Resolve newest ZIP per plugin id. We split at the LAST hyphen so
# `finance-yahoo-0.2.2.zip` parses as id=finance-yahoo + version=0.2.2.
declare -A NEWEST
declare -A NEWEST_MTIME
for z in dist/*.zip; do
    [[ -f "$z" ]] || continue
    fname="$(basename "$z" .zip)"
    id="${fname%-*}"
    mtime="$(stat -c '%Y' "$z" 2>/dev/null || stat -f '%m' "$z")"
    prev_mtime="${NEWEST_MTIME[$id]:-0}"
    if [[ "$mtime" -gt "$prev_mtime" ]]; then
        NEWEST[$id]="$z"
        NEWEST_MTIME[$id]="$mtime"
    fi
done

TO_INSTALL=()
for id in "${!NEWEST[@]}"; do
    if [[ "$ONLY_CHANGED" -eq 1 ]]; then
        found=0
        for c in "${CHANGED_IDS[@]}"; do
            [[ "$c" == "$id" ]] && found=1 && break
        done
        [[ "$found" -eq 0 ]] && continue
    fi
    src="${NEWEST[$id]}"
    dest="$BUNDLED_DIR/$(basename "$src")"
    cp -f "$src" "$dest"
    echo "    mirrored $(basename "$src")"
    TO_INSTALL+=("$(basename "$src")")
done

if [[ "${#TO_INSTALL[@]}" -eq 0 ]]; then
    echo "Nothing to install — exiting."
    exit 0
fi

# --- 3. Log in to the dev server ----------------------------------------
echo "==> Step 3: log in to $SERVER_URL as $USERNAME"
TOKEN=$(curl -fsS -X POST "$SERVER_URL/api/login" \
    -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg u "$USERNAME" --arg p "$EXECLAW_OPERATOR_PASSWORD" \
            '{username: $u, password: $p}')" \
    | jq -r '.access_token // empty')

if [[ -z "$TOKEN" ]]; then
    echo "login failed; no access_token in response" >&2
    exit 1
fi
echo "    got access_token"

# --- 4. Reinstall each plugin via install-bundled ------------------------
echo "==> Step 4: reinstall ${#TO_INSTALL[@]} plugin(s) via install-bundled"
INSTALLED=0
FAILED=()
for zip in "${TO_INSTALL[@]}"; do
    encoded=$(printf '%s' "$zip" | jq -sRr @uri)
    uri="$SERVER_URL/api/admin/plugins/install-bundled?file=$encoded&if_existing=upgrade"
    if resp=$(curl -fsS -X POST "$uri" -H "Authorization: Bearer $TOKEN"); then
        pid=$(echo "$resp" | jq -r '.plugin_id')
        ver=$(echo "$resp" | jq -r '.version')
        echo "    [OK] $zip -> plugin_id=$pid version=$ver"
        INSTALLED=$((INSTALLED + 1))
    else
        echo "    [FAIL] $zip" >&2
        FAILED+=("$zip")
    fi
done

echo
echo "==> Done. $INSTALLED plugin(s) reinstalled."
if [[ "${#FAILED[@]}" -gt 0 ]]; then
    echo "${#FAILED[@]} failure(s):" >&2
    printf '  %s\n' "${FAILED[@]}" >&2
    exit 1
fi
