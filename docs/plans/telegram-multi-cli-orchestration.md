# Plan: Telegram ↔ CLI multi-instance orchestration

**Owner:** Mira (orchestrator)
**Status:** draft — awaiting operator review
**Created:** 2026-05-24

**DEPENDS ON (must land first):**
- [`claudebase-server-foundation.md`](./claudebase-server-foundation.md)
  — the TG bridge runs INSIDE the authenticated claudebase server, on
  top of the HTTP/WSS + auth + service-install foundation. **Do not
  start any phase of this plan until Phases 1-2 of the foundation plan
  are landed.**
- [`agent-registry-multi-cli.md`](./agent-registry-multi-cli.md) Phases
  2-3 — registry + channel-bus + auto-registration. The TG router uses
  the registry to resolve `/switch`, `is_alive`, `first_alive_with_role`,
  and routes via the same channel bus. **Wait for those phases too
  before starting Phase 1 of this plan.**

**Related:**
- [`claudebase-project-dir.md`](./claudebase-project-dir.md) — provides per-project identity (`.claudebase/identity.local`) that this plan's routing relies on
- [`../../../../claude-code-sdlc/docs/plans/telegram-rust-port.md`](../../../../claude-code-sdlc/docs/plans/telegram-rust-port.md) — current self-contained `telegram-plugin-rs` that this plan refactors

## Goal

Move the Telegram poller from being **per-cli** (where each cli runs its own bot client and only one cli can serve a given token at a time due to Telegram's single-consumer `getUpdates` constraint) to being **server-centric** (claudebase server owns the bot connection; each cli is a thin client that subscribes to messages routed to it).

This enables:
- **One Telegram bot serving N cli instances simultaneously** (no more 409 Conflict when two cli's share a token)
- **User in TG having parallel conversations with different agents** via native TG `reply_to_message` threading
- **Server-side bot commands** (`/agents`, `/switch`, `/whoami`, `/here`) for operator-controlled context switching
- **Operator-friendly setup**: one BotFather token, one allowlist, all fleet behind one TG identity

## Motivation

### Current pain (before this plan)

1. **Single-poller constraint vs multi-cli reality.** Telegram allows **exactly one** `getUpdates` consumer per token. Today every cli running `telegram-plugin-rs` IS a consumer. When two cli's start with the same token → second one gets 409 Conflict forever, polling never works.

2. **Bot-per-agent doesn't scale.** Workaround "give each cli its own bot via `TELEGRAM_STATE_DIR=...`" works mechanically but means:
   - N tokens to manage in BotFather
   - User has N separate chats in TG (one per agent), has to remember who is who
   - N allowlists to keep in sync
   - Onboarding a new agent = "go to BotFather, create bot, configure allowlist, restart cli with env var"

3. **No way for user to talk to specific worker.** Current multi-cli plan's Model C (only orchestrator has TG, workers are internal) prevents the natural "ask the architect for project X directly" interaction.

### What this plan delivers

- **Single TG bot** serving the entire fleet. One token. One operator-controlled BotFather setup.
- **Server-side routing**: inbound TG message → server decides which cli is the target → forwards via existing channel-callback bus
- **Native TG threading**: user replies to a message in TG → server reads `reply_to_message.message_id` → looks up which cli sent that original → routes reply to same cli. No fake `@agent:` prefixes.
- **Switch commands in TG**: `/agents` lists alive cli's, `/switch X` changes the "active" cli for follow-up messages without quote-reply

## Architecture

```
                   ┌──────────────────────────────────────┐
                   │  Telegram                             │
                   │  bot @huevyidonbassbot                │
                   └────────────────┬─────────────────────┘
                                    │ getUpdates (long-poll)
                                    │ sendMessage (outbound)
                                    │ — single consumer per token
                                    ▼
            ┌───────────────────────────────────────────────┐
            │  claudebase server (--serve, with TG enabled) │
            │                                                │
            │  ┌──────────────┐  ┌──────────────────────┐   │
            │  │ TG poller    │  │ Message map          │   │
            │  │ (one per     │  │ tg_msg_id → cli_id   │   │
            │  │  token)      │  │ (so reply-quote      │   │
            │  └──────────────┘  │  routes back)        │   │
            │                    └──────────────────────┘   │
            │  ┌──────────────┐  ┌──────────────────────┐   │
            │  │ Per-user     │  │ Bot-command handler  │   │
            │  │ active_cli   │  │ /agents /switch X    │   │
            │  │ state        │  │ /whoami /here        │   │
            │  └──────────────┘  └──────────────────────┘   │
            │  ┌──────────────────────────────────────┐    │
            │  │ Routing decision tree (next section) │    │
            │  └──────────────────────────────────────┘    │
            │                                                │
            │  ── routes inbound to specific cli's via       │
            │     existing chat_subscribe / agent_message    │
            │     channel-bus from agent-registry plan       │
            └─────────────────┬───────────────┬─────────────┘
                              │               │
       ┌──────────────────────┴──────┐  ┌─────┴─────────────────┐
       │ Mira-orchestrator (cli A)   │  │ Mira-architect (cli B) │
       │  - "thin TG client" plugin  │  │  - "thin TG client"    │
       │  - subscribes to msgs for   │  │    plugin              │
       │    this cli                 │  │  - same                │
       └─────────────────────────────┘  └────────────────────────┘
```

### Key change vs today

| | Today | After this plan |
|---|---|---|
| Who owns TG bot connection | Per-cli `telegram-plugin-rs` MCP server | Single claudebase server (one connection per token) |
| What `telegram-plugin-rs` becomes | Full self-contained server + poller | **Thin client** — receives routed msgs from server, sends outbound via server, has NO direct TG connection |
| Maximum concurrent cli's sharing a token | 1 (others get 409) | Unlimited (server is the single consumer) |
| Where bot commands handled | Each cli has its own command handlers | Server handles ALL bot commands; cli's never see `/start` etc |
| Where outbound `reply` / `react` / `edit_message` execute | Cli's own TG client | Cli sends MCP tool call → server proxies to TG |
| Where inbox files saved | `~/.claude/channels/telegram/inbox/` (global) | Server saves to project-scoped `<project>/.claudebase/inbox/` per route (see claudebase-project-dir.md) |

## Routing decision tree (inbound TG → which cli)

When server receives a TG message via `getUpdates`, decide where to route in this order:

```
1. Is this a BOT COMMAND (/agents /switch /whoami /here /start /help /status)?
   → server handles locally, replies to TG, NOTHING goes to any cli

2. Is this a REPLY-QUOTE to a previous message? (msg.reply_to_message present)
   → lookup msg.reply_to_message.message_id in tg_msg_id_map table
   → if hit: route to the cli that originally sent that message
   → if miss (cli offline / message too old): fallback to step 4 with a note
     ("the agent who sent the message you replied to is offline, routing to {active_cli} instead")

3. Was the previous TG message FROM the user, addressing a specific cli? (per-user state has "last addressed cli")
   → not used in v1; revisit if step 4 proves insufficient

4. Free-text without reply quote: route to PER-USER ACTIVE CLI
   → state: active_cli_per_user[user_id] = "<cli_name>"
   → default if unset: "orchestrator" role (or "first alive cli" if no orchestrator)
   → user can change via /switch X

5. No alive cli at all → server replies in TG: "No cli's online. Spawn one with `claudebase run` somewhere."
```

### Routing edge cases

| Scenario | Behaviour |
|---|---|
| Target cli (per active_cli) is offline | Reply in TG: "agent X (your active cli) is offline. Switch with `/switch Y` or wait for restart." Don't route, don't queue. |
| User replies to a thread but cli has died since | Reply in TG: "the agent who sent that message (X) is offline. Routing to active cli (Y) instead." Then routes to active_cli with the prior message's content as context-quote. |
| User replies in a group chat | Group has its own active_cli (group_id → cli_name); fallback to "first alive cli in group's allow list" if unset. |
| User sends a command in group (`/agents`) | Server handles per-group (lists cli's that are configured to listen in that group, not all cli's). |
| Two users share the bot (multi-user allowlist) | Per-user state means user A's active_cli is independent from user B's. Each user's `/switch` only affects their own routing. |

## Bot commands API

All handled SERVER-SIDE, never reach any cli.

| Command | Args | Effect |
|---|---|---|
| `/agents` or `/online` | none | reply with bullet list: "• planner-projectX (developer, last seen 12s ago, cwd ~/proj/X)\n• architect-projectY ...". One line per alive cli. |
| `/switch <agent-name>` | name (required, must be alive) | set `active_cli_per_user[user_id] = name`; reply "switched to <agent-name>. Next free-text msg goes there." Validates target exists + is alive; refuses with helpful error otherwise. |
| `/whoami` | none | reply with active_cli for this user + last 3 messages exchanged with it ("Currently talking to planner-projectX. Last: ... Last: ... Last: ...") |
| `/here` | none | reply with `host:cwd` of active_cli ("active cli runs on `desktop-mira`, cwd=`/Users/aleksandra/projects/X`") |
| `/start` | none | (preserved from current plugin) onboarding text — bot intro + pairing instructions |
| `/help` | none | (preserved) usage summary including new `/agents`, `/switch`, etc |
| `/status` | none | (preserved) pairing status for this user + active_cli summary |

### Bot command UX guidance

- Reply within 1s for every command (server-local, no cli roundtrip).
- Bullet lists use Markdown for readability (TG supports it natively).
- All commands gated by existing access.json allowlist (same gate as inbound messages).
- Unknown commands (e.g. `/foo`) → silent drop, no reply (avoid spam from typos / bot scraping).

## Wire format changes

### Inbound to cli (what cli's TG plugin client sees)

Today (from `telegram-plugin-rs` direct):
```
<channel source="plugin:telegram:telegram" chat_id="..." message_id="..." user="..." user_id="..." ts="...">
hello mira
</channel>
```

After this plan (from server route):
```
<channel source="claudebase:telegram" chat_id="..." message_id="..." user="..." user_id="..." ts="..."
         tg_msg_id="<server's tracking id, opaque>"
         routed_by="server"
         route_reason="reply-quote|active-cli|...">
hello mira
</channel>
```

— `source=` changes from `plugin:telegram:telegram` to `claudebase:telegram` to signal the server-routed path. `tg_msg_id` is needed for outbound reply mapping. `route_reason` lets the cli (and Mira reading the input) understand WHY this message landed here vs another cli.

### Outbound from cli (reply / react / edit_message)

Today: cli calls `mcp__plugin_telegram_telegram__reply` → cli's TG plugin sends directly to TG.

After this plan: cli calls `mcp__claudebase__telegram_reply` (new tool path served by server-mediated MCP) → request goes to claudebase server → server sends to TG → server stores `(returned message_id, cli_id)` in `tg_msg_id_map` table (for future reply-quote lookups).

Same shape parameters (`chat_id`, `text`, `reply_to`, `files`, `format`), so cli code that calls these tools doesn't change semantically — only the tool name + transport.

## Server-side state additions

Three new tables / structures in claudebase server's DB:

### 1. `tg_message_map` (persistent, SQLite)

Tracks every outbound message the server sent to TG, so reply-quote routing can find the origin cli.

```sql
CREATE TABLE tg_message_map (
    tg_message_id INTEGER PRIMARY KEY,    -- Telegram's message_id
    chat_id       INTEGER NOT NULL,
    sender_cli_id TEXT    NOT NULL,       -- which cli's agent_id sent this
    sent_at       INTEGER NOT NULL,       -- unix ms
    content_hash  TEXT    NOT NULL,       -- sha256(content)[:16] for debug
    FOREIGN KEY (sender_cli_id) REFERENCES agent_registry(agent_id)
);
CREATE INDEX tg_message_map_sent_at ON tg_message_map(sent_at);
-- Periodic cleanup of rows older than TTL (default 30 days).
```

### 2. `active_cli_per_user` (in-memory + JSON persist)

```jsonc
// ~/.claudebase/server/state/active_cli.json
{
  "schema_version": 1,
  "users": {
    "434566766": {                       // tg user_id
      "active_cli_name": "planner-projectX",
      "set_at": "2026-05-24T11:23:45Z",
      "set_by": "user_command_/switch"   // or "default" or "auto-fallback"
    }
  },
  "groups": {
    "-1002962597876": {
      "active_cli_name": "architect-projectY",
      "set_at": "...",
      "set_by": "user_command_/switch"
    }
  }
}
```

Persisted to disk on every change (small file, no perf concern). Reloaded on server restart.

### 3. `tg_session_map` (in-memory, ephemeral)

Bot-command-related state per-conversation (e.g. "this user just ran /agents, next number-tap selects from that list"). Not yet load-bearing for v1; placeholder.

## Phases

### Phase 1 — Server-side TG poller (move polling from cli to server)

- New module `claudebase/src/daemon/telegram_bridge.rs`: implements `getUpdates` long-polling loop (using `frankenstein` or `teloxide` already in deps).
- Wires into server's main event loop; managed lifecycle (start/stop with server).
- Single token loaded from server config (NOT from `~/.claude/channels/telegram/.env` — that's per-cli legacy path; server has its own config slot).
- On inbound message: classify (command vs message), dispatch.
- On bot command: server-local handler (Phase 3).
- On non-command message: route to a cli (Phase 4).

**Done when:** `claudebase server start` with TG config → server polls TG; one cli registered with `claudebase`-server connection → user DM → server logs "received from <user_id>: <text>" but doesn't route yet.

### Phase 2 — Cli plugin client (thin)

Refactor `telegram-plugin-rs` to a `claudebase:telegram` plugin client:

- No direct TG connection.
- On `initialize`: subscribe to `claudebase:telegram` channel for this cli's name (via existing `chat_subscribe` infra in `daemon/chat.rs`).
- On receiving routed messages from server (via the channel bus): emit them as channel notifications to this cli's stdout, with `source="claudebase:telegram"` wire format.
- On `tools/call` for `reply` / `react` / `edit_message` / `download_attachment`: send the call via server-side MCP proxy → server sends to TG → server records mapping (Phase 5).
- Plugin no longer needs `~/.claude/channels/telegram/.env` (token lives in server config).

**Done when:** cli starts with new plugin client → registers as agent → server's TG poller receives a TG msg → server routes via channel bus → cli receives the channel callback in its input. Outbound `reply` from cli appears in TG.

### Phase 3 — Bot commands

Server handles `/agents` `/switch` `/whoami` `/here` plus preserved `/start` `/help` `/status`.

- `/agents`: query agent_registry list_alive, format as bullet list.
- `/switch X`: validate target alive, write `active_cli_per_user[user_id]=X`, persist, ack.
- `/whoami`: query agent_registry + recent tg_message_map rows for this user.
- `/here`: query agent_registry for cli's `host` + `cwd`.

**Done when:** all 4 new commands return correct data; existing 3 commands still work.

### Phase 4 — Routing decision tree

Implement the decision tree (5 steps above) in `telegram_bridge::route_inbound`:

```rust
fn route_inbound(msg: TgMessage) -> RoutingDecision {
    if msg.is_bot_command() { return RoutingDecision::HandleLocally(parse_command(msg)); }
    if let Some(reply_to) = msg.reply_to_message {
        if let Some(target) = lookup_tg_message_map(reply_to.message_id) {
            if agent_registry.is_alive(target.sender_cli_id) {
                return RoutingDecision::RouteToCli(target.sender_cli_id, RouteReason::ReplyQuote);
            }
            // else fallthrough to active_cli with a note
        }
    }
    if let Some(active) = active_cli_per_user.get(msg.from.user_id) {
        if agent_registry.is_alive(active) {
            return RoutingDecision::RouteToCli(active, RouteReason::ActiveCli);
        }
    }
    if let Some(default) = agent_registry.first_alive_with_role("orchestrator") {
        return RoutingDecision::RouteToCli(default, RouteReason::DefaultOrchestrator);
    }
    RoutingDecision::ReplyNoAgentsOnline
}
```

**Done when:** all routing paths exercised by unit test + by manual smoke tests: reply-quote routes to original sender, free-text routes to active_cli, /switch changes active_cli, fallback to orchestrator when active offline.

### Phase 5 — Outbound message tracking

Every outbound TG message from server-proxied `reply` / `edit_message` records `(tg_msg_id, chat_id, sender_cli_id, sent_at)` in `tg_message_map`. Background job purges rows older than TTL (default 30 days).

**Done when:** user gets msg from cli A; replies to it in TG; server reads `reply_to_message`, looks up tg_message_map → cli A → routes back to cli A.

### Phase 6 — Group chat support

- Per-group `active_cli` state (in same `active_cli.json`, just under `groups` key).
- `/switch` in group affects group's `active_cli` (per-group), not user's.
- Mention detection: if no active_cli set for group, only mentions trigger routing (mirrors current behavior for groups).
- Bot commands in groups gated by group's allow_from list.

**Done when:** bot in group → user mentions bot with `/switch architect-Y` → all subsequent group messages route to architect-Y until next `/switch`.

### Phase 7 — Cli lifecycle handling

- On `agent_unregister` (cli shuts down): if any `active_cli_per_user` row points to it, soft-clear to "needs-refresh"; on next message to that user, server informs "your active cli X is offline, routing to default" and routes to fallback.
- On `agent_register` (cli starts up): no auto-reattach to past users; user must `/switch` back. Avoids surprise routing to a fresh cli that thinks it's continuing a 3-day-old conversation.
- Periodic `last_seen_at` ping every 30s (existing agent_registry reap logic); reap unresponsive cli's.

**Done when:** cli A is `active_cli` for user → cli A killed → user sends msg → server replies "agent A offline, routing to fallback Y" + routes; later cli A respawns → user must `/switch A` to resume.

### Phase 8 — Migration + backward compat

- New server config flag `[telegram] enabled = true` activates this whole pipeline.
- When `enabled = false` (default for now during rollout), legacy `telegram-plugin-rs` per-cli path keeps working unchanged.
- Conflict gate: when `enabled = true`, server refuses to start if it can't acquire the token's `getUpdates` slot (i.e. a legacy per-cli plugin is still running). Operator must stop the legacy poller first. Clear error message.
- Documentation in `claudebase/docs/RELEASING.md` for the migration steps.

**Done when:** operator can flip `enabled = true`, server takes over polling, all cli's transparently see the new routed messages, legacy per-cli plugin gracefully stops (or warns + exits).

## Acceptance per phase (compact)

| # | Phase | Done when |
|---|---|---|
| 1 | Server TG poller | server polls TG, logs inbound msgs, no routing yet |
| 2 | Thin cli client | cli receives routed msg as `source="claudebase:telegram"` callback; outbound reply appears in TG |
| 3 | Bot commands | `/agents` `/switch` `/whoami` `/here` work; `/start` `/help` `/status` preserved |
| 4 | Routing tree | reply-quote / active-cli / default-orchestrator all route correctly |
| 5 | Outbound tracking | reply-quote round-trip works across server restarts |
| 6 | Group chats | per-group active_cli works; mentions still gate |
| 7 | Lifecycle | active_cli offline → fallback; respawn requires `/switch` reattach |
| 8 | Migration | config flag flips; conflict gate prevents dual-poller; legacy mode preserved |

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Server crash takes down ALL TG access for fleet | Watchdog auto-restart via launchd/systemd/Windows SCM (already wired by Phase 1 of agent-registry-multi-cli.md); fallback documented (operator can flip `enabled=false` and re-enable per-cli polling temporarily) |
| Reply-quote lookup miss (cli died, msg too old, table pruned) | Always have a sane fallback (active_cli or orchestrator); clear UX note in fallback reply explaining the routing |
| TTL purge of tg_message_map → user replies to old message → "agent unknown" | 30-day default TTL is generous; configurable; operator can extend. Edge case: warn user only, not error. |
| Two cli's registered with same name (collision) | agent_registry already validates `validate_agent_name`; on duplicate, second registration rejected; cli has to pick different name |
| Token-rotation while users have active conversations | New token activates new poller; old `tg_message_map` survives (message_ids stable across token rotation per Telegram); seamless |
| Refactor breaks current telegram-plugin-rs users | Migration gate (Phase 8) keeps legacy path until operator opts in; can revert by flipping `enabled=false` |
| Group chat `/switch X` affects all users in the group → confusion | Document clearly: in groups, switch is per-group not per-user; mirrors how Slack/Discord channels work |
| Bot commands collision with existing skills (`/telegram:configure`, `/telegram:access`) | Those are Claude Code SKILL invocations (`/skill:command`) — different namespace; no collision with bot commands (`/agents`, etc) which are TG-server-handled. Document the distinction. |

## Open questions (to settle before Phase 1 starts)

1. **TG client crate choice.** Server-side bot client — keep `frankenstein` (used by telegram-plugin-rs today) or switch to `teloxide` (already in claudebase Cargo.toml deps line 90). Leaning: `teloxide` — more first-class async support, better in long-running server process. But `frankenstein` works; switching is overhead. Defer to Phase 1 implementer.

2. **Active cli persistence format.** JSON file at `~/.claudebase/server/state/active_cli.json` or row in server's SQLite DB? JSON is simpler; SQLite is consistent with other server tables. Leaning JSON for v1 (small, simple); migrate to SQLite if it ever grows.

3. **Per-user vs per-(user,chat).** Right now active_cli is keyed by `user_id`. But what if user has bot in both their personal DM AND a group? Do they want different active_cli per location? Probably yes. Leaning: key by `(user_id, chat_id)` not just `user_id`.

4. **/switch validation strictness.** If user does `/switch architect-X` and architect-X has never existed → reject with "no such cli". If architect-X was alive but is now offline → reject with "cli offline, currently alive: …"? Or accept and auto-fallback when message comes? Leaning: reject strict (force user to pick alive one — clearer mental model).

5. **Server config for TG.** Where does the bot token live? Server-side `~/.claudebase/server/config.toml` (operator manages once at install) or env var `CLAUDEBASE_TG_TOKEN`? Leaning env var primary, config.toml fallback (matches D1 of agent-registry-multi-cli.md auth handling).

6. **Inbox file routing.** TG photo arrives → which cli's inbox? Today's plugin saves to global path. After Phase 2 the routed-to cli should get a project-scoped inbox path per `claudebase-project-dir.md`. But what if the cli isn't running in a `.claudebase/` project? Fall back to global path? Leaning yes — global path is failsafe.

7. **Mentions in DMs**. Today plugin's `gate()` has `isMentioned` for groups. In a DM, mentions don't apply. Should bot commands like `/switch` work in DMs (yes) and in groups (yes, with mention prefix `@botname /switch X`)? Leaning yes for both with mention-required in groups for `/switch` (otherwise it would affect everyone's routing).

8. **Concurrency on tg_message_map writes.** If two cli's both send replies at exact same time, server writes two rows. SQLite handles concurrent writes via WAL; should be fine. But test under load before assuming. Leaning: add stress test in Phase 5.

## Effort estimate

| Phase | Estimate |
|---|---|
| 1 — Server TG poller | 1-2 days |
| 2 — Thin cli client (refactor telegram-plugin-rs) | 3-4 days (this is the big refactor) |
| 3 — Bot commands | 1 day |
| 4 — Routing tree | 1 day |
| 5 — Outbound tracking | 0.5 day |
| 6 — Group chats | 1 day |
| 7 — Lifecycle handling | 1 day |
| 8 — Migration flag + docs | 0.5 day |

**Total: ~9-12 dev days.** Phase 2 is the biggest because telegram-plugin-rs is ~3 KLOC and needs careful refactoring (a lot of code becomes "talk to server" instead of "talk to TG").

## Phasing pause-points (operator-decision gates)

- **After Phase 2**: server-mediated TG works end-to-end for ONE cli. Could ship as "v1 — single cli with server-managed polling" and live with it before adding routing complexity.
- **After Phase 5**: full reply-quote round-trip works. **This is the smallest viable MVP** delivering the user-promised value (single bot, multiple cli's, native threading).
- **After Phase 7**: lifecycle edge cases handled. Production-ready.
- **Phase 8**: migration flag flip — happens whenever operator is ready to retire legacy per-cli polling for good.

## Files (planned changes)

```
claudebase/
├── src/
│   ├── daemon/
│   │   ├── telegram_bridge.rs        ← NEW — server-side TG poller (Phase 1)
│   │   ├── telegram_router.rs        ← NEW — routing decision tree (Phase 4)
│   │   ├── telegram_commands.rs      ← NEW — bot command handlers (Phase 3)
│   │   ├── telegram_state.rs         ← NEW — active_cli_per_user + tg_message_map (Phase 5)
│   │   └── server.rs                 ← wire bridge into server event loop
│   └── plugin/
│       └── (telegram-plugin-rs)       ← refactor: drop polling, drop ALL outbound bot calls;
│                                       become a thin claudebase-channel subscriber (Phase 2)
├── plugins/telegram-rs/                ← gut + thin (Phase 2 refactor); becomes "claudebase
│                                       channel client for TG-routed messages"
└── docs/
    ├── RELEASING.md                    ← migration steps (Phase 8)
    └── plans/
        └── telegram-multi-cli-orchestration.md  ← this file
```

## Open questions also affecting other plans

- The **same TG poller** that this plan introduces is the natural place to plug **other channels** (Discord, Slack, Matrix) — they'd be sibling modules to `telegram_bridge.rs`. Out of scope for v1 but architecture allows trivial extension.
- This plan supersedes the implicit Model C from `agent-registry-multi-cli.md` ("only orchestrator has TG"). Update that plan's Phase 3 to point at this one for the routing layer when implementing.

## Facts

### Verified facts

- Telegram Bot API allows exactly ONE `getUpdates` consumer per token; a second consumer gets HTTP 409 Conflict — verified by direct experience earlier this session when killing/respawning the per-cli plugin caused 409 retry loops in our Rust binary (`telegram-plugin-rs/src/telegram/bot.rs`). Salience: high (this constraint is the entire motivation for moving polling to server).
- `claudebase/src/daemon/chat.rs::ChatBus` already provides per-channel subscribe + broadcast — verified earlier this session during R10 implementation of `telegram-plugin-rs`. Salience: high (Phase 2 reuses this for cli-side message subscription).
- `claudebase/src/daemon/agent_registry.rs::list_alive`/`is_alive`/`validate_agent_name` already exist — verified by grep this session. Salience: high (Phase 4 routing reuses).
- `teloxide` v0.17 already in `claudebase/Cargo.toml` deps (line 90, with `ctrlc_handler` feature) — verified by grep this session. Salience: medium (Phase 1 client-choice question is informed).
- `tg_message_map` SQLite table is greenfield; no equivalent today. Schema simple enough to add without migration tooling. Salience: medium.

### External contracts

- Telegram Bot API — symbol: `getUpdates`, `sendMessage`, `editMessageText`, `setMessageReaction`, `Message.reply_to_message`, `Message.chat.type`, `User.id` — source: existing usage in `claudebase/plugins/telegram-rs/src/telegram/{bot,api}.rs` (verified working this session). Salience: high.
- SQLite WAL mode for concurrent writes — symbol: `PRAGMA journal_mode=WAL` — source: SQLite docs (rusqlite handles via standard pragma). Already used in claudebase's other tables. Salience: medium.
- `serde_json` for `active_cli.json` schema — symbol: `serde_json::{from_str, to_string_pretty}` — already in deps. Salience: low.

### Assumptions

- Operators using TG with multiple cli's strongly prefer one bot to N bots. Verbal confirmation from operator brief this session ("по одному боту на инстанс это не правильно и не масштабируемо"). Salience: high.
- Per-user active_cli routing is sufficient for v1; no per-(user,chat) split for groups. May need to revisit (see Open question 3). Salience: medium.
- Telegram's `Message.message_id` is stable across token rotation (the same message keeps the same id even if the bot's token changes). Believed-true based on Bot API docs but not verified this session. If wrong, tg_message_map would need re-keying on token rotation. Salience: low (token rotation is rare).
- The existing per-cli `telegram-plugin-rs` can be refactored to a thin client without breaking compatibility with skill commands (`/telegram:configure`, `/telegram:access`) — those skills live in `~/.claude/plugins/cache/.../skills/`, owned by the plugin manifest, NOT the server.ts logic. Refactor of server.ts/server-rs doesn't touch skills. Salience: medium.

### Open questions

(See `## Open questions` section above — 8 items deferred to Phase 1 kickoff.)

## Decisions

### Inbound validation

- Operator brief: "не правильно и не масштабируемо ... сделать какую то оркестрацию одного бота на множество инстансов за счет ... claudebase. возможно регистрировать cli + отслеживать реплаи и так понимать какому именно инстансу отвечает человек. если же человек хочет переключить диалог на другое окно через телеграм то сделать телеграм бот команду для вывода списка онлайн cli и переклчюения контекста." Coherent ask. Architectural consequence (TG poller moves from cli to server, telegram-plugin-rs becomes thin) is significant — flagged explicitly in `## Architecture` table. Operator-aware. Proceeding. Salience: high.

### Decisions made

- **Decision:** Server owns TG bot connection. ALL polling, ALL outbound, ALL bot commands centralised. Cli's are thin clients via existing chat_subscribe bus. Alternatives rejected: (a) bot-per-cli (doesn't scale — operator's reject); (b) keep per-cli polling + add coordinator (would still hit 409 conflict; band-aid). Q1-Q5: not a hack ✓ / proportionate (load-bearing refactor justified by the constraint) ✓ / alternatives evaluated ✓ / addresses root cause (TG's single-consumer-per-token constraint) ✓ / n/a. Salience: high.
- **Decision:** Native TG `reply_to_message` for thread routing. NO fake `@agent:` prefix syntax. Operator uses existing TG UX gesture. Alternative rejected: prefix-parsing (introduces parsing rules, confuses non-technical users, breaks if user types `@architect` in unrelated context). Salience: high.
- **Decision:** Per-user `active_cli` for free-text routing fallback. `/switch X` updates it. Default to `orchestrator` role (or first alive cli). Alternative rejected: broadcasting all messages to all cli's (would spam cli inputs + cost N× tokens). Salience: high.
- **Decision:** Bot commands handled SERVER-SIDE, never reach cli. Includes existing `/start /help /status` (preserved with server-side impl since server now owns the bot). Alternative rejected: routing commands through cli (complicates the trivial case + adds latency). Salience: medium.
- **Decision:** Migration flag (Phase 8) keeps legacy per-cli mode default-OFF for new model until operator opts in. Refuses to start with dual-poller if conflict detected. Salience: medium (operator-safety net).

### Hacks acknowledged

- v1 server-side `active_cli.json` is plain JSON file persistence, not a proper SQLite row. Removal path: if scale becomes an issue (>10k users), migrate to SQLite table with single-row upsert. Currently overkill for single-operator-multi-cli use case.
- v1 reply-quote lookup misses (deleted row, old message) fall back to active_cli with a polite note. Removal path: more sophisticated UX (e.g. inline keyboard "route to X / route to Y / cancel") if operator finds the auto-fallback confusing.

### Symptom-only patches

(none) — this plan addresses the root single-consumer-per-token constraint by moving polling to the right architectural layer, not by working around it.
