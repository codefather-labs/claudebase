# ============================================================================
# claudebase installer (Windows PowerShell)
# ============================================================================
#
# Installs the claudebase CLI binary + agent toolkit (rules, commands, agents)
# into %USERPROFILE%\.claude\.
#
# Usage:
#   .\install.ps1                  Install user-level binary + prompts
#   .\install.ps1 -Yes             Skip confirmation prompts
#   .\install.ps1 -Local           Use local checkout (skip git clone)
#   .\install.ps1 -Help            Show help
#
# Invoke via PowerShell directly:
#   powershell.exe -ExecutionPolicy Bypass -File .\install.ps1
# Or via the cmd.exe wrapper (when shipped with one).
# ============================================================================

[CmdletBinding()]
param(
    [switch]$Yes,
    [switch]$Local,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'

# ============================================================================
# Constants
# ============================================================================
$Script:ClaudebaseVersion       = '0.5.0'
$Script:ClaudebasePdfiumVersion = 'chromium/7802'
$Script:RepoUrl                 = 'https://github.com/codefather-labs/claudebase.git'
$Script:ReleaseBase             = 'https://github.com/codefather-labs/claudebase/releases/download'

$Script:ClaudeDir = Join-Path $env:USERPROFILE '.claude'
$Script:ScriptDir = $null

# ============================================================================
# Logging
# ============================================================================
function Write-Info { Write-Host "[INFO]  $($args[0])" -ForegroundColor Blue }
function Write-Ok   { Write-Host "  [OK]  $($args[0])" -ForegroundColor Green }
function Write-Warn { Write-Host "[WARN]  $($args[0])" -ForegroundColor Yellow }
function Write-Err  { Write-Host "[ERROR] $($args[0])" -ForegroundColor Red }

function Show-Help {
@"
claudebase installer (Windows PowerShell)

Installs the claudebase CLI binary and agent toolkit (rules, commands,
agents) into %USERPROFILE%\.claude\.

USAGE:
  .\install.ps1 [-Yes] [-Local] [-Help]

OPTIONS:
  -Yes        Skip confirmation prompts
  -Local      Use local checkout (skip git clone of $($Script:RepoUrl))
  -Help       Show this help

WHAT GETS INSTALLED:
  %USERPROFILE%\.claude\tools\claudebase\claudebase.exe   CLI binary
  %USERPROFILE%\.claude\tools\claudebase\pdfium\bin\pdfium.dll
  %USERPROFILE%\.claude\tools\claudebase\models\          e5 encoder cache
  %USERPROFILE%\.claude\rules\        knowledge-base, knowledge-base-tool, tool-limitations
  %USERPROFILE%\.claude\commands\     knowledge-ingest, reflect, consolidate
  %USERPROFILE%\.claude\agents\       reflection (Drift), consolidator (Mnem)
  %USERPROFILE%\.claude\bin\claudebase.cmd  Global wrapper (User PATH)
"@ | Write-Host
}

function Confirm-Action {
    param([string]$Prompt)
    if ($Yes) { return $true }
    $reply = Read-Host "$Prompt [y/N]"
    return ($reply -match '^[yY]$' -or $reply -match '^(yes|YES)$')
}

# ============================================================================
# Source-dir resolution
# ============================================================================
function Get-SourceDir {
    if ($Local) {
        $Script:ScriptDir = Split-Path -Parent $PSCommandPath
        $rulesDir = Join-Path $Script:ScriptDir 'rules'
        $commandsDir = Join-Path $Script:ScriptDir 'commands'
        $agentsDir = Join-Path $Script:ScriptDir 'agents'
        if (-not (Test-Path $rulesDir) -or -not (Test-Path $commandsDir) -or -not (Test-Path $agentsDir)) {
            Write-Err "-Local requires running from a claudebase checkout root (with rules\, commands\, agents\)"
            exit 1
        }
    } else {
        $Script:ScriptDir = Join-Path $env:TEMP ("claudebase-clone-" + [guid]::NewGuid().ToString())
        Write-Info "Cloning claudebase from $($Script:RepoUrl)..."
        try {
            & git clone --depth 1 --quiet $Script:RepoUrl $Script:ScriptDir 2>$null
            if ($LASTEXITCODE -ne 0) { throw "git clone failed" }
        } catch {
            Write-Err "Failed to clone claudebase. Check internet connection and that git is on PATH."
            exit 1
        }
        Write-Ok "Repository cloned"
    }
}

# ============================================================================
# Copy prompts (rules + commands + agents) into ~/.claude/
# ============================================================================
function Install-Prompts {
    foreach ($sub in 'rules', 'commands', 'agents') {
        $dest = Join-Path $Script:ClaudeDir $sub
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Get-ChildItem (Join-Path $Script:ScriptDir "$sub\*.md") -ErrorAction SilentlyContinue | ForEach-Object {
            Copy-Item -Force $_.FullName $dest
            Write-Ok "$sub\$($_.Name)"
        }
    }
}

# ============================================================================
# Download claudebase.exe from GitHub releases
# ============================================================================
function Install-Binary {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64') {
        Write-Warn "32-bit Windows is not supported by claudebase; skipping binary install"
        return
    }
    $platform = 'windows-x64'

    $targetDir = Join-Path $Script:ClaudeDir 'tools\claudebase'
    New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
    $targetBin = Join-Path $targetDir 'claudebase.exe'

    if (Test-Path $targetBin) {
        try {
            $existingVer = (& $targetBin --version 2>$null) -replace '^claudebase ', ''
            if ($existingVer -eq $Script:ClaudebaseVersion) {
                Write-Ok "claudebase binary already at version $($Script:ClaudebaseVersion)"
                return
            }
        } catch {}
    }

    $url = "$($Script:ReleaseBase)/claudebase-v$($Script:ClaudebaseVersion)/claudebase-$platform.exe"
    $tmp = Join-Path $env:TEMP ("claudebase-" + [guid]::NewGuid().ToString() + ".exe")

    Write-Info "Downloading claudebase.exe v$($Script:ClaudebaseVersion)..."
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing -MaximumRedirection 5 -TimeoutSec 120
    } catch {
        Write-Warn "claudebase binary download failed: $($_.Exception.Message)"
        Write-Warn "  Build from source: cargo install --git $($Script:RepoUrl)"
        if (Test-Path $tmp) { Remove-Item $tmp -Force }
        return
    }

    try {
        $smokeOutput = & $tmp --version 2>&1
        if ($LASTEXITCODE -ne 0) { throw "smoke test failed" }
    } catch {
        Write-Warn "downloaded binary failed --version smoke test; not installing"
        Remove-Item $tmp -Force -ErrorAction SilentlyContinue
        return
    }

    Move-Item -Force $tmp $targetBin
    Write-Ok "tools\claudebase\claudebase.exe ($platform)"
}

# ============================================================================
# Register `claudebase` global wrapper (claudebase.cmd in ~/.claude/bin/)
# ============================================================================
function Register-Alias {
    $targetBin = Join-Path $Script:ClaudeDir 'tools\claudebase\claudebase.exe'
    if (-not (Test-Path $targetBin)) {
        Write-Warn "alias: target binary not found; skipping"
        return
    }

    $binDir = Join-Path $Script:ClaudeDir 'bin'
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    $wrapperPath = Join-Path $binDir 'claudebase.cmd'

    # Write the wrapper batch file. Uses %~* forwarding so all args pass through.
    @"
@echo off
"$targetBin" %*
"@ | Set-Content -Path $wrapperPath -Encoding ASCII

    Write-Ok "wrapper: $wrapperPath"

    # Append $binDir to User PATH if not already present.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $binDir })) {
        [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $binDir), 'User')
        Write-Ok "User PATH updated (open a new terminal for the change to take effect)"
    } else {
        Write-Ok "User PATH already contains $binDir"
    }
}

# ============================================================================
# Merge Bash allowlist into settings.json (for Claude Code permission gating)
# ============================================================================
function Register-BashAllowlist {
    $settings = Join-Path $Script:ClaudeDir 'settings.json'
    $entry = '~/.claude/tools/claudebase/claudebase *'

    if (-not (Test-Path $settings)) {
        $obj = @{ permissions = @{ allow = @($entry) } }
        $obj | ConvertTo-Json -Depth 10 | Set-Content -Path $settings -Encoding UTF8
        Write-Ok "settings.json (created with claudebase allowlist)"
        return
    }

    try {
        $obj = Get-Content $settings -Raw | ConvertFrom-Json
        if ($null -eq $obj.permissions) { $obj | Add-Member -NotePropertyName permissions -NotePropertyValue @{ allow = @() } -Force }
        if ($null -eq $obj.permissions.allow) { $obj.permissions | Add-Member -NotePropertyName allow -NotePropertyValue @() -Force }

        if ($obj.permissions.allow -notcontains $entry) {
            $obj.permissions.allow = @($obj.permissions.allow) + $entry
            $obj | ConvertTo-Json -Depth 10 | Set-Content -Path $settings -Encoding UTF8
            Write-Ok "settings.json (claudebase allowlist merged)"
        } else {
            Write-Ok "settings.json already contains claudebase allowlist"
        }
    } catch {
        Write-Warn "settings.json merge failed: $($_.Exception.Message); add manually: $entry"
    }
}

# ============================================================================
# Download + extract pdfium.dll
# ============================================================================
function Install-Pdfium {
    $targetDir = Join-Path $Script:ClaudeDir 'tools\claudebase\pdfium'
    $binDir = Join-Path $targetDir 'bin'
    $sentinel = Join-Path $targetDir '.version'

    if (Test-Path $sentinel) {
        $existing = (Get-Content $sentinel -Raw).Trim()
        if ($existing -eq $Script:ClaudebasePdfiumVersion) {
            Write-Ok "pdfium already at version $($Script:ClaudebasePdfiumVersion)"
            return
        }
    }

    $asset = 'pdfium-win-x64.tgz'
    $url = "https://github.com/bblanchon/pdfium-binaries/releases/download/$($Script:ClaudebasePdfiumVersion)/$asset"

    $tmpArchive = Join-Path $env:TEMP ("pdfium-" + [guid]::NewGuid().ToString() + ".tgz")
    $staging = Join-Path $env:TEMP ("pdfium-stage-" + [guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Force -Path $staging | Out-Null

    try {
        Invoke-WebRequest -Uri $url -OutFile $tmpArchive -UseBasicParsing -MaximumRedirection 5 -TimeoutSec 120
    } catch {
        Write-Warn "pdfium download failed: $($_.Exception.Message)"
        Write-Warn "  PDF extraction will fail at runtime; install manually or skip PDF ingestion"
        Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
        return
    }

    # tar.exe is built into Windows 10 1803+
    if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) {
        Write-Warn "tar.exe not found (Windows 10 1803+ required); skipping pdfium install"
        Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
        Remove-Item -Force $tmpArchive -ErrorAction SilentlyContinue
        return
    }

    try {
        & tar.exe -xzf $tmpArchive -C $staging
        if ($LASTEXITCODE -ne 0) { throw "tar exit $LASTEXITCODE" }
    } catch {
        Write-Warn "pdfium extraction failed: $($_.Exception.Message)"
        Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
        Remove-Item -Force $tmpArchive -ErrorAction SilentlyContinue
        return
    }

    $extractedDll = Get-ChildItem -Path $staging -Recurse -Filter 'pdfium.dll' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $extractedDll) {
        Write-Warn "no pdfium.dll found in extracted archive"
        Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
        Remove-Item -Force $tmpArchive -ErrorAction SilentlyContinue
        return
    }

    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item -Force $extractedDll.FullName (Join-Path $binDir 'pdfium.dll')
    Set-Content -Path $sentinel -Value $Script:ClaudebasePdfiumVersion -Encoding ASCII

    Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
    Remove-Item -Force $tmpArchive -ErrorAction SilentlyContinue

    Write-Ok "pdfium installed: windows-x64 (version $($Script:ClaudebasePdfiumVersion))"
}

# ============================================================================
# Pre-warm e5 encoder
# ============================================================================
function Preload-Encoder {
    $bin = Join-Path $Script:ClaudeDir 'tools\claudebase\claudebase.exe'
    if (-not (Test-Path $bin)) { return }

    Write-Info "Pre-loading e5-multilingual-small encoder (~120 MB on first run)..."
    try {
        & $bin warmup --quiet 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Ok "encoder ready (cached at ~\.claude\tools\claudebase\models\)"
        } else {
            Write-Warn "encoder pre-load returned exit $LASTEXITCODE; fastembed will retry on first ingest"
        }
    } catch {
        Write-Warn "encoder pre-load failed: $($_.Exception.Message); fastembed will retry"
    }
}

# ============================================================================
# Main
# ============================================================================
if ($Help) { Show-Help; exit 0 }

Write-Host ""
Write-Host "============================================" -ForegroundColor White
Write-Host "  claudebase v$($Script:ClaudebaseVersion) — installer (Windows)" -ForegroundColor White
Write-Host "============================================" -ForegroundColor White
Write-Host ""
Write-Host "  This will install to $($Script:ClaudeDir):"
Write-Host "    tools\claudebase\   (binary + pdfium + e5 model)"
Write-Host "    rules\              (3 files)"
Write-Host "    commands\           (3 files)"
Write-Host "    agents\             (2 files)"
Write-Host ""

if (-not (Confirm-Action "Proceed with installation?")) {
    Write-Info "Aborted."
    exit 0
}

Get-SourceDir
Install-Prompts
Install-Binary
Register-Alias
Register-BashAllowlist
Install-Pdfium
Preload-Encoder

# Optional post-install daemon hook (Slice 2 — STRUCTURAL-2-3)
# Opt-in via `$env:CLAUDEBASE_INSTALL_DAEMON=1`. Fails soft.
if ($env:CLAUDEBASE_INSTALL_DAEMON -eq "1") {
    Write-Info "CLAUDEBASE_INSTALL_DAEMON=1 detected; installing daemon service unit..."
    & claudebase daemon install --no-start --yes
    if ($LASTEXITCODE -eq 0) {
        Write-Ok "Daemon service unit installed (start with 'claudebase daemon start')"
    } else {
        Write-Warn "Daemon install failed (exit $LASTEXITCODE); continuing without daemon"
    }
}

# Cleanup the temp clone (only when we made one).
if (-not $Local -and $Script:ScriptDir -and (Test-Path $Script:ScriptDir) -and $Script:ScriptDir -like "$env:TEMP\*") {
    Remove-Item -Recurse -Force $Script:ScriptDir -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "============================================" -ForegroundColor White
Write-Host "  claudebase install complete" -ForegroundColor White
Write-Host "============================================" -ForegroundColor White
Write-Host ""
Write-Host "  Open a NEW terminal for PATH changes to take effect."
Write-Host ""
Write-Host "  Quick start:"
Write-Host "    claudebase --version                  Confirm binary is on PATH"
Write-Host "    claudebase ingest <path>              Ingest PDF/MD/TXT into <cwd>\.claude\knowledge\"
Write-Host "    claudebase search '<query>' --json    Hybrid retrieval over the books corpus"
Write-Host "    claudebase insight create '...' \"
Write-Host "        --type agent-learned --agent <name>      Persist a cognitive insight"
Write-Host ""
Write-Host "  Skills installed: /knowledge-ingest /reflect /consolidate"
Write-Host "  Agents installed: reflection (Drift), consolidator (Mnem)"
Write-Host ""
