# claudebase Stop hook (Windows PowerShell) — insight-capture nudge.
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

# Guard 1 — loop prevention.
if ($stopActive) { exit 0 }

# Guard 2 — claudebase must be installed.
$cb = Get-Command claudebase -ErrorAction SilentlyContinue
$cbPath = Join-Path $env:USERPROFILE ".claude\tools\claudebase\claudebase.exe"
if (-not $cb -and -not (Test-Path $cbPath)) { exit 0 }

$sysMsg = "🪝 claudebase insight-capture hook — event=Stop project=$projectLabel"

$reason = @"
[claudebase insight-capture] Before you stop — reflect on the turn you just finished.

Did you genuinely learn something, catch a mistake, or have an assumption falsified this turn? Map it to the insight-corpus axes:
  1. SELF-LEARNING (--type agent-learned | self-bias-caught) — a domain concept, a technique, or a blind spot in your own past reasoning you just noticed.
  2. PREDICTION-REALITY MISMATCH (--type prediction-error | assumption-falsified | plan-reality-gap) — something you predicted, assumed, or planned turned out wrong.
  3. OPERATOR-CORRECTION (--type operator-correction) — the operator corrected you in a way worth carrying into future sessions.

If YES — and it is genuinely axis-worthy (NOT mechanical execution, NOT a restatement of the task, NOT generic best-practice) — persist exactly ONE insight, then stop:

    claudebase insight create "<one-sentence insight in your own words>" ``
        --type <source-type> --agent <your-agent-name> --salience <high|medium|low> ``
        [--feature "<feature-slug-if-known>"] [--session "$sessionId"]

Salience honestly: high = loss degrades the whole pipeline (rare); medium = slice/decision-level (default); low = ambient.

If NO — nothing genuinely new, or the turn was mechanical — STOP SILENTLY. Write no insight. Print no commentary, no "no insight" line, nothing. Just end the turn. An honest silent skip is the correct and most common outcome; the corpus rejects manufactured or generic insights, so do not invent one to satisfy this hook.
"@

$out = [ordered]@{
    decision      = 'block'
    reason        = $reason
    systemMessage = $sysMsg
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
