# execlaw dev server — Rust hot-reload via cargo-watch.
#
# Run from the repo root:
#   pwsh scripts/dev-server.ps1
# or from cmd:
#   powershell -ExecutionPolicy Bypass -File scripts\dev-server.ps1
#
# Mirrors `scripts/dev-server.sh` for Windows operators who'd rather
# stay in PowerShell than spawn Git Bash. See the .sh variant for the
# full rationale.

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path "$PSScriptRoot\.."
Set-Location $RepoRoot

$Bind = if ($env:EXECLAW_DEV_BIND) { $env:EXECLAW_DEV_BIND } else { "127.0.0.1:3031" }

$DbFlag = @()
if ($env:EXECLAW_DEV_DB) {
    $DbFlag = @("--db", $env:EXECLAW_DEV_DB)
}

$FeaturesFlag = @()
if ($env:EXECLAW_WATCH_FEATURES) {
    $FeaturesFlag = @("--features", $env:EXECLAW_WATCH_FEATURES)
}

# Build the cargo subcommand string. cargo-watch's `-x` takes a
# single string, not an argv array, so we shell-quote here.
$ServeCmd = "run -p execlaw $($FeaturesFlag -join ' ') -- serve --bind $Bind --no-encrypt $($DbFlag -join ' ')"

# `--clear` only works with a TTY; skip it when the script is piped
# (e.g. running in the background) so cargo-watch doesn't error with
# "The handle is invalid" on Windows.
$WatchFlags = @("--why", "--watch", "crates", "--watch", "Cargo.toml", "--watch", "Cargo.lock")
if (($env:EXECLAW_DEV_CLEAR -ne "0") -and ([Console]::IsOutputRedirected -eq $false)) {
    $WatchFlags += "--clear"
}

& cargo watch @WatchFlags -x $ServeCmd
