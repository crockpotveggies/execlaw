<#
.SYNOPSIS
    Package every operator-installable plugin into dist\<plugin-id>-<version>.zip.

.DESCRIPTION
    PowerShell port of scripts/package-plugins.sh — kept in lockstep
    with the bash version so a contributor switching between
    Windows-from-Powershell and macOS-from-bash gets the same
    artefacts.

    Plugins live under plugins\<dir>\ with at least:
      plugins\<dir>\plugin.toml      — manifest (sourced for id+version)
      plugins\<dir>\main.rhai        — script body (optional for some plugins)
      plugins\<dir>\ui\panel.tsx     — optional React panel (built to panel.js)

    This script:
      1. Enumerates every plugins\* with a `plugin.toml`.
      2. Builds each plugin's UI via `scripts/build-plugin-ui.mjs --all`
         so any ui\panel.tsx that hasn't been recompiled lands fresh.
      3. Zips the plugin dir into dist\<plugin-id>-<version>.zip,
         excluding dev noise (.git, node_modules, __pycache__, *.pyc,
         .DS_Store, target/, dist/, *.log, the source ui\panel.tsx
         itself once we have ui\panel.js).
      4. Emits a SHA-256 sidecar.

    Skips:
      * plugins\_shared\        — shared library, not a plugin

    Idempotent: re-running overwrites the previous ZIP. The
    windows-bundle CI workflow calls this once before attaching
    artifacts; build-windows.ps1 calls it before staging plugin ZIPs
    into the installer's resources\ dir.
#>

[CmdletBinding()]
param()

# We deliberately do NOT set $ErrorActionPreference = 'Stop' here.
# On Windows PowerShell 5.1, that turns every line a native command
# writes to stderr into a terminating `NativeCommandError`, which
# trips on benign tool output like `npm`'s deprecation warnings.
# Use `throw` (always terminating) and explicit `-ErrorAction Stop`
# on cmdlets that need it.

# Move to repo root regardless of where the script was invoked from.
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..') -ErrorAction Stop
Set-Location $RepoRoot -ErrorAction Stop

$DistDir = Join-Path $RepoRoot 'dist'
New-Item -ItemType Directory -Force -Path $DistDir -ErrorAction Stop | Out-Null

# Make sure the root devDependencies (specifically `esbuild`, which
# `scripts/build-plugin-ui.mjs` imports) are installed. The per-
# platform build scripts (`build-mac.sh`, `build-linux.sh`,
# `build-windows.ps1`) only run `npm ci` under `web/`, which doesn't
# populate the root `node_modules`. On a fresh clone — including CI
# runners with a cold cache — the next step would fail with
# `Cannot find package 'esbuild'`. Mirrors the same guard in
# `package-plugins.sh` so the two scripts stay in lock-step.
$RootNodeModules     = Join-Path $RepoRoot 'node_modules'
$RootEsbuildDir      = Join-Path $RootNodeModules 'esbuild'
if (-not (Test-Path -LiteralPath $RootNodeModules) -or `
    -not (Test-Path -LiteralPath $RootEsbuildDir)) {
    Write-Host '==> Step 0: install root devDependencies (esbuild for plugin UI build)'
    # `npm ci` honours package-lock.json and is a no-op when
    # `node_modules/` is already in sync. `--no-audit --no-fund`
    # matches the bash version's flags and trims log noise.
    & npm ci --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) {
        throw "npm ci exited $LASTEXITCODE"
    }
}

Write-Host '==> Step 1: build every plugin''s UI panel'
# `--all` only rebuilds plugins that declare ui_panels; the rest are
# no-ops. Failure here propagates; if a plugin's TS doesn't compile,
# its ZIP would ship broken JS.
& node (Join-Path 'scripts' 'build-plugin-ui.mjs') --all
if ($LASTEXITCODE -ne 0) {
    throw "build-plugin-ui.mjs exited $LASTEXITCODE"
}

Write-Host '==> Step 2: enumerate plugins + package each'

# Extract `id = "..."` or `version = "..."` from a plugin.toml,
# scoped to the `[plugin]` table. Cheap regex parse — sufficient for
# the constrained TOML shape every plugin uses (top-level `[plugin]`
# table with quoted strings). Mirrors the awk-based bash version's
# scoping rule so the two scripts can't drift.
function Read-PluginManifestField {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $Field
    )
    $inPlugin = $false
    foreach ($raw in Get-Content -LiteralPath $Path) {
        $line = $raw.TrimStart()
        if ($line -match '^\[plugin\]') { $inPlugin = $true; continue }
        if ($line -match '^\[')         { $inPlugin = $false }
        if (-not $inPlugin) { continue }
        # Match `field = "value"` — first occurrence wins. Single-
        # quoted TOML values would need a separate branch, but every
        # plugin in-tree uses double quotes.
        if ($line -match ('^' + [regex]::Escape($Field) + '\s*=\s*"([^"]*)"')) {
            return $Matches[1]
        }
    }
    return $null
}

# Files / directories we never ship in a plugin ZIP. Same set as the
# bash version, expressed as wildcard patterns that
# Compress-Archive's -DestinationPath model doesn't natively
# understand — we filter the source file list ourselves before
# handing it to Compress-Archive.
$ExcludePatterns = @(
    '*\.git\*',         '\.git\*'
    '*\node_modules\*', '\node_modules\*'
    '*\__pycache__\*',  '\__pycache__\*'
    '*.pyc'
    '*\target\*',       '\target\*'
    '*\.DS_Store',      '\.DS_Store'
    '*\dist\*',         '\dist\*'
    '*.log'
    '*.tsbuildinfo'
    # Source TS/TSX dropped — only the built JS ships. Drop source
    # maps too; they're 4x the size of the JS and only useful to a
    # developer rebuilding the plugin.
    '*\ui\panel.ts'
    '*\ui\panel.tsx'
    '*\ui\panel.js.map'
)

function Test-IsExcluded {
    param([string] $RelativePath)
    foreach ($pat in $ExcludePatterns) {
        if ($RelativePath -like $pat) { return $true }
    }
    return $false
}

$packagedCount = 0
$pluginDirs = Get-ChildItem -LiteralPath (Join-Path $RepoRoot 'plugins') -Directory -ErrorAction SilentlyContinue

foreach ($dir in $pluginDirs) {
    $name = $dir.Name
    # _shared isn't a plugin — skip silently.
    if ($name -eq '_shared') { continue }

    $manifest = Join-Path $dir.FullName 'plugin.toml'
    if (-not (Test-Path -LiteralPath $manifest)) {
        Write-Host "  ${name}: no plugin.toml - skipping"
        continue
    }
    $id      = Read-PluginManifestField -Path $manifest -Field 'id'
    $version = Read-PluginManifestField -Path $manifest -Field 'version'
    if ([string]::IsNullOrWhiteSpace($id) -or [string]::IsNullOrWhiteSpace($version)) {
        Write-Host "  ${name}: manifest missing id/version - skipping"
        continue
    }

    $zipName = "$id-$version.zip"
    $outPath = Join-Path $DistDir $zipName
    if (Test-Path -LiteralPath $outPath) {
        Remove-Item -LiteralPath $outPath -Force -ErrorAction Stop
    }

    # Enumerate every file under the plugin dir, filter excludes,
    # then add to a fresh zip with paths rooted at the plugin dir
    # (so the archive's top-level entries are `plugin.toml`,
    # `main.rhai`, etc. — the format `stage_zip` reads at
    # crates/plugin-sdk/src/zip_stage.rs:76).
    $pluginRoot = $dir.FullName
    $allFiles = Get-ChildItem -LiteralPath $pluginRoot -Recurse -File
    $keptFiles = @()
    foreach ($f in $allFiles) {
        # `Resolve-Path -Relative` is anchored to the current
        # working directory; we want it anchored to the plugin
        # root so the exclude wildcards match the layout the
        # zip will have. Compute the relative path manually.
        $rel = $f.FullName.Substring($pluginRoot.Length).TrimStart('\', '/')
        $relForMatch = '\' + $rel
        if (Test-IsExcluded -RelativePath $relForMatch) { continue }
        $keptFiles += $f
    }

    if ($keptFiles.Count -eq 0) {
        Write-Host "  ${name}: nothing to ship after excludes - skipping"
        continue
    }

    # Compress-Archive preserves the relative path from the current
    # directory at compression time. Push into the plugin dir so the
    # archive entries land at plugin-relative paths.
    # Write the archive via the .NET `ZipArchive` API directly.
    # PowerShell's `Compress-Archive` accepts an array of file
    # paths but flattens the directory structure into the archive
    # root, which produces a broken plugin zip (panel.js ends up
    # at `/panel.js` instead of `/ui/panel.js` like the manifest's
    # `ui_panels[].entry` points at). The .NET API lets us choose
    # each entry's archive-relative path explicitly. Mirrors what
    # `zip -r` does in package-plugins.sh.
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipStream = [System.IO.File]::Create($outPath)
    try {
        $archive = New-Object System.IO.Compression.ZipArchive(
            $zipStream,
            [System.IO.Compression.ZipArchiveMode]::Create
        )
        try {
            foreach ($file in $keptFiles) {
                $entryName = $file.FullName.Substring($pluginRoot.Length).TrimStart('\', '/')
                # Zip standard uses forward slashes in entry names,
                # regardless of host OS. Normalising here makes the
                # archive cross-platform identical to the bash port.
                $entryName = $entryName -replace '\\', '/'
                $entry = $archive.CreateEntry(
                    $entryName,
                    [System.IO.Compression.CompressionLevel]::Optimal
                )
                $entryStream = $entry.Open()
                try {
                    $src = [System.IO.File]::OpenRead($file.FullName)
                    try {
                        $src.CopyTo($entryStream)
                    } finally {
                        $src.Dispose()
                    }
                } finally {
                    $entryStream.Dispose()
                }
            }
        } finally {
            $archive.Dispose()
        }
    } finally {
        $zipStream.Dispose()
    }

    # SHA-256 sidecar — same `<hash>  <basename>` format
    # `shasum -a 256 -c` expects. Get-FileHash returns uppercase;
    # `shasum -c` is case-insensitive but the reference output from
    # macOS is lowercase, so normalise to match.
    $hash = (Get-FileHash -LiteralPath $outPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$outPath.sha256" -Value "$hash  $zipName" -Encoding ascii -NoNewline

    $size = (Get-Item -LiteralPath $outPath).Length
    $sizeKb = [int]($size / 1024)
    Write-Host ("  {0}: {1} ({2} KB)" -f $name, $zipName, $sizeKb)
    $packagedCount++
}

Write-Host ''
Write-Host "==> $packagedCount plugin(s) packaged under dist\"
Get-ChildItem -LiteralPath $DistDir -Filter '*.zip' | ForEach-Object {
    Write-Host ("  {0}" -f $_.Name)
}
