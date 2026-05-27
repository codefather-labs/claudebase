# claudebase SessionStart hook (Windows PowerShell) - read-insights-on-new-context.
# ASCII-only source: Windows PowerShell 5.1 parses no-BOM scripts in the local code page, so non-ASCII (em-dash, bullets, emoji) corrupts string literals and breaks the script. Keep this file ASCII.
#
# Fires on session startup|resume|compact. Injects a SHORT agent-only reminder:
# when entering a FRESH context window, pull relevant insights from the
# cognitive corpus by tag/category instead of re-reading every prior message.
# Discovery via `claudebase insight tags`, retrieval via `insight search --tag`.
#
# Emits TEXT ONLY - does NOT itself invoke claudebase. The `insight tags`
# subcommand and `--tag` filter ship in the same release as this hook.
#
# Wired via $env:USERPROFILE\.claude\settings.json:
#   hooks.SessionStart[*].hooks[*].command =
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-read-insights-reminder.ps1
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$reminder = @'
[read-insights reminder]

If you are entering a fresh context window (session start, resume, or post-compact), load relevant prior-session insights from the cognitive corpus by tag - do NOT try to re-read every prior message:

1. Discover the tag vocabulary:
     claudebase insight tags --project "<current-project-basename>"
   (lists tags with counts across this project plus general insights)

2. Load the insights that match what you are about to work on:
     claudebase insight search "<keywords>" --tag <tag>
   Run one or two representative searches keyed to the most relevant tags - read by tag/category, not by exhaustive scan.

If you already hold sufficient context (mid-task, no compaction), skip this - do not re-pull on every message.
'@

$out = [ordered]@{
    hookSpecificOutput = [ordered]@{
        hookEventName     = 'SessionStart'
        additionalContext = $reminder
    }
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
