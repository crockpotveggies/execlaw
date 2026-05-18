#!/usr/bin/env bash
# Package every operator-installable plugin into a ZIP under
# dist/<plugin-id>-<version>.zip.
#
# Plugins live under plugins/<dir>/ with at least:
#   plugins/<dir>/plugin.toml      — manifest (sourced for id+version)
#   plugins/<dir>/main.rhai        — script body (optional for some plugins)
#   plugins/<dir>/ui/panel.tsx     — optional React panel (built to panel.js)
#
# This script:
#   1. Enumerates every plugins/* with a `plugin.toml`.
#   2. Builds each plugin's UI via `scripts/build-plugin-ui.mjs --all`
#      so any ui/panel.tsx that hasn't been recompiled lands fresh.
#   3. Zips the plugin dir into dist/<plugin-id>-<version>.zip,
#      excluding dev noise (.git, node_modules, __pycache__, *.pyc,
#      .DS_Store, target/, dist/, *.log, the source ui/panel.tsx
#      itself once we have ui/panel.js).
#   4. Emits a sha256 sidecar.
#
# Skips:
#   * plugins/_shared/        — shared library, not a plugin
#
# Idempotent: re-running overwrites the previous ZIP. The
# macos-bundle CI workflow calls this once before attaching
# artifacts; build-mac.sh calls it before staging plugin ZIPs into
# the .app's resources/ dir.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DIST_DIR="dist"
mkdir -p "$DIST_DIR"

echo "==> Step 1: build every plugin's UI panel"
# `--all` only rebuilds plugins that declare ui_panels; the rest
# are no-ops. Failure here would propagate; that's intentional —
# if a plugin's TS doesn't compile, its ZIP would ship broken JS.
node scripts/build-plugin-ui.mjs --all

echo "==> Step 2: enumerate plugins + package each"

# Extract `id = "..."` or `version = "..."` from a plugin.toml.
# Cheap regex grep — sufficient for the constrained TOML shape
# every plugin uses (top-level `[plugin]` table with quoted
# strings). The macos-bundle workflow runs in a sandbox without
# tomlq / yq / python-toml; sticking to grep keeps the dep
# surface minimal.
read_manifest_field() {
    local file="$1"
    local field="$2"
    # Match the FIRST `field = "value"` at the start of a line
    # inside the `[plugin]` table. We use awk so we can scope to
    # the `[plugin]` section and ignore identically-named fields
    # in other tables (e.g. `[[tools]].name`).
    awk -v field="$field" '
        /^\[plugin\]/ { in_plugin = 1; next }
        /^\[/         { in_plugin = 0 }
        in_plugin && $1 == field {
            # Pull value between double quotes.
            match($0, /"[^"]*"/)
            v = substr($0, RSTART + 1, RLENGTH - 2)
            print v
            exit
        }
    ' "$file"
}

# Files / directories we never ship in a plugin ZIP. Mirrors
# `git clean -fdX` for the common cases plus a few execlaw-
# specific noise patterns. Pre-built ui/panel.js IS shipped (the
# host serves it at /api/admin/plugins/{id}/ui/panel.js); the
# source ui/panel.tsx is dropped because the host doesn't need
# it.
ZIP_EXCLUDES=(
    # Match both root + nested occurrences. `*` in zip's -x
    # globs spans `/`, so `*/.git/*` covers nested but misses
    # `./.git/HEAD` at root — we explicitly cover both cases.
    '.git/*' '*/.git/*'
    'node_modules/*' '*/node_modules/*'
    '__pycache__/*' '*/__pycache__/*' '*.pyc'
    'target/*' '*/target/*'
    '.DS_Store' '*/.DS_Store'
    'dist/*' '*/dist/*'
    '*.log'
    '*.tsbuildinfo'
    # Source TS/TSX dropped — only the built JS ships. Drop
    # source maps too; they're 4x the size of the JS and only
    # useful to a developer rebuilding the plugin.
    'ui/panel.ts' 'ui/panel.tsx'
    'ui/panel.js.map'
)

shopt -s nullglob

PACKAGED_COUNT=0
for plugin_dir in plugins/*/ ; do
    plugin_dir="${plugin_dir%/}"
    name="$(basename "$plugin_dir")"

    # _shared isn't a plugin — skip without comment.
    if [[ "$name" == "_shared" ]]; then
        continue
    fi

    manifest="$plugin_dir/plugin.toml"
    if [[ ! -f "$manifest" ]]; then
        echo "  $name: no plugin.toml — skipping"
        continue
    fi

    id="$(read_manifest_field "$manifest" id)"
    version="$(read_manifest_field "$manifest" version)"
    if [[ -z "$id" || -z "$version" ]]; then
        echo "  $name: manifest missing id/version — skipping"
        continue
    fi

    out="$DIST_DIR/${id}-${version}.zip"
    sha_out="${out}.sha256"

    # Zip from INSIDE the plugin directory so the archive's
    # internal paths land at `plugin.toml`, `main.rhai`, etc. at
    # the root — `stage_zip` reads `tempdir/plugin.toml`
    # directly (see crates/plugin-sdk/src/zip_stage.rs:76) and
    # returns MissingManifest if the manifest is nested under a
    # wrapper directory.
    rm -f "$out"
    out_abs="$(pwd)/$out"
    (
        cd "$plugin_dir"
        # shellcheck disable=SC2086
        zip -qr "$out_abs" . -x "${ZIP_EXCLUDES[@]}"
    )

    # SHA-256 sidecar — same `<hash>  <basename>` format
    # `shasum -a 256 -c` expects.
    (
        cd "$DIST_DIR"
        shasum -a 256 "$(basename "$out")" > "$(basename "$sha_out")"
    )

    size=$(stat -f%z "$out" 2>/dev/null || stat -c%s "$out")
    echo "  $name: $(basename "$out") ($((size / 1024)) KB)"
    PACKAGED_COUNT=$((PACKAGED_COUNT + 1))
done

echo
echo "==> $PACKAGED_COUNT plugin(s) packaged under $DIST_DIR/"
ls -1 "$DIST_DIR"/*.zip 2>/dev/null | sed 's|^|  |'
