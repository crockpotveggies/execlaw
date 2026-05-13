# trace-turn.ps1 — readable pretty-printer for the `agent::turn_timing`
# log surface, in pure PowerShell (no bash / Python dependency).
#
# Usage:
#     pwsh -File scripts/trace-turn.ps1
#     # or, from PowerShell already:
#     ./scripts/trace-turn.ps1
#
# Prerequisite: dev-server.sh must be running with:
#     $env:RUST_LOG = "info,agent::turn_timing=debug"
#     bash scripts/dev-server.sh
#
# Override the log dir via `$env:EXECLAW_LOG_DIR` if you run with a
# non-default location; otherwise the script resolves
# `%USERPROFILE%\.execlaw\logs` automatically. Press Ctrl-C to stop.

$ErrorActionPreference = 'Stop'

# --- resolve log dir ---------------------------------------------------------
if ($env:EXECLAW_LOG_DIR) {
    $logDir = $env:EXECLAW_LOG_DIR
} elseif ($env:USERPROFILE) {
    $logDir = Join-Path $env:USERPROFILE '.execlaw\logs'
} else {
    # Linux/macOS fallback — pwsh can run on either too.
    $logDir = Join-Path $HOME '.execlaw/logs'
}

# Today in UTC — the rotating log appender uses UTC dates.
$today = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
$logFile = Join-Path $logDir "execlaw.jsonl.$today"

Write-Host "# scripts/trace-turn.ps1"
Write-Host "#"
Write-Host "# Before sending your 'hi' — make sure dev-server was started with:"
Write-Host "#"
Write-Host "#   `$env:RUST_LOG = `"info,agent::turn_timing=debug`""
Write-Host "#   bash scripts/dev-server.sh"
Write-Host "#"
Write-Host "# Tailing: $logFile"
Write-Host "# Press Ctrl-C to stop."
Write-Host ""

if (-not (Test-Path -LiteralPath $logFile)) {
    Write-Error "log file not found yet: $logFile`nstart dev-server first; the file is created on the first log line."
    exit 1
}

# --- tail + filter -----------------------------------------------------------
# `Get-Content -Wait` is PowerShell's equivalent of `tail -F`. `-Tail 0`
# starts at the current end so we don't replay the whole day's log on
# startup. Each line is parsed as JSON; non-turn-timing rows are
# dropped silently so the view isn't swamped by sidecar pings and
# tower_http request logs.
$lastConv = $null

Get-Content -LiteralPath $logFile -Wait -Tail 0 | ForEach-Object {
    $raw = $_.Trim()
    if ([string]::IsNullOrWhiteSpace($raw)) { return }

    $obj = $null
    try {
        $obj = $raw | ConvertFrom-Json -ErrorAction Stop
    } catch {
        return
    }

    if ($obj.target -ne 'agent::turn_timing') { return }

    $fields = $obj.fields
    $msg = $fields.message
    $ts  = if ($obj.timestamp) { $obj.timestamp.Substring(11, 12) } else { '' }
    $conv = if ($fields.conversation_id) { $fields.conversation_id } else { '?' }

    if ($conv -ne $lastConv) {
        Write-Host ""
        Write-Host "--- conversation $conv ---" -ForegroundColor Cyan
        $lastConv = $conv
    }

    # Build the kv-pair tail in a stable order so the trace lines up
    # visually across consecutive events. `message` and
    # `conversation_id` are already printed as the headline and the
    # section header respectively, so skip them.
    $parts = New-Object System.Collections.Generic.List[string]
    foreach ($prop in $fields.PSObject.Properties) {
        if ($prop.Name -eq 'message' -or $prop.Name -eq 'conversation_id') { continue }
        $v = $prop.Value
        # Numeric ms fields get a unit suffix for readability.
        if (($prop.Name -like '*_ms' -or $prop.Name -eq 'ms') -and ($v -is [int] -or $v -is [long] -or $v -is [double])) {
            $parts.Add("$($prop.Name)=$($v)ms") | Out-Null
        } else {
            $parts.Add("$($prop.Name)=$v") | Out-Null
        }
    }
    $tail = if ($parts.Count -gt 0) { '  ' + ($parts -join '  ') } else { '' }

    Write-Host ("  {0}  {1}{2}" -f $ts, $msg, $tail)
}
