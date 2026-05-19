#!/usr/bin/env bash
# Build the execlaw Linux .deb bundle + bundled tray app.
#
# Run from any directory; the script `cd`s to the repo root.
# Requires a Debian/Ubuntu-family host with:
#   - Rust toolchain  (`rustup target add x86_64-unknown-linux-gnu`)
#   - Tauri CLI       (`cargo install tauri-cli --version "^2.0" --locked`)
#   - Node 20+        (Vite + SPA build)
#   - System libs    — `libwebkit2gtk-4.1-dev`, `libayatana-appindicator3-dev`,
#                       `librsvg2-dev`, `libgtk-3-dev`, `libsoup-3.0-dev`,
#                       `build-essential`, `pkg-config`, `libssl-dev`.
#                       The CI workflow installs these via apt; on a dev
#                       host run the one-liner in `desktop-linux/README.md`.
#   - rsvg-convert  — `librsvg2-bin` package (for SVG → PNG icon rendering).
#                       Cheaper + more accurate than ImageMagick for our
#                       single SVG source.
#
# Outputs:
#   desktop-linux/src-tauri/target/x86_64-unknown-linux-gnu/release/bundle/deb/execlaw_<version>_amd64.deb

set -euo pipefail

if [[ "$OSTYPE" != "linux-gnu"* ]]; then
    echo "error: this script must run on Linux (current OSTYPE: $OSTYPE)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TARGET="x86_64-unknown-linux-gnu"
TAURI_DIR="desktop-linux/src-tauri"
BUNDLE_BIN_DIR="$TAURI_DIR/bin"
ICONS_DIR="$TAURI_DIR/icons"
PLUGIN_STAGE_DIR="$TAURI_DIR/resources/plugins"

# Probe required tools up front so the operator gets a clear error
# instead of a 200-line cargo / tauri trace deep in the build.
for bin in cargo node npm rsvg-convert; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "error: missing required tool '$bin' on PATH" >&2
        echo "    install via your distro's package manager — see desktop-linux/README.md" >&2
        exit 1
    fi
done
if ! cargo tauri --version >/dev/null 2>&1; then
    echo "error: Tauri CLI not installed" >&2
    echo "    run: cargo install tauri-cli --version '^2.0' --locked" >&2
    exit 1
fi

# Auto-add the rustup target if missing. Faster than the operator
# stumbling into an E0463 on stdlib lookup.
if ! rustup target list --installed 2>/dev/null | grep -q "^$TARGET$"; then
    echo "==> Installing rustup target $TARGET (one-time setup)"
    rustup target add "$TARGET"
fi

echo "==> Step 1: build SPA bundle (web/dist/)"
npm --prefix web ci
npm --prefix web run build

echo "==> Step 2: build server binary for $TARGET"
# Default features ship — SQLCipher bundled, OpenSSL vendored —
# same posture as the macOS + Windows release scripts.
cargo build --release --target "$TARGET" -p execlaw

echo "==> Step 3: stage server binary for Tauri sidecar bundling"
mkdir -p "$BUNDLE_BIN_DIR"
SERVER_BIN="target/$TARGET/release/execlaw"
if [[ ! -x "$SERVER_BIN" ]]; then
    echo "error: expected server binary at $SERVER_BIN — did cargo build silently fail?" >&2
    exit 1
fi
# Tauri's `externalBin` field appends the rustc triple to the
# basename and looks for that exact path; the binary inside the
# .deb ends up as `/usr/bin/execlaw` (Tauri strips the triple at
# bundle time).
cp "$SERVER_BIN" "$BUNDLE_BIN_DIR/execlaw-$TARGET"
chmod +x "$BUNDLE_BIN_DIR/execlaw-$TARGET"

echo "==> Step 4: render icons from SVG sources"
# Tauri's `generate_context!` macro + the execlaw-tray-linux crate's
# `include_bytes!("../icons/128x128.png")` need rendered PNGs at
# compile time. The freedesktop hicolor icon theme expects standard
# sizes (16/22/24/32/48/64/128/256/512); the .deb bundler installs
# at each size it finds.
ICON_SVG="assets/execlaw-color.svg"
if [[ ! -f "$ICON_SVG" ]]; then
    echo "error: missing SVG source: $ICON_SVG" >&2
    exit 1
fi
mkdir -p "$ICONS_DIR"
# Render the four sizes Tauri's config explicitly lists, plus a
# 512-px icon.png the bundler uses as the runtime window icon.
rsvg-convert -w 32   -h 32   "$ICON_SVG" -o "$ICONS_DIR/32x32.png"
rsvg-convert -w 128  -h 128  "$ICON_SVG" -o "$ICONS_DIR/128x128.png"
rsvg-convert -w 256  -h 256  "$ICON_SVG" -o "$ICONS_DIR/128x128@2x.png"
rsvg-convert -w 512  -h 512  "$ICON_SVG" -o "$ICONS_DIR/icon.png"

echo "==> Step 4b: package plugin ZIPs + stage them into the bundle"
# Build every plugin under `plugins/*/` into `dist/<id>-<version>.zip`
# and copy the ZIPs into `desktop-linux/src-tauri/resources/plugins/`.
# `tauri.conf.json`'s `bundle.resources` glob lifts them into the
# .deb's data section; the server's first-run bootstrap copies them
# out into `~/.execlaw/bundled-plugins/` so the SPA's "Install
# plugin" page can list them with a one-click install button (no
# separate download needed). The Linux CI workflow also runs
# `package-plugins.sh` on its own so the resulting `dist/*.zip`
# files attach to the GitHub Release for operators on other distros.
./scripts/package-plugins.sh
rm -rf "$PLUGIN_STAGE_DIR"
mkdir -p "$PLUGIN_STAGE_DIR"
# Only the ZIPs themselves ship inside the .deb; the .sha256
# sidecars stay in dist/ for the release attachments.
cp dist/*.zip "$PLUGIN_STAGE_DIR/"
echo "  staged $(ls "$PLUGIN_STAGE_DIR" | wc -l | tr -d ' ') ZIPs into $PLUGIN_STAGE_DIR"

echo "==> Step 5: tauri build"
(
    cd "$TAURI_DIR"
    # `cargo tauri build` honors `--target` and writes under
    # `target/<triple>/release/bundle/`.
    cargo tauri build --target "$TARGET"
)

BUNDLE_ROOT="$TAURI_DIR/target/$TARGET/release/bundle"
DEB_PATH=$(ls "$BUNDLE_ROOT"/deb/execlaw_*_amd64.deb 2>/dev/null | head -n 1 || true)

if [[ -z "${DEB_PATH:-}" ]]; then
    echo "error: expected .deb under $BUNDLE_ROOT/deb/ — did the bundler fail?" >&2
    exit 1
fi

echo
echo "==> Done."
echo "    .deb: $DEB_PATH"
echo
echo "    Install:   sudo apt install \"$DEB_PATH\""
echo "    Uninstall: sudo apt remove execlaw"
echo
echo "    Unsigned distribution — no signature warnings on Debian,"
echo "    but a future release may add a signing key + apt repo."
