#!/usr/bin/env bash
# Slice 7 of cli-to-cli-routing — PostToolUse:ExitPlanMode hook.
#
# Fires after Claude Code invokes the ExitPlanMode tool. Injects a
# system-reminder mandating the agent to publish the feature
# description to the daemon via `claudebase agent describe` AND
# mirror it to <project>/.claude/scratchpad.md in the SAME turn.
#
# Fallback chain (per FR-C2C-7.2) if PostToolUse:ExitPlanMode does
# not fire on this Claude Code build:
#   Fallback A — UserPromptSubmit + prev-turn ExitPlanMode detection
#                (no transcript access at hook time; limited)
#   Fallback B — Stop hook + content-marker check
#   Fallback C — operator-driven via /bootstrap-feature orchestrator
#                (the agent runs `claudebase agent describe` as a final bootstrap step;
#                already de-facto in place this session)
#
# ASCII-only constraint applies to the .ps1 sibling; this .sh script
# may use UTF-8 because Bash hosts accept it without issue.

set -e

PLAN_PATH=".claude/plan.md"
if [ -f "$PLAN_PATH" ]; then
    FEATURE_HEADING=$(grep -m 1 '^# ' "$PLAN_PATH" 2>/dev/null | sed 's/^# *//' | head -c 240)
    if [ -z "$FEATURE_HEADING" ]; then
        FEATURE_HEADING="(plan.md has no top-level heading)"
    fi
else
    FEATURE_HEADING="(no .claude/plan.md found in cwd)"
fi

CTX="[claudebase feature-describe mandate]\n\nA plan has just been approved via ExitPlanMode. Before continuing, you MUST publish the feature description to the daemon AND mirror it to scratchpad in the SAME turn:\n\n1. Run: claudebase agent describe \"${FEATURE_HEADING}\" -- publishes the label into your registry row. The daemon binds your identity from the session token exported by 'claudebase run'; callers cannot impersonate.\n2. Update .claude/scratchpad.md '## Feature:' line to match the same description.\n\nBoth writes happen in this turn so the daemon (visible to peers via 'claudebase agent list') and your personal scratchpad never drift. Skipping breaks cross-agent discovery."

# Escape for JSON: replace literal newlines with \\n and double quotes
# with \\\". We use printf so the backslash sequences land in the
# OUTPUT JSON exactly as written.
ESC_CTX=$(printf '%s' "$CTX" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || python -c 'import json,sys; print(json.dumps(sys.stdin.read()))')

printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":%s}}\n' "$ESC_CTX"
