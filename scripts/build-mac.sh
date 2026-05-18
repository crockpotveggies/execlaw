#!/usr/bin/env bash
# Build the execlaw macOS .app bundle + .dmg.
#
# Run from any directory; the script `cd`s to the repo root.
# Requires macOS 13+ with:
#   - Xcode Command Line Tools  (`xcode-select --install`)
#   - Rust toolchain (`rustup target add aarch64-apple-darwin`)
#   - Tauri CLI (`cargo install tauri-cli --version "^2.0"`)
#   - Node 20+ (Vite + SPA build)
#
# Outputs:
#   desktop-macos/src-tauri/target/aarch64-apple-darwin/release/bundle/macos/execlaw.app
#   desktop-macos/src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/execlaw_<version>_aarch64.dmg

set -euo pipefail

if [[ "$OSTYPE" != "darwin"* ]]; then
    echo "error: this script must run on macOS (current: $OSTYPE)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TARGET="aarch64-apple-darwin"
TAURI_DIR="desktop-macos/src-tauri"
BUNDLE_BIN_DIR="$TAURI_DIR/bin"
PLIST_SRC="$TAURI_DIR/macos/LaunchAgents/com.execlaw.agent.plist"

echo "==> Step 1: build SPA bundle (web/dist/)"
npm --prefix web ci
npm --prefix web run build

echo "==> Step 2: build server binary for $TARGET"
# We intentionally do NOT pass --no-default-features here — the
# server crate's defaults are what production ships (SQLCipher
# bundled, OpenSSL vendored). If the operator wants a Mac-host dev
# build without SQLCipher, they should run the workspace `cargo
# build` directly, not this release script.
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
# bundle ends up as `Contents/MacOS/execlaw` (Tauri strips the
# triple at bundle time).
cp "$SERVER_BIN" "$BUNDLE_BIN_DIR/execlaw-$TARGET"
chmod +x "$BUNDLE_BIN_DIR/execlaw-$TARGET"

echo "==> Step 4: render icons + DMG background from SVG sources"
# Tauri's `generate_context!` macro + the execlaw-tray crate's
# `include_bytes!("../icons/tray@2x.png")` need rendered PNGs at
# compile time; the bundler reads `icons/icon.icns` for the app
# icon and `icons/dmg-background.png` for the DMG window. None of
# the rendered artefacts are checked in (see
# desktop-macos/src-tauri/.gitignore) — we regenerate from the SVG
# sources under /assets on every build so the icons can't drift
# from the SVG truth.
#
# Requires `sips` (built into macOS, renders SVG since macOS 13)
# and `iconutil` (in Xcode CLT).
ICON_SVG_COLOR="assets/execlaw-color.svg"   # green Liquid Glass app icon
ICON_SVG_MONO="assets/execlaw.svg"          # monochrome silhouette → tray template
DMG_BG_SVG="assets/dmg-background.svg"
ICONS_DIR="$TAURI_DIR/icons"
ICONSET_DIR="$ICONS_DIR/icon.iconset"

if [[ ! -f "$ICON_SVG_COLOR" || ! -f "$ICON_SVG_MONO" || ! -f "$DMG_BG_SVG" ]]; then
    echo "error: expected SVG sources under assets/ — see assets/README or git history" >&2
    exit 1
fi

rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"

# Standard iconutil sizes — 16, 32, 64, 128, 256, 512, 1024 across
# @1x and @2x slots.
for pair in \
    "icon_16x16.png:16" \
    "icon_16x16@2x.png:32" \
    "icon_32x32.png:32" \
    "icon_32x32@2x.png:64" \
    "icon_128x128.png:128" \
    "icon_128x128@2x.png:256" \
    "icon_256x256.png:256" \
    "icon_256x256@2x.png:512" \
    "icon_512x512.png:512" \
    "icon_512x512@2x.png:1024" ; do
    fn="${pair%%:*}"
    sz="${pair##*:}"
    sips -s format png -Z "$sz" "$ICON_SVG_COLOR" --out "$ICONSET_DIR/$fn" >/dev/null
done

iconutil --convert icns "$ICONSET_DIR" --output "$ICONS_DIR/icon.icns"
rm -rf "$ICONSET_DIR"

# `generate_context!`'s bundle.icon entry expects icon.png at the
# `tauri.conf.json` level too (used as the runtime window icon when
# the bundler embeds it). 1024 px → Tauri downscales.
sips -s format png -Z 1024 "$ICON_SVG_COLOR" --out "$ICONS_DIR/icon.png" >/dev/null

# Menu-bar tray icon — black + alpha so macOS treats it as a
# template and tints to match the system menu bar (light/dark).
# `app.rs` sets icon_as_template(true). 22 pt is the menu-bar
# nominal size; @2x = 44 px for Retina.
sips -s format png -Z 22 "$ICON_SVG_MONO" --out "$ICONS_DIR/tray.png" >/dev/null
sips -s format png -Z 44 "$ICON_SVG_MONO" --out "$ICONS_DIR/tray@2x.png" >/dev/null

# DMG window background. 660x400 matches bundle.macOS.dmg
# windowSize in tauri.conf.json; @2x = 1320x800 for Retina.
sips -s format png -Z 660 "$DMG_BG_SVG" --out "$ICONS_DIR/dmg-background.png" >/dev/null
sips -s format png -Z 1320 "$DMG_BG_SVG" --out "$ICONS_DIR/dmg-background@2x.png" >/dev/null

echo "==> Step 5: tauri build"
(
    cd "$TAURI_DIR"
    # `cargo tauri build` honors the `--target` flag and writes
    # under target/<triple>/release/bundle/. Use `--no-bundle` for
    # a quick smoke build that produces only the binary.
    cargo tauri build --target "$TARGET"
)

BUNDLE_ROOT="$TAURI_DIR/target/$TARGET/release/bundle"
APP_PATH="$BUNDLE_ROOT/macos/execlaw.app"
DMG_PATH=$(ls "$BUNDLE_ROOT"/dmg/execlaw_*_aarch64.dmg 2>/dev/null | head -n 1 || true)

if [[ ! -d "$APP_PATH" ]]; then
    echo "error: expected $APP_PATH after tauri build" >&2
    exit 1
fi

echo "==> Step 6: inject LaunchAgent plist into bundle"
# Tauri's bundler doesn't write into Contents/Library/, so we
# copy the plist in post-build. SMAppService requires this exact
# path: Contents/Library/LaunchAgents/<name>.plist.
LAUNCH_AGENTS_DIR="$APP_PATH/Contents/Library/LaunchAgents"
mkdir -p "$LAUNCH_AGENTS_DIR"
cp "$PLIST_SRC" "$LAUNCH_AGENTS_DIR/com.execlaw.agent.plist"

# Verify the bundle is internally consistent — the BundleProgram
# referenced in the plist must exist inside the bundle. Catches
# the case where externalBin staging silently failed.
if [[ ! -x "$APP_PATH/Contents/MacOS/execlaw" ]]; then
    echo "error: $APP_PATH/Contents/MacOS/execlaw missing — externalBin stage failed" >&2
    exit 1
fi

# Re-codesign the bundle ad-hoc after our post-build mutation —
# any change inside the bundle invalidates Tauri's initial
# ad-hoc signature. macOS refuses to launch a bundle whose
# code-signature doesn't match the actual contents, even for
# ad-hoc.
echo "==> Step 7: re-codesign (ad-hoc) after plist injection"
codesign --force --deep --sign - "$APP_PATH"

echo
echo "==> Done."
echo "    .app: $APP_PATH"
if [[ -n "${DMG_PATH:-}" ]]; then
    echo "    .dmg: $DMG_PATH"
else
    echo "    .dmg: (not produced — check tauri.conf.json bundle.targets)"
fi
echo
echo "    Unsigned distribution: first launch needs right-click → Open"
echo "    to bypass Gatekeeper. See docs/setup-mac.md."
