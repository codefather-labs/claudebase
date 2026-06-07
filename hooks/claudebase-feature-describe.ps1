# Slice 7 of cli-to-cli-routing -- PostToolUse:ExitPlanMode hook.
#
# Fires after Claude Code invokes the ExitPlanMode tool. Injects a
# system-reminder mandating the agent to publish the feature
# description to the daemon via the agent_describe MCP tool AND
# mirror it to <project>/.claude/scratchpad.md in the SAME turn.
#
# Fallback chain (per FR-C2C-7.2) if PostToolUse:ExitPlanMode does
# not fire on this Claude Code build:
#   Fallback A -- UserPromptSubmit + prev-turn ExitPlanMode detection
#   Fallback B -- Stop hook + content-marker check
#   Fallback C -- operator-driven via /bootstrap-feature orchestrator
#
# ASCII-ONLY: this file MUST contain only bytes with codepoint <= 127.
# Verified by the matching integration test in
# tests/cli_feature_describe_hook_test.rs (Slice 7).

$ErrorActionPreference = 'SilentlyContinue'

$planPath = ".claude/plan.md"
$featureHeading = "(no .claude/plan.md found in cwd)"
if (Test-Path $planPath) {
    $line = Get-Content $planPath -ErrorAction SilentlyContinue |
        Where-Object { $_ -match '^# ' } |
        Select-Object -First 1
    if ($line) {
        $featureHeading = ($line -replace '^# *', '').Trim()
        if ($featureHeading.Length -gt 240) {
            $featureHeading = $featureHeading.Substring(0, 240)
        }
    }
    if (-not $featureHeading) {
        $featureHeading = "(plan.md has no top-level heading)"
    }
}

$ctx = "[claudebase feature-describe mandate]`n`nA plan has just been approved via ExitPlanMode. Before continuing, you MUST publish the feature description to the daemon AND mirror it to scratchpad in the SAME turn:`n`n1. Call MCP tool agent_describe with description=<feature label>. Suggested: '" + $featureHeading + "'. The daemon binds your agent_id from connection_id (FR-C2C-4.6); callers cannot impersonate.`n2. Update .claude/scratchpad.md '## Feature:' line to match the same description.`n`nBoth writes happen in this turn so the daemon (visible to peer CLIs via 'claudebase agent list-alive') and your personal scratchpad never drift. Skipping breaks cross-agent discovery."

$payload = @{
    hookSpecificOutput = @{
        hookEventName     = "PostToolUse"
        additionalContext = $ctx
    }
} | ConvertTo-Json -Compress -Depth 5

Write-Output $payload
