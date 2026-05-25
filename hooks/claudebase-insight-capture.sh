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

reason='[claudebase insight-capture] Before you stop — reflect on the turn you just finished.

Did you genuinely learn something, catch a mistake, or have an assumption falsified this turn? Map it to the insight-corpus axes:
  1. SELF-LEARNING (--type agent-learned | self-bias-caught) — a domain concept, a technique, or a blind spot in your own past reasoning you just noticed.
  2. PREDICTION-REALITY MISMATCH (--type prediction-error | assumption-falsified | plan-reality-gap) — something you predicted, assumed, or planned turned out wrong.
  3. OPERATOR-CORRECTION (--type operator-correction) — the operator corrected you in a way worth carrying into future sessions.

If YES — and it is genuinely axis-worthy (NOT mechanical execution, NOT a restatement of the task, NOT generic best-practice) — persist exactly ONE insight, then stop:

    claudebase insight create "<one-sentence insight in your own words>" \
        --type <source-type> --agent <your-agent-name> --salience <high|medium|low> \
        [--feature "<feature-slug-if-known>"] [--session "'"$session_id"'"]

Salience honestly: high = loss degrades the whole pipeline (rare); medium = slice/decision-level (default); low = ambient.

If NO — nothing genuinely new, or the turn was mechanical — STOP SILENTLY. Write no insight. Print no commentary, no "no insight" line, nothing. Just end the turn. An honest silent skip is the correct and most common outcome; the corpus rejects manufactured or generic insights, so do not invent one to satisfy this hook.'

jq -n --arg sm "$sys_msg" --arg rs "$reason" \
  '{ decision: "block", reason: $rs, systemMessage: $sm }'

exit 0
