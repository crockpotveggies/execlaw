<#
.SYNOPSIS
    Build the execlaw Windows NSIS installer (.exe) + bundled tray app.

.DESCRIPTION
    Windows analogue of scripts/build-mac.sh — kept in lockstep so a
    contributor reading either script sees the same shape.

    Run from any directory; the script `cd`s to the repo root.

    Requires Windows 10+ with:
      - PowerShell 5.1+ (ships with Windows)
      - Rust toolchain (`rustup target add x86_64-pc-windows-msvc`)
      - Tauri CLI (`cargo install tauri-cli --version "^2.0" --locked`)
      - Node 20+ (Vite + SPA build)
      - ImageMagick (for SVG -> ICO icon rendering); install via
        `winget install ImageMagick.ImageMagick` or `choco install
        imagemagick`. The CI workflow installs it via choco.

    Outputs:
      desktop-windows\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\nsis\execlaw_<version>_x64-setup.exe
#>

[CmdletBinding()]
param()

# We deliberately do NOT set $ErrorActionPreference = 'Stop' here.
# On Windows PowerShell 5.1, that turns every line a native command
# writes to stderr into a terminating `NativeCommandError`, which
# trips on benign tool output like `npm`'s deprecation warnings and
# `cargo`'s progress bars. The build would halt mid-stream with no
# real failure.
#
# Instead, every native invocation below checks `$LASTEXITCODE` and
# `throw`s explicitly on non-zero; cmdlets that need terminating
# behaviour carry `-ErrorAction Stop` per-call. `throw` is always
# terminating regardless of preference, so the existing throws keep
# their semantics.

# Make 'magick.exe' / 'cargo.exe' resolution robust against the
# default `cmd`-style PATHEXT in fresh PowerShell sessions.
$env:PATHEXT = '.COM;.EXE;.BAT;.CMD;.PS1'

if (-not $IsWindows -and -not ([Environment]::OSVersion.Platform -eq 'Win32NT')) {
    Write-Error 'This script must run on Windows (current platform is not Win32NT).'
    exit 1
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..') -ErrorAction Stop
Set-Location $RepoRoot -ErrorAction Stop

$Target          = 'x86_64-pc-windows-msvc'
$TauriDir        = Join-Path $RepoRoot 'desktop-windows\src-tauri'
$BundleBinDir    = Join-Path $TauriDir 'bin'
$IconsDir        = Join-Path $TauriDir 'icons'
$PluginStageDir  = Join-Path $TauriDir 'resources\plugins'

# -----------------------------------------------------------------
# Pin the Rust HOST toolchain to MSVC for this script invocation.
#
# This matters even when the user's default `rustup` toolchain is
# `stable-x86_64-pc-windows-gnu`: build-time deps (`tauri-build`,
# `tauri-winres`, every `build.rs`) compile against the HOST
# triple, not the `--target`. `tauri-winres` in particular
# evaluates `cfg!(target_env = "msvc")` at its own compile time;
# when its host is GNU it picks the `windres` code path and chokes
# on path-handling bugs that don't exist when it uses MSVC's
# `rc.exe`.
#
# `RUSTUP_TOOLCHAIN` overrides the resolved channel for child
# processes only — it doesn't write to rustup's persistent state,
# so the operator's `rustup default ...` survives the build.
# -----------------------------------------------------------------
$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'

# -----------------------------------------------------------------
# Verify rustup MSVC target + import the VS Developer environment.
#
# Two prerequisites that are easy to miss and produce confusing
# downstream errors:
#
# 1. The `x86_64-pc-windows-msvc` rustup target. Without it,
#    `cargo build --target $Target` fails compiling `core` /
#    `std` with E0463.
#
# 2. The MSVC build environment (cl.exe + link.exe on PATH, plus
#    INCLUDE / LIB / LIBPATH env vars). Without it, `cc-rs` in
#    crates like `libsqlite3-sys` picks the first `gcc.exe` on
#    PATH — typically a MinGW / MSYS2 install — and produces
#    GNU-flavoured object files that MSVC link.exe rejects with
#    "unresolved external symbol ___chkstk_ms".
#
# We resolve both proactively rather than letting the operator
# stare at a 200-line linker dump.
# -----------------------------------------------------------------
$installedTargets = & rustup target list --installed 2>$null
if ($LASTEXITCODE -ne 0) {
    throw 'rustup not found on PATH - install Rust via https://rustup.rs/ and retry.'
}
if ($installedTargets -notcontains $Target) {
    Write-Host "==> Installing rustup target $Target (one-time setup)"
    & rustup target add $Target
    if ($LASTEXITCODE -ne 0) {
        throw "rustup target add $Target failed ($LASTEXITCODE)"
    }
}

# Skip the vcvars import if the caller already lives inside a VS
# Developer Prompt (VCINSTALLDIR is the canonical signal). Otherwise
# locate vcvarsall.bat and import its env into this PowerShell
# session.
if (-not $env:VCINSTALLDIR) {
    Write-Host '==> Importing MSVC build environment (vcvarsall.bat x64)'
    $vsBases = @(
        ${env:ProgramFiles(x86)}
        $env:ProgramFiles
    ) | Where-Object { $_ } | ForEach-Object { Join-Path $_ 'Microsoft Visual Studio' }
    $editions = @('BuildTools', 'Community', 'Professional', 'Enterprise', 'Preview')
    $years    = @('2022', '2019')
    $vcvars   = $null
    foreach ($base in $vsBases) {
        foreach ($year in $years) {
            foreach ($edition in $editions) {
                $candidate = Join-Path $base "$year\$edition\VC\Auxiliary\Build\vcvarsall.bat"
                if (Test-Path -LiteralPath $candidate) {
                    $vcvars = $candidate
                    break
                }
            }
            if ($vcvars) { break }
        }
        if ($vcvars) { break }
    }
    if (-not $vcvars) {
        throw @"
Could not find vcvarsall.bat under any Visual Studio install path.
Install VS 2022 Build Tools with the "Desktop development with C++"
workload, then retry:

    winget install Microsoft.VisualStudio.2022.BuildTools \
        --override "--passive --add Microsoft.VisualStudio.Workload.VCTools \
                    --includeRecommended"
"@
    }
    # Run vcvarsall x64 + `set` in cmd.exe, then import every
    # `KEY=VALUE` line back into the current PowerShell session.
    # `2>&1` keeps the cmd-side stderr (vcvarsall sometimes prints
    # informational banners there) from tripping anything.
    $vcLines = & cmd.exe /d /c "`"$vcvars`" x64 && set" 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "vcvarsall.bat x64 failed ($LASTEXITCODE)"
    }
    foreach ($line in $vcLines) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path "Env:$($Matches[1])" -Value $Matches[2]
        }
    }
    if (-not $env:VCINSTALLDIR) {
        throw 'vcvarsall.bat ran but did not export VCINSTALLDIR - environment import failed.'
    }
}

# Force `cc-rs` (used by libsqlite3-sys + ring + every other crate
# that compiles C in its build.rs) to pick `cl.exe` instead of
# scanning PATH and grabbing the first `gcc.exe` it finds — which on
# a host with MSYS2 / MinGW installed produces object files
# referencing `___chkstk_ms`, a GCC stack-probe symbol that
# `link.exe` rejects (LNK2019). vcvarsall puts cl.exe on PATH ahead
# of gcc, but cc-rs's auto-detection on Windows doesn't always
# honour PATH order, so we make the selection explicit.
#
# Env-var naming: cc-rs accepts the rustc triple with hyphens OR
# underscores; PowerShell `$env:NAME` syntax can't use hyphens, so
# we use the underscored form. cc-rs reads both.
$env:CC_x86_64_pc_windows_msvc  = 'cl.exe'
$env:CXX_x86_64_pc_windows_msvc = 'cl.exe'
$env:AR_x86_64_pc_windows_msvc  = 'lib.exe'

Write-Host '==> Step 1: build SPA bundle (web\dist\)'
& npm --prefix (Join-Path $RepoRoot 'web') ci
if ($LASTEXITCODE -ne 0) { throw "npm ci failed ($LASTEXITCODE)" }
& npm --prefix (Join-Path $RepoRoot 'web') run build
if ($LASTEXITCODE -ne 0) { throw "npm run build failed ($LASTEXITCODE)" }

Write-Host "==> Step 2: build server binary for $Target"
# We intentionally do NOT pass --no-default-features here — the
# server crate's defaults are what production ships (SQLCipher
# bundled, OpenSSL vendored). If the operator wants a host dev build
# without SQLCipher, they should run the workspace `cargo build`
# directly, not this release script.
& cargo build --release --target $Target -p execlaw
if ($LASTEXITCODE -ne 0) { throw "cargo build -p execlaw failed ($LASTEXITCODE)" }

Write-Host '==> Step 3: stage server binary for Tauri sidecar bundling'
New-Item -ItemType Directory -Force -Path $BundleBinDir -ErrorAction Stop | Out-Null
$ServerBin = Join-Path $RepoRoot "target\$Target\release\execlaw.exe"
if (-not (Test-Path -LiteralPath $ServerBin)) {
    throw "expected server binary at $ServerBin - did cargo build silently fail?"
}
# Tauri's `externalBin` field appends the rustc triple to the
# basename and looks for that exact path; the binary inside the
# installer ends up as `<INSTDIR>\execlaw.exe` (Tauri strips the
# triple at bundle time).
$ServerStaged = Join-Path $BundleBinDir "execlaw-$Target.exe"
Copy-Item -LiteralPath $ServerBin -Destination $ServerStaged -Force -ErrorAction Stop

Write-Host '==> Step 4: render icons from SVG sources'
# Tauri's `generate_context!` macro + the execlaw-tray-win crate's
# `include_bytes!("../icons/tray.ico")` need a rendered ICO at
# compile time. ImageMagick handles SVG -> ICO + SVG -> PNG.
$IconSvgColor = Join-Path $RepoRoot 'assets\execlaw-color.svg'
$IconSvgMono  = Join-Path $RepoRoot 'assets\execlaw.svg'

foreach ($svg in @($IconSvgColor, $IconSvgMono)) {
    if (-not (Test-Path -LiteralPath $svg)) {
        throw "missing SVG source: $svg"
    }
}

$Magick = Get-Command magick -ErrorAction SilentlyContinue
if (-not $Magick) {
    throw @"
ImageMagick (`magick`) not found on PATH.
Install it via one of:
  winget install ImageMagick.ImageMagick
  choco install imagemagick
  scoop install imagemagick
"@
}

New-Item -ItemType Directory -Force -Path $IconsDir -ErrorAction Stop | Out-Null

# App icon — single multi-resolution .ico. Generate each size as a
# PNG then composite them into the final ICO. Windows looks at
# 16/24/32/48/64/128/256 inside an ICO; matches the iconutil sizes
# the macOS build emits to .icns.
$AppIconStages = @()
foreach ($sz in 16, 24, 32, 48, 64, 128, 256) {
    $stage = Join-Path $IconsDir "icon-$sz.png"
    & $Magick.Source -background none -density 384 $IconSvgColor `
        -resize "${sz}x${sz}" -define 'png:color-type=6' $stage
    if ($LASTEXITCODE -ne 0) { throw "magick render of $stage failed" }
    $AppIconStages += $stage
}
& $Magick.Source @AppIconStages (Join-Path $IconsDir 'icon.ico')
if ($LASTEXITCODE -ne 0) { throw 'magick combine into icon.ico failed' }

# 1024 px PNG — Tauri's bundle.icon entry expects icon.png alongside
# the .ico (used as the runtime WebView window icon).
& $Magick.Source -background none -density 1024 $IconSvgColor `
    -resize 1024x1024 (Join-Path $IconsDir 'icon.png')
if ($LASTEXITCODE -ne 0) { throw 'magick render of icon.png failed' }

# Tray (notification-area) icon — 16/24/32 from the monochrome
# silhouette. Windows doesn't have macOS's template-image system, so
# the tray icon is the same shape on light + dark taskbars.
$TrayStages = @()
foreach ($sz in 16, 24, 32) {
    $stage = Join-Path $IconsDir "tray-$sz.png"
    & $Magick.Source -background none -density 384 $IconSvgMono `
        -resize "${sz}x${sz}" -define 'png:color-type=6' $stage
    if ($LASTEXITCODE -ne 0) { throw "magick render of $stage failed" }
    $TrayStages += $stage
}
& $Magick.Source @TrayStages (Join-Path $IconsDir 'tray.ico')
if ($LASTEXITCODE -ne 0) { throw 'magick combine into tray.ico failed' }

# Clean up the per-size intermediates; only the .ico + 1024.png ship.
foreach ($stage in $AppIconStages + $TrayStages) {
    Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
}

Write-Host '==> Step 4b: package plugin ZIPs + stage them into the installer'
# Build every plugin under `plugins\*\` into
# `dist\<id>-<version>.zip` and copy the ZIPs into
# `desktop-windows\src-tauri\resources\plugins\`.
# `tauri.conf.json`'s `bundle.resources` glob lifts them into the
# installer's data section; the NSIS template extracts them to
# `<INSTDIR>\resources\plugins\`, and the server's first-run
# bootstrap copies them out into `%USERPROFILE%\.execlaw\bundled-plugins\`
# so the SPA's "Install plugin" page can list them with a one-click
# install button. The Windows CI workflow also runs
# `package-plugins.ps1` on its own so the resulting `dist\*.zip`
# files attach to the GitHub Release for Linux operators.
& (Join-Path $PSScriptRoot 'package-plugins.ps1')
if ($LASTEXITCODE -ne 0) { throw "package-plugins.ps1 failed ($LASTEXITCODE)" }
if (Test-Path -LiteralPath $PluginStageDir) {
    Remove-Item -LiteralPath $PluginStageDir -Recurse -Force -ErrorAction Stop
}
New-Item -ItemType Directory -Force -Path $PluginStageDir -ErrorAction Stop | Out-Null
# Only the ZIPs themselves ship inside the installer; the .sha256
# sidecars stay in dist\ for the release attachments.
Get-ChildItem -LiteralPath (Join-Path $RepoRoot 'dist') -Filter '*.zip' |
    ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $PluginStageDir -ErrorAction Stop }
$stagedCount = (Get-ChildItem -LiteralPath $PluginStageDir -Filter '*.zip').Count
Write-Host "  staged $stagedCount ZIPs into resources\plugins\"

Write-Host '==> Step 5: tauri build'
Push-Location $TauriDir -ErrorAction Stop
try {
    & cargo tauri build --target $Target
    if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$BundleRoot = Join-Path $TauriDir "target\$Target\release\bundle"
$NsisDir    = Join-Path $BundleRoot 'nsis'

if (-not (Test-Path -LiteralPath $NsisDir)) {
    throw "expected NSIS output dir at $NsisDir - did the bundler fail?"
}

$InstallerExe = Get-ChildItem -LiteralPath $NsisDir -Filter 'execlaw_*-setup.exe' |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $InstallerExe) {
    throw "no execlaw_*-setup.exe found under $NsisDir"
}

Write-Host ''
Write-Host '==> Done.'
Write-Host "    installer: $($InstallerExe.FullName)"
Write-Host ''
Write-Host '    Unsigned distribution: SmartScreen may warn on first launch.'
Write-Host '    See docs/setup-windows.md (if/when present) for SmartScreen bypass instructions.'
