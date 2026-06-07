# Issue 006 — Telegram voice/message transcribes correctly but never reaches the live Claude Code session

**Status:** OPEN (root cause confirmed; fix is a design change — needs a planned pipeline pass, not a live patch)
**Date:** 2026-06-07
**Severity:** High (cli-to-cli routing / TG channel delivery)
**Area:** `src/plugin/bridge.rs`, `src/daemon/server.rs`, `src/daemon/telegram.rs`, `.claudebase/config.json`

## Symptom

Operator sends a Telegram voice note (or `/start`) to the bound bot. The daemon polls
it, ASR transcribes it correctly, and the row lands in `chat_messages` — but the
operator gets no response and the live Claude Code session never receives the
`<channel …>` injection. `/switch` / `/agents` intermittently do not list the session.

## What is NOT the problem (verified working this session)

- **ASR transcription works** — `раз два три четыре` → `1 2 3 4 1 2 3 4` (correct, source-language
  auto-detect, not English-translated) landed in `chat_messages` (`thread_id=telegram:434566766`).
  Shipped in v0.8.1 (`--features asr-whisper` + `set_language("auto")` + default `[asr] backend`).
- **Daemon Telegram polling works** — 0 sustained `getUpdates` Conflicts once the operator's own
  diagnostic `curl getUpdates` probes stopped (each probe was itself a rival consumer → see doc#106).
- **Daemon-side broadcast publish runs** — `telegram.rs:2007 bus.publish(&thread, frame)`.

## Falsified assumption (was a wrong diagnosis for ~several turns)

`chat_messages.delivered_at` is **NOT** a delivery indicator for INBOUND telegram messages.
The inbound publish path (`telegram.rs:2007`) only logs `subscribers=n` at DEBUG and does **not**
call `mark_delivered`. `delivered_at` is set only by: outbound TG-send (`telegram.rs:1694`),
backlog-drain on subscribe (`chat.rs drain_backlog`), and the agent-to-agent DND-drain
(`server.rs:1816`). So an inbound voice with `delivered_at=NULL` may well have been broadcast to
live subscribers. (Captured: insights doc#56.)

## Confirmed root causes

1. **Pinned, shared `agent_id` across every session.** `.claudebase/config.json` contains a
   FIXED `"session_id": "fa6c34ec-568c-4690-8b0b-fab1f5e632a2"`, and `derive_identity()`
   (`bridge.rs:735`) uses `cfg.session_id` verbatim as the `agent_id`. Therefore EVERY Claude Code
   session launched in this repo derives the **same** `agent_id` `fa6c34ec…`. This was intentional
   for "stable agent_name/binding survives restart" (commit 69b36e1), but it means concurrent
   sessions are indistinguishable.

2. **No eviction of stale same-`agent_id` connections on re-register.** `handle_agent_register`
   (`server.rs`) updates the registry row's `connection_id` to the newest connection but does NOT
   tear down the OLDER connection's `chat_subscribe` forwarding tasks. The stale connection's
   broadcast subscription stays live.

3. **Stale bridge processes accumulate.** Observed **7** live `claudebase plugin serve` processes
   from accumulated CC session churn (operator restarts + agent debug restarts). A dead CC session's
   bridge does not reliably die, so multiple live bridges all claim `fa6c34ec` and all stay
   subscribed to `telegram:434566766`. A broadcast then reaches a stale bridge whose stdout is a
   dead CC session — the message is "delivered" to nobody visible.

4. **Per-session bridge sometimes does not expose its MCP tools / subscribe.** This session's bridge
   never surfaced `mcp__claudebase__*` tools (ToolSearch found none) despite the agent showing
   `alive` in `list-alive`, indicating the live session's bridge was not fully connected/subscribed
   while a *different* (stale) connection (`fa9d3ccd`, subscribed 15:01:35) held the subscription.
   This is the parked bridge-auto-subscribe symptom (doc#54).

## Why daemon restarts made it worse (operator-visible)

Each `claudebase daemon restart` drops every bridge's UDS connection → the daemon marks the agent
`orphaned` and the bridge must reconnect+re-subscribe. The reconnect is unreliable (root cause 4),
so debug-restarting the daemon under a live session orphaned the very connection being observed.
**Do not debug-restart the daemon while live sessions are bridged** (insights doc#55, doc#107).

## Fix design (needs a planned pipeline pass — NOT a live hack)

- **Option A (recommended) — bind by stable NAME, unique per-session `agent_id`.** Make
  `derive_identity()` produce a UNIQUE `agent_id` per session (real CC session id / per-process
  uuid) while keeping a STABLE `name` (`claude-code-sdlc`). Route + bind chats by **name**, resolving
  name → newest-alive `agent_id` at routing time. Binding survives restart (by name) AND concurrent
  sessions no longer collide. Requires: `active_cli_per_chat` keyed/resolved by name; routing tree
  name-resolution; migration of the pinned-id config.

- **Option B — daemon evicts stale same-`agent_id` connections on re-register.** When a new
  connection registers `agent_id` X, close/tear down any OTHER live connection's subscriptions for
  X so the newest live bridge wins. Smaller change; does not fix the concurrent-distinct-sessions
  case, but fixes the operator's serial-restart case.

- **Option C — bridge lifecycle.** Ensure a bridge process dies when its CC session ends (or the
  daemon eagerly evicts a connection whose stdout peer is gone), preventing stale-bridge
  accumulation.

A + C together are the principled fix; B is a pragmatic stopgap.

## Immediate operational mitigation (no code)

Clean environment: kill stale `claudebase plugin serve` processes, keep exactly one live CC session,
restart the daemon once cleanly, then one CC restart so a single bridge subscribes. This sidesteps
roots 1–3 for a single-session operator until the code fix lands.

## Implementation plan (Option B — ready to execute, operator chose "one session at a time")

Confirmed scoping fact: the daemon has **no** global `connection_id → outbound_tx` registry — each
`outbound_tx` is local to its `handle_connection` task (`server.rs:520`). Eviction therefore needs a
new shared cancel-registry. Steps:

1. **Add a shared connection-cancel registry** `Arc<Mutex<HashMap<Uuid, oneshot::Sender<()>>>>`,
   created in `serve()` and passed into `handle_connection` (alongside `bus`).
2. **`handle_connection`**: create `(close_tx, close_rx)`; insert `close_tx` under `connection_id`;
   pass `close_rx` into `run_request_loop`; remove the entry on task exit.
3. **`run_request_loop`**: `tokio::select!` between `read_frame(...)` and `close_rx` → on close fire,
   return cleanly (Evicted). Clean return drops `outbound_tx` → writer + per-thread forwarder tasks
   exit → the connection's `chat_subscribe` broadcast subscriptions are torn down.
4. **`handle_agent_register`**: BEFORE `register()` updates the row, read the prior `connection_id`
   bound to `agent_id`; if it differs from the current connection AND is present in the registry,
   remove it and fire its `close_tx` (evict the stale bridge).
5. **`agent_registry`**: add `prior_connection_id(conn, agent_id) -> Option<String>`.
6. **Regression test** (extend `tests/chat_tools_e2e_test.rs`): conn1 registers agent X + subscribes
   thread T → conn2 registers agent X → assert conn1 is evicted (socket closed / stops receiving) AND
   a publish to T reaches conn2 only.

Concurrency invariants the `tokio-async-specialist` MUST verify before ship:
- No `.await` while holding the registry `Mutex` (lock → take handle → drop guard → then fire).
- `oneshot::Sender::send` is non-blocking; the select arm must not drop a partially-read frame.
- Evicting a connection that is concurrently exiting is a safe no-op (`remove` returns `None`).

This is concurrency-sensitive daemon surgery; per "сделай как положено" it ships only after the
implementation passes the tokio-async-specialist audit + the regression test + a clean build — NOT
as a rushed end-of-session patch.

## Facts

### Verified facts
- `.claudebase/config.json` pins `session_id: "fa6c34ec-…"` and `derive_identity()` uses it as
  `agent_id` — source: `~/.claude/.../.claudebase/config.json` + `src/plugin/bridge.rs:735` read this session — salience: high
- `handle_agent_register` updates the registry connection_id but does not evict prior connections —
  source: `src/daemon/server.rs` `handle_agent_register` body read this session — salience: high
- Inbound publish (`telegram.rs:2007`) does not mark `delivered_at` — source: read this session — salience: high
- 7 live `claudebase plugin serve` processes observed — source: `ps aux` this session — salience: medium
- bridge subscribed `telegram:434566766` at 15:01:35 (connection `fa9d3ccd`) — source: daemon.err.log — salience: medium

### External contracts
- **Telegram Bot API** — `getUpdates` single-consumer-per-token (409 Conflict on a rival) — source: Telegram Bot API docs (not opened this session) — verified: no — assumption — salience: medium

### Assumptions
- Operator's intended pattern is ONE long-lived "Mira" session bound to the chat (not many concurrent) — risk: if many concurrent sessions are intended, Option A is mandatory not optional — how to verify: ask operator — salience: high

### Open questions
- Does the operator run multiple concurrent CC sessions against this bot, or one at a time? — needs: operator decision — salience: high

## Decisions

### Inbound validation
- Operator said "сделай как положено" after prolonged live-debugging — challenged: yes — outcome: stopped live-poking; documented confirmed root cause + fix design as this issue instead of patching live — salience: high

### Decisions made
- Documented root cause + 3 fix options rather than rushing a live patch — Q1 hack? no (avoids a band-aid) | Q2 sane? yes | Q3 alternatives? live patch (rejected — non-convergent, perturbs state) | Q4 cause? yes (identity model) | Q5 tracked? yes (this issue) — salience: high

### Hacks / workarounds acknowledged
- "Immediate operational mitigation" (kill stale bridges + single session) is an explicit stopgap, not the fix — removal path: implement Option A + C via the pipeline — salience: medium

### Symptom-only patches (with root-cause links)
- (none)
