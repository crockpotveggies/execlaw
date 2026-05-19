# execlaw — Linux desktop bundle

A Tauri 2 notification-area (tray) app that wraps the execlaw
control plane in a Debian `.deb` package and registers it as a
`systemd --user` service. Built only on Linux; the crate is
outside the main Rust workspace to keep `cargo check` cheap on
macOS / Windows dev hosts (the same trick `desktop-macos/` and
`desktop-windows/` already play).

## What it ships

- `execlaw_<version>_amd64.deb` — installable with
  `sudo apt install ./execlaw_<v>_amd64.deb` (or `dpkg -i` if
  you don't mind resolving deps manually). The .deb places:
  - `/usr/bin/execlaw-tray` — the tray UI (this crate).
  - `/usr/bin/execlaw` — the unmodified server binary from
    the root workspace, bundled as a Tauri "external binary."
  - `/usr/share/execlaw/plugins/*.zip` — every plugin from
    `plugins/*/`, packaged by `scripts/package-plugins.sh`
    and lifted into the .deb's data section via
    `bundle.resources` in `tauri.conf.json`.
  - `/usr/share/applications/execlaw.desktop` — Tauri's
    auto-generated XDG desktop entry (so the operator can
    launch the tray from GNOME / KDE / XFCE menus).
  - `/usr/share/icons/hicolor/<size>/apps/execlaw.png` — icon
    set at standard freedesktop.org sizes.

## What it does

On `.deb` install (via apt's dpkg machinery):

1. Files land at the paths above. **No service registration
   happens at apt-install time** — apt's `postinst` runs as root,
   but `systemd --user` units must live in the operator's HOME
   to start under their UID. The tray app does the registration
   on first launch instead (same pattern macOS uses with
   `SMAppService.register()`).

On tray launch (`execlaw-tray`):

1. Shows a notification-area icon. On KDE Plasma, XFCE, MATE,
   Cinnamon, and elementary OS this Just Works via SNI
   (StatusNotifierItem). On vanilla GNOME (Fedora Workstation,
   Debian's default GNOME spin) the operator needs the
   *AppIndicator and KStatusNotifierItem Support* extension;
   Ubuntu has bundled it since 22.04.
2. Calls `execlaw service install --user` via the bundled
   `/usr/bin/execlaw` CLI. Writes
   `~/.config/systemd/user/execlaw.service` via the
   [`service-manager`](https://crates.io/crates/service-manager)
   crate's systemd backend (same code path covered by the CLI's
   integration tests). Idempotent — a second register is a noop.
3. Calls `execlaw service start --user`. Brings the service up
   immediately on a fresh install; for subsequent launches it's
   a noop (the unit is already running from the previous
   session, with `linger` enabled or after re-login).
4. Builds the tray menu and starts a 5-second poller that
   queries `systemctl --user is-active execlaw.service` plus the
   server's `/api/ping`. The status row reflects whichever of
   {`Running`, `First-run setup pending`, `Setup wizard pending`,
   `Stopped`, `Failed`, `Pending`, `not installed`} applies.
5. On *Open execlaw* → opens a WebView2-equivalent
   (webkit2gtk-4.1) window pointed at the local server.
6. On *Restart service* → calls `execlaw service restart --user`.
   No `pkexec` / `sudo` dance — user units run under the
   operator's UID, no privilege escalation needed.
7. On *View logs (journalctl)…* → spawns a terminal running
   `journalctl --user -u execlaw -f`. Prefers
   `gnome-terminal` > `konsole` > `xfce4-terminal` > `xterm`,
   whichever is on PATH first.

On *Uninstall execlaw…* (tray menu): confirm → call
`execlaw service stop --user` + `service uninstall --user` →
optional `~/.execlaw/` data wipe → quit. The tray app itself
exits, but `/usr/bin/execlaw` and `/usr/bin/execlaw-tray`
remain — use `sudo apt remove execlaw` to remove the program
files. Same two-step teardown as macOS (drag-to-Trash + tray
uninstall) and Windows (Apps & Features + tray uninstall).

**`apt remove` caveat:** apt removes the binaries + plugin ZIPs
but leaves `~/.config/systemd/user/execlaw.service` alone (per-
user systemd units are out of apt's reach without elevating to
the operator's UID, which apt doesn't do). To fully clean up:
use the tray's *Uninstall execlaw…* first, then `apt remove`.
If you ran `apt remove` first by mistake, the user unit still
exists but its `ExecStart` points at the now-deleted
`/usr/bin/execlaw` — run `systemctl --user disable --now execlaw`
and `rm ~/.config/systemd/user/execlaw.service` to finish the
cleanup manually.

## Building

The build is Linux-only and runs from the repo root:

```bash
./scripts/build-linux.sh
```

That script:

1. `npm --prefix web ci && npm --prefix web run build` — produces
   `web/dist/` which the server binary embeds.
2. `cargo build --release --target x86_64-unknown-linux-gnu
   -p execlaw` — produces the server binary.
3. Copies the server binary to
   `desktop-linux/src-tauri/bin/execlaw-x86_64-unknown-linux-gnu`.
4. Renders `icons/{32x32,128x128,128x128@2x,icon}.png` from
   `assets/execlaw-color.svg` via `rsvg-convert`.
5. Runs `scripts/package-plugins.sh` and copies every
   `dist/*.zip` into
   `desktop-linux/src-tauri/resources/plugins/`.
6. `cd desktop-linux/src-tauri && cargo tauri build --target
   x86_64-unknown-linux-gnu` — produces the `.deb`.

Output lands at
`desktop-linux/src-tauri/target/x86_64-unknown-linux-gnu/release/bundle/deb/execlaw_<version>_amd64.deb`.

## Toolchain requirements

- A Debian-family Linux host (Ubuntu 22.04+, Debian 12+, Mint 21+,
  Pop_OS! 22.04+).
- System libs — one apt install covers everything:

  ```bash
  sudo apt install \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
      librsvg2-dev librsvg2-bin libgtk-3-dev libsoup-3.0-dev \
      libssl-dev pkg-config build-essential
  ```

- Rust 1.85+ with `x86_64-unknown-linux-gnu` (the host default
  on most installs). The build script auto-adds the target if
  missing.
- Node 20+ (Vite + SPA build).
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`.
- `rsvg-convert` from `librsvg2-bin` (icon rendering).

## Symmetry with `desktop-macos/` + `desktop-windows/`

| Concern                 | macOS                                                | Windows                                              | Linux                                                |
|-------------------------|------------------------------------------------------|------------------------------------------------------|------------------------------------------------------|
| Background service      | LaunchAgent plist + `SMAppService.register()`        | SCM entry via `execlaw service install --system`     | systemd `--user` unit via `execlaw service install --user` |
| Service runs as         | Logged-in user (LaunchAgent)                         | `LocalSystem`                                        | Operator's UID (`systemd --user`)                    |
| Service start at boot   | Yes (RunAtLoad + KeepAlive)                          | Yes (autostart=true)                                 | Yes (after `loginctl enable-linger` or on next login)|
| Data dir                | `~/.execlaw/`                                        | `%USERPROFILE%\.execlaw\`                            | `~/.execlaw/`                                        |
| First-launch register   | Tray app calls `SMAppService.register()`             | NSIS post-install hook calls `service install`       | Tray app calls `execlaw service install --user`      |
| First-launch start      | LaunchAgent loaded at register-time                  | NSIS hook calls `service start`                      | Tray app calls `execlaw service start --user`        |
| Uninstall (clean)       | Drag .app to Trash — macOS auto-disables             | Settings → Apps → execlaw → Uninstall (NSIS hook)    | `sudo apt remove execlaw` (binaries only) + tray Uninstall (service) |
| Uninstall (in-tray)     | *Uninstall execlaw…* → `SMAppService.unregister()`   | *Uninstall execlaw…* → UAC → `service uninstall`     | *Uninstall execlaw…* → `service uninstall --user`    |
| Tray icon style         | Monochrome template (`icon_as_template(true)`)       | Multi-color .ico                                     | Multi-color PNG via SNI                              |
| Chat window             | WKWebView at 127.0.0.1:3031                          | WebView2 at 127.0.0.1:3031                           | webkit2gtk-4.1 at 127.0.0.1:3031                     |
| Bundle target           | `.app` + `.dmg`                                      | NSIS `.exe`                                          | `.deb`                                               |
| Plugin ZIP staging      | `Contents/Resources/plugins/`                        | `<INSTDIR>\resources\plugins\`                       | `/usr/share/execlaw/plugins/`                        |
| Privilege escalation    | Not needed (SMAppService is per-user)                | UAC `runas` via `ShellExecuteW`                      | Not needed (`systemctl --user` doesn't escalate)     |

## What's NOT in v1

- `.rpm` target — only `.deb`. Fedora / RHEL operators can use
  the `.AppImage` (when added) or build from source. Adding
  `bundle.targets = ["deb", "rpm"]` + a Fedora-runner CI job is
  a follow-up.
- `.AppImage` target — would give a single-file portable build
  for non-Debian distros. The .AppImage doesn't register the
  systemd service (no install step at all), so the operator
  would have to `execlaw service install --user` from a shell.
  Add when there's demand.
- Code signing / apt repo — the .deb is unsigned, distributed as
  a standalone file. `gpg` signing + a real apt repo (apt.execlaw.dev?)
  comes later.
- aarch64 (`arm64`) — x86_64 only for v1. The Tauri bundler
  builds .deb for any rustc target you point it at, so adding
  `--target aarch64-unknown-linux-gnu` is a one-line change once
  CI gets a real arm64 runner.
- AppImage / Flatpak / Snap distribution channels.
- Voice mic Tauri capability — added when Phase 8 voice UI
  ships.
