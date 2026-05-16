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

echo "==> Step 4: ensure tauri.conf.json sees an icon"
if [[ ! -f "$TAURI_DIR/icons/icon.icns" ]]; then
    echo "WARN: $TAURI_DIR/icons/icon.icns missing — generating a 1x1 placeholder."
    echo "      Replace with a real icon before shipping; see $TAURI_DIR/icons/README.md."
    # Minimal valid ICNS: a single 32x32 ic08 chunk filled with
    # transparent pixels. Total file is ~4 KB. We synthesise it
    # via a Python one-liner so the script has zero external
    # dependencies beyond Python (which ships with macOS).
    python3 - "$TAURI_DIR/icons/icon.icns" <<'PY'
import struct, sys, zlib
path = sys.argv[1]
# Build a 32x32 transparent PNG.
def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
sig = b"\x89PNG\r\n\x1a\n"
ihdr = struct.pack(">IIBBBBB", 32, 32, 8, 6, 0, 0, 0)
raw = b"".join(b"\x00" + b"\x00\x00\x00\x00" * 32 for _ in range(32))
idat = zlib.compress(raw)
png = sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")
icns = b"icns" + struct.pack(">I", 8 + 8 + len(png)) + b"ic08" + struct.pack(">I", 8 + len(png)) + png
with open(path, "wb") as f:
    f.write(icns)
PY
fi

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
