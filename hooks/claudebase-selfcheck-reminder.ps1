# claudebase UserPromptSubmit hook (Windows PowerShell) — self-check reminder.
#
# Fires before the agent responds. Injects a SHORT agent-only reminder of the
# three cognitive-self-check protocols. Channel: additionalContext (no
# systemMessage — per-prompt operator bubble would be noise).
#
# Ships with claudebase because the cognitive-self-check rule it reminds about
# (~/.claude/rules/cognitive-self-check.md) is part of claudebase's
# cognitive-infrastructure layer.
#
# Wired via $env:USERPROFILE\.claude\settings.json:
#   hooks.UserPromptSubmit[*].hooks[*].command =
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-selfcheck-reminder.ps1
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$reminder = @'
[self-check reminder] Before you respond, confirm you are running the three cognitive-self-check protocols (~/.claude/rules/cognitive-self-check.md):
  • Protocol 3 (Inbound) — challenge the task BEFORE executing; push-back is not failure.
  • Protocol 1 (Facts) — cite file:line / source verified THIS session; training-data recall is not evidence.
  • Protocol 2 (Decisions) — hack? sane? alternatives? symptom-or-cause? root-cause tracked?
If you have drifted from these over the session, re-read the rule now.
'@

$out = [ordered]@{
    hookSpecificOutput = [ordered]@{
        hookEventName     = 'UserPromptSubmit'
        additionalContext = $reminder
    }
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
