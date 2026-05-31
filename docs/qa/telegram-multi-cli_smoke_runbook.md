# telegram-multi-cli — Live Smoke Runbook

The automated Rust suite (181 lib + 4 `e2e_routing`) covers the routing
decision tree, DB durability, the conflict-detection predicate, and the
daemon-down sentinel. It deliberately does NOT cover the cases that cross
the real Telegram HTTP boundary — those require a live bot token and a
human tapping inline-keyboard buttons. This runbook is that manual pass.

Run it against the feature binary built from `feat/telegram-multi-cli`:
`/Users/aleksandra/Documents/claude-code-sdlc/claudebase/target/debug/claudebase`
(alias it: `alias cb=...target/debug/claudebase`).

The operator drives the daemon + the Telegram app; Mira verifies the
evidence (log lines, `daemon status` fields, DB rows). Capture each
case's evidence and paste it back (or run the verify command via `! cb ...`).

## Prerequisites (one-time)

1. **Bot token** in `~/.config/claudebase/secrets.toml` (mode 0600):
   ```toml
   [telegram]
   bot_token = "<your BotFather token>"
   ```
   `chmod 600 ~/.config/claudebase/secrets.toml` (the loader REFUSES group/other-readable — SEC-15).
2. **Config** `~/.config/claudebase/daemon.toml`:
   ```toml
   [telegram]
   enabled = true
   ```
3. **Verify config is seen (no token leak):** `cb daemon config show` — `bot_token` appears MASKED. Evidence: masked token + `enabled = true`.

## Case S-1 — Daemon comes online and owns the bot

- **Action:** in a dedicated terminal, `cb daemon serve` (blocking; leave it running). In another terminal: `cb daemon status --json`.
- **Evidence (Mira verifies):** `status` JSON shows the daemon running AND `tg_bot_state` = `connected` (a successful first `getUpdates`). `cb daemon logs` shows `telegram long-poll spawned` with no 401/error.
- **PASS when:** daemon up + bot connected, no auth error.

## Case S-2 — Live message routing (chat-as-id)  [needs 1 connected CLI]

- **Setup:** start a Claude Code CLI with the claudebase plugin pointed at the daemon; it registers (`agent_register`) and subscribes to its chat thread. Note its `agent_name`. Bind a chat: from Telegram (or `cb`) `/switch <agent_name>` in the chat.
- **Action:** send a plain text message in that Telegram chat.
- **Evidence:** `cb daemon logs` shows the inbound message routed with `target_agent_id` = the bound CLI's agent_id; the connected CLI receives the `notifications/claude/channel` frame. DB: `active_cli_per_chat` row for this `chat_id` points at the bound CLI.
- **PASS when:** the message reaches ONLY the bound CLI; a second chat bound elsewhere is unaffected.

## Case S-3 — Bot commands  [/agents needs ≥0 CLIs; /switch needs a live name]

- **Action:** in the Telegram chat, send `/agents`, then `/whoami`, then `/here`, then `/switch <name>`.
- **Evidence:** each replies within ~1s. `/agents` lists alive CLIs (or a "no CLIs" line). `/switch <alive>` acks + writes `active_cli_per_chat`. `/switch <unknown>` is rejected with a helpful error. `/whoami` shows the bound CLI; `/here` shows host:cwd or "unavailable" (v1 best-effort).
- **PASS when:** all four reply correctly; `/switch` to a dead/unknown name is rejected.

## Case S-4 — chat_ask button round-trip (the headline feature)  [needs 1 connected CLI]

- **Action:** from the connected CLI (DM chat — chat_ask is DM-only in v1), have the agent call `chat_ask(thread="telegram:<chat_id>", question="pick one", options=[{label:"A"},{label:"B"},{label:"C"}])`. In Telegram, the message renders with 3 inline-keyboard buttons. Tap **B**.
- **Evidence:** (a) buttons appear; (b) tapping dismisses the spinner (`answerCallbackQuery`); (c) the agent receives the answer "B" via the ChatBus; (d) `cb daemon logs` shows the callback validated + routed; (e) the `pending_questions` row for this `question_id` is DELETED after the tap. Forged/stale taps route nothing.
- **PASS when:** the tapped option reaches the asking CLI and only that CLI; spinner dismissed; pending row cleared.

## Case S-5 — 409 conflict gate (migration safety — the core claim)

- **Setup:** have the LEGACY per-CLI telegram plugin running (holding the bot's `getUpdates` slot). Then start `cb daemon serve` (same token).
- **Evidence:** `cb daemon logs` shows EXACTLY ONE warn line containing "409" and "legacy telegram-plugin-rs poller still running" — NOT one per poll. The daemon process stays alive (`cb daemon status` still responds; UDS still accepts). Leave it ~3 min: still only ONE 409 line (60s backoff, log-once — F-2).
- **PASS when:** exactly one 409 log line, daemon never crashes, UDS responsive.

## Case S-6 — Clean cutover takeover

- **Action:** with the daemon backing off on 409 (from S-5), STOP the legacy plugin. Wait ≤60s for the next daemon poll.
- **Evidence:** `cb daemon logs` shows `telegram getUpdates conflict cleared — daemon poller now owns the bot`. A new Telegram message now routes through the daemon (source `claudebase`). Revert check: set `enabled = false` in daemon.toml, `cb daemon restart` → logs show `disabled via [telegram] enabled=false`, daemon does NOT poll.
- **PASS when:** daemon takes over within one poll cycle after the plugin stops; `enabled=false` cleanly reverts.

## Recording

For each case paste: the `cb daemon logs` excerpt + the `cb daemon status --json` field (or DB row) named in Evidence. Mira records PASS/FAIL per case and only proceeds to `/merge-ready` once S-1, S-4, S-5, S-6 (the load-bearing ones) are PASS. S-2/S-3 are PASS-desirable but a connected-CLI environment may defer them to the connected-CLI smoke.
