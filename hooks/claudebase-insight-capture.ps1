# claudebase Stop hook (Windows PowerShell) - insight-capture nudge.
# ASCII-only source: Windows PowerShell 5.1 parses no-BOM scripts in the local code page, so non-ASCII (em-dash, bullets, emoji) corrupts string literals and breaks the script. Keep this file ASCII.
#
# Fires when the main agent finishes a turn (Stop event). Unconditionally
# prompts the agent to reflect: did it learn anything, catch a mistake, or
# have an assumption falsified? If yes and axis-worthy, the agent persists it
# via `claudebase insight create`. If not, the agent stops SILENTLY.
#
# Wired via $env:USERPROFILE\.claude\settings.json:
#   hooks.Stop[*].hooks[*].command =
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-insight-capture.ps1
#
# Loop safety: check stop_hook_active first; exit 0 so the forced reflection
# turn never re-triggers the hook.
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$payload = ''
try { $payload = [Console]::In.ReadToEnd() } catch {}
$stopActive = $false
$cwd = ''
$sessionId = ''
if ($payload) {
    try {
        $obj = $payload | ConvertFrom-Json
        if ($obj.stop_hook_active) { $stopActive = [bool]$obj.stop_hook_active }
        if ($obj.cwd)              { $cwd = $obj.cwd }
        if ($obj.session_id)       { $sessionId = $obj.session_id }
    } catch {}
}
if (-not $cwd) { $cwd = (Get-Location).Path }
$projectLabel = Split-Path -Path $cwd -Leaf

# Guard 1 - loop prevention.
if ($stopActive) { exit 0 }

# Guard 2 - claudebase must be installed.
$cb = Get-Command claudebase -ErrorAction SilentlyContinue
$cbPath = Join-Path $env:USERPROFILE ".claude\tools\claudebase\claudebase.exe"
if (-not $cb -and -not (Test-Path $cbPath)) { exit 0 }

$sysMsg = "[hook] claudebase insight-capture - event=Stop project=$projectLabel"

# Reason is operator-visible by Claude Code design (block reasons always
# surface). Keep it to ONE compact line - the agent already knows the full
# insight protocol from ~/.claude/rules/knowledge-base-tool.md.
$reason = "Reflect on the turn you just finished: if it produced a genuine insight (self-learning, a falsified prediction/assumption, or an operator correction worth carrying forward), persist exactly ONE via ``claudebase insight create ""<one sentence>"" --type <agent-learned|self-bias-caught|prediction-error|assumption-falsified|plan-reality-gap|operator-correction> --agent <you> --salience <high|medium|low> --session $sessionId``. Otherwise stop silently - no insight, no commentary, do not manufacture one."

$out = [ordered]@{
    decision      = 'block'
    reason        = $reason
    systemMessage = $sysMsg
    suppressOutput = $true
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
