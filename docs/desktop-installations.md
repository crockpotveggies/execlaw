# Desktop installations

execlaw ships three first-party desktop bundles, one per supported
desktop OS. They are deliberately symmetric: same Tauri 2 architecture,
same tray-app-fronts-a-background-service shape, same `127.0.0.1:3031`
local SPA. Operators install once, get a tray/menu-bar icon, and the
control plane runs in the background under whatever service manager
that OS natively prefers.

This page is the cross-OS reference. Per-bundle build details + the
fiddly OS-specific bits live in the three crate READMEs:

- [`desktop-macos/README.md`](../desktop-macos/README.md) — Apple
  Silicon `.app` / `.dmg`.
- [`desktop-windows/README.md`](../desktop-windows/README.md) — x86_64
  NSIS `.exe` setup.
- [`desktop-linux/README.md`](../desktop-linux/README.md) — Debian-
  family `.deb`.

For the CLI install path (`execlaw install`) used on headless servers
and Intel Macs, see [README](../README.md#quick-start-production).

## Why three separate crates

Tauri's cross-platform story is "one source tree, three targets," but
each desktop has its own:

- **Service-manager API** — `SMAppService` on macOS, the Service
  Control Manager on Windows, `systemd --user` on Linux. The bundle
  needs to know which one to call on install + uninstall.
- **Bundle format** — `.app` + `.dmg`, NSIS `.exe`, `.deb`. Each
  bundler has its own resource-staging rules.
- **Build host** — code-signing tooling, icon renderers, WebView
  runtimes, system development headers. A single CI runner can't build
  all three from one host.
- **System libraries linked into the tray binary** — Cocoa /
  AppKit on macOS, the Win32 + WebView2 SDKs on Windows,
  webkit2gtk-4.1 + libayatana-appindicator3 on Linux.

To keep `cargo check` cheap on dev hosts, each desktop crate sits
**outside** the main Rust workspace (`workspace.exclude` in the root
`Cargo.toml`). The three crates only build on their native OS; on every
other host they may as well not exist. `crates/cli/` still cross-
compiles cleanly to all three triples, which is what the bundles
actually ship as their background service.

## The shape, in one paragraph

Each bundle ships **two binaries**: the unmodified `execlaw` control
plane (built from `crates/cli/`) and a tiny Tauri 2 tray app
(`execlaw-tray`). The tray app's only job is the OS-native UX —
notification-area icon, status row, *Open execlaw* (a WebView pointed
at the local server), *Restart service*, *Uninstall*. The control
plane runs under that OS's service manager and serves both the JSON
API and the React SPA from `127.0.0.1:3031`. Tray clicks shell out to
`execlaw service {install,start,stop,restart,uninstall}` — the same
CLI verbs an operator would use over SSH.

## Cross-OS symmetry table

| Concern | macOS | Windows | Linux |
|---|---|---|---|
| Bundle target | `.app` + `.dmg` | NSIS `.exe` setup | Debian `.deb` |
| Target triple | `aarch64-apple-darwin` | `x86_64-pc-windows-msvc` | `x86_64-unknown-linux-gnu` |
| WebView backend | WKWebView | WebView2 | webkit2gtk-4.1 |
| Tray protocol | NSStatusItem (AppKit) | Shell_NotifyIcon (Win32) | StatusNotifierItem (SNI) |
| Tray icon style | Monochrome template | Multi-color `.ico` | Multi-color PNG |
| Service backend | LaunchAgent (per-user) | SCM service (LocalSystem) | systemd `--user` unit |
| Registration API | `SMAppService.register()` | `execlaw service install --system` | `execlaw service install --user` |
| Registration runs at | Tray-app first launch | NSIS post-install hook | Tray-app first launch |
| Service starts at | Register-time (LaunchAgent + `RunAtLoad`) | NSIS hook (`service start`) | Tray-app first launch |
| Service starts on boot | Yes (LaunchAgent) | Yes (SCM autostart) | Yes (after `loginctl enable-linger` or next login) |
| Service runs as | Logged-in user | `LocalSystem` | Operator's UID |
| Privilege escalation | Not needed (per-user) | UAC via `ShellExecuteW runas` | Not needed (`systemctl --user`) |
| Data directory | `~/.execlaw/` | `%USERPROFILE%\.execlaw\` | `~/.execlaw/` |
| Plugin ZIP staging | `Contents/Resources/plugins/` | `<INSTDIR>\resources\plugins\` | `/usr/share/execlaw/plugins/` |
| Uninstall (clean) | Drag `.app` to Trash | Settings → Apps → execlaw → Uninstall | `sudo apt remove execlaw` + tray *Uninstall* |
| Uninstall (in-tray) | `SMAppService.unregister()` | UAC → `service uninstall` | `service uninstall --user` |
| Build script | `scripts/build-mac.sh` | `scripts/build-windows.ps1` | `scripts/build-linux.sh` |
| CI workflow | `.github/workflows/macos-bundle.yml` | `.github/workflows/windows-bundle.yml` | `.github/workflows/linux-bundle.yml` |
| Code signing in v1 | Ad-hoc; right-click → Open gate | Unsigned; SmartScreen "Run anyway" | Unsigned `.deb` |

## What each bundle ships

### macOS — `execlaw_<v>_aarch64.dmg`

- `execlaw.app/Contents/MacOS/execlaw-tray` — menu-bar UI.
- `execlaw.app/Contents/MacOS/execlaw` — bundled server binary
  (Tauri "external binary").
- `execlaw.app/Contents/Library/LaunchAgents/com.execlaw.agent.plist`
  — LaunchAgent plist, injected post-bundle by `scripts/build-mac.sh`.
- `execlaw.app/Contents/Resources/plugins/*.zip` — every plugin
  ZIP from `dist/*.zip`.
- Standard Tauri-generated icons + metadata.

`.app` is wrapped in a `.dmg` with a Finder-side `Applications`
symlink so the operator drags execlaw into Applications without
needing Terminal.

### Windows — `execlaw_<v>_x64-setup.exe`

NSIS installer. Default install path `C:\Program Files\execlaw\`.

- `execlaw-tray.exe` — tray UI.
- `execlaw.exe` — server binary; also the executable the SCM spawns
  (`execlaw service run` dispatches to the
  `windows-service` SCM event loop so the same binary answers
  Stop/Pause control messages correctly).
- `resources\plugins\*.zip` — every plugin ZIP from `dist\*.zip`.
- WebView2 bootstrapper glue — downloads the WebView2 runtime at
  install time on Windows 10 hosts where it's missing
  (`tauri.conf.json` → `bundle.windows.webviewInstallMode =
  "downloadBootstrapper"`).

### Linux — `execlaw_<v>_amd64.deb`

Standard Debian package. Installs to:

- `/usr/bin/execlaw-tray` — tray UI.
- `/usr/bin/execlaw` — server binary.
- `/usr/share/execlaw/plugins/*.zip` — every plugin ZIP.
- `/usr/share/applications/execlaw.desktop` — XDG desktop entry so
  the tray app shows up in GNOME / KDE / XFCE menus.
- `/usr/share/icons/hicolor/<size>/apps/execlaw.png` — icon set
  at standard freedesktop.org sizes.

Apt resolves dependencies (`libwebkit2gtk-4.1-0`,
`libayatana-appindicator3-1`, `libgtk-3-0`) automatically.

## Install flows

### macOS

1. Download `execlaw_<v>_aarch64.dmg` from Releases.
2. Open `.dmg` → drag execlaw to `/Applications`.
3. First launch: right-click → *Open* (the build is unsigned, so a
   plain double-click hits Gatekeeper). macOS remembers the exception.
4. Menu bar icon appears. macOS surfaces *Background Items Added* —
   that's `SMAppService` registering the LaunchAgent. Approve in
   *System Settings → General → Login Items & Extensions* if
   prompted.
5. *Open execlaw* → WKWebView opens onto `http://127.0.0.1:3031/`.
   First-run wizard takes it from there.

### Windows

1. Download `execlaw_<v>_x64-setup.exe` from Releases.
2. Run the installer; UAC prompt fires (the installer registers a
   `LocalSystem` SCM service, so it needs admin). NSIS's
   `NSIS_HOOK_POSTINSTALL` calls:
   ```text
   execlaw.exe service install --system --db "%USERPROFILE%\.execlaw\execlaw.db"
   execlaw.exe service start --system
   ```
3. SmartScreen warns "Windows protected your PC" on first run since
   the installer is unsigned — *More info → Run anyway*.
4. Notification-area icon appears. *Open execlaw* opens a WebView2
   window on `127.0.0.1:3031`.

The same binary `execlaw.exe` is what the SCM later spawns under
`LocalSystem`; the `windows-service` event-loop dispatch lives in
[`crates/cli/src/service.rs::windows_runtime`](../crates/cli/src/service.rs).

### Linux (Debian / Ubuntu / Mint / Pop_OS!)

1. Download `execlaw_<v>_amd64.deb` from Releases.
2. Install: `sudo apt install ./execlaw_<v>_amd64.deb` (or
   `dpkg -i` if you don't mind resolving deps manually).
   *Important:* `.deb` install runs as root via apt's `postinst`,
   but `systemd --user` units must live in the operator's HOME to
   start under their UID. **No service registration happens at
   apt-install time.** The tray app does it on first launch.
3. Launch `execlaw-tray` from the application menu (or
   `/usr/bin/execlaw-tray` from a shell). The tray app:
   - Calls `execlaw service install --user` → writes
     `~/.config/systemd/user/execlaw.service`.
   - Calls `execlaw service start --user` → unit goes Active.
4. SNI tray icon appears. On **vanilla GNOME** (Fedora Workstation,
   Debian's default GNOME spin) the operator first needs the
   *AppIndicator and KStatusNotifierItem Support* extension; Ubuntu
   has bundled it since 22.04. Tray Just Works on KDE Plasma, XFCE,
   MATE, Cinnamon, and elementary OS.
5. *Open execlaw* → webkit2gtk-4.1 window on `127.0.0.1:3031`.

For boot-time start without an interactive login, run
`loginctl enable-linger $USER` once.

## Uninstall flows

In every case, the **clean** path is the same: in-tray *Uninstall
execlaw…* first (deregisters the service + optionally wipes
`~/.execlaw/`), then remove the program files through the OS's
native package mechanism.

| OS | In-tray uninstall does | Then remove program files via |
|---|---|---|
| macOS | `SMAppService.unregister()` + optional data wipe | Drag `.app` to Trash |
| Windows | UAC → `execlaw service stop/uninstall --system` + optional data wipe | Settings → Apps → execlaw → Uninstall |
| Linux | `execlaw service stop/uninstall --user` + optional data wipe | `sudo apt remove execlaw` |

**Linux `apt remove` caveat:** `apt remove` removes
`/usr/bin/execlaw*` and `/usr/share/execlaw/plugins/` but leaves
`~/.config/systemd/user/execlaw.service` alone (per-user systemd
units are out of apt's reach without elevating to the operator's
UID). To fully clean up: tray *Uninstall* first, then `apt remove`.
If you ran `apt remove` first by mistake, run `systemctl --user
disable --now execlaw` and `rm
~/.config/systemd/user/execlaw.service` to finish manually.

## What the tray app does at runtime

All three tray apps converge on the same menu shape:

- **Service: <status>** — live row, polled every 5s. Status comes
  from the OS service manager (`launchctl list`, SCM
  `QueryServiceStatus`, `systemctl --user is-active`) *plus* the
  server's `/api/ping` endpoint when the service reports Running, so
  first-run wizard states (`First-run setup pending`, `Setup wizard
  pending`) bubble up here too.
- **Open execlaw** — opens the OS's native WebView pointed at
  `http://127.0.0.1:3031/`. The SPA is served by the Rust binary via
  `rust-embed`, so the webview is same-origin with the API. No CORS
  preflight, no token-passing between processes.
- **Restart service** — shells out to `execlaw service restart`. On
  Windows the call goes through `ShellExecuteW runas` so UAC fires
  (SCM control verbs need admin); on macOS / Linux it's a direct
  exec (per-user services don't need privilege escalation).
- **View logs (journalctl / log stream / Event Viewer…)** — Linux
  spawns a terminal running `journalctl --user -u execlaw -f`
  (preferring `gnome-terminal` > `konsole` > `xfce4-terminal` >
  `xterm`); macOS opens `Console.app` filtered to `process ==
  "execlaw"`; Windows opens `eventvwr.msc`.
- **Open data folder** — opens `~/.execlaw/` (or
  `%USERPROFILE%\.execlaw\`) in the platform's file manager.
- **Uninstall execlaw…** — confirmation dialog → service-uninstall
  CLI call → optional data wipe → tray app quits.
- **Quit** — just quits the tray app. Service keeps running.

## Building from source

Each bundle has its own build script that handles SPA bundling, server
compilation, icon rendering, plugin packaging, and the Tauri bundler in
one shot:

| OS | Command | Output |
|---|---|---|
| macOS | `./scripts/build-mac.sh` | `desktop-macos/src-tauri/target/aarch64-apple-darwin/release/bundle/{macos,dmg}/` |
| Windows | `./scripts/build-windows.ps1` | `desktop-windows/src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/` |
| Linux | `./scripts/build-linux.sh` | `desktop-linux/src-tauri/target/x86_64-unknown-linux-gnu/release/bundle/deb/` |

Toolchain prerequisites differ per OS — check the relevant crate
README for the exact `apt` / `brew` / `winget` lines.

## What's NOT in v1

- **Code signing / notarization** anywhere. All three bundles are
  unsigned. The first-launch gate (Gatekeeper / SmartScreen) is the
  cost; an Apple Developer cert + a Windows code-signing cert + a
  GPG apt-repo signing key are follow-ups.
- **Auto-updater.** Operators download fresh bundles from GitHub
  Releases manually.
- **Architectures other than the v1 floor.** Apple Silicon only on
  macOS (Intel Macs use the CLI install — see
  [`docs/setup-mac.md`](setup-mac.md) for why); x86_64 only on
  Windows and Linux. WoA (`aarch64-pc-windows-msvc`) and Linux arm64
  (`aarch64-unknown-linux-gnu`) build cleanly through Tauri once a
  CI runner exists.
- **Bundled inference runtimes.** No model weights, no Ollama, no
  Docker. The operator installs those separately — see
  [`docs/ollama.md`](ollama.md) for the recommended Ollama setup
  on each OS.
- **Voice mic Tauri capability / Info.plist key
  (`NSMicrophoneUsageDescription`).** Added when Phase 8 voice UI
  ships.
- **`.rpm` + `.AppImage`** on Linux. `.deb` only in v1. Fedora /
  RHEL operators build from source or use a `.deb`-to-`.rpm`
  conversion until a real Fedora-runner CI job lands.
- **macOS Intel (`x86_64-apple-darwin`) bundle.** Same reason
  Apple-Silicon-only is supported: the only macOS-specific path
  that justifies a dedicated build is Metal-accelerated inference,
  which doesn't exist on Intel Macs.
- **Per-user Windows install.** Current installer is
  `installMode: perMachine` (requires admin). A `currentUser`
  variant would need either no service registration at all or a
  per-user-service registration model — both need their own design
  pass.
