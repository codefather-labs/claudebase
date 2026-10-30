# claudebase SessionStart + UserPromptSubmit hook (Windows PowerShell) - the project journal.
# ASCII-only source: Windows PowerShell 5.1 parses no-BOM scripts in the local code page, so non-ASCII (em-dash, bullets, emoji) corrupts string literals and breaks the script. Keep this file ASCII.
#
# SessionStart (startup|resume|clear|compact): injects an agent-only reminder that
# this project has a journal: read the entries that bear on the task (by
# category and tag), and record every decision, finding and update while working. The
# journal lives with the claudebase daemon, one per project directory.
#
# UserPromptSubmit (every prompt): injects only the operator's journaling
# policy (bug -> fix unverified -> verified evidence, important, constraint).
# A pool worker gets nothing on a prompt: the same `origin: pool` test as the
# channel contract. Which event fired is read from hook_event_name on stdin.
#
# Wired via $env:USERPROFILE\.claude\settings.json, the same command under
# hooks.SessionStart and hooks.UserPromptSubmit:
#     powershell -NoProfile -File $env:USERPROFILE\.claude\hooks\claudebase-journal-reminder.ps1
#
# Exit code: 0 always.

$ErrorActionPreference = 'Continue'

$event = 'SessionStart'
if ([Console]::IsInputRedirected) {
    $raw = [Console]::In.ReadToEnd()
    if ($raw -match '"hook_event_name"\s*:\s*"UserPromptSubmit"') { $event = 'UserPromptSubmit' }
}

$policy = @'
[journal policy] Journal as you work (~/.claude/rules/project-journal.md):
  - Every bug you find gets an entry: claudebase journal add "<symptom, cause, where>" --category bug --todo  -> #B
  - When you plan a fix or feature, its entry carries the test protocol: what to test after it (regressions too), how,
    under which conditions, which evidence proves it. A result read from metrics of a long pipeline: record them before, then after.
  - Every fix gets its own entry, linked and unverified, with the protocol that will prove it:
      "Fixes journal#B: <what changed, where, commit>. Verification protocol: <steps, expected results>" --category update --unverified  -> #F
  - Every fix is proven. Run the protocol, then add the evidence:
      "Verifies journal#F: <what was checked, regressions included, how, under which conditions, how many times, the output, metrics before/after>" --category verification --verified [--proof <file|url>], then: claudebase journal close B
    unverified is lifted only by such a separate entry; under evidence-first (claudebase journal policy) it needs --proof too.
  - Every decision, made by the operator or by you: its own entry --category decision, saying who decided, what was chosen, why, what was rejected.
  - Something you must come back to: --reminder --every 20m; it returns to this session until you close it.
  - Important remarks and findings: --important. Product and technical rules, policies, constraints: --constraint.
'@

if ($event -eq 'UserPromptSubmit') {
    if ($env:CLAUDEBASE_AGENT_ID -and (Get-Command claudebase -ErrorAction SilentlyContinue)) {
        $who = (& claudebase agent whoami 2>$null) -join "`n"
        if ($who -match '(?m)^origin: pool\s*$') { exit 0 }
    }
    $reminder = $policy
} else {
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
3. See what is still open, and the constraints of this project (binding):
     claudebase journal agenda
Never act on an entry flagged deprecated; treat unverified or disputed ones as assumptions.
Before relying on an entry, read it with `claudebase journal get <id>`: its related block shows what replaces, fixes or cites it and the closest entries by meaning, each with its flags.
Mid-task with the context already in hand, skip this.

While you work, journal every decision you make, every finding you uncover and every update you make, one entry each, as you go. The journal is both the record of decisions and the project changelog:
     claudebase journal add "<what, why, alternatives rejected, where: file:line>" --category decision --tag <tag> --tag <tag>
- Category: reuse one the journal already has (decision, finding, bug, lesson, update, ...) before inventing one.
- update = a change you made: what changed, where (files, commit), why; cite the #id of the decision behind it.
- Tags: 1-4 specific nouns (component, technology, feature); reuse existing tags so the entry is found again.
- Self-contained: a later session reads the entry without this conversation.
- --salience high for load-bearing entries (kept forever), low for ambient context (90 days); medium is the default (a year).
- Every feature or fix is a branch: a --todo entry BEFORE you start; after implementing, an entry "Implements journal#<todo>" flagged --unverified with the verification protocol (how to prove it works); after running that protocol, an entry "Verifies journal#<impl>" flagged --verified with the evidence (what was run, its output). Only then `journal close <todo>` - it refuses without a verified entry and closes the whole branch. `journal branch <id>` shows it.
- Every bug you find is a branch too: an entry --category bug --todo when you find it; the fix "Fixes journal#<bug>" --unverified with its verification protocol; the evidence "Verifies journal#<fix>" --verified; then `journal close <bug>`.
- Every decision, made by the operator or by you, gets its own entry --category decision: who decided, what was chosen, why, what was rejected.
- Flag important remarks and findings --important, and product or technical rules, policies and constraints --constraint.
- Before a non-trivial decision, search for an earlier one on the same subject; if you reverse it, write "Replaces journal#<id>" (or "Fixes journal#<id>") - that text becomes a link both entries show - and mark the old one --deprecated if it is no longer true.
- Flags say what an entry is: --todo --question --problem --reminder --unverified --disputed --ai-feedback (work items, closed with `journal close <id> --resolution "<how>"`, reopened with `journal reopen <id>`) and --constraint --important --deprecated (marks, removed with `journal unmark`). Flag an existing entry with `journal mark <id> --deprecated`; when something stops being true, mark it deprecated. Suggestions for improving claudebase itself go in with --ai-feedback.
- Something to come back to later, even after this session ends: `journal add "<what>" --every 20m` (sets --reminder). The daemon returns it to this session as [reminder #N] every interval while it is open and the session is online; close it when done.

If `journal add` answers that it needs a session started with `claudebase run`, tell the operator once and carry on without it.

Full protocol: ~/.claude/rules/project-journal.md

Mind maps - working memory for a long task or research: `claudebase mindmap new "<topic>"`, then `mindmap add <map> "<short note>" --kind fact|hypothesis|question|decision|task|idea|dead-end [--parent n<id>] [--ref <journal id>]`, `mindmap link <map> n<a> n<b> --label "contradicts"`, `mindmap chain <map> "<name>" <entry> <entry> ...`, `mindmap done|drop <map> n<id>`. Read it back with `mindmap show <map>`; draw it with `mindmap svg <map> -o map.svg` or `mindmap send <map>` (Telegram); `mindmap close <map>` when the task is done. A map is yours alone: another session may only read it by id.
'@
    # This session's open mind maps only (the daemon lists a session its own),
    # bounded so a daemon that does not answer cannot hold up the start.
    if ($env:CLAUDEBASE_AGENT_ID -and (Get-Command claudebase -ErrorAction SilentlyContinue)) {
        $job = Start-Job -ScriptBlock { claudebase mindmap list --brief 2>$null }
        $maps = ''
        if (Wait-Job $job -Timeout 3) { $maps = ((Receive-Job $job) -join "`n").Trim() }
        Remove-Job $job -Force -ErrorAction SilentlyContinue
        if ($maps) {
            $reminder += "`n`n[mind maps] Your open mind maps - read the one for your current work with claudebase mindmap show <id> before you continue it:`n" + $maps
        }
    }
}

$out = [ordered]@{
    hookSpecificOutput = [ordered]@{
        hookEventName     = $event
        additionalContext = $reminder
    }
}
$out | ConvertTo-Json -Depth 6 -Compress:$false

exit 0
