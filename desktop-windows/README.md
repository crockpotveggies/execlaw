# execlaw — Windows desktop bundle

A Tauri 2 notification-area (tray) app that wraps the execlaw control
plane in an NSIS `.exe` installer and registers it as a Windows
service via `Service Control Manager`. Built only on Windows; the
crate is outside the main Rust workspace to keep `cargo check` cheap
on macOS / Linux dev hosts (the same trick `desktop-macos/`
already plays).

## What it ships

- `execlaw_<version>_x64-setup.exe` — single-file NSIS installer.
  Install destination defaults to `C:\Program Files\execlaw\` and
  contains:
  - `execlaw-tray.exe` — the tray UI (this crate).
  - `execlaw.exe` — the unmodified server binary from the root
    workspace, bundled as a Tauri "external binary."
  - `resources/plugins/*.zip` — every plugin from `plugins/*/`,
    packaged by `scripts/package-plugins.ps1` and lifted into the
    installer via `bundle.resources` in `tauri.conf.json`.
  - Standard Tauri runtime + WebView2 bootstrapper glue.

## What it does

On NSIS install (`installer/hooks.nsh::NSIS_HOOK_POSTINSTALL`):

1. Pre-creates `%USERPROFILE%\.execlaw\` for the install-time admin.
2. Calls `execlaw.exe service install --system --db
   "%USERPROFILE%\.execlaw\execlaw.db"` — registers the SCM service
   under the label `execlaw`, owned by `LocalSystem`, autostart on
   boot.
3. Calls `execlaw.exe service start --system`.

The same `execlaw.exe` is what the SCM later spawns via
`execlaw service run`; that command dispatches into the
`windows-service` SCM event loop so the binary correctly answers
Stop/Pause control messages from the SCM (see
`crates/cli/src/service.rs::windows_runtime`).

On tray launch (`execlaw-tray.exe`):

1. Shows a notification-area icon in the taskbar tray.
2. Builds a context menu (Service status / Open execlaw / Restart
   service / Open data folder / Uninstall / Quit).
3. Polls the SCM every 5 seconds and rewrites the *Service: …* row
   with the live state (Running / Stopped / Pending / Paused /
   NotInstalled). When the server is Running, defers to the
   `/api/ping` probe so first-run wizard states surface in the same
   row.
4. *Open execlaw* opens a WebView2 window pointed at
   `http://127.0.0.1:3031/`. The SPA is served by the Rust binary
   via `rust-embed`, so the webview is same-origin with the API.
5. *Restart service* re-launches the bundled `execlaw.exe service
   restart` with the `runas` shell verb so UAC fires (SCM control
   verbs need Administrator). *Uninstall execlaw…* does the same
   for `service uninstall`.

On NSIS uninstall (`installer/hooks.nsh::NSIS_HOOK_PREUNINSTALL`):

1. `execlaw.exe service stop --system` (idempotent).
2. `execlaw.exe service uninstall --system` (idempotent).
3. NSIS proceeds to remove the program files, Start Menu shortcuts,
   and Add/Remove Programs entry.

Operator data at `%USERPROFILE%\.execlaw\` is intentionally NOT
deleted — the user has to explicitly opt-in via the tray's
*Uninstall execlaw…* → "Also delete your data?" flow, mirroring the
macOS bundle's behaviour.

## Building

The build is Windows-only and runs from the repo root:

```powershell
./scripts/build-windows.ps1
```

That script:

1. `npm --prefix web ci && npm --prefix web run build` — produces
   `web\dist\` which the server binary embeds.
2. `cargo build --release --target x86_64-pc-windows-msvc -p execlaw`
   — produces the server binary.
3. Copies the server binary to
   `desktop-windows\src-tauri\bin\execlaw-x86_64-pc-windows-msvc.exe`.
4. Renders `icons\icon.ico`, `icons\icon.png`, and `icons\tray.ico`
   from `assets/execlaw-color.svg` + `assets/execlaw.svg` via
   ImageMagick.
5. Runs `scripts/package-plugins.ps1` and copies every
   `dist\*.zip` into `desktop-windows\src-tauri\resources\plugins\`.
6. `cd desktop-windows\src-tauri && cargo tauri build --target
   x86_64-pc-windows-msvc` — produces the NSIS installer.

Output lands at
`desktop-windows\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\nsis\execlaw_<version>_x64-setup.exe`.

## Toolchain requirements

- Windows 10 1903+ or Windows 11.
- PowerShell 5.1+ (ships with Windows).
- Visual Studio Build Tools 2022 with the "Desktop development with
  C++" workload (provides `link.exe`, `rc.exe`, and the Windows SDK):

  ```powershell
  winget install Microsoft.VisualStudio.2022.BuildTools --override `
      "--passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
  ```

- Rust 1.85+ with the **MSVC host toolchain** installed (not just
  the target):

  ```powershell
  rustup toolchain install stable-x86_64-pc-windows-msvc
  rustup target add x86_64-pc-windows-msvc
  ```

  The HOST matters because `tauri-build` and its `tauri-winres`
  dependency are *build-time* crates compiled against the host
  triple. `tauri-winres` evaluates `cfg!(target_env = "msvc")` at
  its own compile time; if the host is GNU it picks the legacy
  `windres` code path, which trips a path-handling bug. The build
  script sets `RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-msvc`
  per-invocation so it works even when the operator's default is
  the GNU toolchain.

- Node 20+ (Vite + SPA build).
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`.
- ImageMagick (for SVG → ICO icon rendering):
  `winget install ImageMagick.ImageMagick`.
- WebView2 Runtime — pre-installed on Windows 11 and recent Windows
  10; otherwise the installer downloads it via the bootstrapper at
  install time (see `tauri.conf.json` →
  `bundle.windows.webviewInstallMode = "downloadBootstrapper"`).

The `scripts/build-windows.ps1` script handles two additional
fiddly pieces automatically so the operator doesn't have to:

1. **VS Developer environment** — `vcvarsall.bat x64` is imported
   into the script's PowerShell session so `cl.exe` / `link.exe`
   / `rc.exe` and the right `INCLUDE` / `LIB` / `LIBPATH` are set
   before `cargo build` runs.
2. **`cc-rs` compiler selection** — `CC_x86_64_pc_windows_msvc` and
   friends are pinned to `cl.exe` / `lib.exe`. Without this, on a
   host with MSYS2 / MinGW installed, `cc-rs` discovers `gcc.exe`
   off `PATH` and produces GNU-flavoured object files that
   `link.exe` rejects with "unresolved external symbol
   `___chkstk_ms`" deep inside `libsqlite3-sys`.

## Symmetry with `desktop-macos/`

| Concern                 | macOS (`desktop-macos/`)                            | Windows (`desktop-windows/`)                                  |
|-------------------------|------------------------------------------------------|----------------------------------------------------------------|
| Background service      | LaunchAgent plist + `SMAppService.register()`        | SCM entry installed via `execlaw service install`              |
| Service runs as         | Logged-in user (LaunchAgent)                         | `LocalSystem`                                                  |
| Service start at boot   | Yes (RunAtLoad + KeepAlive)                          | Yes (autostart=true, service-manager RestartPolicy)            |
| Data dir                | `~/.execlaw/`                                        | `%USERPROFILE%\.execlaw\` (install-time admin's profile)       |
| First-launch register   | Tray app calls `SMAppService.register()`             | NSIS post-install hook calls `execlaw service install`         |
| First-launch start      | LaunchAgent loaded at register-time                  | NSIS hook calls `execlaw service start`                        |
| Uninstall (clean)       | Drag .app to Trash — macOS auto-disables             | Settings → Apps → execlaw → Uninstall (runs pre-uninstall hook)|
| Uninstall (in-tray)     | *Uninstall execlaw…* → `SMAppService.unregister()`   | *Uninstall execlaw…* → UAC → `execlaw service uninstall`       |
| Tray icon style         | Monochrome template (`icon_as_template(true)`)       | Standard multi-color .ico                                      |
| Chat window             | WebView2 / WKWebView at `127.0.0.1:3031`             | WebView2 at `127.0.0.1:3031`                                   |
| Bundle target           | `.app` + `.dmg`                                      | NSIS `.exe`                                                    |
| Plugin ZIP staging      | `Contents/Resources/plugins/`                        | `<INSTDIR>\resources\plugins\`                                 |

## What's NOT in v1

- Code signing — the installer is unsigned. Windows SmartScreen
  warns "Windows protected your PC" on first run; operators bypass
  with *More info → Run anyway*. Equivalent to the macOS "right-click
  → Open" gate.
- Auto-updater — operators download fresh installers manually from
  GitHub Releases.
- WoA (`aarch64-pc-windows-msvc`) — x64 only for v1. Can be added
  via a parallel build target when there's demand.
- Bundled Ollama / Docker / model weights — operators install
  separately, same as on macOS.
- Voice mic Tauri capability — added when Phase 8 voice UI ships.
- Per-user install — current installer is `installMode: perMachine`
  (requires admin). Per-user is possible (`currentUser`) but the
  Windows service install would either go away or move to a
  per-user-service registration, both of which need their own
  design pass.
