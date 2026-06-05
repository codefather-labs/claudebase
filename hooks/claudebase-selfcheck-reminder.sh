#!/usr/bin/env bash
# claudebase UserPromptSubmit hook — cognitive-self-check reminder.
#
# Fires before the agent starts responding to a user prompt. Injects a SHORT
# agent-only reminder of the three cognitive-self-check protocols so the agent
# doesn't silently drift from them over a long session. If the agent has
# drifted, the reminder points it back to the authoritative rule file.
#
# Lives in claudebase (not SDLC): the cognitive-self-check protocol rule
# (~/.claude/rules/cognitive-self-check.md) is shipped by claudebase as part of
# its cognitive-infrastructure layer — the books/insights corpora and the
# Facts/Decisions evidence discipline all rest on it. The reminder hook ships
# with the rule it reminds about.
#
# Wired via ~/.claude/settings.json:
#   hooks.UserPromptSubmit[*].hooks[*].command =
#     ~/.claude/hooks/claudebase-selfcheck-reminder.sh
#
# Channel: hookSpecificOutput.additionalContext (agent-only). Deliberately NO
# systemMessage — this fires on every prompt, and a per-prompt operator bubble
# would be noise. The operator's CLI stays clean; only the agent sees the nudge.
#
# Exit code: 0 always. Never blocks the prompt.

set -u

reminder='[self-check reminder] Before you respond, confirm you are running the three cognitive-self-check protocols (~/.claude/rules/cognitive-self-check.md):
  • Protocol 3 (Inbound) — challenge the task BEFORE executing; push-back is not failure.
  • Protocol 1 (Facts) — cite file:line / source verified THIS session; training-data recall is not evidence.
  • Protocol 2 (Decisions) — hack? sane? alternatives? symptom-or-cause? root-cause tracked?
If you have drifted from these over the session, re-read the rule now.'

if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$reminder" \
    '{ hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: $ctx } }'
else
  # No jq — plain stdout on UserPromptSubmit is added to the agent context.
  printf '%s\n' "$reminder"
fi

exit 0
