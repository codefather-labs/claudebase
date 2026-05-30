# claudebase UserPromptSubmit hook (Windows PowerShell) - pre-response reminder.
# ASCII-only source: Windows PowerShell 5.1 parses no-BOM scripts in the local code page, so non-ASCII (em-dash, bullets, emoji) corrupts string literals and breaks the script. Keep this file ASCII.
#
# Fires before the agent responds. Injects a SHORT agent-only reminder covering
# (1) the three cognitive-self-check protocols and (2) insight-capture: if the
# PREVIOUS turn produced a genuine insight, persist it now.
#
# Insight-capture lives here (not on Stop) because a Stop hook forces reflection
# only via `decision: block`, which Claude Code renders as "Stop hook error:" -
# looks like a failure and forces an extra turn. UserPromptSubmit injects
# agent-only additionalContext with no operator bubble and no "error" framing.
#
# Wired via $env:USERPROFILE\.claude\settings.json:
#   hooks.UserPromptSubmit[*].hooks[*].command =
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-selfcheck-reminder.ps1
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$reminder = @'
[pre-response reminder]

1. Run the three cognitive-self-check protocols (~/.claude/rules/cognitive-self-check.md):
   - Protocol 3 (Inbound) - challenge the task BEFORE executing; push-back is not failure.
   - Protocol 1 (Facts) - cite file:line / source verified THIS session; training-data recall is not evidence.
   - Protocol 2 (Decisions) - hack? sane? alternatives? symptom-or-cause? root-cause tracked?

2. Insight-capture: if your PREVIOUS turn produced a genuine insight worth carrying across sessions - self-learning, a falsified prediction/assumption, or an operator correction - persist exactly ONE now, before continuing:
     claudebase insight create "<one sentence>" --type <agent-learned|self-bias-caught|prediction-error|assumption-falsified|plan-reality-gap|operator-correction> --agent <you> --category <general|project> --tags <tag> --salience <high|medium|low>
   --category (required: general=cross-project lesson -> global db; project=this-project insight -> local db) and --tags (required: at least one free-form tag, e.g. a feature slug or a domain like sqlite) are MANDATORY - omitting either exits 2. Read-time --tag filtering is OR / any-intersection.
   If the previous turn was mechanical or produced nothing genuinely new, skip silently - do not manufacture an insight.
'@

$out = [ordered]@{
    hookSpecificOutput = [ordered]@{
        hookEventName     = 'UserPromptSubmit'
        additionalContext = $reminder
    }
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
