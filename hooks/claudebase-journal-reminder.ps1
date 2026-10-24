# claudebase SessionStart hook (Windows PowerShell) - the project journal.
# ASCII-only source: Windows PowerShell 5.1 parses no-BOM scripts in the local code page, so non-ASCII (em-dash, bullets, emoji) corrupts string literals and breaks the script. Keep this file ASCII.
#
# Fires on session startup|resume|compact. Injects an agent-only reminder that
# this project has a journal: read the entries that bear on the task (by
# category and tag), and record every decision and finding while working. The
# journal lives with the claudebase daemon, one per project directory.
#
# Emits TEXT ONLY - does NOT itself invoke claudebase.
#
# Wired via $env:USERPROFILE\.claude\settings.json:
#   hooks.SessionStart[*].hooks[*].command =
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-journal-reminder.ps1
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$reminder = @'
[journal reminder]

This project keeps a journal: the decisions and findings of earlier sessions, held by the claudebase daemon for the directory `claudebase run` was started in. Every `claudebase journal` command works on this project only.

On a fresh context window (session start, resume, post-compact), read before you work:
1. See what is there:
     claudebase journal categories
     claudebase journal tags
2. Pull what bears on the task, by tag and category - not an exhaustive scan:
     claudebase journal search "<keywords>" --tag <tag>
     claudebase journal list --category decision --tag <tag>
Mid-task with the context already in hand, skip this.

While you work, journal every decision you make and every finding you uncover, one entry each, as you go:
     claudebase journal add "<what, why, alternatives rejected, where: file:line>" --category decision --tag <tag> --tag <tag>
- Category: reuse one the journal already has (decision, finding, bug, lesson, ...) before inventing one.
- Tags: 1-4 specific nouns (component, technology, feature); reuse existing tags so the entry is found again.
- Self-contained: a later session reads the entry without this conversation.
- --salience high for load-bearing entries (kept forever), low for ambient context (90 days); medium is the default (a year).
- Before a non-trivial decision, search for an earlier one on the same subject; if you reverse it, say so and cite its #id.

If `journal add` answers that it needs a session started with `claudebase run`, tell the operator once and carry on without it.

Full protocol: ~/.claude/rules/project-journal.md
'@

$out = [ordered]@{
    hookSpecificOutput = [ordered]@{
        hookEventName     = 'SessionStart'
        additionalContext = $reminder
    }
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
