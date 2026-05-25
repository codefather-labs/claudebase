#!/usr/bin/env bash
# claudebase Stop hook — insight-capture nudge.
#
# Fires when the main agent finishes a turn (Stop event). Unconditionally
# prompts the agent to reflect on the turn it just finished: did it learn
# anything new, catch a mistake, or have an assumption falsified? If yes and
# it is genuinely axis-worthy, the agent persists it via `claudebase insight
# create`. If not, the agent stops SILENTLY — no insight written, no comment.
#
# Why a claudebase hook (not SDLC): the insights corpus, the `insight`
# subcommand, and insights.db are all claudebase features. The tool that owns
# insights owns the trigger that fills them.
#
# Wired via ~/.claude/settings.json:
#   hooks.Stop[*].hooks[*].command = ~/.claude/hooks/claudebase-insight-capture.sh
#
# Loop safety: Claude Code sets `stop_hook_active=true` on the payload when a
# prior Stop hook already forced a continuation in this cycle. We check it
# FIRST and exit 0 — so the reflection turn we force never re-triggers the
# hook. Without this guard the hook would loop forever (block -> reflect ->
# stop -> block -> ...).
#
# Output: JSON envelope per https://code.claude.com/docs/en/hooks.
#   - `decision: "block"` + `reason` -> agent does ONE reflection turn
#   - `systemMessage` -> operator-visible CLI bubble (🪝 style), shown every
#     time the hook fires so the operator can see it is active.
#
# Exit code: 0 always. Never hard-fails the agent.

set -u

payload="$(cat 2>/dev/null || true)"
stop_active="false"
cwd=""
session_id=""
if command -v jq >/dev/null 2>&1 && [ -n "$payload" ]; then
  stop_active="$(printf '%s' "$payload" | jq -r '.stop_hook_active // false' 2>/dev/null || echo false)"
  cwd="$(printf '%s' "$payload" | jq -r '.cwd // empty' 2>/dev/null || true)"
  session_id="$(printf '%s' "$payload" | jq -r '.session_id // empty' 2>/dev/null || true)"
fi
[ -z "$cwd" ] && cwd="$(pwd 2>/dev/null || echo .)"
project_label="$(basename "$cwd" 2>/dev/null || echo project)"

# Guard 1 — loop prevention. The reflection turn we forced must be allowed to stop.
if [ "$stop_active" = "true" ]; then
  exit 0
fi

# Guard 2 — claudebase must be installed (insights are a claudebase feature).
# If the binary is absent there is nowhere to persist insights; stop silently.
if ! command -v claudebase >/dev/null 2>&1 && [ ! -x "$HOME/.claude/tools/claudebase/claudebase" ]; then
  exit 0
fi

# Guard 3 — jq required to emit the structured block envelope.
if ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

sys_msg="🪝 claudebase insight-capture hook — event=Stop project=${project_label}"

# Reason is operator-visible by Claude Code design (block reasons always
# surface). Keep it to ONE compact line so the operator's collapsed view is a
# short pointer, not a wall — the agent already knows the full insight protocol
# from ~/.claude/rules/knowledge-base-tool.md.
reason="Reflect on the turn you just finished: if it produced a genuine insight (self-learning, a falsified prediction/assumption, or an operator correction worth carrying forward), persist exactly ONE via \`claudebase insight create \"<one sentence>\" --type <agent-learned|self-bias-caught|prediction-error|assumption-falsified|plan-reality-gap|operator-correction> --agent <you> --salience <high|medium|low> --session ${session_id}\`. Otherwise stop silently — no insight, no commentary, do not manufacture one."

jq -n --arg sm "$sys_msg" --arg rs "$reason" \
  '{ decision: "block", reason: $rs, systemMessage: $sm, suppressOutput: true }'

exit 0
