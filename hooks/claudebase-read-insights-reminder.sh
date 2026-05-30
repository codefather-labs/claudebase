#!/usr/bin/env bash
# claudebase SessionStart hook - read-insights-on-new-context reminder.
#
# Fires on session startup|resume|compact. Injects a SHORT agent-only reminder:
# when entering a FRESH context window, the agent should pull relevant insights
# from the cognitive corpus by TAG/CATEGORY instead of re-reading every prior
# message. Discovery via `claudebase insight tags`, retrieval via
# `claudebase insight search --tag`.
#
# This hook emits TEXT ONLY - it does NOT itself invoke `claudebase`. The agent
# decides whether the pull applies (the phrasing is conditional on "fresh
# context") and runs the CLI calls itself.
#
# The `insight tags` subcommand and the `--tag` search filter referenced below
# ship in the same release as this hook (insights-hybrid-corpus Slices 3-4).
#
# Wired via ~/.claude/settings.json:
#   hooks.SessionStart[*].hooks[*].command =
#     ~/.claude/hooks/claudebase-read-insights-reminder.sh
#
# Channel: hookSpecificOutput.additionalContext (agent-only). No systemMessage -
# this fires on every session start; an operator bubble would be noise.
#
# Exit code: 0 always. Never blocks the session.

set -u

reminder='[read-insights reminder]

If you are entering a fresh context window (session start, resume, or post-compact), load relevant prior-session insights from the cognitive corpus by tag - do NOT try to re-read every prior message:

1. Discover the tag vocabulary:
     claudebase insight tags --project "$(basename "$PWD")"
   (lists tags with counts across this project plus general insights)

2. Load the insights that match what you are about to work on:
     claudebase insight search "<keywords>" --tag <tag>
   Run one or two representative searches keyed to the most relevant tags - read by tag/category, not by exhaustive scan.

If you already hold sufficient context (mid-task, no compaction), skip this - do not re-pull on every message.'

if command -v jq >/dev/null 2>&1; then
  jq -n --arg ctx "$reminder" \
    '{ hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: $ctx } }'
else
  printf '%s\n' "$reminder"
fi

exit 0
