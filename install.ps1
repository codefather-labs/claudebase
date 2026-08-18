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
$Script:ClaudebaseVersion       = '0.9.2'
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
  %USERPROFILE%\.claude\rules\        cognitive-self-check
  %USERPROFILE%\.claude\commands\     knowledge-ingest, reflect, consolidate, update-claudebase
  %USERPROFILE%\.claude\agents\       reflection (Drift), consolidator (Mnem)
  %USERPROFILE%\.claude\skills\       daemon-change-nick, daemon-setup-auth-token,
                                      daemon-callback-info
  %USERPROFILE%\.claude\hooks\        SessionStart x2 (channel contract, read-insights),
                                      UserPromptSubmit (self-check),
                                      Pre/PostToolUse (peer routing, feature describe)
  %USERPROFILE%\.claude\bin\claudebase.cmd  Global wrapper (User PATH)

WHAT IS *REMOVED* (reverse migration, idempotent):
  the old Stop[insight-capture] hook, the claudebase patch inside the official
  Anthropic Telegram plugin, and the retired claudebase@claudebase-dev marketplace.

AFTER INSTALLING:
  claudebase run                        start a session the daemon can reach
  claudebase telegram addbot "<token>"  wire up Telegram
  claudebase daemon callback enable     open the HTTP callback endpoint (off by default)
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

    # Skills ship as DIRECTORIES (SKILL.md filename is fixed). See the sh twin
    # for why access control is deliberately not among them.
    $skillsRoot = Join-Path $Script:ClaudeDir 'skills'
    $skillsSrc = Join-Path $Script:ScriptDir 'prompts\skills'
    if (Test-Path $skillsSrc) {
        New-Item -ItemType Directory -Force -Path $skillsRoot | Out-Null
        Get-ChildItem $skillsSrc -Directory -ErrorAction SilentlyContinue | ForEach-Object {
            $src = Join-Path $_.FullName 'SKILL.md'
            if (-not (Test-Path $src)) {
                Write-Warn "skills\$($_.Name) has no SKILL.md - skipping"
                return
            }
            $dest = Join-Path $skillsRoot $_.Name
            New-Item -ItemType Directory -Force -Path $dest | Out-Null
            Copy-Item -Force $src $dest
            Write-Ok "skills\$($_.Name)"
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

    # -Local: build the binary from THIS checkout and install it, NEVER
    # downloading a release. -Local means "install the code in front of me";
    # pulling a pre-built release asset (possibly older or different, or
    # absent after a tag was deleted) would silently contradict that intent.
    # Requires a rust toolchain (cargo on PATH).
    if ($Local) {
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            Write-Err "-Local binary build needs cargo (install the rust toolchain via rustup)"
            return
        }
        # No OpenSSL preflight here, unlike install.sh. `native-tls` declares
        # openssl-sys only under cfg(not(any(windows, apple))) -- on Windows it
        # uses SChannel, so this build needs no OpenSSL headers at all. What it
        # DOES need is the MSVC toolchain, and cargo's own error names it
        # clearly ("linker `link.exe` not found"), so there is nothing to add.
        if (-not (Get-Command link.exe -ErrorAction SilentlyContinue) -and
            -not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
            Write-Warn "MSVC build tools not detected on PATH; if the build fails with"
            Write-Warn "  'linker `link.exe` not found', install the Visual Studio Build Tools"
            Write-Warn "  (Desktop development with C++) and re-run from a Developer prompt."
        }
        Write-Info "Building claudebase from local checkout ($($Script:ScriptDir)) - cargo build --release"
        Push-Location $Script:ScriptDir
        & cargo build --release --features asr-whisper
        $buildExit = $LASTEXITCODE
        Pop-Location
        if ($buildExit -ne 0) {
            Write-Err "local 'cargo build --release' failed; binary not installed"
            return
        }
        $localBin = Join-Path $Script:ScriptDir 'target\release\claudebase.exe'
        if (-not (Test-Path $localBin)) {
            Write-Err "local build produced no binary at $localBin"
            return
        }
        Copy-Item -Force $localBin $targetBin
        Write-Ok "tools\claudebase\claudebase.exe (local build, $platform)"
        return
    }

    if (Test-Path $targetBin) {
        try {
            $existingVer = (& $targetBin --version 2>$null) -replace '^claudebase ', ''
            if ($existingVer -eq $Script:ClaudebaseVersion) {
                Write-Ok "claudebase binary already at version $($Script:ClaudebaseVersion)"
                return
            }
        } catch {}
    }

    $asset = "claudebase-$platform.exe"
    $url   = "$($Script:ReleaseBase)/claudebase-v$($Script:ClaudebaseVersion)/$asset"
    # GitHub redirects releases/latest/download/<asset> to the newest published
    # release. The pinned version can legitimately not exist yet: the bump lands
    # on `main` when the tag is pushed, the release build takes minutes, and this
    # script is fetched from `main`. Installing in that window used to produce no
    # binary at all while every other step reported success.
    $fallbackUrl = ($Script:RepoUrl -replace '\.git$','') + "/releases/latest/download/$asset"
    $tmp = Join-Path $env:TEMP ("claudebase-" + [guid]::NewGuid().ToString() + ".exe")

    Write-Info "Downloading claudebase.exe v$($Script:ClaudebaseVersion)..."
    $got = $false
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing -MaximumRedirection 5 -TimeoutSec 120
        $got = $true
    } catch {
        Write-Warn "v$($Script:ClaudebaseVersion) not downloadable yet; falling back to the latest release"
        try {
            Invoke-WebRequest -Uri $fallbackUrl -OutFile $tmp -UseBasicParsing -MaximumRedirection 5 -TimeoutSec 120
            $got = $true
        } catch {
            Write-Warn "claudebase binary download failed: $($_.Exception.Message)"
        }
    }
    if (-not $got) {
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

    # Windows will not let a RUNNING .exe be overwritten, and the daemon from a
    # previous install is normally running -- so `Move-Item -Force` failed with
    # "Cannot create a file when that file already exists" on every upgrade.
    # (On Unix the same operation works: renaming swaps the inode and the live
    # process keeps the old one open.)
    #
    # Windows does permit RENAMING a running executable, which is the standard
    # self-update route: move the old one aside, put the new one in place, then
    # delete the leftover best-effort. Deleting fails while the old process
    # lives, so the stale file is cleaned on the next run instead of blocking
    # this one. Stopping the daemon first would also work, but it would take a
    # working install down to upgrade it.
    if (Test-Path $targetBin) {
        $stale = "$targetBin.old"
        if (Test-Path $stale) { Remove-Item $stale -Force -ErrorAction SilentlyContinue }
        try {
            Move-Item -Force $targetBin $stale -ErrorAction Stop
        } catch {
            Write-Err "cannot replace $targetBin ($($_.Exception.Message))"
            Write-Err "  a claudebase process is holding it and could not be moved aside."
            Write-Err "  Stop it and re-run:  claudebase daemon stop"
            Remove-Item $tmp -Force -ErrorAction SilentlyContinue
            return
        }
    }
    Move-Item -Force $tmp $targetBin
    Remove-Item "$targetBin.old" -Force -ErrorAction SilentlyContinue
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
    # Two forms on purpose: the absolute path (how hooks and older prompts call
    # it) and the bare name (how an agent calls it after Register-Alias put a
    # wrapper on PATH - `claudebase telegram send ...`). Without the bare form
    # every reply to a Telegram message raises a permission prompt.
    $entries = @('~/.claude/tools/claudebase/claudebase *', 'claudebase *')

    # Issue 003: Set-Content -Encoding UTF8 writes UTF-8 WITH BOM on
    # Windows PowerShell 5.1; some JSON parsers (notably Claude Code's
    # MCP loader) silently reject BOM-prefixed config. Use
    # [System.IO.File]::WriteAllText with an explicit no-BOM UTF8Encoding
    # so we get clean output on both PS 5.1 and PS 7+.
    if (-not (Test-Path $settings)) {
        $obj = @{ permissions = @{ allow = $entries } }
        $json = $obj | ConvertTo-Json -Depth 10
        [System.IO.File]::WriteAllText($settings, $json, [System.Text.UTF8Encoding]::new($false))
        Write-Ok "settings.json (created with claudebase allowlist)"
        return
    }

    try {
        $obj = Get-Content $settings -Raw | ConvertFrom-Json
        if ($null -eq $obj.permissions) { $obj | Add-Member -NotePropertyName permissions -NotePropertyValue @{ allow = @() } -Force }
        if ($null -eq $obj.permissions.allow) { $obj.permissions | Add-Member -NotePropertyName allow -NotePropertyValue @() -Force }

        $missing = @($entries | Where-Object { $obj.permissions.allow -notcontains $_ })
        if ($missing.Count -gt 0) {
            $obj.permissions.allow = @($obj.permissions.allow) + $missing
            $json = $obj | ConvertTo-Json -Depth 10
            [System.IO.File]::WriteAllText($settings, $json, [System.Text.UTF8Encoding]::new($false))
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
#   - SessionStart -> claudebase-channel-contract.ps1 (how messages from outside
#     the terminal arrive, and that a reply only leaves through a CLI call)
#   - SessionStart -> claudebase-read-insights-reminder.ps1 (prior insights)
#   - UserPromptSubmit -> claudebase-selfcheck-reminder.ps1 (self-check nudge)
#   - PreToolUse:EnterPlanMode -> claudebase-agent-routing-reminder.ps1
#   - PostToolUse:ExitPlanMode -> claudebase-feature-describe.ps1
# The retired Stop[insight-capture] hook is actively UNWIRED and its files
# deleted -- see below.
# Idempotent -- dedup by command-string equality so re-running never duplicates.
# ============================================================================
function Install-ClaudebaseHooks {
    $hooksDir = Join-Path $Script:ClaudeDir 'hooks'
    $settings = Join-Path $Script:ClaudeDir 'settings.json'
    New-Item -ItemType Directory -Force -Path $hooksDir | Out-Null

    # Remove the retired Stop insight-capture hook files (superseded).
    foreach ($old in 'claudebase-insight-capture.sh', 'claudebase-insight-capture.ps1') {
        $p = Join-Path $hooksDir $old
        if (Test-Path $p) { Remove-Item -Force $p }
    }

    # Deploy both variants of the self-check reminder + read-insights reminder;
    # Windows wires the .ps1 variants.
    foreach ($hook in 'claudebase-selfcheck-reminder.sh', 'claudebase-selfcheck-reminder.ps1', 'claudebase-read-insights-reminder.sh', 'claudebase-read-insights-reminder.ps1', 'claudebase-agent-routing-reminder.sh', 'claudebase-agent-routing-reminder.ps1', 'claudebase-channel-contract.sh', 'claudebase-channel-contract.ps1', 'claudebase-feature-describe.sh', 'claudebase-feature-describe.ps1') {
        $src = Join-Path $Script:ScriptDir "hooks\$hook"
        $dst = Join-Path $hooksDir $hook
        if (-not (Test-Path $src)) { Write-Warn "hooks/$hook missing in source -- skipping"; continue }
        Copy-Item -Force $src $dst
        Write-Ok "hooks/$hook"
    }

    $stopCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-insight-capture.ps1')`""
    $selfcheckCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-selfcheck-reminder.ps1')`""
    $readinsCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-read-insights-reminder.ps1')`""
    $routingCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-agent-routing-reminder.ps1')`""
    # pty-transport v0.10 -- the external-channel contract must be in context
    # BEFORE the first [telegram_message]: line arrives, so it rides SessionStart.
    $channelCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-channel-contract.ps1')`""
    $describeCmd = "powershell -NoProfile -File `"$(Join-Path $hooksDir 'claudebase-feature-describe.ps1')`""

    if (-not (Test-Path $settings)) {
        $obj = [ordered]@{ permissions = [ordered]@{ allow = @() } }
        $obj | ConvertTo-Json -Depth 10 | Set-Content -Path $settings -Encoding UTF8
    }

    try {
        $json = Get-Content -Raw $settings | ConvertFrom-Json
        if (-not ($json.PSObject.Properties.Name -contains 'hooks')) {
            $json | Add-Member -NotePropertyName 'hooks' -NotePropertyValue ([pscustomobject]@{}) -Force
        }

        # Helper -- idempotent merge of one event by command-string equality.
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

        & $mergeEvent 'UserPromptSubmit' $selfcheckCmd

        # Idempotent merge of SessionStart read-insights reminder by command-string
        # equality. Official SessionStart shape: {matcher, hooks[{type,command}]}.
        if (-not ($json.hooks.PSObject.Properties.Name -contains 'SessionStart')) {
            $json.hooks | Add-Member -NotePropertyName 'SessionStart' -NotePropertyValue @() -Force
        }
        $ssExisting = @($json.hooks.SessionStart)
        $ssAlready = $false
        foreach ($entry in $ssExisting) {
            if ($entry.hooks) { foreach ($h in $entry.hooks) { if ($h.command -eq $readinsCmd) { $ssAlready = $true; break } } }
            if ($ssAlready) { break }
        }
        if (-not $ssAlready) {
            $ssNewEntry = [pscustomobject]@{ matcher = 'startup|resume|compact'; hooks = @([pscustomobject]@{ type = 'command'; command = $readinsCmd }) }
            $json.hooks.SessionStart = @($ssExisting) + $ssNewEntry
        }

        # SessionStart -> channel-contract (how telegram / peer messages arrive
        # and how to answer them). Same idempotent command-string match.
        $ccExisting = @($json.hooks.SessionStart)
        $ccAlready = $false
        foreach ($entry in $ccExisting) {
            if ($entry.hooks) { foreach ($h in $entry.hooks) { if ($h.command -eq $channelCmd) { $ccAlready = $true; break } } }
            if ($ccAlready) { break }
        }
        if (-not $ccAlready) {
            $ccNewEntry = [pscustomobject]@{ matcher = 'startup|resume|compact'; hooks = @([pscustomobject]@{ type = 'command'; command = $channelCmd }) }
            $json.hooks.SessionStart = @($ccExisting) + $ccNewEntry
        }

        # PreToolUse:EnterPlanMode -> agent-routing-reminder (peer read side).
        if (-not ($json.hooks.PSObject.Properties.Name -contains 'PreToolUse')) {
            $json.hooks | Add-Member -NotePropertyName 'PreToolUse' -NotePropertyValue @() -Force
        }
        $preExisting = @($json.hooks.PreToolUse)
        $preAlready = $false
        foreach ($entry in $preExisting) {
            if ($entry.hooks) { foreach ($h in $entry.hooks) { if ($h.command -eq $routingCmd) { $preAlready = $true; break } } }
            if ($preAlready) { break }
        }
        if (-not $preAlready) {
            $preNewEntry = [pscustomobject]@{ matcher = 'EnterPlanMode'; hooks = @([pscustomobject]@{ type = 'command'; command = $routingCmd }) }
            $json.hooks.PreToolUse = @($preExisting) + $preNewEntry
        }

        # PostToolUse:ExitPlanMode -> feature-describe (cli-to-cli write side).
        if (-not ($json.hooks.PSObject.Properties.Name -contains 'PostToolUse')) {
            $json.hooks | Add-Member -NotePropertyName 'PostToolUse' -NotePropertyValue @() -Force
        }
        $postExisting = @($json.hooks.PostToolUse)
        $postAlready = $false
        foreach ($entry in $postExisting) {
            if ($entry.hooks) { foreach ($h in $entry.hooks) { if ($h.command -eq $describeCmd) { $postAlready = $true; break } } }
            if ($postAlready) { break }
        }
        if (-not $postAlready) {
            $postNewEntry = [pscustomobject]@{ matcher = 'ExitPlanMode'; hooks = @([pscustomobject]@{ type = 'command'; command = $describeCmd }) }
            $json.hooks.PostToolUse = @($postExisting) + $postNewEntry
        }

        # Unwire the retired Stop insight-capture hook: strip its command from
        # any Stop matcher block, drop now-empty blocks, drop empty Stop key.
        if ($json.hooks.PSObject.Properties.Name -contains 'Stop') {
            $kept = @()
            foreach ($entry in @($json.hooks.Stop)) {
                if ($entry.hooks) {
                    $entry.hooks = @($entry.hooks | Where-Object { $_.command -ne $stopCmd })
                }
                if ($entry.hooks -and @($entry.hooks).Count -gt 0) { $kept += $entry }
            }
            if ($kept.Count -gt 0) { $json.hooks.Stop = $kept }
            else { $json.hooks.PSObject.Properties.Remove('Stop') }
        }

        $json | ConvertTo-Json -Depth 12 | Set-Content -Path $settings -Encoding UTF8
        Write-Ok "settings.json (UserPromptSubmit[selfcheck] + SessionStart[read-insights, channel-contract] wired; retired Stop[insight-capture] unwired)"
    } catch {
        Write-Warn "settings.json hook merge failed ($($_.Exception.Message)); add manually:"
        Write-Warn "  hooks.Stop[*].hooks[*].command = $stopCmd"
        Write-Warn "  hooks.UserPromptSubmit[*].hooks[*].command = $selfcheckCmd"
        Write-Warn "  hooks.SessionStart[*].hooks[*].command = $readinsCmd"
        Write-Warn "  (and remove any hooks.Stop entry pointing at claudebase-insight-capture.ps1)"
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
# Model (~1.5 GB ggml-medium.bin) is NOT downloaded here - lazy on first voice.
# ============================================================================
function Install-WhisperStack {
    if ($env:CLAUDEBASE_SKIP_WHISPER -eq '1') {
        Write-Info "CLAUDEBASE_SKIP_WHISPER=1 - skipping ffmpeg + whisper-cli install"
        return
    }

    $needFfmpeg = -not (Get-Command ffmpeg -ErrorAction SilentlyContinue)
    $needWhisper = -not (Get-Command whisper-cli -ErrorAction SilentlyContinue)
    if (-not $needFfmpeg -and -not $needWhisper) {
        Write-Ok "ffmpeg + whisper-cli already on PATH (voice transcription ready)"
        return
    }

    # Detect package manager - winget preferred, then choco, then scoop.
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
            # winget requires BOTH --accept-source-agreements (source EULA,
            # one-time) AND --accept-package-agreements (per-package
            # license). Without the second flag winget exits with
            # APPINSTALLER_CLI_ERROR_PACKAGE_AGREEMENTS_NOT_ACCEPTED
            # (-1978335212 / 0x8A150014) on packages that ship a EULA.
            ffmpeg  = @('install', '--accept-source-agreements', '--accept-package-agreements', '-e', 'Gyan.FFmpeg')
            whisper = @('install', '--accept-source-agreements', '--accept-package-agreements', '-e', 'ggerganov.whisper-cpp')
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
        Write-Info "voice transcription stack ready - model auto-downloads on first voice msg"
        Write-Info "  (or pre-download to ~\AppData\Local\whisper-cpp\models\ggml-medium.bin)"
    }
}

# ============================================================================
# Undo the old plugin-slot hijack (v0.10 reverse migration)
# ============================================================================
# Until v0.10 the installer wrote our own binary into the OFFICIAL Anthropic
# Telegram plugin's cache and rewrote its `.mcp.json` so the plugin's MCP
# server was `claudebase plugin serve`. That transport is gone: Telegram lives
# inside the daemon and messages reach Claude Code through the PTY supervisor
# (`claudebase run`).
#
# The patch is actively UNDONE rather than merely stopped: an operator who
# upgrades would otherwise keep a third-party plugin permanently pointing at
# our binary. `.mcp.json` is restored from the backup saved at patch time, and
# `server-rs` is deleted. With no backup the file is left alone and the
# operator is told to reinstall the plugin -- guessing upstream's command line
# would be worse than saying so plainly.
#
# Also drops the v0.6-era `claudebase@claudebase-dev` registration.
# Idempotent: every step no-ops when its artefact is already absent.
function Invoke-UnpatchOfficialTelegramPlugin {
    if (-not (Get-Command claude -ErrorAction SilentlyContinue)) { return }

    $prevErrPref = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try { & claude plugin uninstall claudebase@claudebase-dev 2>&1 | Out-Null } catch {}
    try { & claude plugin marketplace remove codefather-labs/claudebase 2>&1 | Out-Null } catch {}
    $ErrorActionPreference = $prevErrPref

    $pluginRoot = Join-Path $Script:ClaudeDir 'plugins\cache\claude-plugins-official\telegram'
    if (-not (Test-Path $pluginRoot)) {
        Write-Ok "no official telegram plugin cache - nothing to unpatch"
        return
    }

    $restored = 0; $removed = 0; $orphaned = 0
    Get-ChildItem $pluginRoot -Directory -ErrorAction SilentlyContinue | ForEach-Object {
        $mcp    = Join-Path $_.FullName '.mcp.json'
        $backup = Join-Path $_.FullName '.mcp.json.upstream-backup'
        if (Test-Path $backup) {
            Move-Item -Force $backup $mcp
            $restored++
        } elseif ((Test-Path $mcp) -and (Select-String -Path $mcp -Pattern 'claudebase' -Quiet)) {
            $orphaned++
        }
        foreach ($name in 'server-rs', 'server-rs.exe') {
            $bin = Join-Path $_.FullName $name
            if (Test-Path $bin) { Remove-Item -Force $bin; $removed++ }
        }
    }

    if ($restored -gt 0 -or $removed -gt 0) {
        Write-Ok "official telegram plugin unpatched (.mcp.json restored: $restored, server-rs removed: $removed)"
    } else {
        Write-Ok "official telegram plugin carries no claudebase patch (idempotent)"
    }
    if ($orphaned -gt 0) {
        Write-Warn "$orphaned plugin dir(s) still point at claudebase with no upstream backup"
        Write-Warn "  restore upstream with: claude plugin install telegram@claude-plugins-official"
    }
}

# ============================================================================
# Main
# ============================================================================
if ($Help) { Show-Help; exit 0 }

Write-Host ""
Write-Host "============================================" -ForegroundColor White
Write-Host "  claudebase v$($Script:ClaudebaseVersion) - installer (Windows)" -ForegroundColor White
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
# See install.sh: defined and never called until 2026-08-17.
Install-ClaudebaseHooks
Install-Binary
Register-Alias
Register-BashAllowlist
Install-Pdfium
Install-WhisperStack
Preload-Encoder
Invoke-UnpatchOfficialTelegramPlugin

# Post-install daemon spawn (default-on, current-user, no admin needed)
#
# 2026-06-04: This block was reworked from `daemon install` (Windows SCM
# service) to `daemon serve` (detached current-user process). The SCM
# install path was structurally broken under operator's environment:
# the Windows service ran as `NT AUTHORITY\LocalService` (per
# `src/daemon/service.rs::windows::service_account`), so its
# `$env:USERPROFILE` pointed at
# `C:\Windows\ServiceProfiles\LocalService\` instead of the operator's
# profile. The daemon then resolved `~/.claude/channels/claudebase/.env`
# to a path that did NOT exist, came up with no Telegram token, and
# silently ran in chat-only mode (no long-poll, no inbound messages).
# Even the binary's USERPROFILE-fallback (commit 1e337c7) cannot rescue
# that because the LocalService USERPROFILE is technically valid - just
# pointing at the wrong tree.
#
# The current-user spawn path here uses `Start-Process` with
# `-WindowStyle Hidden`, which produces a detached child that survives
# this script's exit but stays scoped to the operator's login session.
# Survival across reboots is now handled at first `claudebase run`
# (Slice 25: `spawn_daemon_detached` in src/main.rs) which auto-spawns
# the daemon if its UDS / named-pipe is unreachable. So the operator
# does NOT need a Windows service at all - the daemon comes back on
# first interactive invocation, with all the right env (HOME +
# USERPROFILE) inherited from the user's shell.
#
# Opt-out: `$env:CLAUDEBASE_SKIP_DAEMON=1`.
if ($env:CLAUDEBASE_SKIP_DAEMON -eq "1") {
    Write-Info "CLAUDEBASE_SKIP_DAEMON=1 - skipping daemon spawn"
} else {
    $claudebaseExe = Join-Path $Script:ClaudeDir 'tools\claudebase\claudebase.exe'
    if (-not (Test-Path $claudebaseExe)) {
        Write-Warn "Daemon spawn skipped: $claudebaseExe not found"
    } else {
        # Best-effort: stop any prior daemon process so this script's
        # respawn produces a process running the freshly-installed
        # binary. Failures are NORMAL on a fresh box.
        $prevErrPref = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        Get-Process claudebase -ErrorAction SilentlyContinue | ForEach-Object {
            try { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue } catch {}
        }
        Start-Sleep -Milliseconds 500
        $ErrorActionPreference = $prevErrPref

        # Spawn detached daemon under current user. Logs to the same
        # file `spawn_daemon_detached` writes to so the operator has one
        # place to grep regardless of who started the daemon (this
        # install script or `claudebase run`'s auto-spawn helper).
        $logDir = Join-Path $Script:ClaudeDir 'logs'
        if (-not (Test-Path $logDir)) {
            New-Item -ItemType Directory -Path $logDir -Force | Out-Null
        }
        $logFile = Join-Path $logDir 'claudebase-daemon.log'
        # Start-Process REFUSES the same path for both streams ("Give different
        # inputs and Run your command again"), so pointing both here meant the
        # daemon never auto-started on Windows at all -- the install reported a
        # warning and moved on. Separate files; the daemon's own output is
        # structured JSON either way.
        $errFile = Join-Path $logDir 'claudebase-daemon.err.log'
        try {
            Start-Process -FilePath $claudebaseExe `
                -ArgumentList 'daemon', 'serve' `
                -WindowStyle Hidden `
                -RedirectStandardOutput $logFile `
                -RedirectStandardError $errFile `
                -ErrorAction Stop | Out-Null
            # Brief wait for the daemon to bind its named pipe so the
            # operator sees a positive `daemon status` immediately after
            # install completes.
            Start-Sleep -Seconds 2
            $status = & $claudebaseExe daemon status --json 2>$null
            if ($status -match '"state":\s*"running"') {
                Write-Ok "Daemon started (current-user, detached); logs at $logFile"
            } else {
                Write-Warn "Daemon spawn issued but status is not yet 'running'"
                Write-Warn "  Check $logFile or run: claudebase daemon status"
            }
        } catch {
            Write-Warn "Daemon spawn failed: $_"
            Write-Warn "  Daemon will auto-spawn on first 'claudebase run' invocation"
        }
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
Write-Host "  Commands installed: /knowledge-ingest /reflect /consolidate /update-claudebase"
Write-Host "  Skills installed:   /claudebase-daemon-change-nick"
Write-Host "                      /claudebase-daemon-callback-info"
Write-Host "                      /claudebase-daemon-setup-auth-token"
Write-Host "  Agents installed:   reflection (Drift), consolidator (Mnem)"
Write-Host ""
