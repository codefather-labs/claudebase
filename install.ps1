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
$Script:ClaudebaseVersion       = '0.6.0'
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
        $rulesDir = Join-Path $Script:ScriptDir 'prompts\rules'
        $commandsDir = Join-Path $Script:ScriptDir 'prompts\commands'
        $agentsDir = Join-Path $Script:ScriptDir 'prompts\agents'
        if (-not (Test-Path $rulesDir) -or -not (Test-Path $commandsDir) -or -not (Test-Path $agentsDir)) {
            Write-Err "-Local requires running from a claudebase checkout root (with prompts\{rules,commands,agents}\)"
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
        Get-ChildItem (Join-Path $Script:ScriptDir "prompts\$sub\*.md") -ErrorAction SilentlyContinue | ForEach-Object {
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
# Install claudebase hooks into ~/.claude/hooks/ and wire into settings.json:
#   - Stop -> claudebase-insight-capture.ps1 (insight-capture reflection)
#   - UserPromptSubmit -> claudebase-selfcheck-reminder.ps1 (self-check nudge)
# Idempotent — dedup by command-string equality so re-running never duplicates.
# ============================================================================
function Install-ClaudebaseHooks {
    $hooksDir = Join-Path $Script:ClaudeDir 'hooks'
    $settings = Join-Path $Script:ClaudeDir 'settings.json'
    New-Item -ItemType Directory -Force -Path $hooksDir | Out-Null

    # Deploy BOTH variants of BOTH hooks; Windows wires the .ps1, the .sh is harmless.
    foreach ($hook in 'claudebase-insight-capture.sh', 'claudebase-insight-capture.ps1',
                      'claudebase-selfcheck-reminder.sh', 'claudebase-selfcheck-reminder.ps1') {
        $src = Join-Path $Script:ScriptDir "hooks\$hook"
        $dst = Join-Path $hooksDir $hook
        if (-not (Test-Path $src)) { Write-Warn "hooks/$hook missing in source — skipping"; continue }
        Copy-Item -Force $src $dst
        Write-Ok "hooks/$hook"
    }

    $stopCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-insight-capture.ps1')`""
    $selfcheckCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-selfcheck-reminder.ps1')`""

    if (-not (Test-Path $settings)) {
        $obj = [ordered]@{ permissions = [ordered]@{ allow = @() } }
        $obj | ConvertTo-Json -Depth 10 | Set-Content -Path $settings -Encoding UTF8
    }

    try {
        $json = Get-Content -Raw $settings | ConvertFrom-Json
        if (-not ($json.PSObject.Properties.Name -contains 'hooks')) {
            $json | Add-Member -NotePropertyName 'hooks' -NotePropertyValue ([pscustomobject]@{}) -Force
        }

        # Helper — idempotent merge of one event by command-string equality.
        $mergeEvent = {
            param($eventName, $command)
            if (-not ($json.hooks.PSObject.Properties.Name -contains $eventName)) {
                $json.hooks | Add-Member -NotePropertyName $eventName -NotePropertyValue @() -Force
            }
            $existing = @($json.hooks.$eventName)
            $already = $false
            foreach ($entry in $existing) {
                if ($entry.hooks) {
                    foreach ($h in $entry.hooks) { if ($h.command -eq $command) { $already = $true; break } }
                }
                if ($already) { break }
            }
            if (-not $already) {
                $newEntry = [pscustomobject]@{ hooks = @([pscustomobject]@{ type = 'command'; command = $command }) }
                $json.hooks.$eventName = @($existing) + $newEntry
            }
        }

        & $mergeEvent 'Stop' $stopCmd
        & $mergeEvent 'UserPromptSubmit' $selfcheckCmd

        $json | ConvertTo-Json -Depth 12 | Set-Content -Path $settings -Encoding UTF8
        Write-Ok "settings.json (Stop[insight-capture] + UserPromptSubmit[selfcheck] hooks wired)"
    } catch {
        Write-Warn "settings.json hook merge failed ($($_.Exception.Message)); add manually:"
        Write-Warn "  hooks.Stop[*].hooks[*].command = $stopCmd"
        Write-Warn "  hooks.UserPromptSubmit[*].hooks[*].command = $selfcheckCmd"
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
# Install whisper-cli + ffmpeg (voice transcription dependencies). Mirrors
# install.sh's install_whisper_stack. Best-effort + idempotent.
# Opt-out: set $env:CLAUDEBASE_SKIP_WHISPER=1.
# Model (~1.5 GB ggml-medium.bin) is NOT downloaded here — lazy on first voice.
# ============================================================================
function Install-WhisperStack {
    if ($env:CLAUDEBASE_SKIP_WHISPER -eq '1') {
        Write-Info "CLAUDEBASE_SKIP_WHISPER=1 — skipping ffmpeg + whisper-cli install"
        return
    }

    $needFfmpeg = -not (Get-Command ffmpeg -ErrorAction SilentlyContinue)
    $needWhisper = -not (Get-Command whisper-cli -ErrorAction SilentlyContinue)
    if (-not $needFfmpeg -and -not $needWhisper) {
        Write-Ok "ffmpeg + whisper-cli already on PATH (voice transcription ready)"
        return
    }

    # Detect package manager — winget preferred, then choco, then scoop.
    $pkgMgr = $null
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        $pkgMgr = 'winget'
    } elseif (Get-Command choco -ErrorAction SilentlyContinue) {
        $pkgMgr = 'choco'
    } elseif (Get-Command scoop -ErrorAction SilentlyContinue) {
        $pkgMgr = 'scoop'
    } else {
        Write-Warn "no supported package manager detected (winget/choco/scoop); voice transcription disabled"
        Write-Warn "  to enable, install manually:"
        Write-Warn "    winget install ggerganov.whisper-cpp Gyan.FFmpeg"
        Write-Warn "    OR choco install whisper-cpp ffmpeg"
        Write-Warn "    OR scoop install whisper-cpp ffmpeg"
        return
    }

    $cmds = @{
        winget = @{
            ffmpeg  = @('install', '--accept-source-agreements', '-e', 'Gyan.FFmpeg')
            whisper = @('install', '--accept-source-agreements', '-e', 'ggerganov.whisper-cpp')
        }
        choco = @{
            ffmpeg  = @('install', '-y', 'ffmpeg')
            whisper = @('install', '-y', 'whisper-cpp')
        }
        scoop = @{
            ffmpeg  = @('install', 'ffmpeg')
            whisper = @('install', 'whisper-cpp')
        }
    }

    if ($needFfmpeg) {
        Write-Info "installing ffmpeg via $pkgMgr..."
        try {
            & $pkgMgr @($cmds[$pkgMgr]['ffmpeg']) | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Ok "ffmpeg installed"
            } else {
                Write-Warn "ffmpeg install via $pkgMgr returned exit $LASTEXITCODE; install manually"
            }
        } catch {
            Write-Warn "ffmpeg install via $pkgMgr failed: $($_.Exception.Message)"
        }
    }

    if ($needWhisper) {
        Write-Info "installing whisper-cli via $pkgMgr (this can take a few minutes)..."
        try {
            & $pkgMgr @($cmds[$pkgMgr]['whisper']) | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Ok "whisper-cli installed"
            } else {
                Write-Warn "whisper-cli install via $pkgMgr returned exit $LASTEXITCODE; install manually"
            }
        } catch {
            Write-Warn "whisper-cli install via $pkgMgr failed: $($_.Exception.Message)"
        }
    }

    if ((Get-Command ffmpeg -ErrorAction SilentlyContinue) -and (Get-Command whisper-cli -ErrorAction SilentlyContinue)) {
        Write-Info "voice transcription stack ready — model auto-downloads on first voice msg"
        Write-Info "  (or pre-download to ~\AppData\Local\whisper-cpp\models\ggml-medium.bin)"
    }
}

# ============================================================================
# Install the Rust port of the official Anthropic Telegram plugin.
# Mirrors install.sh's install_telegram_plugin — always downloads server-rs
# from the matching claudebase release asset; no cargo build fallback.
# Opt-out: $env:CLAUDEBASE_SKIP_TELEGRAM=1.
# Requires: `claude` CLI on PATH.
# Idempotent. Patches `.mcp.json` with direct exec of server-rs.exe.
# ============================================================================
function Install-TelegramPlugin {
    if ($env:CLAUDEBASE_SKIP_TELEGRAM -eq '1') {
        Write-Info "CLAUDEBASE_SKIP_TELEGRAM=1 — skipping telegram plugin install"
        return
    }
    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
        Write-Info "claude CLI not on PATH; skipping telegram plugin install"
        Write-Info "  to install manually after Claude Code is installed:"
        Write-Info "    claude plugin install telegram@claude-plugins-official"
        Write-Info "    then re-run this installer to patch the Rust binary"
        return
    }

    # ----- 1. Install official plugin if not already -----
    $marketplaceAlready = $false
    try {
        $mpList = & claude plugin marketplace list 2>&1
        if ($mpList -match "claude-plugins-official") { $marketplaceAlready = $true }
    } catch {}
    if (-not $marketplaceAlready) {
        Write-Info "Adding marketplace anthropics/claude-plugins-official..."
        try { & claude plugin marketplace add anthropics/claude-plugins-official 2>&1 | Out-Null } catch {}
    }
    Write-Info "Installing telegram@claude-plugins-official (idempotent)..."
    try { & claude plugin install telegram@claude-plugins-official 2>&1 | Out-Null } catch {}

    # ----- 2. Locate installed plugin dir (newest version) -----
    $pluginRoot = Join-Path $Script:ClaudeDir 'plugins\cache\claude-plugins-official\telegram'
    if (-not (Test-Path $pluginRoot)) {
        Write-Warn "official telegram plugin not found at $pluginRoot — skipping Rust patch"
        return
    }
    $versionDir = Get-ChildItem -Path $pluginRoot -Directory -ErrorAction SilentlyContinue `
        | Sort-Object -Property Name -Descending `
        | Select-Object -First 1
    if (-not $versionDir) {
        Write-Warn "no version subdir found under $pluginRoot — skipping Rust patch"
        return
    }
    $pluginDir = $versionDir.FullName
    Write-Info "patching plugin v$($versionDir.Name) at $pluginDir"

    # ----- 3. Resolve binary: download from GH release first; cargo build
    #         fallback only if download fails (no release with this asset
    #         yet, offline, etc). Mirrors install.sh download-first pattern. -----
    $platform = $null
    switch ("$(if ([System.Environment]::Is64BitOperatingSystem) {'x64'} else {'x86'})") {
        'x64' { $platform = 'windows-x64' }
        default {
            Write-Warn "unsupported Windows arch; skipping telegram-plugin-rs"
            return
        }
    }
    $targetBin = Join-Path $pluginDir 'server-rs.exe'
    $url = "$($Script:ReleaseBase)/claudebase-v$($Script:ClaudebaseVersion)/telegram-plugin-rs-$platform.exe"
    $downloaded = $false
    $tmp = New-TemporaryFile

    Write-Info "downloading telegram-plugin-rs binary from GH release for $platform..."
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmp.FullName -UseBasicParsing -TimeoutSec 120 -ErrorAction Stop
        $downloaded = $true
    } catch {
        Write-Warn "telegram-plugin-rs download failed: $($_.Exception.Message)"
    }

    if ($downloaded -and (Test-Path $tmp.FullName) -and ((Get-Item $tmp.FullName).Length -gt 0)) {
        Move-Item -Force $tmp.FullName $targetBin
        Write-Ok "server-rs.exe downloaded ($((Get-Item $targetBin).Length) bytes) -> $targetBin"
    } else {
        if (Test-Path $tmp.FullName) { Remove-Item -Force $tmp.FullName }
        Write-Warn "telegram-plugin-rs download failed for $platform from $url"
        Write-Warn "  the upstream TSX plugin will run unchanged via bun"
        Write-Warn "  to force a build from source locally: cargo build --release -p telegram-plugin-rs"
        Write-Warn "  then copy target\release\telegram-plugin-rs.exe -> $targetBin"
        return
    }

    # ----- 5. Patch .mcp.json (backup upstream first) -----
    $mcpJson = Join-Path $pluginDir '.mcp.json'
    $mcpBackup = Join-Path $pluginDir '.mcp.json.upstream-backup'
    if ((Test-Path $mcpJson) -and (-not (Test-Path $mcpBackup))) {
        Copy-Item $mcpJson $mcpBackup
        Write-Ok ".mcp.json.upstream-backup preserved"
    }
    # On Windows, cmd.exe is the available shell for the toggle. Simpler:
    # just exec server-rs.exe directly when present. (The bash-toggle is for
    # Unix; Windows users opt-out by removing the binary.)
    $cfg = @{
        mcpServers = @{
            telegram = @{
                command = $targetBin
                args = @()
            }
        }
    }
    $cfg | ConvertTo-Json -Depth 6 | Set-Content -Path $mcpJson -Encoding UTF8
    Write-Ok ".mcp.json patched (Windows: direct exec of server-rs.exe)"

    Write-Info "to enable: launch Claude Code with"
    Write-Info "  claude --channels plugin:telegram@claude-plugins-official"
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
Install-ClaudebaseHooks
Install-Pdfium
Install-WhisperStack
Preload-Encoder
Install-TelegramPlugin

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
