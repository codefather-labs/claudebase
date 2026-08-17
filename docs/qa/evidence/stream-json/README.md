# Evidence — `claude -p --input-format stream-json` as a fallback transport (Plan Б-1)

**Date:** 2026-08-16 · **Claude Code:** 2.1.233

## Question

Is `--input-format stream-json` a one-shot (`-p` prints and exits) or a real long-lived session that
accepts multiple messages over time? The whole value of Plan Б-1 hinges on the second.

## Result — long-lived, multi-message, context-preserving

`probe.py` starts ONE `claude` process, sends `MSG-A`, waits 12 s, sends `MSG-B` into the same
stdin, and reads stdout. Both were answered (`альфа`, `браво`) with:

- the same `session_id` across both turns,
- `cache_read_input_tokens` growing 16 016 → 41 092 on the second turn, i.e. the second message
  saw the first turn's context — it is one conversation, not two cold starts,
- `--replay-user-messages` echoing each inbound message back as `{"type":"user", …,
  "isReplay":true}` — a delivery acknowledgement the PTY transport cannot offer,
- structured lifecycle events: `system/init` (with the full tool list), `assistant`, and a final
  result object carrying `stop_reason`, `usage`, `total_cost_usd`.

See `probe-output.txt` for the raw run.

## Why this is a fallback, not the plan

`-p` has **no TUI**: no operator typing, no slash commands, no permission dialogs, no rendered
output. It is a headless worker whose only conversational channel would be Telegram. That is a
different product from "operator sits in Claude Code and can *also* be reached from a phone", which
is what `claudebase run` is for.

Its value is exactly where PTY is weakest — it structurally cannot hit findings F-2 (submit
timing), F-3 (modals eating messages), or F-6 (concatenating with the operator's draft), because
none of those concepts exist without a terminal.

## Reproduce

```bash
python3 docs/qa/evidence/stream-json/probe.py
```

Message shape accepted on stdin (one JSON object per line):

```json
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
```
