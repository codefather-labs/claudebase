---
name: 🔌 Claude Code integration question
about: "Question about how claudebase integrates with Claude Code: the PTY supervisor, the Telegram channel, or peer messaging"
title: '[plugin] '
labels: ['integration', 'question']
assignees: []
---

## What you're trying to do

<!-- One sentence — the end goal -->

## What you've tried

<!-- Steps you've already taken, including config + commands -->

## What's happening

<!-- Symptoms — error messages, missing behavior, unexpected output -->

```text
<paste relevant output / logs / wire format>
```

## Environment

| Field | Value |
|---|---|
| claudebase version | `<claudebase --version>` |
| Claude Code version | `<claude --version>` |
| Daemon | output of `claudebase daemon status` |
| Bot registered | output of `claudebase telegram bots` (secrets are never printed) |
| Session launched with | `claudebase run` or bare `claude`? Inbound messages only arrive under the former. |
| OS | mac / linux / windows |

## Relevant logs

<!-- `claudebase daemon logs` for the daemon side (Telegram polling, delivery);
     `~/.claude/logs/claudebase-run-<pid>.log` for the supervisor side (subscriptions,
     `injected inbound block`, gate stalls) — it is written there by default so the
     terminal stays clean. -->

```text
<paste tail of log here>
```
