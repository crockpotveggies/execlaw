<#
.SYNOPSIS
    Rebuild every plugin ZIP and reinstall it into a running dev server.

.DESCRIPTION
    Phase E of the Flows middleware redesign added default flows to 9
    plugins. After bumping their versions + rebuilding the ZIPs, the
    dev server still has the OLD versions installed — the new flow
    rows don't land until each plugin is reinstalled through the
    `[[default_automations]]` importer at install / upgrade time.

    This script automates that loop:

      1. Run `scripts/package-plugins.ps1` to rebuild every plugin's
         UI panel + ZIP.
      2. Mirror every `dist/*.zip` into the operator's
         `~/.execlaw/bundled-plugins/` directory (the source the
         `/api/admin/plugins/install-bundled` endpoint reads from).
      3. Log in to the running dev server (`POST /api/login`) to
         obtain a JWT.
      4. For every ZIP, POST
         `/api/admin/plugins/install-bundled?file=<zip>&if_existing=upgrade`
         which routes through the same staging + hook-registration
         path as a manual operator install.

    Requires the dev server to be running (default 127.0.0.1:3031)
    and the operator password to be available via the
    `$env:EXECLAW_OPERATOR_PASSWORD` env var. The username defaults
    to `admin`; override with `$env:EXECLAW_OPERATOR_USERNAME`.

    Optional: pass `-OnlyChanged` to reinstall only the 9 Phase-E
    plugins (signal, whatsapp, sms-socket, discord, slack,
    google-apps, google-places, open-meteo, finance-yahoo).
#>

[CmdletBinding()]
param(
    [string] $ServerUrl = $(if ($env:EXECLAW_SERVER_URL) { $env:EXECLAW_SERVER_URL } else { 'http://127.0.0.1:3031' }),
    [string] $Username  = $(if ($env:EXECLAW_OPERATOR_USERNAME) { $env:EXECLAW_OPERATOR_USERNAME } else { 'admin' }),
    [switch] $OnlyChanged,
    [switch] $SkipBuild
)

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..') -ErrorAction Stop
Set-Location $RepoRoot -ErrorAction Stop

# --- 1. Build every plugin's ZIP ---------------------------------------
if (-not $SkipBuild) {
    Write-Host '==> Step 1: package all plugins (rebuilds UI panels + ZIPs)'
    & (Join-Path 'scripts' 'package-plugins.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "package-plugins.ps1 exited $LASTEXITCODE"
    }
} else {
    Write-Host '==> Step 1: skipping package step (-SkipBuild set)'
}

# --- 2. Mirror dist/*.zip into ~/.execlaw/bundled-plugins/ -------------
$DataDir = if ($env:EXECLAW_DATA_DIR) {
    $env:EXECLAW_DATA_DIR
} elseif ($env:USERPROFILE) {
    Join-Path $env:USERPROFILE '.execlaw'
} else {
    Join-Path $HOME '.execlaw'
}
$BundledDir = Join-Path $DataDir 'bundled-plugins'
New-Item -ItemType Directory -Force -Path $BundledDir -ErrorAction Stop | Out-Null
Write-Host "==> Step 2: mirror dist/*.zip → $BundledDir"

# The 9 plugins that gained default flows in Phase E. When -OnlyChanged
# is passed we filter to these; otherwise we mirror everything.
$ChangedIds = @(
    'signal', 'whatsapp', 'sms-socket', 'discord', 'slack',
    'google-apps', 'google-places', 'open-meteo', 'finance-yahoo'
)

# Build a map of plugin_id → newest ZIP path in dist/. The ZIP naming
# convention is `<id>-<version>.zip`. We use the file's mtime as the
# tiebreaker so re-runs that produce the same version (no bump) still
# pick up the fresh build.
$DistDir = Join-Path $RepoRoot 'dist'
$zipsByPlugin = @{}
Get-ChildItem -LiteralPath $DistDir -Filter '*.zip' -File | ForEach-Object {
    $name = $_.BaseName  # strip .zip
    # `<id>-<version>` where id can contain hyphens. Split at the last
    # hyphen so a `finance-yahoo-0.2.2` parses as id=finance-yahoo +
    # version=0.2.2.
    $lastHyphen = $name.LastIndexOf('-')
    if ($lastHyphen -lt 0) { return }
    $id = $name.Substring(0, $lastHyphen)
    # `plugin-hello` is the special case where the ZIP id and the
    # manifest id diverge (the ZIP carries the `plugin-` prefix; the
    # manifest has `id = "hello"`). The install endpoint reads the
    # manifest, so we don't have to map it here.
    if ($zipsByPlugin.ContainsKey($id)) {
        $existing = $zipsByPlugin[$id]
        if ($_.LastWriteTime -gt $existing.LastWriteTime) {
            $zipsByPlugin[$id] = $_
        }
    } else {
        $zipsByPlugin[$id] = $_
    }
}

# Copy the chosen ZIPs into the bundled dir.
$toInstall = @()
foreach ($entry in $zipsByPlugin.GetEnumerator()) {
    $id = $entry.Key
    $file = $entry.Value
    if ($OnlyChanged -and ($ChangedIds -notcontains $id)) {
        continue
    }
    $dest = Join-Path $BundledDir $file.Name
    Copy-Item -LiteralPath $file.FullName -Destination $dest -Force -ErrorAction Stop
    Write-Host "    mirrored $($file.Name)"
    $toInstall += $file.Name
}

if ($toInstall.Count -eq 0) {
    Write-Host 'Nothing to install — exiting.'
    exit 0
}

# --- 3. Log in to the dev server --------------------------------------
$pw = $env:EXECLAW_OPERATOR_PASSWORD
if ([string]::IsNullOrEmpty($pw)) {
    throw "EXECLAW_OPERATOR_PASSWORD env var must be set (operator login password for the dev server)"
}

Write-Host "==> Step 3: log in to $ServerUrl as $Username"
$loginBody = @{ username = $Username; password = $pw } | ConvertTo-Json
$loginResp = $null
try {
    $loginResp = Invoke-RestMethod `
        -Uri "$ServerUrl/api/login" `
        -Method Post `
        -ContentType 'application/json' `
        -Body $loginBody `
        -ErrorAction Stop
} catch {
    throw "Login failed at $ServerUrl/api/login : $($_.Exception.Message)"
}
$token = $loginResp.access_token
if ([string]::IsNullOrEmpty($token)) {
    throw "Login response did not include access_token. Response: $($loginResp | ConvertTo-Json -Compress)"
}
Write-Host '    got access_token'

# --- 4. Reinstall each plugin via install-bundled ---------------------
Write-Host "==> Step 4: reinstall $($toInstall.Count) plugin(s) via install-bundled"
$installed = 0
$failed = @()
foreach ($zipName in $toInstall) {
    $uri = "$ServerUrl/api/admin/plugins/install-bundled?file=" + [uri]::EscapeDataString($zipName) + "&if_existing=upgrade"
    try {
        $resp = Invoke-RestMethod `
            -Uri $uri `
            -Method Post `
            -Headers @{ Authorization = "Bearer $token" } `
            -ErrorAction Stop
        Write-Host ("    [OK] {0} → plugin_id={1} version={2}" -f $zipName, $resp.plugin_id, $resp.version)
        $installed++
    } catch {
        $msg = $_.Exception.Message
        Write-Host ("    [FAIL] {0}: {1}" -f $zipName, $msg) -ForegroundColor Red
        $failed += [PSCustomObject]@{ Zip = $zipName; Error = $msg }
    }
}

Write-Host ''
Write-Host "==> Done. $installed plugin(s) reinstalled."
if ($failed.Count -gt 0) {
    Write-Host "$($failed.Count) failure(s):" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $($_.Zip): $($_.Error)" -ForegroundColor Red }
    exit 1
}
