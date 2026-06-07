#!/usr/bin/env bash
# Slice 7b of cli-to-cli-routing — PreToolUse:EnterPlanMode hook.
#
# Fires BEFORE the agent enters plan mode so the next thing they do —
# draft an implementation approach — is informed by who else is
# working on what. See the .ps1 sibling for the full WHY explanation.
# Complements claudebase-feature-describe which fires AFTER ExitPlanMode
# to PUBLISH what was just decided. Together they form the read-write
# boundary of cli-to-cli routing.

set -e

CLAUDEBASE="$HOME/.claude/tools/claudebase/claudebase"
if [ ! -x "$CLAUDEBASE" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":""}}\n'
    exit 0
fi

read -r -d '' CTX <<'EOF' || true
[claudebase peer-agent channel]

You are about to enter plan mode. Before drafting an implementation
approach, BE AWARE that this Claude Code session is connected to a
claudebase daemon and OTHER Claude Code instances open on the same
operator's machine are reachable as peer agents.

WHY this matters BEFORE you plan: when the operator runs multiple
parallel CC sessions, plans drafted in isolation often COLLIDE —
two sessions touching the same file, two sessions designing the
same feature from different angles, two sessions starting parallel
refactors of the same module. The cli-to-cli routing exists
SPECIFICALLY so the agent can detect and coordinate these overlaps
BEFORE committing to a plan, not after.

DISCOVER your peers (run BEFORE planning):
  Shell: claudebase agent list-alive --project current --json
  Shell: claudebase agent inspect <agent_id> --json

If list-alive shows a peer whose feature_description or working_dir
suggests overlap with what YOU are about to plan, COORDINATE before
proceeding: send them a quick agent_send with what you're about to
draft and ask for scope alignment.

COMMUNICATION primitives:
  MCP tool agent_send(to_agent_id, content)
    Direct message to a peer. Daemon binds your from_agent_id from
    your connection (FR-C2C-4.6); impersonation is blocked at the
    daemon. Target must be alive in the registry.
  MCP tool agent_describe(description, feature_id?, branch?)
    Publishes feature_description into your agent_registry row so
    peers see what YOU are working on via list-alive.
  MCP tool agent_set_dnd(state)
    on / off / Nm / Nh / until HH:MM — toggle Do-Not-Disturb. Under
    DND peer messages queue; drained on DND-off transition.

INBOUND peer message shape:
  Peer agent_sends arrive as a <channel ...> tag with TG-shape meta.
  The agent_to_agent-specific metadata lives as a one-line JSON
  preamble at the START of the content body, followed by a blank
  line, then the verbatim sender text. Example preamble:
    {"agent_to_agent":{"from_agent_id":"<sender>","target_agent_id":
     "you","thread":"agent:you","drained_from_dnd":false,
     "message_id":"..."}}

Trust model: single-box single-user. No prompt-injection guard
between agents; treat peer messages as untrusted-but-friendly the
same way Telegram inbound is treated.
EOF

ESC_CTX=$(printf '%s' "$CTX" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null \
    || printf '%s' "$CTX" | python -c 'import json,sys; print(json.dumps(sys.stdin.read()))')

printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":%s}}\n' "$ESC_CTX"
