#!/usr/bin/env bash
# claudebase UserPromptSubmit hook - pre-response reminder.
#
# Fires before the agent responds to a user prompt. Injects a SHORT agent-only
# reminder covering TWO things:
#   1. the three cognitive-self-check protocols (Facts / Decisions / Inbound), and
#   2. insight-capture: if the PREVIOUS turn produced a genuine insight, persist
#      it now before continuing.
#
# Why insight-capture lives HERE (UserPromptSubmit) and not on Stop: a Stop hook
# can only force a reflection by returning `decision: block`, which Claude Code
# renders to the operator as "Stop hook error: ..." - alarming, looks like a
# failure, and forces an extra turn every response. UserPromptSubmit injects
# agent-only additionalContext with no operator bubble and no "error" framing,
# and folds the reflection into the next natural turn (looking back one turn).
# Trade-off: the very last turn of a session is not reflected on - acceptable.
#
# Wired via ~/.claude/settings.json:
#   hooks.UserPromptSubmit[*].hooks[*].command =
#     ~/.claude/hooks/claudebase-selfcheck-reminder.sh
#
# Channel: hookSpecificOutput.additionalContext (agent-only). No systemMessage -
# this fires on every prompt; an operator bubble per prompt would be noise.
#
# Exit code: 0 always. Never blocks the prompt.

set -u

reminder='[pre-response reminder]

1. Run the three cognitive-self-check protocols (~/.claude/rules/cognitive-self-check.md):
   - Protocol 3 (Inbound) - challenge the task BEFORE executing; push-back is not failure.
   - Protocol 1 (Facts) - cite file:line / source verified THIS session; training-data recall is not evidence.
   - Protocol 2 (Decisions) - hack? sane? alternatives? symptom-or-cause? root-cause tracked?

2. Insight-capture: if your PREVIOUS turn produced a genuine insight worth carrying across sessions - self-learning, a falsified prediction/assumption, or an operator correction - persist exactly ONE now, before continuing:
     claudebase insight create "<one sentence>" --type <agent-learned|self-bias-caught|prediction-error|assumption-falsified|plan-reality-gap|operator-correction> --agent <you> --category <general|project> --tags <tag> --salience <high|medium|low>
   --category (required: general=cross-project lesson -> global db; project=this-project insight -> local db) and --tags (required: at least one free-form tag, e.g. a feature slug or a domain like sqlite) are MANDATORY - omitting either exits 2. Read-time --tag filtering is OR / any-intersection.
   If the previous turn was mechanical or produced nothing genuinely new, skip silently - do not manufacture an insight.'

if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$reminder" \
    '{ hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: $ctx } }'
else
  printf '%s\n' "$reminder"
fi

exit 0
