# Use Cases: Multi-Agent Telegram Routing on v0.6 Foundation

> Based on [PRD §18](../PRD.md#18-multi-agent-telegram-routing-on-v06-foundation) and [.claude/plan.md](../../.claude/plan.md).
>
> Feature slug: `multi-agent-telegram-on-v0.6`. Branch: `feat/multi-agent-on-v0.6` from tag `claudebase-v0.6.0`.
>
> **Scope frame:** one Telegram bot (`@X`), single `TELEGRAM_BOT_TOKEN`, three independent Claude Code CLI instances (A / B / C) launched from three different cwds (or `--agent-id`s). Routing key = `(chat_id: i64, message_thread_id: Option<i64>)`. KP1 = DM → CLI A; KP2 = group topic α → CLI B; KP3 = same group, topic β → CLI C.
>
> **Verification-class hint for downstream qa-planner:** all PRIMARY KP cases are `Mixed` (UI/UX + CLI + DB). ALT-FLOW bot commands are `Mixed` (UI/UX + DB). ERROR cases are `CLI` + `FS` (log inspection). EDGE cases are `Mixed` (DB + log).

---

## UC-MAT-1: Operator DM → Bot @X → routes to CLI A (= KP1, PRIMARY)

**Actor**: SDLC operator (Telegram client); bot `@X`; daemon (teloxide Dispatcher); CLI instance A (Claude Code session bound to DM).

**Preconditions**:
- Daemon is running (`claudebase daemon status` → `state: "running"`); UDS socket up.
- `TELEGRAM_BOT_TOKEN` env var set; teloxide Dispatcher connected (no HTTP 401 / 409 in startup log).
- CLI A has been registered against routing key `(dm_chat_id, None)` — i.e. `agent_registry` contains a row with `state='alive'`, `routing_chat_id = <dm_chat_id>`, `routing_thread_id = NULL`, `agent_name = "A"`.
- Operator is paired (per v0.6 §17 pairing model preserved by Slice 6); the operator's TG `user_id` is in the access gate's allowlist.
- The DM conversation between operator and bot `@X` is open and reachable (operator has previously `/start`-ed the bot at least once).

**Trigger**: Operator types `"hi"` in the DM with bot `@X` and hits send.

### Primary Flow (Happy Path)

1. Telegram delivers the `Update::Message` to the daemon's teloxide `Dispatcher` (the daemon is the sole `getUpdates` consumer per FR-MAT-3.3).
2. The Dispatcher handler tree first checks for bot commands (`/agents`, `/switch`, `/whoami`, `/here`). `"hi"` is not a command — proceed.
3. Routing-key extractor reads `Message.chat.id` → `<dm_chat_id>` (i64) and `Message.message_thread_id` → `None` (DM has no thread). Routing key = `(<dm_chat_id>, None)` per FR-MAT-1.2.
4. Daemon looks up `agent_registry` via the partial-unique index `agent_registry_routing_alive_uniq_idx` (FR-MAT-2.2): `SELECT cli_id, agent_name FROM agent_registry WHERE routing_chat_id = ? AND routing_thread_id IS NULL AND state = 'alive'`. Returns CLI A.
5. Daemon writes log line `routed (chat_id=<N>, thread_id=None) -> cli_id=A`.
6. Daemon emits a `notifications/claude/channel` event into the chat broadcast bus, scoped to CLI A's subscription. Meta JSON object includes the existing v0.6 fields (`source = "plugin:telegram:telegram"`, `chat_id`, `user`, `content`) AND the NEW OPTIONAL `thread_id: null` (per FR-MAT-7.1; null because this is a DM).
7. CLI A's Claude Code session renders the inbound as `<channel source="plugin:telegram:telegram" chat_id="..." thread_id="" user="..."> hi </channel>`.
8. Claude Code (inside CLI A) decides to reply; calls the plugin's CLI-facing `reply` MCP tool with params `{ chat_id: "<dm_chat_id>", text: "hello!" }` — the `message_thread_id` param is OMITTED (per FR-MAT-6.3, DM replies do not carry a thread id).
9. The plugin's `handle_reply` (in `plugins/telegram-rs/src/mcp/server.rs`) forwards the call to the daemon over the UDS via the daemon-internal envelope `internal.send_message` (FR-MAT-5.1).
10. Daemon's outbound pipeline parses the envelope; because no `message_thread_id` is present, it does NOT call `SendMessageSetters::message_thread_id` (FR-MAT-6.3). It invokes teloxide `Bot::send_message(<dm_chat_id>, "hello!")`.
11. Telegram delivers `"hello!"` to the operator's DM with bot `@X`.

**Postconditions**:
- The operator's DM thread with `@X` shows `[operator] hi` followed by `[bot @X] hello!`.
- `agent_registry` still contains one row for CLI A with `(routing_chat_id, routing_thread_id) = (<dm_chat_id>, NULL)`, `state='alive'`.
- The daemon's tail log (last ~50 lines) contains the literal substring `routed (chat_id=<dm_chat_id>, thread_id=None) -> cli_id=A`.
- The CLI A session transcript shows the `<channel ...>` event and the assistant turn that issued `reply`.

**Data Requirements**:
- Input: `Message { chat: Chat { id: <dm_chat_id> }, message_thread_id: None, text: Some("hi"), from: Some(User { id: <operator_user_id>, ... }) }`.
- Output: TG sendMessage to `<dm_chat_id>` with text `"hello!"`.
- Side Effects: no DB writes (the binding pre-existed); one log line; one MCP notification frame; one MCP tool-call response frame.

**FR Coverage**: FR-MAT-1.1, FR-MAT-1.2, FR-MAT-1.3, FR-MAT-2.2 (lookup uses the partial index), FR-MAT-3.1, FR-MAT-3.2, FR-MAT-5.1, FR-MAT-5.2, FR-MAT-6.3, FR-MAT-7.1. Acceptance: **AC-MAT-KP1**.

### Alternative Flows

- **UC-MAT-1-A: Operator sends a non-text message (sticker / photo / voice)** — the Dispatcher routing-key extraction still works (`Message.chat.id` and `Message.message_thread_id` exist on every `Message` variant); the `notifications/claude/channel` payload's content shape depends on §17's existing media-handling and is out of scope here. KP1 routing still resolves to CLI A.
- **UC-MAT-1-B: Operator sends `"hi"` BEFORE CLI A has registered** — UC-MAT-11 (orphan inbound).

### Error Flows

- **UC-MAT-1-E1**: CLI A's session is alive in `agent_registry` but the UDS broadcast subscription dropped (e.g. CLI A's `bridge.rs` reconnected but did not re-subscribe). Daemon emits the notification into the bus; nobody listens; daemon logs `delivery skipped: no live subscription for cli_id=A`. CLI A's v0.6 `notifications/tools/list_changed` reconnect mechanism (per FR-MAT-10.3) eventually restores the subscription. The original `"hi"` is NOT re-delivered (no replay queue per the explicit no-session-cache constraint).

### Edge Cases

- **UC-MAT-1-EC1**: Two operators with different `user_id`s both message `@X` in the SAME `<dm_chat_id>` — impossible by Telegram design (a DM is unique to one `user_id` ↔ bot pair). Not a real case.

---

## UC-MAT-2: Operator → Bot @X in group topic α → routes to CLI B (= KP2, PRIMARY)

**Actor**: SDLC operator (Telegram client); bot `@X` (added to a group `G` as a member with Forum Topics permission); daemon; CLI instance B (≠ A).

**Preconditions**:
- All preconditions of UC-MAT-1 except the binding row: CLI B is registered against `(group_chat_id, Some(thread_id_α))`. SQL: `INSERT INTO agent_registry (cli_id, agent_name, routing_chat_id, routing_thread_id, state, ...) VALUES ('B', 'B', <group_chat_id>, <α_id>, 'alive', ...)`.
- Group `G` has Forum Topics enabled (chat-admin setting).
- Bot `@X` is a member of group `G` with permission to read and send messages in forum topics.

**Trigger**: Operator opens group `G`, opens topic α, and types `"hi B"` and sends.

### Primary Flow (Happy Path)

1. Telegram delivers `Update::Message` to the daemon's Dispatcher. The `Message.chat.id = <group_chat_id>` and crucially `Message.message_thread_id = Some(<α_id>)` because the message was posted in a forum topic.
2. Routing-key extractor: `(<group_chat_id>, Some(<α_id>))`.
3. Bot-command check passes (`"hi B"` is not a command).
4. Daemon looks up `agent_registry` with `routing_chat_id = <group_chat_id> AND routing_thread_id = <α_id> AND state = 'alive'`. Returns CLI B.
5. Daemon log: `routed (chat_id=<G>, thread_id=Some(α_id)) -> cli_id=B`.
6. Daemon emits `notifications/claude/channel` to CLI B's subscription. Meta `thread_id = "<α_id>"` (string per the v0.6 ID-as-string discipline per FR-MAT-7.1).
7. CLI B's session renders `<channel source="plugin:telegram:telegram" chat_id="<G>" thread_id="<α_id>" user="..."> hi B </channel>`.
8. Claude Code in CLI B calls `reply` with `{ chat_id: "<G>", text: "hi from B in α!", message_thread_id: "<α_id>" }` (the optional `message_thread_id` is now present per FR-MAT-6.1 — CLI B echoes it back from the inbound meta).
9. Plugin forwards `internal.send_message` envelope to daemon, propagating `message_thread_id` unchanged (FR-MAT-6.2).
10. Daemon parses, translates `message_thread_id: "<α_id>"` → `i64`, calls `Bot::send_message(<G>, "hi from B in α!").message_thread_id(<α_id>).send()` (the teloxide `SendMessageSetters::message_thread_id` setter per FR-MAT-1.3 + FR-MAT-6.2).
11. Telegram posts the reply INTO topic α of group `G`.

**Postconditions**:
- Topic α of group `G` shows `[operator] hi B` followed by `[bot @X] hi from B in α!`. The reply does NOT leak into topic β or into the main group thread.
- `agent_registry` row for CLI B unchanged.
- Daemon tail log contains `routed (chat_id=<G>, thread_id=Some(α_id)) -> cli_id=B`.

**Data Requirements**:
- Input: `Message { chat: Chat { id: <group_chat_id> }, message_thread_id: Some(<α_id> as i32 / i64 — assumed: tested at Slice 1 architect call), text: Some("hi B"), from: Some(User { id: <operator_user_id>, ... }) }`.
- Output: TG sendMessage targeting `(<group_chat_id>, message_thread_id = <α_id>)`.

**FR Coverage**: FR-MAT-1.1, FR-MAT-1.2, FR-MAT-1.3, FR-MAT-2.2, FR-MAT-3.1, FR-MAT-3.2, FR-MAT-5.1, FR-MAT-5.2, FR-MAT-6.1, FR-MAT-6.2, FR-MAT-7.1. Acceptance: **AC-MAT-KP2**.

### Alternative Flows

- **UC-MAT-2-A: Group `G` has Forum Topics DISABLED at operator's chat-admin level** — then no `message_thread_id` is set on inbound messages. Routing key collapses to `(<group_chat_id>, None)`. If no CLI is bound to `(<group_chat_id>, None)`, UC-MAT-11 fires.

### Error Flows

- **UC-MAT-2-E1**: Bot `@X` lacks Forum Topics permission. Telegram silently strips `message_thread_id` from outbound `sendMessage` (or rejects with `Bad Request: message can't be replied to`). Daemon log records the teloxide error; the reply does not land. The operator sees no reply in topic α. **QA implication:** smoke runbook MUST verify Forum Topics permission as a precondition before declaring KP2 failed.

### Edge Cases

- **UC-MAT-2-EC1**: `message_thread_id` on the inbound `Message` could plausibly be `Option<i32>` rather than `Option<i64>` in teloxide 0.17's strict type. **The exact type is DEFERRED to Slice 1 architect verification (OQ-MAT-2 / R-MAT-4).** If teloxide types it as `i32` but `agent_registry.routing_thread_id` is `INTEGER` (which SQLite treats as i64), the daemon's i32→i64 widening cast is lossless and correct.

---

## UC-MAT-3: Operator → Bot @X in same group, topic β → routes to CLI C (= KP3, PRIMARY)

**Actor**: SDLC operator; bot `@X` in group `G`; daemon; CLI instance C (≠ A, ≠ B).

**Preconditions**:
- Same as UC-MAT-2 with CLI C bound to `(<group_chat_id>, Some(thread_id_β))` where `β_id ≠ α_id` and topic β is a distinct topic in the SAME group `G`.
- CLI A and CLI B are both still alive in `agent_registry`. All three rows `(A, dm, NULL)`, `(B, G, α)`, `(C, G, β)` coexist with `state='alive'`.

**Trigger**: Operator opens group `G`, opens topic β, types `"hi C"`, sends.

### Primary Flow (Happy Path)

Same structure as UC-MAT-2 with `α_id` replaced by `β_id` throughout.

1–11. As UC-MAT-2 with `(<group_chat_id>, Some(<β_id>))` resolving to CLI C, daemon log line `routed (chat_id=<G>, thread_id=Some(β_id)) -> cli_id=C`, and the reply landing in topic β.

**Postconditions**:
- Topic β of group `G` shows `[operator] hi C` then `[bot @X] hi from C in β!`. Topics α and the DM are unaffected by this exchange.
- `agent_registry` contains all three rows side-by-side: `(A, <dm_chat_id>, NULL, 'alive')`, `(B, <G>, <α_id>, 'alive')`, `(C, <G>, <β_id>, 'alive')`.
- Daemon log contains the new `routed ... -> cli_id=C` line.

**FR Coverage**: same as UC-MAT-2. Demonstrates NFR-MAT-5 (single bot serves all three routing keys). Acceptance: **AC-MAT-KP3**.

### Alternative Flows

- **UC-MAT-3-A: Operator messages topic α and topic β in rapid succession** — the Dispatcher routes them independently and atomically (one task per `Update`). CLI B and CLI C both receive their respective notifications. No cross-bleed.

### Error Flows

- (none beyond those in UC-MAT-2 — symmetric to that scenario.)

### Edge Cases

- **UC-MAT-3-EC1**: Operator deletes topic β at the Telegram client level mid-conversation. The next inbound `Message.message_thread_id` for `β_id` becomes invalid (Telegram returns 400 on outbound `sendMessage` with the stale thread id). Daemon logs the teloxide error; the binding row stays in `agent_registry`. Recovery requires operator to `/switch <new-cli>` in a different topic OR delete the stale row manually. **Tracked as OQ-MAT-2 follow-up; not load-bearing for KP3 sign-off.**

---

## UC-MAT-4: `/agents` command in topic α — daemon lists CLIs bound to `(G, α)` (ALT FLOW)

**Actor**: SDLC operator (in group `G`, topic α); daemon's Dispatcher command handler.

**Preconditions**:
- Daemon is running, Dispatcher connected.
- `agent_registry` contains at least one `state='alive'` row whose `(routing_chat_id, routing_thread_id) = (<G>, <α_id>)`. (Per FR-MAT-2.2 there is at most one; this case assumes exactly one — CLI B.)

**Trigger**: Operator types `/agents` in topic α of group `G`.

### Primary Flow (Happy Path)

1. Dispatcher receives `Update::Message` with `text = "/agents"`, `chat.id = <G>`, `message_thread_id = Some(<α_id>)`.
2. The bot-command handler runs BEFORE the routing-key lookup (per FR-MAT-8.1).
3. `/agents` handler reads the COMMAND message's own `(chat_id, message_thread_id)` per FR-MAT-8.2 → `(<G>, Some(<α_id>))`.
4. Handler queries `agent_registry`: `SELECT agent_name, cli_id, last_pinged_at, cwd FROM agent_registry WHERE routing_chat_id = <G> AND routing_thread_id = <α_id> AND state = 'alive' ORDER BY agent_name`.
5. Handler formats results into a TG-Markdown bullet list:
   ```
   Agents bound to this topic:
   • B (cli_id=…, last seen 2026-06-02T14:23:11Z, cwd=/path/to/CLI-B)
   ```
6. Daemon calls `Bot::send_message(<G>, <formatted_text>).message_thread_id(<α_id>).send()` so the reply lands IN topic α.

**Postconditions**:
- Topic α shows operator's `/agents` and the bot's reply listing CLI B.
- No mutation in `agent_registry`.

**Data Requirements**:
- Input: command `Message`.
- Output: TG reply IN the originating topic.

**FR Coverage**: FR-MAT-8.1, FR-MAT-8.2, FR-MAT-7.1 (reply lands in topic). Indirectly verifies FR-MAT-2.1 schema columns are queryable.

### Alternative Flows

- **UC-MAT-4-A: `/agents` in main group thread (no topic)** — `message_thread_id` is absent → routing key for the LIST query is `(<G>, NULL)` per FR-MAT-8.2 + R-MAT-8: lists CLIs bound to the main thread of group `G`. If none, the reply is `No agents bound to this thread.`.
- **UC-MAT-4-B: `/agents` in DM** — lists CLIs bound to `(<dm_chat_id>, NULL)` (i.e. CLI A in the KP1 scenario).
- **UC-MAT-4-C: Multiple CLIs bound to the same routing key (shouldn't happen given the partial-unique index)** — the partial-unique index from FR-MAT-2.2 prevents this. If somehow it occurred (manual DB tamper), `/agents` lists both for diagnostic transparency.

### Error Flows

- **UC-MAT-4-E1**: Empty result set — reply `No agents bound to this topic.` (no `/switch` hint to avoid noisy guidance).

### Edge Cases

- **UC-MAT-4-EC1**: 30+ CLIs bound to the same routing key (impossible with the unique index but defensive): handler truncates at 10 entries with `… and N more`.

---

## UC-MAT-5: `/switch <cli-name>` in topic α — daemon rebinds routing key (ALT FLOW)

**Actor**: SDLC operator (in topic α of group `G`); daemon.

**Preconditions**:
- `agent_registry` has CLI B bound to `(<G>, <α_id>)` with `state='alive'`.
- Another CLI named `D` exists in `agent_registry` with `state='alive'` but is currently UNBOUND (i.e. its `routing_chat_id` and `routing_thread_id` are NULL) OR it is bound to a different routing key the operator wants to move from.
- The operator passes the security check of FR-MAT-8.6: their `user_id` matches CLI B's binding's `last_user_id` OR they are a chat admin of group `G`. (UC-MAT-15 covers the denial case.)

**Trigger**: Operator types `/switch D` in topic α.

### Primary Flow (Happy Path)

1. Dispatcher receives `/switch D` with `(chat.id=<G>, message_thread_id=Some(<α_id>))`.
2. Command handler runs BEFORE routing lookup (FR-MAT-8.1).
3. Security check (FR-MAT-8.6): operator's `user_id` is verified against CLI B's binding's `last_user_id` (stored in `agent_registry` — assumption: schema carries `last_user_id` per OQ3 default policy; if not, fall back to chat-admin check via `Bot::get_chat_administrators(<G>)`).
4. Handler opens a SQLite write transaction (FR-MAT-8.3): `BEGIN IMMEDIATE; UPDATE agent_registry SET routing_chat_id = NULL, routing_thread_id = NULL WHERE cli_id = 'B'; UPDATE agent_registry SET routing_chat_id = <G>, routing_thread_id = <α_id>, last_user_id = <operator_user_id> WHERE agent_name = 'D' AND state = 'alive'; COMMIT;`.
5. SQLite's `UNIQUE INDEX ... WHERE state='alive'` (FR-MAT-2.2) accepts the new binding because B's row was unbound first in the same transaction.
6. Handler sends TG reply IN topic α: `/switch ok: topic α is now bound to D (was B). Use /whoami to confirm.`.

**Postconditions**:
- `agent_registry` row for `D` carries `(<G>, <α_id>)`. Row for `B` carries `(NULL, NULL)`.
- Subsequent inbound messages to `(<G>, <α_id>)` route to CLI D (verified by UC-MAT-2 with B replaced by D).

**Data Requirements**: input = `/switch D` command, output = TG reply + 2-row UPDATE.

**FR Coverage**: FR-MAT-8.1, FR-MAT-8.3, FR-MAT-8.6, FR-MAT-2.2 (partial-unique constraint is the load-bearing mechanism).

### Alternative Flows

- **UC-MAT-5-A: `/switch <cli>` where `<cli>` does NOT exist in `agent_registry`** — handler replies `/switch failed: no CLI named D registered. Use /agents to list bound or /claudebase run --agent-id D from a cwd to register.`.
- **UC-MAT-5-B: `/switch <cli>` to the CURRENTLY BOUND CLI** (idempotent rebind) — handler detects no-op, replies `/switch: D is already bound to this topic.`.
- **UC-MAT-5-C: `/switch <cli>` when the target CLI is already bound to a DIFFERENT routing key** — handler unbinds the target from its prior key in the same transaction, then binds it to the new one. Replies `/switch ok: D moved from <prior key> to topic α.`.

### Error Flows

- **UC-MAT-5-E1: Unauthorized operator (UC-MAT-15)** — see UC-MAT-15.
- **UC-MAT-5-E2: SQLite write transaction conflict** (extremely unlikely given serialization, but a `BUSY` retry path exists). Handler retries up to 3 times with 50 ms backoff, then replies `/switch failed: DB busy, try again.`.

### Edge Cases

- **UC-MAT-5-EC1: Concurrent `/switch` taps** — covered by UC-MAT-14.

---

## UC-MAT-6: `/whoami` in DM — daemon returns current binding (ALT FLOW)

**Actor**: SDLC operator (in DM with bot `@X`); daemon.

**Preconditions**:
- Daemon running, Dispatcher connected.
- CLI A is bound to `(<dm_chat_id>, NULL)` per UC-MAT-1.

**Trigger**: Operator types `/whoami` in DM.

### Primary Flow (Happy Path)

1. Dispatcher receives `/whoami` with `(chat.id=<dm_chat_id>, message_thread_id=None)`.
2. Command handler reads the originating routing key from the COMMAND message itself.
3. Queries `agent_registry`: `SELECT agent_name, cli_id, host, cwd FROM agent_registry WHERE routing_chat_id = <dm_chat_id> AND routing_thread_id IS NULL AND state = 'alive'`.
4. Replies (in DM): `Bound to: A (cli_id=…, host=…, cwd=…)`.

**Postconditions**: no DB mutation; one TG reply in DM.

**FR Coverage**: FR-MAT-8.1, FR-MAT-8.4.

### Alternative Flows

- **UC-MAT-6-A: `/whoami` in topic α** — returns CLI B per the analogous lookup. Reply lands in topic α (FR-MAT-7.1).
- **UC-MAT-6-B: `/whoami` where no binding exists** — reply `No agent bound to this conversation. Use /agents to list candidates.`.

### Error Flows

- (none specific.)

### Edge Cases

- (none.)

---

## UC-MAT-7: `/here` in topic β — daemon returns host:cwd of bound CLI (ALT FLOW)

**Actor**: SDLC operator (in topic β of group `G`); daemon.

**Preconditions**:
- CLI C is bound to `(<G>, <β_id>)` per UC-MAT-3.

**Trigger**: Operator types `/here` in topic β.

### Primary Flow (Happy Path)

1. Dispatcher receives `/here` with `(chat.id=<G>, message_thread_id=Some(<β_id>))`.
2. Command handler reads originating routing key.
3. Queries `agent_registry` and joins / reads the v0.6 `host` / `cwd` / `pid` columns (existing per §17.7 — the new feature does NOT add these columns; they pre-existed).
4. Replies (in topic β): `Agent C running on <host>:<cwd> (pid <N>, last seen <timestamp>)`.

**Postconditions**: no DB mutation; one TG reply in topic β.

**FR Coverage**: FR-MAT-8.1, FR-MAT-8.5. **Assumption:** the v0.6 `agent_registry` columns `host`, `cwd`, `pid` exist OR are derivable from `connection_id` per §17.7. **Verification path:** Slice 5 implementer confirms; if columns are missing, qa-planner adds a needs-implementation flag.

### Alternative Flows

- **UC-MAT-7-A: `/here` in DM** — analogous; returns CLI A's host/cwd.
- **UC-MAT-7-B: `/here` where the CLI is `state='orphaned'`** — reply `Agent C marked orphaned (last seen <timestamp>); the process may be dead. Use /switch to rebind.`.

### Error Flows

- (none specific.)

### Edge Cases

- (none.)

---

## UC-MAT-8: Daemon startup with no `TELEGRAM_BOT_TOKEN` env — TG disabled, daemon stays up (ERROR)

**Actor**: SDLC operator (CLI launcher); daemon process.

**Preconditions**:
- `TELEGRAM_BOT_TOKEN` env var is UNSET (e.g. operator forgot to source `~/.config/claudebase/secrets.toml` OR the token is intentionally absent because the operator wants chat-only with no TG).
- Daemon binary is present; `chat.db` is migratable.

**Trigger**: Operator runs `claudebase daemon serve --foreground` (or the service-manager-driven start).

### Primary Flow (Happy Path — degraded)

1. Daemon starts, runs the `chat.db` migration (Slice 2 idempotent migration completes).
2. Daemon UDS listener comes up.
3. Telegram subsystem boot probes `std::env::var("TELEGRAM_BOT_TOKEN")` → `Err(NotPresent)`.
4. Telegram subsystem logs WARN: `TELEGRAM_BOT_TOKEN unset; Telegram support disabled — daemon will serve chat / agent_registry / plugin bridge but not Telegram.`
5. Telegram Dispatcher is NOT spawned. teloxide is NOT initialized. No `getUpdates` is issued.
6. Daemon process stays alive; `claudebase daemon status` returns `state: "running"` with a sub-status `tg_bot_state: "not-configured"`.

**Postconditions**:
- Daemon is up; UDS serves non-TG MCP tools.
- No teloxide-related log lines beyond the WARN at startup.
- CLI A / B / C may still register with `agent_registry` but no routing actually fires (no inbound messages can arrive without TG).

**FR Coverage**: FR-MAT-9.1.

### Alternative Flows

- **UC-MAT-8-A: Operator later sets `TELEGRAM_BOT_TOKEN` and restarts** — Telegram subsystem comes up normally. (No hot-reload; restart is required. **Tracked as out-of-scope per the plan's lack of hot-reload mention.**)

### Error Flows

- (this UC IS the error flow.)

### Edge Cases

- (none.)

---

## UC-MAT-9: Invalid bot token → teloxide HTTP 401 → Dispatcher terminates, daemon stays up (ERROR)

**Actor**: Daemon process; teloxide library.

**Preconditions**:
- `TELEGRAM_BOT_TOKEN` is set to a syntactically-valid but Telegram-rejected value (revoked / typo / wrong bot).
- Daemon is starting.

**Trigger**: Daemon Telegram subsystem spawns the teloxide Dispatcher.

### Primary Flow (Happy Path — graceful failure)

1. Dispatcher calls `getMe` (teloxide's standard sanity probe) on startup.
2. Telegram returns HTTP 401 `{"ok":false,"error_code":401,"description":"Unauthorized"}`.
3. teloxide surfaces the error to the Dispatcher's error handler.
4. Daemon logs ERROR: `Telegram dispatcher: 401 Unauthorized — bot token is invalid or revoked. Check ~/.config/claudebase/secrets.toml. Telegram support disabled for this daemon session.`.
5. Dispatcher task is gracefully aborted (FR-MAT-9.2).
6. Rest of the daemon (chat, agent_registry, plugin bridge UDS) stays alive.

**Postconditions**:
- Daemon process is up; `daemon status` shows `tg_bot_state: "error: 401"` (or equivalent).
- No teloxide retries (the token is invalid; backoff would be wasteful — different from 409).

**FR Coverage**: FR-MAT-9.2.

### Alternative Flows

- **UC-MAT-9-A: 401 occurs mid-stream** (token revoked after daemon was running) — same handler: log ERROR, terminate Dispatcher gracefully, daemon stays alive. Operator must rotate the token and restart.

### Error Flows

- (this UC IS the error flow.)

### Edge Cases

- (none.)

---

## UC-MAT-10: Another consumer holds the bot token → teloxide HTTP 409 → backoff + operator hint (ERROR)

**Actor**: Daemon process; teloxide; an unidentified second `getUpdates` consumer (stale v0.6 plugin direct-poll process OR a second daemon OR an unrelated bot client).

**Preconditions**:
- `TELEGRAM_BOT_TOKEN` is valid.
- Another process is currently long-polling `getUpdates` with the same token.

**Trigger**: Daemon's Dispatcher attempts to start its long-poll loop.

### Primary Flow (Graceful retry)

1. teloxide `getUpdates` returns HTTP 409 `{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}`.
2. Daemon logs WARN with operator-facing instruction: `Telegram dispatcher: 409 Conflict — another process is polling this bot. Kill any other claudebase daemon / plugin-direct-poll process holding TELEGRAM_BOT_TOKEN; the daemon is the sole owner in this build. Backing off and retrying in <N>s.`
3. Daemon applies exponential backoff (e.g. 2s, 4s, 8s, capped at 60s).
4. When the other consumer goes away, teloxide successfully takes over and Dispatcher begins serving normally.

**Postconditions**:
- Daemon process stays alive throughout backoff.
- Daemon log contains the WARN + the instruction substring `kill any other claudebase daemon`.
- After successful takeover, normal routing resumes (UC-MAT-1 / 2 / 3 work).

**FR Coverage**: FR-MAT-9.3, R-MAT-6 (resolves the v0.6 latent dual-poller vulnerability).

### Alternative Flows

- **UC-MAT-10-A: 409 persists indefinitely** (operator never kills the other consumer) — backoff caps at 60s; daemon keeps logging the WARN periodically. Daemon is otherwise functional for non-TG subsystems.

### Error Flows

- (this UC IS the error flow.)

### Edge Cases

- **UC-MAT-10-EC1: The other consumer is the SAME daemon's plugin running with `cfg(feature = "legacy-direct-poll")` enabled** (the deliberate escape-hatch hack acknowledged in FR-MAT-4.1). The 409 storm fires until the operator disables the cfg flag.

---

## UC-MAT-11: Orphan inbound — message arrives at `(chat_id, thread_id)` with no CLI bound (EDGE)

**Actor**: SDLC operator (Telegram client); daemon.

**Preconditions**:
- Daemon running, Dispatcher connected.
- `agent_registry` has NO `state='alive'` row matching the inbound routing key.

**Trigger**: Operator sends `"anyone home?"` in topic γ (a topic with no bound CLI).

### Primary Flow (Default fallback — log + drop, optional TG hint)

1. Dispatcher extracts routing key `(<G>, <γ_id>)`.
2. Lookup returns 0 rows.
3. Daemon logs INFO: `orphan inbound: (chat_id=<G>, thread_id=Some(γ_id)) — no CLI bound. Dropping.` per FR-MAT-9.5.
4. **Architect-deferred sub-decision (OQ-MAT-5):** the default fallback ALSO replies in TG with `No CLI bound to this conversation. Use 'claudebase run' from a working directory to spawn one.`. The exact UX text and whether the TG reply is unconditional or gated by some "first-time per routing key" flag is refined at bootstrap by the architect.

**Postconditions**:
- Daemon log carries the orphan entry.
- No DB mutation (no auto-binding).
- Operator may or may not see a TG hint reply (depends on the architect's refinement of OQ-MAT-5).

**FR Coverage**: FR-MAT-9.5. **Open question:** OQ-MAT-5 (orphan-fallback UX exact shape).

### Alternative Flows

- **UC-MAT-11-A: Orphan inbound in DM with bot @X** — operator messages bot in DM, no CLI bound to `(<dm_chat_id>, NULL)`. Same fallback. The TG hint is especially friendly in DM since the operator started the conversation directly.

### Error Flows

- **UC-MAT-11-E1: Bot lacks send permission in the conversation** (e.g. bot was removed from the group) — the optional TG hint reply itself fails with 403. Daemon logs the secondary error; the orphan inbound is still treated as dropped per FR-MAT-9.5.

### Edge Cases

- (none beyond E1.)

---

## UC-MAT-12: CLI process dies mid-conversation → registry row marked `orphaned` → next inbound triggers UC-MAT-11 (EDGE)

**Actor**: CLI instance (process dies); daemon; SDLC operator (sends a message after the death).

**Preconditions**:
- CLI B has been operating normally per UC-MAT-2 with binding `(B, <G>, <α_id>, 'alive')`.
- The CLI B process dies abruptly (OS kill, crash, machine reboot, network partition).

**Trigger Sequence**:

1. The daemon's UDS connection to CLI B drops (TCP FIN / EOF / heartbeat timeout per v0.6 §17 connection management).
2. Daemon's connection-watcher (existing v0.6 component) detects the drop and UPDATE-s the row to `state='orphaned'`: `UPDATE agent_registry SET state='orphaned', last_disconnected_at = NOW WHERE cli_id='B'`.
3. The PARTIAL-UNIQUE index `agent_registry_routing_alive_uniq_idx` no longer covers this row (because the index is `WHERE state='alive'` per FR-MAT-2.2). The routing key `(<G>, <α_id>)` is now effectively unbound from the alive set.
4. Operator sends `"still there?"` in topic α (FR-MAT-2.1 lookup returns 0 alive rows).
5. UC-MAT-11 fires: orphan-inbound fallback (log + drop + optional TG hint).

**Postconditions**:
- `agent_registry` row for B has `state='orphaned'`.
- A subsequent `/switch <other-cli>` from the operator can re-bind topic α to a different CLI per UC-MAT-5.

**FR Coverage**: FR-MAT-9.5 (orphan inbound), FR-MAT-2.2 (partial-unique index gates routing on `state='alive'`). **Assumes** the v0.6 connection-drop → `state='orphaned'` flow exists in §17's `agent_registry` lifecycle (per the existing `agent_reap` MCP tool and the `state` enum `('alive','orphaned','dead')` from the §17.7 schema). **Verification path:** Slice 5/Slice 7 implementer confirms the v0.6 lifecycle still fires on UDS drop.

### Alternative Flows

- **UC-MAT-12-A: CLI B reconnects (process didn't actually die, just had a transient network blip)** — v0.6's `bridge.rs` reconnect mechanism (FR-MAT-10.3) re-registers CLI B; daemon transitions the row back to `state='alive'`. Routing resumes.

### Error Flows

- (none specific.)

### Edge Cases

- **UC-MAT-12-EC1: Two CLIs both registering as `B` after a death** (e.g. operator manually launches a replacement) — the partial-unique index on `(agent_name)` per §17 v6 `agent_registry_thread_name_alive_idx` (preserved per PRD §18.7) plus the routing-key index together resolve the conflict. Exact behavior depends on §17's existing collision-resolution which is preserved as-is.

---

## UC-MAT-13: Daemon restart with 3 alive bindings → all routing-key bindings restore from SQLite (EDGE)

**Actor**: SDLC operator (restarts the daemon); daemon process; SQLite `chat.db`.

**Preconditions**:
- Per UC-MAT-1/2/3: `agent_registry` contains three `state='alive'` rows: `(A, <dm_chat_id>, NULL)`, `(B, <G>, <α_id>)`, `(C, <G>, <β_id>)`.
- All three CLI processes are still alive (their PIDs are live; their UDS connections to the daemon will time out during the restart but they will reconnect via v0.6's `bridge.rs` mechanism per FR-MAT-10.3).

**Trigger**: Operator runs `claudebase daemon restart` (OR the underlying service manager fires a restart).

### Primary Flow (Happy Path)

1. Daemon process receives SIGTERM, gracefully closes UDS connections, shuts down Dispatcher.
2. CLI A / B / C all lose their UDS connection — their `bridge.rs` enters reconnect loop (v0.6 behavior preserved).
3. Daemon process restarts.
4. Migrations run (idempotent — Slice 2 / FR-MAT-2.4).
5. teloxide Dispatcher comes up; `getUpdates` resumes.
6. The three `state='alive'` rows in `agent_registry` survived the restart (SQLite persistence per FR-MAT-10.1).
7. CLI A / B / C reconnect to the new daemon UDS via `bridge.rs`; the daemon recognises each CLI by its `connection_id` / `agent_id` and the `agent_registry` row transitions back to `state='alive'` (per FR-MAT-10.2 + the v0.6 `agent_register` re-association flow).
8. Operator sends `"after restart"` in DM → routes to CLI A (KP1 still works). Same for KP2 and KP3.

**Postconditions**:
- `agent_registry` rows unchanged (3 alive bindings).
- KP1 / KP2 / KP3 all pass without operator re-pairing.
- Daemon log contains `routed (...) -> cli_id=A`, `-> cli_id=B`, `-> cli_id=C` after the operator-sent post-restart messages.

**FR Coverage**: FR-MAT-10.1, FR-MAT-10.2, FR-MAT-10.3.

### Alternative Flows

- **UC-MAT-13-A: CLI A started BEFORE daemon came up** (i.e. order of start was CLI → daemon, not daemon → CLI) — v0.6 fallback: CLI A's `bridge.rs` polls `claudebase_daemon_status: { status: "down" }`; once daemon starts, `notifications/tools/list_changed` fires and CLI A reconnects. FR-MAT-10.3 mandates this behavior is unchanged from v0.6.

### Error Flows

- **UC-MAT-13-E1: Migration fails on restart** (e.g. disk full) — daemon refuses to start; logs ERROR; operator must intervene. Routing is broken until the migration succeeds.

### Edge Cases

- **UC-MAT-13-EC1: Daemon restart while a TG inbound is in flight** — the in-flight `Message` is lost (no replay queue per the explicit no-session-cache constraint per FR-MAT-10.3). Operator must re-send. **This is acknowledged as an intentional design choice (NOT a v0.8-style reconnect-replay buffer).**

---

## UC-MAT-14: Two `/switch` commands tap simultaneously for the same routing key — second tap wins (EDGE — race)

**Actor**: Two SDLC operators (in topic α of group `G`) OR one operator double-tapping; daemon.

**Preconditions**:
- Topic α is currently bound to CLI B.
- Both operators have permission to `/switch` per FR-MAT-8.6.
- They send `/switch C` and `/switch D` near-simultaneously.

**Trigger**: Two `Update::Message` events arrive at the daemon's Dispatcher within milliseconds.

### Primary Flow (Serialised by SQLite)

1. Dispatcher spawns two parallel handler tasks (teloxide's `handler_tree!` is concurrent).
2. Task #1 (`/switch C`) opens a SQLite `BEGIN IMMEDIATE` transaction. SQLite grants the write lock to task #1.
3. Task #2 (`/switch D`) attempts `BEGIN IMMEDIATE` — blocks until task #1 commits.
4. Task #1 unbinds B and binds C; commits.
5. Task #2 acquires the write lock; unbinds C and binds D; commits.
6. Final state: topic α bound to D (the second-completed tap wins per FR-MAT-8.3 default policy).
7. Daemon sends TG replies for BOTH commands (in topic α). The replies arrive in serial order, possibly with the SECOND reply showing the final state.

**Postconditions**:
- `agent_registry`: D is bound to `(<G>, <α_id>)`. B and C are unbound. Single row in the partial-unique index per FR-MAT-2.2.
- A subsequent `/whoami` from either operator confirms D is bound.

**FR Coverage**: FR-MAT-8.3, FR-MAT-2.2 (partial-unique index is load-bearing for atomicity).

### Alternative Flows

- **UC-MAT-14-A: Both taps target the SAME CLI** (both say `/switch C`) — second tap is a no-op rebind (UC-MAT-5-B); replies are both "ok".
- **UC-MAT-14-B: First tap's security check fails, second's succeeds** — task #1 replies with the denial (UC-MAT-15); task #2 applies its rebind.

### Error Flows

- **UC-MAT-14-E1: SQLite BUSY exceeds retry budget** (e.g. an unrelated long-running transaction holds the write lock) — one or both `/switch` commands reply `/switch failed: DB busy, try again`.

### Edge Cases

- **UC-MAT-14-EC1: Three or more concurrent taps** — serialised one at a time; last commit wins. Defensive truncation of TG-reply spam not implemented (acceptable scope for v1).

---

## UC-MAT-15: Non-binding user attempts `/switch` in a group → daemon refuses per FR-MAT-8.6 (EDGE — security)

**Actor**: An UNAUTHORIZED Telegram user (paired enough to be in the access gate but NOT the binding's `last_user_id` and NOT a chat admin of group `G`); daemon.

**Preconditions**:
- Topic α is bound to CLI B; `agent_registry` row stores `last_user_id = <orig_operator_user_id>`.
- Unauthorized user `U` has `user_id = <U_user_id>` where `<U_user_id> ≠ <orig_operator_user_id>` and `U` is NOT a chat admin of `G`.
- `U` passes the v0.6 access gate (they ARE allowed to message the bot in this chat; the v0.6 pairing model permits them).

**Trigger**: `U` types `/switch D` in topic α.

### Primary Flow (Denial path)

1. Dispatcher receives the command.
2. Security check (FR-MAT-8.6): handler reads `agent_registry.last_user_id` for the binding `(<G>, <α_id>)` → `<orig_operator_user_id>`. Compares with the command's `from.id` → `<U_user_id>`. Mismatch.
3. Handler calls `Bot::get_chat_administrators(<G>)` (teloxide) — `<U_user_id>` is NOT in the admin list.
4. Both checks fail. Handler does NOT mutate `agent_registry`.
5. Handler replies in topic α: `/switch denied: only @<orig_operator_username> or a chat admin can rebind this conversation.` (Username resolution requires a separate `Bot::get_chat_member` call; the daemon caches per the v0.6 §17 model OR falls back to displaying the numeric user_id.)
6. Daemon logs WARN: `unauthorized /switch attempt: chat=<G>, thread=<α_id>, by user_id=<U_user_id>, current binding's last_user_id=<orig_operator_user_id>. Denied.`

**Postconditions**:
- `agent_registry` UNCHANGED.
- TG reply in topic α explains the denial.
- Audit log line WARN-level.

**FR Coverage**: FR-MAT-8.6.

### Alternative Flows

- **UC-MAT-15-A: User `U` IS a chat admin of `G`** — admin override per FR-MAT-8.6; the rebind proceeds per UC-MAT-5 happy path.

### Error Flows

- **UC-MAT-15-E1: `get_chat_administrators` fails (network error, rate limit)** — handler fails closed: denies the rebind and replies `/switch failed: cannot verify admin status; try again later`. **Decision: fail-closed is the safer default for a security-gated command. Tracked under Decisions → Decisions made.**

### Edge Cases

- **UC-MAT-15-EC1: The binding row has `last_user_id = NULL`** (e.g. CLI was bound via daemon-internal API without a TG context) — fall back to chat-admin-only authorization. The first /switch from any chat admin succeeds and STAMPS `last_user_id`. **Tracked under OQ-MAT-3 — red-team may refine.**

---

> **⚠️ AR-9 amendment applies to UC-MAT-16 + UC-MAT-17 below (2026-06-04).** All step-11/14 descriptions of channel-event meta keys (`meta.is_callback_response`, `meta.value`, `meta.values`, `meta.multi`, `meta.question`, `meta.options`, `meta.originating_agent_id`) are SUPERSEDED. CC's `<channel>` surface renderer silently drops frames carrying meta keys outside the inbound-Telegram schema; Slice 8 round-trip data lives in `params.content` as a preamble. Single-select preamble: `[chat_ask kind=single ask_id=<uuid> value=<v>]`. Multi-select Done preamble: `[chat_ask kind=multi ask_id=<uuid> values=v1,v2,...]`. Compaction-resilience (UC-MAT-16-B): Mira reconstructs context from the preamble's `ask_id` only; the question / options are no longer in meta. AR-4 dead-agent fallback (UC-MAT-16-E3): `target_agent_id` is still OMITTED when originator dead, but `originating_agent_id` is no longer added — the ask_id breadcrumb serves the same role. Full spec at `docs/PRD.md` §18.10.9.

## UC-MAT-16: `chat_ask` single-select question → Telegram inline keyboard → operator tap → response routes to originating CC

> Updated 2026-06-03: Added UC-MAT-16, UC-MAT-17, UC-MAT-18 for Slice 8 (`chat_ask` MCP tool + inline keyboard + CallbackQuery handling). Architect Review Resolutions AR-1 through AR-7 (2026-06-04) are incorporated throughout.

**Actor**: Mira (the calling CC session); daemon; SDLC operator (tapping a button in Telegram); Telegram Bot API; originating CC bridge.

**Preconditions**:
- Daemon is running with `TELEGRAM_BOT_TOKEN` set; teloxide Dispatcher connected (no 401/409).
- Operator is paired — `access.json` `allowFrom` contains the operator's Telegram `user_id`.
- Mira's CC has an active UDS bridge connection to the daemon.
- The `pending_asks` table exists in `chat.db` (AR-6 schema applied via `apply_pending_asks_migration`).
- `OUTBOUND_TG_KEYBOARD` mpsc sender initialized (AR-1 single-Bot-owner pattern).
- The `chat_ask` tool name is in `TOOL_WHITELIST` (FR-MAT-11.1).

**Trigger**: Mira calls `chat_ask(thread="telegram:<chat_id>", question="Approve this plan?", options=[{label:"Approve", value:"approve"}, {label:"Reject", value:"reject"}])` with `multi` omitted (defaults to `false`).

### Primary Flow (Happy Path)

1. Daemon's `handle_chat_ask` handler validates the input schema: `thread`, `question`, and `options` present; `multi = false` (default); each option's formatted `callback_data` = `<uuid_v4_36>:<value>` is ≤ 64 bytes (AR-1 budget check); all valid.
2. Handler generates `ask_id` via uuid v4 (unguessable — SEC-7 constraint). Sends to `OUTBOUND_TG_KEYBOARD` with tuple `(chat_id, message_thread_id, question_text, InlineKeyboardMarkup)` and waits on a oneshot channel for the returned `message_id`.
3. The `run_long_poll` send loop drains `OUTBOUND_TG_KEYBOARD`; calls `Bot::send_message(chat_id, question_text).reply_markup(InlineKeyboardMarkup(...))` (same teloxide `Bot` instance — no second HTTP-client owner per AR-1).
4. Telegram delivers the `sendMessage` response; daemon captures `message_id` and returns it to the oneshot. Handler receives `message_id`.
5. Handler inserts `pending_asks` row: `(ask_id, chat_id, message_thread_id, message_id, requesting_agent_id=Mira.agent_id, question, options_json, multi=0, selected_values_json=NULL, created_at=now, expires_at=now+86400000)` (AR-6 schema; `question` and `options_json` stored for AR-5 CC-compaction resilience). **Send-then-insert ordering**: if `sendMessage` failed, no orphan row is inserted.
6. Handler returns `{ask_id, status: "pending"}` to Mira's CC via MCP tool response. Mira records `ask_id` for correlation.
7. Operator sees the question as a Telegram message with two inline buttons: `[Approve]` `[Reject]` (FR-MAT-11.2).
8. Operator taps `[Approve]`. Telegram delivers `Update::CallbackQuery { id, from: {user_id: <operator_user_id>}, chat_instance, data: "<ask_id>:approve", message: {message_id, chat: {id: <chat_id>}} }` to the daemon's long-poll loop (FR-MAT-11.3).
9. **Access-gate:** daemon calls `gate_callback(access, sender_id=<operator_user_id>)` (AR-3). Operator IS in `allowFrom` → passes. Daemon calls `Bot::answer_callback_query(id)` FIRST (AR-1 spinner-clear requirement) — no delay.
10. Daemon reads `pending_asks` row by `ask_id`. Row found, not expired, `multi=0`. Checks `agent_registry` for `requesting_agent_id` (AR-4 alive-check). Mira's CC is alive → set `meta.target_agent_id = Mira.agent_id`.
11. Daemon calls `build_channel_notification_callback_response(ask_id, value="approve", requesting_agent_id)` producing a `notifications/claude/channel` event with meta: `{is_callback_response: true, ask_id, value: "approve", question: "Approve this plan?", options: [...], multi: false, target_agent_id: "mira"}` (AR-5: `question`/`options`/`multi` included for compaction resilience).
12. Bridge filter at `bridge.rs` matches `meta.target_agent_id == "mira"` — delivers the channel event exclusively to Mira's CC (FR-MAT-11.5; Slice `0ba2c41` filter).
13. Daemon deletes the `pending_asks` row (ask resolved).
14. Mira's CC session receives `<channel meta.is_callback_response="true" meta.ask_id="<X>" meta.value="approve" ...>` and resumes decision-making with the operator's choice.

**Postconditions**:
- `pending_asks` row for `ask_id` is deleted.
- Operator's Telegram button no longer shows a loading spinner (answered via `answerCallbackQuery`).
- Mira's CC has received the `<channel>` event with `meta.value="approve"`.
- Daemon log contains `callback_response: ask_id=<X> value="approve" routed to agent_id=mira`.

**FR Coverage**: FR-MAT-11.1, FR-MAT-11.2, FR-MAT-11.3, FR-MAT-11.4, FR-MAT-11.5, FR-MAT-11.7. AR-1, AR-3, AR-4, AR-5, AR-6.

### Alternative Flows

- **UC-MAT-16-A: Daemon restarts between `chat_ask` send and operator tap (restart resilience)** — daemon goes down after step 5 (row inserted); operator taps after daemon comes back up. On restart, `pending_asks` row survives in `chat.db` (SQLite persistence per FR-MAT-11.7). The long-poll loop resumes; callback arrives and is processed via steps 8–14. **Precondition:** daemon restart does NOT delete `pending_asks` rows. Postcondition: identical to primary flow.
- **UC-MAT-16-B: Mira's CC compacts between `chat_ask` call and operator response (AR-5 compaction resilience)** — Mira's in-session memory of `ask_id` is lost during CC compaction. When the `<channel>` event arrives (step 14), the event carries `meta.question`, `meta.options`, and `meta.multi` — Mira reconstructs the semantic context from `meta` alone without needing in-session state. Mira correlates via `meta.ask_id` if she has it; otherwise treats it as a new operator input and reads question + options from `meta`.

### Exception Flows

- **UC-MAT-16-E1: Non-allowed user taps the button (AR-3 access-gate)** — `gate_callback` at step 9 returns `false`. Daemon drops the CallbackQuery entirely — no `answerCallbackQuery`, no pairing code, no log noise beyond a single `callback from non-allowed user_id=<X>; dropping` line. The operator does NOT see a loading spinner cleared (the non-allowed user sees a spinner that eventually clears on Telegram's side automatically). `pending_asks` row untouched.
- **UC-MAT-16-E2: `option_id` overflows 20-byte `callback_data` budget (AR-1 validation)** — at step 1, `validate_options_callback_data_budget` computes `36 + 1 + len(value)` for single-select. If any option's `value` exceeds 27 bytes, the `handle_chat_ask` handler returns MCP error `-32602` (Invalid params) with message `option value '<V>' exceeds callback_data budget (max 27 bytes for single-select)`. No DB write, no Telegram message sent. Mira receives the error as a tool-call failure.
- **UC-MAT-16-E3: Originating CC is no longer alive when callback arrives (AR-4 dead-agent fallback)** — at step 10, `agent_registry::list_alive(conn, None)` does NOT find `requesting_agent_id`. Daemon omits `meta.target_agent_id` and adds `meta.originating_agent_id = requesting_agent_id`. Bridge filter treats absent `target_agent_id` as unaddressed broadcast — channel event surfaces in ALL active CCs. Any live operator session sees `meta.originating_agent_id` and knows which agent originally asked the question.
- **UC-MAT-16-E4: Telegram 429 on `answerCallbackQuery` (AR-7)** — `answerCallbackQuery` is a cheap call; if it 429s, the spinner persists on the operator's side. Daemon logs the 429, continues processing the callback (DB write + channel notification proceed). The response is still delivered to Mira's CC. Per AR-7, `answerCallbackQuery` 429 does not retry; the notification channel is the load-bearing delivery path.
- **UC-MAT-16-E5: Unknown `callback_data` (no matching `pending_asks` row)** — `data` does not match any open row (expired, never existed, or already answered). Daemon logs `unknown callback_data; ask_id not found in pending_asks; dropping` and calls `answerCallbackQuery` to clear the spinner. No channel notification emitted.

### Edge Cases

- **UC-MAT-16-EC1: Ask TTL expires before operator taps** — GC predicate `expires_at < now()` runs per long-poll batch tail. Row is deleted at GC time. If operator taps after GC, falls into E5 path. Mira's CC never receives a response; Mira must treat the absence as a timeout and re-ask if needed.
- **UC-MAT-16-EC2: Operator taps the SAME button twice in rapid succession** — second tap's CallbackQuery arrives; step 9 `answerCallbackQuery` clears spinner again (idempotent for Telegram). Step 10: `pending_asks` row already deleted after first tap. Second tap falls into E5 path (row not found, dropped silently). Only one `<channel>` event is delivered to Mira.

---

## UC-MAT-17: `chat_ask` multi-select question → toggleable ✓ markers + Done button → operator finalizes → array routes back

**Actor**: Mira (the calling CC session); daemon; SDLC operator (tapping toggles and Done in Telegram); Telegram Bot API; originating CC bridge.

**Preconditions**:
- Same as UC-MAT-16.
- `chat_ask` invoked with `multi=true` and 2–8 options (FR-MAT-11.2 — max 8).

**Trigger**: Mira calls `chat_ask(thread="telegram:<chat_id>", question="Which slices to implement?", options=[{label:"Slice A", value:"slice_a"}, {label:"Slice B", value:"slice_b"}, {label:"Slice C", value:"slice_c"}], multi=true)`.

### Primary Flow (Happy Path)

1. Daemon's `handle_chat_ask` validates input: `multi=true`, 3 options. Callback-data budget: `<uuid_36>:toggle:<option_id>` = 44 + `len(option_id)` bytes; max option_id ≤ 20 bytes (AR-1 multi-select budget). Options valid.
2. Generates `ask_id` via uuid v4. Sends to `OUTBOUND_TG_KEYBOARD` with initial keyboard: 3 toggleable option buttons (no ✓ markers yet) + 1 `[Done]` button. Waits on oneshot for `message_id`.
3. `run_long_poll` drains queue; calls `Bot::send_message(...).reply_markup(InlineKeyboardMarkup(...))`. Captures `message_id`.
4. Inserts `pending_asks` row: `(ask_id, ..., multi=1, selected_values_json=NULL, question, options_json)`.
5. Returns `{ask_id, status: "pending"}` to Mira.
6. Operator sees 3 option buttons + `[Done]` button.
7. Operator taps `[Slice A]`. Telegram delivers `CallbackQuery { data: "<ask_id>:toggle:slice_a", ... }`.
8. `gate_callback` passes (allowed user). Daemon calls `answerCallbackQuery(id)` FIRST (AR-7 spinner-clear).
9. Daemon opens a SQLite transaction: `UPDATE pending_asks SET selected_values_json = '["slice_a"]' WHERE ask_id = ? RETURNING selected_values_json` (AR-7 atomic read-post-state). Transaction commits. New state: `["slice_a"]`.
10. Daemon calls `Bot::edit_message_reply_markup(chat_id, message_id, InlineKeyboardMarkup)` with updated keyboard: `[✓ Slice A]`, `[Slice B]`, `[Slice C]`, `[Done]` (FR-MAT-11.6).
11. Operator taps `[Slice C]`. Steps 8–10 repeat for `toggle:slice_c`. State becomes `["slice_a", "slice_c"]`. Keyboard updated: `[✓ Slice A]`, `[Slice B]`, `[✓ Slice C]`, `[Done]`.
12. Operator taps `[Done]`. Telegram delivers `CallbackQuery { data: "<ask_id>:done", ... }`.
13. `gate_callback` passes. `answerCallbackQuery(id)` called first.
14. Daemon reads `pending_asks` row via `SELECT ... RETURNING` for final `selected_values_json = ["slice_a", "slice_c"]`. Checks AR-4 alive-check for `requesting_agent_id`.
15. Daemon calls `build_channel_notification_callback_response(ask_id, values=["slice_a","slice_c"], requesting_agent_id)`. Meta: `{is_callback_response: true, ask_id, values: ["slice_a","slice_c"], question: "...", options: [...], multi: true, target_agent_id: "mira"}`.
16. Bridge filter delivers exclusively to Mira's CC.
17. Daemon deletes `pending_asks` row.
18. Mira's CC receives `<channel meta.is_callback_response="true" meta.values='["slice_a","slice_c"]' meta.multi="true" ...>`.

**Postconditions**:
- `pending_asks` row deleted.
- Mira receives `meta.values = ["slice_a", "slice_c"]`.
- Operator's inline keyboard buttons are no longer interactive (message edit finalized).
- Daemon log: `callback_response multi: ask_id=<X> values=["slice_a","slice_c"] routed to agent_id=mira`.

**FR Coverage**: FR-MAT-11.1, FR-MAT-11.2, FR-MAT-11.3, FR-MAT-11.4, FR-MAT-11.6, FR-MAT-11.7. AR-1, AR-3, AR-4, AR-5, AR-6, AR-7.

### Alternative Flows

- **UC-MAT-17-A: Operator untogles a previously-selected option** — operator taps `[✓ Slice A]` again. `toggle:slice_a` callback arrives. Atomic `UPDATE ... RETURNING` removes `slice_a` from `selected_values_json` (JSON array filter). Keyboard updated: `[Slice A]`, `[Slice B]`, `[✓ Slice C]`, `[Done]`. State: `["slice_c"]`.
- **UC-MAT-17-B: Operator taps Done with empty selection** — `selected_values_json` is `NULL` or `[]` at Done time. Daemon emits channel notification with `meta.values = []` (empty array). Mira receives an explicit empty-selection result. **Spec decision: empty selection is permitted at the protocol level; Mira may re-ask if her logic requires at least one selection.** No daemon-side rejection.
- **UC-MAT-17-C: Daemon restart mid-toggle session (AR-5 / FR-MAT-11.7)** — partial `selected_values_json` state survives in SQLite. On restart, the daemon's long-poll resumes. Next tap (toggle or Done) finds the row with its accumulated state intact; processing continues from that state. No loss of partial selections.

### Exception Flows

- **UC-MAT-17-E1: Non-allowed user taps a toggle (AR-3)** — same as UC-MAT-16-E1; CallbackQuery dropped silently. `pending_asks` row and `selected_values_json` untouched.
- **UC-MAT-17-E2: `option_id` overflows 20-byte multi-select budget (AR-1)** — format `<36>:toggle:<option_id>` requires `option_id` ≤ 20 bytes. Validated at step 1; MCP error `-32602` returned to Mira. No DB write, no TG message.
- **UC-MAT-17-E3: Telegram 429 on `editMessageReplyMarkup` (AR-7)** — `answerCallbackQuery` already called (spinner cleared). Daemon retries `editMessageReplyMarkup` once after `retry_after` delay (mirroring `telegram.rs:25-28`). On second 429, gives up silently. **SQLite row remains correct** (the toggle was committed before the edit call). UI keyboard is stale until operator next taps — the next tap re-edits the keyboard from the current DB state.
- **UC-MAT-17-E4: Originating CC dead at Done time (AR-4)** — same as UC-MAT-16-E3; `meta.target_agent_id` omitted, `meta.originating_agent_id` added, broadcast to all active CCs.
- **UC-MAT-17-E5: Concurrent taps within 50 ms (AR-7 SQLite serialization)** — two `toggle` callbacks arrive nearly simultaneously. First transaction acquires write-lock via `UPDATE ... RETURNING`; second blocks until first commits. Each tap is processed serially; no phantom read; `selected_values_json` is always consistent post-commit. Per AR-7 guarantee: «SQLite serializes via row write-lock + `UPDATE ... RETURNING` atomic».

### Edge Cases

- **UC-MAT-17-EC1: More than 8 options passed to `chat_ask`** — validated at step 1; `handle_chat_ask` returns MCP error `-32602` with message `chat_ask supports at most 8 options per FR-MAT-11.2`. No DB write.
- **UC-MAT-17-EC2: Ask TTL expires with a partially-toggled state** — GC deletes the row. Neither a Done response nor any channel event is emitted. Mira never receives a response. Partial selections are silently discarded (no audit trail of partial state by design — the 24h TTL is the only backstop; Mira must handle the silence as a timeout).

---

## UC-MAT-18: `chat_list_pending_asks` debug tool — Mira lists open asks via MCP

**Actor**: Mira (any CC session calling the MCP tool) OR any Claude Code CLI with a daemon bridge connection.

**Preconditions**:
- Daemon is running; `chat.db` accessible.
- `chat_list_pending_asks` is in `TOOL_WHITELIST` (FR-MAT-11.8; Slice 8c).
- `pending_asks` table exists (AR-6 schema applied).

**Trigger**: Mira invokes `chat_list_pending_asks()` — optionally with `{agent_id: "mira"}` or `{thread: "telegram:<chat_id>"}` filter params.

### Primary Flow (Happy Path — unfiltered)

1. Daemon's `handle_chat_list_pending_asks` receives the MCP call. Optional filter params absent.
2. Handler calls `pending_asks::list_open(conn, agent_id=None, chat_id=None)`.
3. `list_open` executes: `SELECT ask_id, chat_id, message_thread_id, question, requesting_agent_id, multi, options_json, created_at, expires_at FROM pending_asks WHERE expires_at > unixepoch('now','milliseconds') AND selected_values_json IS NULL ORDER BY created_at ASC`.
4. Returns array of `PendingAsk` structs serialized as JSON objects: `[{ask_id, chat_id, message_thread_id, question, requesting_agent_id, multi, options, created_at, expires_at}, ...]`.
5. Daemon wraps in MCP tool-result envelope: `{result: {asks: [...]}}`.
6. Mira's CC receives the list. Can inspect open asks, identify stale or stuck asks, correlate `ask_id`s with pending flows.

**Postconditions**:
- No mutation to `pending_asks` or any other table (read-only tool).
- Daemon log optionally carries `chat_list_pending_asks: returned <N> open asks` at DEBUG level.
- `pending_asks` table state unchanged.

**FR Coverage**: FR-MAT-11.8. Slice 8c.

### Alternative Flows

- **UC-MAT-18-A: Filtered by `agent_id`** — Mira calls `chat_list_pending_asks({agent_id: "mira"})`. `list_open` appends `AND requesting_agent_id = 'mira'` to the query. Returns only Mira's open asks. Useful when multiple agents have concurrent open asks and Mira wants to audit only her own.
- **UC-MAT-18-B: Filtered by `thread`** — Mira calls `chat_list_pending_asks({thread: "telegram:<chat_id>"})`. Handler parses `chat_id` from the thread string. `list_open` appends `AND chat_id = <N>`. Returns only asks on that Telegram conversation.
- **UC-MAT-18-C: Empty result (no open asks)** — `list_open` returns `[]`. Handler returns `{result: {asks: []}}`. Mira receives an empty list — normal state when no `chat_ask` calls are pending or all have been answered/expired.

### Exception Flows

- **UC-MAT-18-E1: `chat.db` inaccessible (disk error, permissions)** — `list_open` returns a SQLite error. Daemon returns MCP error `-32603` (Internal error) with a message describing the DB failure. No partial result.
- **UC-MAT-18-E2: Invalid `thread` string format** — `thread` provided but does not match `telegram:<integer>` pattern. Handler returns MCP error `-32602` (Invalid params) with `"thread must be 'telegram:<chat_id>'"`. No DB query executed.

### Edge Cases

- **UC-MAT-18-EC1: Large number of open asks (e.g. 100+)** — `list_open` returns all unexpired, unanswered rows. No pagination in Slice 8c MVP. Mira's CC receives a large JSON array; Claude Code renders it inline. No truncation by the daemon.
- **UC-MAT-18-EC2: Ask answered between `list_open` query execution and Mira reading the result** — inherent TOCTOU gap in a read-only debug tool. The returned list is a snapshot at query time; a tap processed in parallel may have deleted a row that appeared in the list. Mira treats the result as a best-effort snapshot, not a real-time lock. Not a correctness issue (the tool is diagnostic only).
- **UC-MAT-18-EC3: Expired asks that have not yet been GC'd** — GC runs per long-poll batch tail (AR-6 spec). Between GC runs, an expired row with `expires_at < now()` might still exist in the table. The `WHERE expires_at > now()` predicate in `list_open` excludes them — Mira never sees expired rows, even un-GC'd ones.

---

## Cross-cutting notes (for qa-planner)

- **`Verification Class` hint for KP1 / KP2 / KP3:** `Mixed` (UI/UX + CLI + DB). Each requires the operator-side TG-Desktop screenshot, terminal screenshot of the CLI receiving the `<channel>` event, daemon log tail, and SQL row from `agent_registry`. Per PRD §18.5 evidence table.
- **`Verification Class` hint for UC-MAT-4 through UC-MAT-7 (bot commands):** `Mixed` (UI/UX + DB). Screenshot of TG reply + SQL state.
- **`Verification Class` hint for UC-MAT-8 / 9 / 10:** `CLI` + `FS` (log inspection). Daemon log substrings are the load-bearing evidence; no TG screenshot required (TG is OFF or unreachable in these flows).
- **`Verification Class` hint for UC-MAT-11 / 12 / 13:** `Mixed` (DB + log). DB row state + daemon log + (optional) TG hint screenshot.
- **`Verification Class` hint for UC-MAT-14 / 15:** `Mixed` (DB + log + TG reply screenshot).
- **`Evidence Required` placeholders should reference exact log substrings from the plan (`routed (chat_id=<N>, thread_id=...) -> cli_id=<X>`) and exact SQL queries (`SELECT cli_id, chat_id, thread_id FROM agent_registry WHERE state='alive'`).** Vague evidence ("works correctly", "no errors") fails the Plan Critic.
- **`Verification Class` hint for UC-MAT-16 / UC-MAT-17:** `Mixed` (UI/UX + DB). Evidence: TC-CHA-1 screenshot (TG inline buttons rendered), TC-CHA-3/TC-CHA-6 screenshots (multi-select ✓ markers), SQL `SELECT * FROM pending_asks WHERE ask_id='<X>'` showing row deleted after tap, daemon log `callback_response: ask_id=<X>`, Claude Code CC transcript showing the `<channel>` event received.
- **`Verification Class` hint for UC-MAT-18:** `DB` + `CLI`. Evidence: `chat_list_pending_asks` tool-call result JSON printed in CC transcript; SQL `SELECT count(*) FROM pending_asks WHERE expires_at > unixepoch('now','milliseconds') AND selected_values_json IS NULL` row count matching the returned array length. No TG screenshot required (read-only tool, no TG side effect).
- **UC-MAT-16-E3 / UC-MAT-17-E4 (dead-agent broadcast) evidence:** daemon log substring `callback_response unaddressed: originating_agent_id=<X> broadcasting`; absence of `target_agent_id` in the channel notification meta; channel event visible in ALL active CC sessions (not just the originator's).
- **UC-MAT-16-E1 / UC-MAT-17-E1 (non-allowed callback drop) evidence:** TC-CHA-2 from the plan — daemon log `callback from non-allowed user_id=<U>; dropping`; no `answerCallbackQuery` response (spinner persists on the non-allowed user's Telegram client); `pending_asks` row count unchanged.
- **UC-MAT-17-E3 (429 on editMessageReplyMarkup) evidence:** daemon log `editMessageReplyMarkup 429; retrying after <N>ms`; on second 429, log `editMessageReplyMarkup abandoned after retry`; SQLite row state correct (`SELECT selected_values_json FROM pending_asks WHERE ask_id='<X>'` returns the accumulated toggle state).
- **Needs-implementation flag for qa-planner — UC-MAT-18:** confirm whether `list_open` predicate is `selected_values_json IS NULL` or `expires_at > now()` (OQ-MAT-UC-5) before authoring TC-CHA-9; otherwise the test expectation for partially-toggled open rows will be ambiguous.
- **Needs-implementation flags for qa-planner:**
  - UC-MAT-5 step 3 + UC-MAT-15 step 2 assume `agent_registry.last_user_id` column exists or is added by Slice 5. v0.6 baseline does NOT expose this column per the §17.7 schema. **qa-planner: flag Slice 5 as needing a tiny additive column (or per-row JSON metadata) for `last_user_id`.**
  - UC-MAT-7 step 3 assumes `host` / `cwd` / `pid` columns are readable from `agent_registry` or derivable from `connection_id`. v0.6 §17.7 lists `connection_id` but not `host/cwd/pid` as explicit columns. **qa-planner: flag Slice 5 as needing verification that this metadata is reachable.**
  - UC-MAT-2 step 10 + UC-MAT-1 step 10 assume teloxide 0.17's `SendMessageSetters::message_thread_id` exists. **Marked `verified: no — assumption` in External contracts; Slice 1 architect verifies.**
  - UC-MAT-1 step 6 + UC-MAT-2 step 6 assume `notifications/claude/channel` meta JSON has a `thread_id` field added per FR-MAT-7.1. This is an additive contract change — no v0.6 baseline code emits it; Slice 3 adds the emission.

---

## Facts

### Verified facts

- **[UC-MAT-16/17/18 addition — 2026-06-03]** Slice 8 spec read from `.claude/plan.md` lines 312–398 this session. FR-MAT-11.1–11.8 are listed ONLY in the plan's Slice 8 spec; they do NOT yet appear in `docs/PRD.md` (confirmed by `Grep FR-MAT-11` → 0 hits in PRD). The plan is the authoritative source for these FRs until a PRD addendum is authored. Salience: high.
- **[UC-MAT-16/17/18 addition — 2026-06-03]** Architect Review Resolutions AR-1 through AR-8 (labelled `### Slice 8 — Architect Review Resolutions (2026-06-04)`) read from `.claude/plan.md` lines 335–378 this session. Verdict: PASS-WITH-CONCERNS. AR-1 (Bot-ownership / answerCallbackQuery / callback_data budget), AR-2 (CallbackQuery struct field set), AR-3 (gate_callback separate from gate_dm), AR-4 (dead-agent broadcast), AR-5 (CC-compaction resilience via meta fields), AR-6 (pending_asks schema), AR-7 (multi-select atomic SQLite transaction + 429 retry-once). Salience: high.
- **[UC-MAT-16/17/18 addition — 2026-06-03]** Slice 8c spec read from `.claude/plan.md` lines 386–398. `chat_list_pending_asks` is a new read-only MCP tool (FR-MAT-11.8). Files affected: `server.rs` handler + `pending_asks.rs::list_open` + `mcp.rs` TOOL_WHITELIST entry. Salience: medium.
- **[UC-MAT-16/17/18 addition — 2026-06-03]** `insights.db` absent (operator noted corpus corrupt in OQ resolution at plan line 384) — insights query skipped per `~/.claude/rules/knowledge-base.md` activation sentinel. Knowledge-base `index.db` also absent. Corpus protocol silently skipped. Salience: low.
- PRD §18 read in this session (lines 783–971 of `docs/PRD.md` per `wc -l` = 1041; the §18 section ends before the `## Facts` block at line 972). 10 FR groups (FR-MAT-1 through FR-MAT-10), 6 NFRs, 3 ACs (KP1/KP2/KP3), 9 risks (R-MAT-1 through R-MAT-9). Salience: high.
- Plan at `.claude/plan.md` read in full this session (321 lines, v2 post-Plan-Critic). KP1-KP3 success criteria at plan lines 37–48 verbatim. 8 slices, sequential single Wave 1. Salience: high.
- v0.6 `agent_registry` schema columns at v0.6.0 baseline: `(agent_id, agent_name, connection_id, chat_thread_id: Option<String>, state, spawned_at, last_pinged_at)` with partial-unique index `(chat_thread_id, agent_name) WHERE state='alive'`. Source: plan Facts line 253 (citing `git show claudebase-v0.6.0:src/daemon/agent_registry.rs:92-138`). **Does NOT include `last_user_id`, `host`, `cwd`, or `pid` as explicit columns** — UC-MAT-5 / UC-MAT-7 / UC-MAT-15 flag this gap. Salience: high.
- Routing key `(chat_id: i64, message_thread_id: Option<i64>)` is mandated by plan line 21 (architecture decision C3) and FR-MAT-1.1 (PRD line 808). Salience: high.
- Three CLIs (A, B, C) are independent Claude Code processes launched by `claudebase run` from three different cwds or with three different `--agent-id`s — per plan line 47. Salience: high.
- Daemon log line literal `routed (chat_id=<N>, thread_id=<T>) -> cli_id=<X>` is the load-bearing evidence string for KP1/KP2/KP3 per plan lines 54–56 and PRD §18.5 AC-MAT-KP1/2/3. Salience: high.
- v0.6 IPC framing: length-prefixed 4-byte big-endian + UTF-8 JSON, 16 MiB cap. Source: plan Facts line 250. Salience: medium.
- The `agent_register` re-association flow on daemon restart (UC-MAT-13 step 7) is inherited from v0.6 §17 connection-management — preserved as-is per FR-MAT-10.3 + plan §"Open for modification → bridge.rs" line 101. Salience: medium.
- Knowledge-base / insights corpus NOT activated on this branch — `<project>/.claude/knowledge/index.db` and `insights.db` absent per Mira's spawn-prompt instruction. Corpus protocol silently skipped per `~/.claude/rules/knowledge-base.md` `## Activation sentinel`. Salience: low.

### External contracts

- **[UC-MAT-16/17/18 addition]** **Telegram Bot API `CallbackQuery` object** — symbol: `{id: String, from: User, chat_instance: String, data: Option<String>, message: Option<MessageRef>}` per AR-2 (revised minimal field set); `chat_instance: String` is REQUIRED; `data` is optional (absent for game buttons). Source: `https://core.telegram.org/bots/api#callbackquery` — cited in plan AR-2 (`plan.md` line 347); docs URL not opened this session — verified: **no — assumption** (plan AR-2 cites the URL; implementer MUST verify at impl-time per AR-2 directive). Risk: missing `chat_instance` in struct definition causes deserialization failure on every CallbackQuery. Salience: high.
- **[UC-MAT-16/17/18 addition]** **Telegram Bot API `answerCallbackQuery`** — symbol: `Bot::answer_callback_query(callback_query_id: String)` — must be called within ~15 seconds of receipt to clear the loading spinner (AR-1). Source: `https://core.telegram.org/bots/api#answercallbackquery` — referenced in plan AR-1 (`plan.md` line 343); not opened this session — verified: **no — assumption** (15-second SLA and spinner behavior are well-documented Telegram Bot API behavior but not re-verified in this session). Salience: high.
- **[UC-MAT-16/17/18 addition]** **Telegram Bot API `editMessageReplyMarkup`** — symbol: `Bot::edit_message_reply_markup(chat_id, message_id, InlineKeyboardMarkup)` — used by multi-select toggle flow (AR-7); retried once on 429 then abandoned (AR-7). Source: `https://core.telegram.org/bots/api#editmessagereplymarkup` — referenced in plan Slice 8 spec (plan.md line 328); not opened this session — verified: **no — assumption**. Risk: method signature on teloxide 0.17 wrapper may differ. How to verify: Slice 8 implementer reads teloxide 0.17 docs at impl-time. Salience: high.
- **[UC-MAT-16/17/18 addition]** **Telegram Bot API `callback_data` 64-byte budget** — symbol: `callback_data` field max 1–64 bytes per Telegram Bot API. Format single-select: `<uuid36>:<value>` = 37 + len(value) ≤ 64, so value ≤ 27 bytes. Format multi-select toggle: `<uuid36>:toggle:<option_id>` = 44 + len(option_id) ≤ 64, so option_id ≤ 20 bytes. Source: `https://core.telegram.org/bots/api#inlinekeyboardbutton` — referenced in plan AR-1 (plan.md line 345); not opened this session — verified: **no — assumption** (64-byte limit is widely documented; exact byte-count arithmetic derived from format strings stated in plan AR-1). Salience: high.
- **[UC-MAT-16/17/18 addition]** **SQLite `UPDATE ... RETURNING` atomicity** — symbol: `UPDATE pending_asks SET selected_values_json = ? WHERE ask_id = ? RETURNING selected_values_json` — used by AR-7 multi-select concurrency control. Source: SQLite docs (https://www.sqlite.org/lang_returning.html); referenced in plan AR-7 (plan.md line 376); not opened this session — verified: **no — assumption** (standard SQLite RETURNING behavior; broadly relied-on). Risk: SQLite version predating RETURNING support (added in 3.35.0, 2021-03-12). How to verify: Slice 8 implementer checks SQLite version in CI. Salience: medium.
- **[UC-MAT-16/17/18 addition]** **`OnceLock<mpsc::UnboundedSender<...>>` pattern (AR-1 OUTBOUND_TG_KEYBOARD)** — symbol: `std::sync::OnceLock` used for `OUTBOUND_TG_KEYBOARD` parallel to existing `OUTBOUND_TG` (plan.md line 341; `telegram.rs:79` for the pattern reference). Source: plan AR-1 cite + `src/daemon/telegram.rs:79` (not read this session beyond the plan's cite) — verified: **no — assumption** (pattern is consistent with existing `OUTBOUND_TG` usage as described in the plan). Salience: medium.
- **[UC-MAT-16/17/18 addition]** **`uuid` crate v4 generation** — symbol: `uuid::Uuid::new_v4().to_string()` — used for unguessable `ask_id` (SEC-7). Source: plan Slice 8 spec (plan.md line 322); uuid crate docs not opened this session — verified: **no — assumption**. Risk: uuid crate not in Cargo.toml at v0.6 baseline. How to verify: Slice 8 implementer confirms `uuid` dep exists or adds it. Salience: medium.
- **[UC-MAT-16/17/18 addition]** **`pending_asks` table schema (AR-6)** — symbol: columns `(ask_id TEXT PK, chat_id INTEGER, message_thread_id INTEGER NULL, message_id INTEGER NOT NULL, requesting_agent_id TEXT NOT NULL, question TEXT NOT NULL, options_json TEXT NOT NULL, multi INTEGER NOT NULL DEFAULT 0, selected_values_json TEXT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL)` + index `pending_asks_expires_idx ON pending_asks(expires_at)`. Lives in `chat.db`. Source: plan AR-6 (plan.md lines 355–372) — read this session — verified: **yes** (verbatim from plan AR-6). Salience: high.
- **teloxide 0.17 `Update::Message` + `Dispatcher::builder` + `handler_tree!`** — symbol: standard high-level event-driven entry points; `Message.chat.id` (i64), `Message.message_thread_id` (expected `Option<i32>` per Telegram Bot API) — source: `Cargo.toml:90` PIN verified by plan Facts line 244 (NOT opened crate docs this session) — verified: **PIN yes; `message_thread_id` field-existence DEFERRED to Slice 1 architect verification per R-MAT-4 / OQ-MAT-2**. Salience: high.
- **teloxide 0.17 `SendMessageSetters::message_thread_id`** — symbol: outbound builder method for forum-topic targeting — source: NOT opened this session — verified: **no — assumption**. Risk: method may have a different name (`with_message_thread_id`, etc.). How to verify: Slice 1 / Slice 4 architect pre-review. Used by UC-MAT-2 step 10 and UC-MAT-3 step 10. Salience: high.
- **teloxide 0.17 `Bot::get_chat_administrators`** — symbol: standard Telegram Bot API wrapper used by UC-MAT-15 chat-admin verification — source: NOT opened this session — verified: **no — assumption**. Risk: method name or sync/async signature may differ. How to verify: Slice 5 implementer reads docs at pin time. Salience: medium.
- **teloxide 0.17 `Bot::get_chat_member`** — symbol: used by UC-MAT-15 step 5 for username resolution in denial replies — source: NOT opened this session — verified: **no — assumption**. Salience: low (cosmetic in the denial message; numeric user_id fallback is acceptable).
- **Telegram Bot API `Message.message_thread_id`** — symbol: optional integer present on inbound Message when the message originates from a forum topic — source: `https://core.telegram.org/bots/api#message` (NOT opened this session) — verified: **no — assumption**. Salience: high.
- **Telegram Bot API `sendMessage` with `message_thread_id`** — symbol: outbound parameter that targets a specific forum topic — source: `https://core.telegram.org/bots/api#sendmessage` (NOT opened this session) — verified: **no — assumption**. Salience: high.
- **Telegram Bot API HTTP status codes** — 401 (UC-MAT-9), 409 (UC-MAT-10), 403 (UC-MAT-11-E1), 400 (UC-MAT-3-EC1) — source: NOT opened this session; standard HTTP semantics for the Bot API — verified: **no — assumption**. Salience: medium.
- **MCP `notifications/claude/channel` notification method** — symbol: existing v0.6 method (`plugins/telegram-rs/src/mcp/notification.rs:59` per plan Facts line 248). Adds optional `thread_id: Option<String>` meta field per FR-MAT-7.1 in this feature. — verified: **partial** (method name verified by plan Facts; new `thread_id` field is the additive contract change). Salience: high.
- **MCP `reply` tool params** — symbol: existing v0.6 CLI-facing tool. Gains optional `message_thread_id: Option<String>` param per FR-MAT-6.1. — verified: **partial** (tool name verified by plan Facts line 249; new param is the additive contract change). Salience: high.
- **MCP `notifications/tools/list_changed` reconnect mechanism** — symbol: v0.6 mechanism by which CLI's `bridge.rs` discovers daemon-availability changes (per UC-MAT-13-A and FR-MAT-10.3) — source: PRD §17 + plan Slice 7 line 160 — verified: partial (inherited from v0.6; not re-read in this session). Salience: medium.
- **JSON-RPC 2.0 additive-evolution convention** — symbol: «consumers MUST ignore unknown fields» — source: JSON-RPC 2.0 spec (NOT opened this session); broadly applied industry convention — verified: **no — assumption** (operator confirmed v0.6 Claude Code client tolerance per plan Assumptions line 264). Salience: medium.
- **SQLite `BEGIN IMMEDIATE` write-lock serialization** — symbol: write-lock acquisition for the SQLite transaction used by UC-MAT-5 and UC-MAT-14 — source: SQLite docs (NOT opened this session); standard SQLite behavior — verified: **partial** (standard behavior, broadly relied on; not re-verified in this session). Salience: medium.

### Assumptions

- **[UC-MAT-16/17 addition]** FR-MAT-11.1 through FR-MAT-11.7 are cited solely from the plan's Slice 8 spec (plan.md lines 318–319), not from a PRD section authored with `## Facts` discipline. Risk: if the PRD addendum re-numbers or revises these FRs, the UC citations become stale. How to verify: prd-writer addendum for Slice 8 should reconcile FR numbers before the QA planner consumes this file. Salience: high.
- **[UC-MAT-16/17]** Empty multi-select selection at Done time (UC-MAT-17-B) is resolved as "permit empty — return `[]`". The plan does not state this explicitly; it is a spec decision made in this UC. Risk: the implementer may instead chose to reject Done with empty selection. How to verify: confirm with operator or architect at Slice 8 pre-review; the alternative (reject with MCP error) is equally valid. Salience: medium.
- **[UC-MAT-17]** The multi-select ✓-marker keyboard representation (replacing the option label with `✓ <label>` on the button) is inferred from plan line 328 ("taps update ✓ markers via `editMessageReplyMarkup`"). Exact button text format is an assumption. Risk: implementer may use a different ✓ representation (emoji vs ASCII, prefix vs suffix). How to verify: TC-CHA-6 screenshot evidence in QA plan will capture the actual rendering. Salience: low.
- **[UC-MAT-18]** `selected_values_json IS NULL` is used in `list_open` to exclude answered rows (including Done-finalized multi-select rows). For single-select, `selected_values_json` is populated only after the callback resolves — but the row is immediately deleted after resolution (UC-MAT-16 step 13). For multi-select, intermediate toggles populate `selected_values_json` before Done. Risk: a partially-toggled multi-select row (non-NULL `selected_values_json` before Done) would be EXCLUDED from `list_open` by the IS NULL predicate even though it is still open. How to verify: Slice 8c implementer must clarify whether the filter should be `selected_values_json IS NULL` (only untouched rows) or `expires_at > now()` (all unresolved rows). **This is an open question — see OQ-MAT-UC-5.** Salience: high.
- The v0.6 `agent_registry` schema does NOT carry `last_user_id` (the field is needed by UC-MAT-5 / UC-MAT-15 to enforce FR-MAT-8.6). The plan does not call out adding this column explicitly. **Risk:** UC-MAT-5 and UC-MAT-15 may be un-implementable on the v0.6 baseline without a tiny additive column or a per-row JSON metadata blob. **How to verify:** Slice 5 implementer audits the existing v0.6 columns AND flags this gap; qa-planner adds an explicit needs-implementation TC. Salience: high.
- The v0.6 `agent_registry` schema does NOT explicitly carry `host` / `cwd` / `pid` columns (needed by UC-MAT-7 step 3). They MAY be derivable from `connection_id` (e.g. the daemon stores process metadata against the connection in memory) but this is not confirmed in the plan facts. **Risk:** UC-MAT-7 may be un-implementable. **How to verify:** Slice 5 implementer audits; qa-planner adds needs-implementation TC. Salience: high.
- The `notifications/claude/channel` meta JSON has a `thread_id` field that the v0.6 Claude Code CLI client gracefully ignores when unknown (per JSON-RPC convention; per plan Assumptions line 264). Risk: a strict consumer might reject; operator confirms the v0.6 client is tolerant. Salience: medium.
- The operator's «one bot serves three CLIs» model presumes Telegram permits a single bot to receive Updates from a DM + a group with topics simultaneously. Standard bot capability per Telegram docs; not session-verified. Risk: if the bot lacks Forum Topics permission in the group, KP2/KP3 fail silently. How to verify: smoke runbook step «invite bot as admin, enable Forum Topics on the group». Salience: high.
- v0.6's `bridge.rs` reconnect mechanism (FR-MAT-10.3) is unchanged and CONTINUES to fire `notifications/tools/list_changed` after a daemon restart. Risk: subtle drift between the v0.6 baseline and the new daemon behavior. How to verify: UC-MAT-13 happy-path live test by qa-engineer. Salience: medium.
- The connection-drop → `state='orphaned'` transition (UC-MAT-12 step 2) is an EXISTING v0.6 behavior that the new feature does not change. Risk: behavior may not actually fire on every drop class (e.g. half-open TCP). How to verify: Slice 5 / Slice 7 implementer confirms. Salience: medium.

### Open questions

- **OQ-MAT-UC-5 (`list_open` filter predicate for answered multi-select rows — UC-MAT-18)** — the `WHERE selected_values_json IS NULL` predicate excludes partially-toggled multi-select rows that are still open. Should `list_open` use `selected_values_json IS NULL` (exclude any row that has been touched) or simply `expires_at > now()` (include all non-expired rows regardless of toggle state)? The plan Slice 8c spec does not resolve this. Needs: Slice 8c implementer decision, documented under `### Decisions made`. Salience: high.
- **OQ-MAT-UC-6 (FR-MAT-11.x numbering alignment with PRD addendum)** — FR-MAT-11.1 through FR-MAT-11.8 are cited from the plan Slice 8 spec only; the PRD does not yet carry these FRs. When prd-writer authors the PRD addendum for Slice 8, the FR numbers must align. Needs: prd-writer addendum before the qa-planner consumes UC-MAT-16/17/18. Salience: medium.
- **OQ-MAT-UC-1 (`last_user_id` column existence on v0.6 `agent_registry`)** — UC-MAT-5 and UC-MAT-15 enforce FR-MAT-8.6 using this field. The v0.6 baseline does NOT advertise it. Needs: Slice 5 architect call + possibly a tiny additive schema column. Salience: high.
- **OQ-MAT-UC-2 (`host` / `cwd` / `pid` reachability for UC-MAT-7)** — Slice 5 architect call. Salience: high.
- **OQ-MAT-UC-3 (Orphan-inbound fallback UX exact shape)** — should the TG hint reply be unconditional, gated by «first time per routing key», or omitted entirely? Default: log + reply unconditionally. Refined at bootstrap by architect per OQ-MAT-5 (PRD Open Questions list). Salience: medium.
- **OQ-MAT-UC-4 (Fail-closed vs fail-open on `get_chat_administrators` errors in UC-MAT-15-E1)** — current decision: fail-closed. Operator may override at bootstrap. Salience: low (defensible default).

## Decisions

### Inbound validation

- **[UC-MAT-16/17/18 addition — 2026-06-03]** Inbound task: «append UC-MAT-16, UC-MAT-17, UC-MAT-18 for Slice 8 (`chat_ask` + inline keyboard + CallbackQuery) without disturbing UC-MAT-1 through UC-MAT-15». Challenged: yes — verified plan.md Slice 8 spec (lines 312–398), AR-1 through AR-8 (lines 335–378), and Slice 8c spec (lines 386–398) are mutually consistent. Push-back surfaced: FR-MAT-11.x numbers are cited from the plan only; PRD does not yet carry them (confirmed by Grep). This is not a contradiction — it is a known documentation gap. Outcome: proceed with plan as authoritative source; add OQ-MAT-UC-6 flagging the need for PRD alignment before qa-planner consumes this file. One assumption surfaced: empty multi-select at Done time (UC-MAT-17-B) — plan is silent; spec decision made here as «permit empty, return `[]`»; logged as medium-salience assumption. Outcome: proceeded as instructed. Salience: high.
- Inbound task: «author 15 UCs covering KP1-KP3 happy paths, 4 alt flows for bot commands, 3 error flows for token/dispatcher errors, and 5 edge cases for orphan / death / restart / race / security» with explicit format requirements (Actors / Preconditions / Trigger / Main Flow / Postconditions / Data / FR Coverage / Alt Flows / Errors / Edges) and FR-MAT mapping. Challenged: yes — verified PRD §18 (lines 783–971) and `.claude/plan.md` (321 lines) are mutually consistent on the 10 FR groups, 3 ACs, routing-key shape, and the 8-slice plan. **One push-back surfaced:** Mira's spawn prompt lists 15 distinct UCs, but UC-MAT-5 (`/switch`) and UC-MAT-15 (denial) both depend on a `last_user_id` field that the v0.6 `agent_registry` baseline does NOT carry (per plan Facts line 253). This is NOT a contradiction in the upstream documents — both PRD §18.8 and plan FR-MAT-8.6 explicitly defer to «red-team at bootstrap» (per plan R3 line 205 and OQ3 line 225) — but the gap WILL bite the qa-planner unless the use-cases file flags it. Outcome: proceeded with the 15 UCs as instructed AND added explicit `OQ-MAT-UC-1` / `OQ-MAT-UC-2` + needs-implementation hints in the cross-cutting notes section for qa-planner. Salience: high.
- Spawn-prompt instruction «do NOT drop any of the 15; may merge or split if a UC is genuinely 2 distinct scenarios» challenged briefly. Outcome: kept all 15 as separate top-level UCs since each maps cleanly to a distinct testable scenario; no merge/split applied. Salience: low.
- Spawn-prompt date `2026-06-02` matches the session reminder `currentDate: 2026-06-02` and matches PRD §18 Date and the plan's Date drafted. No drift. Salience: low.

### Decisions made

- **[UC-MAT-16/17/18 addition]** Authored UC-MAT-16 (single-select `chat_ask` round-trip), UC-MAT-17 (multi-select toggle round-trip), UC-MAT-18 (`chat_list_pending_asks` debug tool) as instructed. Each covers primary flow, 2–3 alternative flows, 4–5 exception flows, 2–3 edge cases. Q1 hack? no | Q2 sane? yes — the scenario decomposition mirrors the TC-CHA-1 through TC-CHA-9 plan entries and the AR-1/3/4/5/7 resolutions, which together define 5 distinct failure modes and 2 distinct architectural edge cases, each warranting a separate exception-flow entry | Q3 alternatives? collapsing 16/17 into a single multi-mode UC considered and rejected (they have distinct keyboard rendering logic, distinct callback-data formats, distinct concurrency paths — a combined UC would obscure the per-mode testability) | Q4 cause (all flows address root cause, not symptom) | Q5 n/a. Salience: high.
- **[UC-MAT-17-B spec decision]** Empty multi-select selection at Done time: resolved as «daemon emits `values=[]`, Mira decides what to do». Alternative: daemon returns `-32602` rejection. Q1 hack? no | Q2 sane? yes — delegating «minimum selection» policy to the calling agent (Mira) is the more flexible design; the daemon is a transport layer | Q3 alternatives? considered daemon-side minimum-1 enforcement; rejected because some callers may legitimately want to know the operator chose nothing | Q4 cause | Q5 n/a. Logged as medium-salience assumption (OQ not raised — this is a spec decision, not an unresolved question; the decision is made and documented). Salience: medium.
- **[UC-MAT-16-EC2 idempotency decision]** Duplicate tap on an already-answered ask: resolved as UC-MAT-16-E5 path (unknown ask_id, drop + answerCallbackQuery). Alternative: re-deliver the original response. Q1 hack? no | Q2 sane? yes — re-delivery would violate exactly-once semantics for Mira's conversation state | Q3 alternatives? considered re-delivery; rejected (same rationale as the plan's reasoning against replay queues per FR-MAT-10.3) | Q4 cause | Q5 n/a. Salience: low.
- Authored 15 UCs as instructed: 3 PRIMARY (UC-MAT-1/2/3 = KP1/KP2/KP3), 4 ALT-FLOW (UC-MAT-4/5/6/7 = `/agents`, `/switch`, `/whoami`, `/here`), 3 ERROR (UC-MAT-8/9/10 = token-unset, 401, 409), 5 EDGE (UC-MAT-11/12/13/14/15 = orphan, CLI death, daemon restart, race, security). Each UC has the mandated 9-block format. Q1 hack? no | Q2 sane? yes | Q3 alternatives? merging UC-MAT-1/2/3 into a single «routing matrix» UC considered and rejected (each maps to a separate AC and a separate evidence artifact in the QA plan; merging would obscure the per-AC traceability) | Q4 cause | Q5 n/a. Salience: high.
- Each UC's FR Coverage list cites explicit FR-MAT identifiers from PRD §18 (e.g. UC-MAT-1: FR-MAT-1.1, 1.2, 1.3, 2.2, 3.1, 3.2, 5.1, 5.2, 6.3, 7.1 and AC-MAT-KP1). This direct mapping enables the qa-planner to produce a TC row per (UC, FR) tuple without re-deriving the cross-reference. Q1 hack? no | Q2 sane? yes (matches the §17 use-cases file convention which the qa-planner consumes) | Q3 alternatives? omitting the explicit list considered and rejected (would shift derivation cost to qa-planner). Salience: medium.
- For UC-MAT-15-E1 (`get_chat_administrators` network failure), default decision is fail-closed (deny the rebind, reply «try again later»). Q1 hack? no | Q2 sane? yes (security-gated command should never silently succeed on uncertainty) | Q3 alternatives? fail-open (rejected — opens a denial-of-service-as-rebind vector where an attacker who briefly disrupts the daemon's network can force chat-admin verification to fail and proceed unauthorized; would defeat FR-MAT-8.6 entirely) | Q4 cause | Q5 root-cause-tracked: yes — surfaced as OQ-MAT-UC-4 for operator-override at bootstrap. Salience: medium.
- Cross-cutting `Verification Class` hints (Mixed / CLI+FS) added at the end of the file for the qa-planner. Q1 hack? no | Q2 sane? yes (this is the documented hand-off path per `~/.claude/CLAUDE.md` qa-planner expectations) | Q3 alternatives? embedding hints inline per UC considered and rejected (clutters the human-readable use-case body; the cross-cutting section is the canonical place). Salience: low.
- All teloxide / Telegram Bot API contracts marked `verified: no — assumption` because no teloxide source / docs / Telegram Bot API page was opened in this session. The PIN itself (teloxide 0.17) is verified via the plan's Facts citation, but field-level symbols (`Bot::send_message`, `SendMessageSetters::message_thread_id`, `get_chat_administrators`, etc.) are explicitly deferred to Slice 1 / Slice 4 / Slice 5 architect verification. Conservative labeling per Protocol 1 Q4. Salience: high.

### Hacks / workarounds acknowledged

- UC-MAT-15-EC1 («binding row has `last_user_id = NULL` → fall back to chat-admin-only authorization, first /switch stamps the field») is a deliberate transitional workaround for the v0.6 baseline gap where `last_user_id` is not a schema column yet. **Why a hack:** the proper fix is to add `last_user_id` to the additive schema migration in Slice 2 (alongside `routing_chat_id` / `routing_thread_id`); the «stamp on first valid /switch» pattern is a backfill mechanism for legacy bindings that pre-date the column. **Removal path:** once Slice 2 / Slice 5 add the column AND a `register_routing` API that stamps it at bind time, the «first /switch stamps» path is dead code and the UC-MAT-15-EC1 fallback can be removed in a follow-up cleanup. Tracked in OQ-MAT-UC-1. Salience: medium.

### Symptom-only patches (with root-cause links)

- (none — the meta-level symptom-only patch acknowledged in `.claude/plan.md` line 294 (operator's rollback rather than fix-forward v0.8) is tracked at the PRD §18 level, not at the use-cases level. The use-cases file itself does not introduce a new symptom-only patch.)
