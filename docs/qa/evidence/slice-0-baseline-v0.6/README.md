# Slice 0 — v0.6 Baseline TG↔notification Evidence

**Captured:** 2026-06-02 ~23:42 local (UTC 2026-06-02T20:42-20:43)
**Branch:** `feat/multi-agent-on-v0.6` (HEAD = tag `claudebase-v0.6.0`, **zero code changes**)
**Operator:** sent `baseline-smoke-v0.6` from Telegram user `@gubernatorcalifornii` (id `8791871989`) to bot `@heymytechcclaude_bot` (id `8935349375`) via DM

## Why this evidence exists

Per plan v4 § Slice 0, before any of Slices 1-7 modify v0.6 code, this is the **regression baseline**: v0.6's Telegram → Claude Code notification path delivers a `<channel ...>`-equivalent MCP notification frame correctly. Every subsequent slice's `Done when:` includes "post-slice TG↔notification smoke matches this baseline". Failure to match = BLOCKED.

## What was proved

| Claim | Evidence file | Salient line |
|---|---|---|
| Bot token valid; teloxide-side daemon polls TG without 401/409 | `daemon-startup.log` | `"telegram long-poll starting"` |
| v0.6 plugin loads, polls TG via frankenstein, registers MCP capabilities | `plugin-stderr.log` lines 1-2 | `polling started bot_username=heymytechcclaude_bot` |
| v0.6 plugin advertises **frozen wire contract** in MCP `initialize` response | `plugin-stdout.jsonl` line 1 | `"experimental":{"claude/channel":{},"claude/channel/permission":{}}` |
| v0.6 plugin emits exact `notifications/claude/channel` JSON-RPC frame on inbound | `plugin-stdout.jsonl` line 2 | `"method":"notifications/claude/channel"` |
| `chat_id`, `message_id`, `user_id` serialized as **strings** in v0.6 already | `plugin-stdout.jsonl` line 2 | `"chat_id":"8791871989"`, `"message_id":"37"`, `"user_id":"8791871989"` |
| Meta shape matches PRD §18 frozen-contract enumeration | `plugin-stdout.jsonl` line 2 | `{chat_id, message_id, ts, user, user_id}` (no `thread_id` for DM — correct) |
| Plugin's internal log confirms it routed the inbound as a `kind=text` channel event | `plugin-stderr.log` line 3 | `emitting channel notification from=gubernatorcalifornii user_id=8791871989 chat_id=8791871989 msg_id=37 kind=text` |

## Evidence-side facts that updated the plan

- **Plan v4 R-MAT-Y (string-id discipline) had a misattribution.** Plan v2/v4 referenced commit `f69c634` ("serialize channel-meta chat_id as a string") as a v0.8 fix. Reality: v0.6 already serializes chat_id, message_id, user_id as strings (see line 2 of `plugin-stdout.jsonl`). The v0.8 commit must have been re-applying / preserving v0.6 behavior or fixing a v0.7-era regression — NOT introducing the string-id discipline. Salience: medium for downstream — Slice 3 implementer can implement matching string-id discipline without inventing a new convention; v0.6 has the canonical example already.
- **MCP `initialize` capabilities use experimental `claude/channel` namespace.** Not `notifications/claude/channel` as Mira's PRD §18 phrasing implied; the *method name* of the notification IS `notifications/claude/channel`, but the *capability advertisement* is `experimental.claude/channel`. Both must stay frozen — they are sibling artifacts of the same contract.

## Procedure to re-run this baseline (regression check after any slice)

```bash
# 1. Build v0.6 baseline
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"   # libclang for ocr-rs bindgen on Windows
cargo build --release -p claudebase -p telegram-plugin-rs

# 2. (Optional sanity) Boot daemon, verify token+polling clean
target/release/claudebase.exe daemon serve   # foreground; check log line "telegram long-poll starting"; Ctrl+C

# 3. Boot plugin alone with piped MCP initialize, capture stdout/stderr
(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"slice-N-regression","version":"0.1"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}'; sleep 600) \
  | TELEGRAM_BOT_TOKEN='<TOKEN>' RUST_LOG=info target/release/telegram-plugin-rs.exe \
  > plugin-stdout.jsonl 2> plugin-stderr.log

# 4. Send "regression-smoke-slice-N" from same TG user to same bot in DM

# 5. Verify plugin-stdout.jsonl line 2 matches this baseline's shape (method, meta keys, string types)
```

A subsequent slice that breaks this — channel notification not emitted, method renamed, payload field types changed, etc — is a regression that BLOCKS the slice per plan v4 § Per-Slice Evidence Gate.

## Security notes — token rotation

The bot token used during this baseline was shared in plain text in the conversation that drove this bootstrap. The transcript persists in logs (Anthropic-side at minimum). Recommended action after Slice 0 is approved:

1. BotFather → `/mybots → @heymytechcclaude_bot → API Token → Revoke current token`
2. Capture the new token
3. Update `~/.claude/channels/claudebase/.env` (daemon source) AND the env-var pipeline used by the plugin
4. Re-run this baseline to confirm the new token works

## Files in this directory

- `daemon-startup.log` — claudebase daemon stderr (JSON-formatted tracing) showing IPC bind + long-poll startup
- `plugin-stdout.jsonl` — telegram-plugin-rs stdout — JSON-RPC frames (initialize response + channel notification)
- `plugin-stderr.log` — telegram-plugin-rs stderr — human-readable tracing
- `README.md` — this file
