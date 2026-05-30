# Use Cases: Telegram Multi-CLI Orchestration

> Based on [PRD §19](../PRD.md#19-telegram-multi-cli-orchestration--chat-as-id-routing-bot-commands-inline-keyboard-questionnaires-and-plugin-cutover) and [.claude/plan.md](../../.claude/plan.md)

---

## UC-TMC-1: Schema v7 Migration on First Daemon Start with Existing v6 Database

**Actor**: The daemon (automated, triggered at startup)
**Preconditions**:
- `chat.db` exists and is at schema v6 (tables `agent_registry`, `messages`, `chat_sessions`, and other v6 tables present with existing rows)
- Neither `active_cli_per_chat` nor `tg_message_map` tables exist yet
- The daemon binary contains the v7 migration in `ensure_chat_db_schema`

**Trigger**: Daemon process starts; `ensure_chat_db_schema` is called during initialisation

### Primary Flow (Happy Path)
1. Daemon calls `ensure_chat_db_schema(conn)`.
2. Function inspects schema version; detects v6 state (no `active_cli_per_chat` column in `sqlite_master`).
3. Executes `CREATE TABLE IF NOT EXISTS active_cli_per_chat (chat_id INTEGER PRIMARY KEY, active_cli_name TEXT NOT NULL, active_agent_id TEXT NOT NULL, set_at INTEGER NOT NULL, set_by TEXT NOT NULL)`.
4. Executes `CREATE TABLE IF NOT EXISTS tg_message_map (tg_msg_id INTEGER NOT NULL, chat_id INTEGER NOT NULL, sender_agent_id TEXT NOT NULL, sent_at INTEGER NOT NULL, PRIMARY KEY (chat_id, tg_msg_id))`.
5. Both `CREATE TABLE IF NOT EXISTS` statements succeed without error.
6. Daemon continues startup; all pre-existing v6 rows in `agent_registry`, `messages`, etc. are intact (row counts unchanged).

**Postconditions**:
- `active_cli_per_chat` table exists with the five specified columns.
- `tg_message_map` table exists with the four specified columns and composite PK.
- All pre-existing v6 rows survive unchanged.
- `PRAGMA table_info(active_cli_per_chat)` returns exactly: `chat_id`, `active_cli_name`, `active_agent_id`, `set_at`, `set_by`.
- `PRAGMA table_info(tg_message_map)` returns exactly: `tg_msg_id`, `chat_id`, `sender_agent_id`, `sent_at`.

### Alternative Flows

- **UC-TMC-1-A1: Schema already at v7 (idempotent second start)** — both tables already exist. The `CREATE TABLE IF NOT EXISTS` statements execute and return success without error; no rows are dropped or altered. Daemon starts normally. Postconditions match primary flow.
- **UC-TMC-1-A2: Fresh database (no prior schema)** — `chat.db` does not exist at all. `ensure_chat_db_schema` creates the database file and applies all schema versions (v5, v6, v7) in sequence. Both new tables are created alongside all pre-existing table definitions. No pre-existing rows are present to validate.

### Error Flows

- **UC-TMC-1-E1: Disk full during migration** — `CREATE TABLE` fails with SQLite `SQLITE_FULL`. Daemon logs the error and exits non-zero. Operator must free disk space; re-running daemon retries the migration. Database is left in a consistent pre-migration state (SQLite rolls back the statement).
- **UC-TMC-1-E2: Database file locked by another process** — `SQLITE_BUSY`. Daemon logs "chat.db locked; retry after releasing other processes" and exits non-zero.

### Edge Cases

- **UC-TMC-1-EC1**: Migration is applied to a `chat.db` that has never had Telegram messages. Both new tables are created empty; no functional impact on routing until messages arrive.
- **UC-TMC-1-EC2**: `chat.db` exists but the `agent_registry` table (v6) is absent — database is corrupt or from an earlier version. `ensure_chat_db_schema` must apply all intermediate schema steps before v7; if any intermediate step fails, the daemon must exit non-zero with a clear diagnostic rather than leaving the database in a partially-migrated state.

### Data Requirements

- **Input**: Existing `chat.db` at v6 (or absent)
- **Output**: `chat.db` now contains `active_cli_per_chat` and `tg_message_map` tables; all pre-existing rows intact
- **Side Effects**: None beyond SQLite DDL; no external calls

**FR Coverage**: FR-TMC-1.1, FR-TMC-1.2, AC-TMC-1, AC-TMC-2, NFR-TMC-2

---

## UC-TMC-2: `is_alive` Helper — Checking Whether a Specific CLI Is Registered

**Actor**: The daemon (internal, called by the routing tree and bot-command handlers)
**Preconditions**:
- `agent_registry` table exists (v6 schema or later)
- Daemon has a live SQLite connection to `chat.db`

**Trigger**: Any code path that needs to verify whether a named agent_id is currently registered and non-orphaned (e.g., routing tree step 2, reply-quote fallback, `/switch` validation)

### Primary Flow (Happy Path)
1. Caller invokes `is_alive(conn, "agent-id-abc")`.
2. Function queries `agent_registry` for a row with `agent_id = "agent-id-abc"` and a non-orphaned status.
3. Row exists and status is non-orphaned.
4. Returns `true`.

**Postconditions**: Caller receives `true`; database is not modified.

### Alternative Flows

- **UC-TMC-2-A1: Agent not registered** — no row exists for the given `agent_id`. Returns `false`.
- **UC-TMC-2-A2: Agent registered but orphaned** — row exists but status indicates orphaned (e.g., connection lost without clean unregister). Returns `false`.
- **UC-TMC-2-A3: Agent was alive but unregistered between two consecutive calls** — first call returns `true`, second call returns `false`. This is correct; callers must not cache the result across a routing decision.

### Error Flows

- **UC-TMC-2-E1: Database query fails** — `is_alive` propagates the `anyhow::Result` error to the caller. Caller is responsible for treating the error as "cannot confirm alive; treat as dead" and logging a diagnostic.

### Edge Cases

- **UC-TMC-2-EC1**: `agent_id` is an empty string or contains SQL-injection-style special characters. The function uses parameterised queries; no SQL injection is possible. Returns `false` for an empty-string lookup (no such row).
- **UC-TMC-2-EC2**: Called concurrently from two routing decisions at the same instant for the same `agent_id`. SQLite reader concurrency is safe; both calls return the same value (assuming no interleaved write).

### Data Requirements

- **Input**: `agent_id` (string); open SQLite connection
- **Output**: `bool` (wrapped in `anyhow::Result`)
- **Side Effects**: None (read-only query)

**FR Coverage**: FR-TMC-1.4, AC-TMC-3

---

## UC-TMC-3: `first_alive` Helper — Resolving the Default CLI for a Chat

**Actor**: The daemon (internal, called when no explicit binding exists for a chat)
**Preconditions**:
- `agent_registry` table exists
- Zero or more alive agents are registered

**Trigger**: Routing tree step 4 finds no valid binding in `active_cli_per_chat`, or `/whoami` needs the default target

### Primary Flow (Happy Path — prefer_role match)
1. Caller invokes `first_alive(conn, thread=None, prefer_role=Some("orchestrator"))`.
2. Function queries `agent_registry` for alive (non-orphaned) agents.
3. At least one agent whose `agent_name` contains "orchestrator" is found.
4. Returns `Some(AgentRow)` for the first matching orchestrator agent.

**Postconditions**: Caller receives an `AgentRow`; database not modified.

### Alternative Flows

- **UC-TMC-3-A1: No prefer_role match, fallback to any alive** — no alive agent whose name contains "orchestrator" exists. Function returns the first alive agent by any other criteria (e.g., registration order or agent_id). Returns `Some(AgentRow)`.
- **UC-TMC-3-A2: No alive agents at all** — `agent_registry` contains zero non-orphaned rows. Returns `None`.
- **UC-TMC-3-A3: Thread filter specified** — `thread=Some("telegram:111")` narrows the search to agents subscribed to that thread before applying `prefer_role`. If no matching agent is alive on that thread, falls back to any alive agent (or `None` if none exist).

### Error Flows

- **UC-TMC-3-E1: Database query error** — `first_alive` returns `Err(...)`. Caller treats this as no alive CLI and replies "No CLIs online" rather than crashing.

### Edge Cases

- **UC-TMC-3-EC1**: Two agents both have "orchestrator" in their name. Function returns the first one (deterministic ordering; implementation defines the tiebreak, e.g., by registration time or `agent_id` alphabetical order). The QA test MUST confirm deterministic output.
- **UC-TMC-3-EC2**: One CLI is registered but has a status that is borderline (e.g., `reap` was called but the row was not fully removed). `first_alive` must use the same alive-status predicate as `is_alive` for consistency.

### Data Requirements

- **Input**: `conn`, optional `thread`, optional `prefer_role`
- **Output**: `Option<AgentRow>` wrapped in `anyhow::Result`
- **Side Effects**: None (read-only)

**FR Coverage**: FR-TMC-1.5, AC-TMC-3

---

## UC-TMC-4: Free-Text Message in a Chat Bound to a Specific CLI (Chat-as-ID Routing)

**Actor**: Operator on Telegram (human, authenticated via `access.json` allowlist from §17)
**Preconditions**:
- Daemon is running with `[telegram] enabled = true`
- Daemon holds the sole `getUpdates` polling slot (no per-CLI plugin also polling)
- CLI-1 is registered in `agent_registry` with a non-orphaned status; its `agent_id` = "cli-1-id"
- `active_cli_per_chat` has a row: `chat_id = 111, active_agent_id = "cli-1-id"`
- The operator sends a plain-text message (not a bot command, not a reply-to)

**Trigger**: Operator types "What is the status of slice 3?" in Telegram chat 111 and sends

### Primary Flow (Happy Path)
1. Daemon's `getUpdates` loop receives a `message` update with `chat.id = 111`, `text = "What is the status of slice 3?"`.
2. Routing tree step 1: `text` does not start with `/` — not a bot command. Continue.
3. Routing tree step 2: `reply_to_message` is absent — not a reply-quote. Continue.
4. Routing tree step 4: Look up `chat_id = 111` in `active_cli_per_chat`. Row found with `active_agent_id = "cli-1-id"`. Call `is_alive(conn, "cli-1-id")`. Returns `true`.
5. Set `target_agent_id = "cli-1-id"`.
6. Build a channel notification with `meta.target_agent_id = "cli-1-id"` and publish to `ChatBus` on thread `telegram:111`.
7. CLI-1's plugin bridge (subscribed to thread `telegram:111`) receives the notification.
8. CLI-1 processes the message and sends a response via `chat_reply`.

**Postconditions**:
- CLI-1 has received the message; CLI-2 (if registered) has NOT received any notification.
- `chat_id = 222` (a different chat, regardless of its binding) is completely unaffected.
- No routing decision has been logged to any other CLI.

### Alternative Flows

- **UC-TMC-4-A1: No binding for chat_id 111 — default to first_alive** — `active_cli_per_chat` has no row for `chat_id = 111`. Routing tree step 4 calls `first_alive(prefer_role="orchestrator")`. Returns CLI-1 (the only alive agent). Routes to CLI-1 with a note in the internal log that the default was used (no error to the operator; routing proceeds normally).
- **UC-TMC-4-A2: Bound CLI is dead — fall through to first_alive** — `active_cli_per_chat[111]` names `active_agent_id = "cli-dead-id"`, but `is_alive("cli-dead-id")` returns `false`. Routing tree step 4 calls `first_alive(prefer_role="orchestrator")`. If another alive CLI exists, routes to it. Internal log notes the dead-binding fallback.
- **UC-TMC-4-A3: Message arrives in chat_id 222 bound to CLI-2** — entirely independent routing path. CLI-1 does not receive the message; CLI-2 does. Chat-as-ID isolation holds.

### Error Flows

- **UC-TMC-4-E1: No alive CLIs** — `is_alive` false AND `first_alive` returns `None`. Routing tree step 5: daemon sends a Telegram reply to `chat_id = 111`: "No CLIs online. Spawn one with `claudebase run`." No channel notification is published.
- **UC-TMC-4-E2: `ChatBus` publish fails** — daemon logs the error and does NOT send an error reply to the operator (the operator has no visibility into internal bus failures from a routing-only failure; however, the message is lost for this cycle).

### Edge Cases

- **UC-TMC-4-EC1**: Two CLIs are alive; both are bound to different chats; a third chat has no binding. A message in the third chat routes to `first_alive` (the orchestrator-preferred CLI), NOT to the CLI bound to either of the other chats. Chat isolation is absolute.
- **UC-TMC-4-EC2**: Operator sends an empty message (Telegram allows sticker-only or media-only updates with no `text` field). The routing tree still applies; `text` is absent, which is not a bot command. Routes to the bound CLI as a non-text channel notification. The CLI handles the media type; routing logic is unchanged.
- **UC-TMC-4-EC3**: Operator sends a message containing `@bot_username` in the text but it is NOT a reply-quote and NOT a `/command`. Under chat-as-id, `@-mention` routing is no longer used (the `extract_first_mention` precursor is replaced). Routes to the bound CLI per step 4, ignoring the mention.
- **UC-TMC-4-EC4**: `active_cli_per_chat` row exists but `active_agent_id` is an empty string (data corruption). `is_alive("")` returns `false` (no such row). Falls through to `first_alive` as if no binding existed. Daemon logs a warning about the malformed row.

### Data Requirements

- **Input**: Telegram `message` update (chat_id, text, from.id)
- **Output**: Channel notification published to `ChatBus` with `meta.target_agent_id`
- **Side Effects**: None to persistent state; `tg_message_map` is NOT written for inbound messages (only outbound)

**FR Coverage**: FR-TMC-2.1 (steps 1/2/4/5), FR-TMC-2.2, FR-TMC-2.3, AC-TMC-4, AC-TMC-7, NFR-TMC-1

---

## UC-TMC-5: Reply-Quote Routing to the Originating CLI

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- CLI-1 previously sent a Telegram message via `chat_reply`; the daemon recorded it in `tg_message_map` as `(tg_msg_id=9001, chat_id=111, sender_agent_id="cli-1-id", sent_at=<recent>)`
- CLI-1 is still alive (`is_alive("cli-1-id")` returns `true`)

**Trigger**: Operator taps "Reply" on message 9001 in chat 111 and types "Yes, proceed."

### Primary Flow (Happy Path)
1. Daemon receives a `message` update with `chat.id = 111`, `reply_to_message.message_id = 9001`, `text = "Yes, proceed."`.
2. Routing tree step 1: not a bot command. Continue.
3. Routing tree step 2: `reply_to_message` is present. Look up `(chat_id=111, tg_msg_id=9001)` in `tg_message_map`. Row found: `sender_agent_id = "cli-1-id"`. Call `is_alive("cli-1-id")`. Returns `true`. Set `target_agent_id = "cli-1-id"`.
4. Build channel notification with `meta.target_agent_id = "cli-1-id"`, publish to `ChatBus`.
5. CLI-1 receives the reply.

**Postconditions**:
- CLI-1 received the reply; CLI-2 (if registered) did not.
- `tg_message_map` was read but not written.

### Alternative Flows

- **UC-TMC-5-A1: Originating CLI is dead — fallback to active binding** — `tg_message_map` lookup succeeds (`sender_agent_id = "cli-dead-id"`) but `is_alive("cli-dead-id")` returns `false`. Daemon logs a diagnostic: "original sender CLI is no longer alive; falling through to active binding". Routing continues at step 4 (active binding lookup) for `chat_id = 111`. If a binding exists and the bound CLI is alive, routes there. Otherwise `first_alive`. Reply reaches a live CLI; the operator's reply is not lost.
- **UC-TMC-5-A2: Reply-quote to an unknown message_id** — `(chat_id=111, tg_msg_id=8000)` not in `tg_message_map` (message predates 30-day TTL or was never sent by the daemon). Routing tree step 2 finds no row; falls through to step 4. Routes to the active binding for `chat_id = 111` as if it were a free-text message.
- **UC-TMC-5-A3: Daemon was restarted since the original message** — `tg_message_map` is persisted in `chat.db`; the row survives the restart. Reply-quote routing works exactly as in the primary flow.

### Error Flows

- **UC-TMC-5-E1: `tg_message_map` query fails (database error)** — daemon logs the error, falls through to step 4 (active binding). Routing degrades gracefully rather than dropping the message.

### Edge Cases

- **UC-TMC-5-EC1**: Operator reply-quotes a message in chat 222 that was sent by CLI-2. `tg_message_map` row has `chat_id=222`. Routing correctly uses the `chat_id=222` key; CLI-2 receives the reply. CLI-1 is not affected. Chat isolation holds within reply-quote routing.
- **UC-TMC-5-EC2**: Two CLIs both sent messages in the same chat; operator reply-quotes CLI-2's message. `tg_message_map` lookup returns `sender_agent_id = "cli-2-id"`. Routes to CLI-2 specifically, even if the active binding for `chat_id = 111` is CLI-1.
- **UC-TMC-5-EC3**: Message in `tg_message_map` has `sent_at` exactly at the 30-day TTL boundary (within 1 second). The purge task uses `<` (strictly older than 30 days); the boundary-at-exactly-30-days row is retained until the next purge cycle. No reply-quote routing is lost at the boundary.

### Data Requirements

- **Input**: Telegram `message` update with `reply_to_message.message_id`
- **Output**: Channel notification to `ChatBus`; optional diagnostic log entry
- **Side Effects**: None (read-only path)

**FR Coverage**: FR-TMC-2.1 (step 2), AC-TMC-5, AC-TMC-6

---

## UC-TMC-6: Outbound Message Tracking (Recording Daemon-Proxied Messages)

**Actor**: A bound CLI instance (sends a reply via `chat_reply` MCP tool)
**Preconditions**:
- Daemon is running with Telegram enabled
- CLI-1 is registered and bound to `chat_id = 111`
- Daemon holds the sole `getUpdates` slot

**Trigger**: CLI-1 calls `chat_reply` with `thread = "telegram:111"` and a text body

### Primary Flow (Happy Path)
1. CLI-1's `chat_reply` MCP call arrives at the daemon's `server.rs` dispatch.
2. Daemon routes the outbound message through `enqueue_outbound_tg(chat_id=111, text="Here is my response")`.
3. Daemon calls Telegram's `sendMessage` API with `chat_id = 111`.
4. `sendMessage` returns HTTP 200 with a response body containing `result.message_id = 9001`.
5. Daemon inserts into `tg_message_map`: `(tg_msg_id=9001, chat_id=111, sender_agent_id="cli-1-id", sent_at=<current_unix_seconds>)`.
6. Daemon returns success to CLI-1.

**Postconditions**:
- Row `(9001, 111, "cli-1-id", <ts>)` exists in `tg_message_map`.
- `SELECT sender_agent_id FROM tg_message_map WHERE chat_id=111 AND tg_msg_id=9001` returns `"cli-1-id"`.

### Alternative Flows

- **UC-TMC-6-A1: Multiple CLIs send messages to the same chat** — CLI-1 sends message 9001 (recorded with `sender_agent_id="cli-1-id"`), CLI-2 sends message 9002 (recorded with `sender_agent_id="cli-2-id"`). Both rows coexist in `tg_message_map` under the same `chat_id`. Reply-quote routing distinguishes them correctly by `tg_msg_id`.
- **UC-TMC-6-A2: `sendMessage` succeeds but the recording INSERT uses `INSERT OR IGNORE`** — on a retry after a transient failure, the `tg_msg_id` already exists in `tg_message_map` (primary key: `(chat_id, tg_msg_id)`). The `INSERT OR IGNORE` silently skips the duplicate insert. Exactly one row exists. No data is corrupted.

### Error Flows

- **UC-TMC-6-E1: `sendMessage` API call fails (network error, Telegram 5xx)** — daemon does NOT insert into `tg_message_map` (there is no `tg_msg_id` to record). Returns an error to CLI-1. No partial row is created.
- **UC-TMC-6-E2: `sendMessage` succeeds but the INSERT into `tg_message_map` fails (disk full)** — daemon logs the error. The message was delivered to Telegram but the routing map entry is absent. A subsequent reply-quote to this `tg_msg_id` will fall through to the active binding (UC-TMC-5-A2). This is an acceptable degradation; the message is not lost, only reply-quote tracking is impaired.

### Edge Cases

- **UC-TMC-6-EC1**: CLI sends a message with `reply_markup` (for `chat_ask`, Slice 5). The `enqueue_outbound_tg` path records the returned `tg_msg_id` into `tg_message_map` exactly as for plain text messages. The presence of `reply_markup` does not change the recording logic.
- **UC-TMC-6-EC2**: Daemon restarts immediately after `sendMessage` succeeds but before the INSERT. The row is never recorded. This is an acceptable gap (daemon restart is not atomic with API calls); the operator can still reply-quote but routing will fall through to the active binding.

### Data Requirements

- **Input**: CLI's `chat_reply` MCP call (`thread`, text body); Telegram `sendMessage` API response
- **Output**: Row inserted in `tg_message_map`
- **Side Effects**: `tg_message_map` write; Telegram API `sendMessage` call

**FR Coverage**: FR-TMC-4.1, FR-TMC-4.2, FR-TMC-4.3, AC-TMC-12, AC-TMC-5

---

## UC-TMC-7: TTL Purge of Stale Reply-Quote Map Entries

**Actor**: The daemon (automated background task)
**Preconditions**:
- `tg_message_map` contains rows with various `sent_at` timestamps
- Some rows have `sent_at < (current_unix_seconds - 2592000)` (older than 30 days)
- Other rows have `sent_at >= (current_unix_seconds - 2592000)` (within 30 days)

**Trigger**: Daemon startup (and periodically thereafter per FR-TMC-1.3)

### Primary Flow (Happy Path)
1. Purge task calculates the cutoff: `current_unix_seconds - 2592000`.
2. Executes: `DELETE FROM tg_message_map WHERE sent_at < <cutoff>`.
3. Rows older than 30 days are deleted; rows within 30 days survive.
4. Purge task logs the number of rows deleted (or "0 rows purged" if none expired).

**Postconditions**:
- All rows with `sent_at` more than 30 days ago are absent.
- All rows with `sent_at` within 30 days are present and unchanged.
- Reply-quote routing for recent messages continues to work.

### Alternative Flows

- **UC-TMC-7-A1: No rows are expired** — `DELETE` affects 0 rows. Task logs "0 rows purged". No functional change.
- **UC-TMC-7-A2: All rows are expired** — `DELETE` removes all rows. `tg_message_map` is empty. Reply-quote routing for all future reply-quotes falls through to active binding (UC-TMC-5-A2).

### Error Flows

- **UC-TMC-7-E1: Purge fails due to database lock** — daemon logs the error and schedules the next purge attempt at the next cycle. The stale rows remain; no data corruption occurs.

### Edge Cases

- **UC-TMC-7-EC1**: Row has `sent_at` exactly equal to `(current_unix_seconds - 2592000)`. The condition is strictly `<` so this row is NOT deleted. It will be deleted on the next purge cycle 1 second later. Acceptable — no message routing is impacted.
- **UC-TMC-7-EC2**: Clock skew causes `current_unix_seconds` to appear smaller than the actual time (e.g., NTP correction). Rows that appear "not yet expired" by the skewed clock are retained longer. This is acceptable (they expire on the next correct-time purge).

### Data Requirements

- **Input**: Current Unix timestamp; `tg_message_map` rows
- **Output**: Rows older than 30 days deleted
- **Side Effects**: `tg_message_map` rows deleted (irreversible for that cycle)

**FR Coverage**: FR-TMC-1.3, AC-TMC-13

---

## UC-TMC-8: `/agents` Bot Command — List Alive CLI Instances

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- At least one CLI is registered in `agent_registry`

**Trigger**: Operator sends `/agents` in any Telegram chat served by the daemon

### Primary Flow (Happy Path)
1. Daemon receives a `message` update with `text = "/agents"`.
2. Routing tree step 1: text starts with `/`; matches `/agents`. Bot-command path activated.
3. Daemon calls `list_alive(conn, thread=None)`.
4. Returns a list of alive agents (e.g., `[{agent_name: "mira", agent_id: "cli-1-id"}, {agent_name: "worker", agent_id: "cli-2-id"}]`).
5. Daemon formats and sends a Telegram reply to the originating `chat_id`: e.g., "Alive CLIs:\n• mira (cli-1-id)\n• worker (cli-2-id)".
6. Routing tree terminates. No channel notification is sent to any CLI.

**Postconditions**:
- Operator receives a formatted list of alive CLIs.
- No row is written to `active_cli_per_chat`.
- No CLI receives a channel notification.

### Alternative Flows

- **UC-TMC-8-A1: No CLIs alive** — `list_alive` returns an empty list. Reply: "No CLIs currently online."
- **UC-TMC-8-A2: `/agents` sent in a group chat** — chat_id is a negative integer (Telegram group). Routing is identical; `list_alive` returns the same global list regardless of chat type. Reply is sent to the group chat.

### Error Flows

- **UC-TMC-8-E1: `list_alive` database query fails** — daemon logs the error and sends: "Error retrieving CLI list. Please try again." to the operator. Does not crash.
- **UC-TMC-8-E2: Telegram `sendMessage` for the reply fails** — daemon logs the error. No retry for bot-command replies (operator can re-send the command).

### Edge Cases

- **UC-TMC-8-EC1**: Ten or more CLIs are alive. Reply lists all of them; no truncation is mandated by the PRD but the response must not exceed Telegram's message length limit (4096 characters). If the formatted list would exceed 4096 characters, the daemon MUST split into multiple messages or truncate with a "..." note.
- **UC-TMC-8-EC2**: `/agents` contains trailing whitespace (e.g., `/agents `). Must still be matched and handled as `/agents`.

### Data Requirements

- **Input**: Telegram `message` update with text `/agents`
- **Output**: Telegram `sendMessage` reply listing alive CLIs
- **Side Effects**: None (read-only; no state changes)

**FR Coverage**: FR-TMC-3.1, AC-TMC-8, AC-TMC-21

---

## UC-TMC-9: `/switch <name>` Bot Command — Rebinding a Chat to a Named CLI

**Actor**: Operator on Telegram (any participant in the chat, since binding is chat-level)
**Preconditions**:
- Daemon is running with Telegram enabled
- CLI "mira" is registered and alive in `agent_registry`

**Trigger**: Operator sends `/switch mira` in Telegram chat 111

### Primary Flow (Happy Path)
1. Daemon receives `message` with `text = "/switch mira"`, `chat.id = 111`, `from.id = 99999`, `from.username = "operator"`.
2. Routing tree step 1: matches `/switch`. Bot-command path activated.
3. Parse `<name>` = "mira" from the text after `/switch `.
4. Call `list_alive(conn, thread=None)`. Find the agent row where `agent_name = "mira"`. Agent is alive; `agent_id = "cli-1-id"`.
5. Execute `INSERT OR REPLACE INTO active_cli_per_chat (chat_id, active_cli_name, active_agent_id, set_at, set_by) VALUES (111, "mira", "cli-1-id", <current_unix_seconds>, "99999")`.
6. Send Telegram reply to `chat_id = 111`: "Chat 111 is now bound to CLI 'mira'. (Note: in a group chat this rebinds for all participants.)"
7. Routing tree terminates. No CLI receives a channel notification.

**Postconditions**:
- `active_cli_per_chat[chat_id=111]` row exists with `active_cli_name="mira"`, `active_agent_id="cli-1-id"`, `set_by="99999"`.
- Subsequent free-text messages in chat 111 route to CLI "mira".
- Any previous binding for `chat_id = 111` is overwritten atomically (INSERT OR REPLACE).
- No CLI receives a channel notification from the `/switch` command itself.

### Alternative Flows

- **UC-TMC-9-A1: Rebinding from one alive CLI to another** — `active_cli_per_chat[111]` previously held `active_cli_name="worker"`. After `/switch mira`, the row is replaced. Old binding is gone; new binding is active. Both CLIs remain alive.
- **UC-TMC-9-A2: `/switch` in a group chat** — `chat.id` is a negative integer. Routing logic is identical; the binding is set for the group `chat_id`. Reply explicitly mentions group-rebind semantics: "This rebinds the entire group chat for all participants."
- **UC-TMC-9-A3: `/switch` with no argument** — `text = "/switch"` with no `<name>`. Daemon replies: "Usage: /switch <cli-name>. Available CLIs: mira, worker."

### Error Flows

- **UC-TMC-9-E1: Named CLI not in `list_alive`** — "mira" does not appear as an alive agent. Daemon replies: "Unknown CLI: 'mira'. Available CLIs: worker, runner. (Use /agents to see all alive CLIs.)" Does not write to `active_cli_per_chat`.
- **UC-TMC-9-E2: Named CLI was alive at step 4 but unregistered before the INSERT** — TOCTOU window. INSERT OR REPLACE writes the binding with an `active_agent_id` that is no longer valid. The next routing decision will call `is_alive` on that `agent_id`, find it dead, and fall through to `first_alive`. The stale binding persists until overwritten by a future `/switch` or a cleanup pass. Log a warning.
- **UC-TMC-9-E3: `INSERT OR REPLACE` fails (database error)** — daemon logs and sends: "Error saving binding. Please try again."

### Edge Cases

- **UC-TMC-9-EC1**: `/switch` is executed by a participant in a group chat who is NOT the operator. Binding is still applied (chat-as-id, any participant can rebind the group's CLI). This is intentional per PRD §19.10 #6 and §19.9 #5. The reply note informs the group.
- **UC-TMC-9-EC2**: `/switch <name>` where `<name>` matches a partial substring of an agent's name but not exactly (e.g., `/switch mir` when the agent is `mira`). The daemon uses an exact match on `agent_name`; partial match is rejected with the "Unknown CLI" error (UC-TMC-9-E1), listing available names including "mira".
- **UC-TMC-9-EC3**: Agent name contains spaces (e.g., `/switch my worker`). `agent_name` validation from §17 `validate_agent_name` presumably rejects spaces; if so, no such agent can exist. Document that `<name>` is the exact `agent_name` as registered. If the validator allows spaces, the parser takes everything after the first space as the name.

### Data Requirements

- **Input**: Telegram `message` update (`text`, `chat.id`, `from.id`/`from.username`)
- **Output**: Row in `active_cli_per_chat` (upserted); Telegram reply
- **Side Effects**: `active_cli_per_chat` write (atomic upsert); routing for `chat_id` changes immediately

**FR Coverage**: FR-TMC-3.2, AC-TMC-9, AC-TMC-21, NFR-TMC-1

---

## UC-TMC-10: `/whoami` Bot Command — Showing the Chat's Bound CLI

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled

**Trigger**: Operator sends `/whoami` in Telegram chat 111

### Primary Flow (Happy Path — explicit binding exists)
1. Daemon receives `text = "/whoami"`, `chat.id = 111`.
2. Routing tree step 1: matches `/whoami`. Bot-command path.
3. Look up `active_cli_per_chat[chat_id=111]`. Row found: `active_cli_name="mira"`, `active_agent_id="cli-1-id"`.
4. Send reply: "Chat 111 is bound to CLI 'mira' (agent_id: cli-1-id)."
5. Routing terminates. No CLI receives a notification.

**Postconditions**:
- Operator sees the bound CLI name and agent_id.
- No state changes.

### Alternative Flows

- **UC-TMC-10-A1: No explicit binding — show default** — `active_cli_per_chat` has no row for `chat_id = 111`. Call `first_alive(prefer_role="orchestrator")`. Returns `{agent_name="mira", agent_id="cli-1-id"}`. Reply: "No explicit binding set for chat 111. Default CLI: 'mira' (agent_id: cli-1-id) — use /switch <name> to bind."
- **UC-TMC-10-A2: Binding exists but bound CLI is dead** — row exists but `is_alive("cli-dead-id")` returns `false`. Reply: "Chat 111 was bound to 'dead-cli' (now offline). Routing will fall back to the default CLI. Use /switch to rebind."
- **UC-TMC-10-A3: No binding and no alive CLI** — `first_alive` returns `None`. Reply: "No explicit binding and no CLIs are currently online. Spawn one with `claudebase run`."

### Error Flows

- **UC-TMC-10-E1: Database query fails** — daemon logs error and replies: "Error retrieving binding information."

### Edge Cases

- **UC-TMC-10-EC1**: `/whoami` in a group chat. Reports the group's chat-level binding, not any per-user binding (there is no per-user binding). Reply clarifies that all participants share this binding.

### Data Requirements

- **Input**: Telegram `message` with `/whoami`, `chat.id`
- **Output**: Telegram reply describing the current binding or default
- **Side Effects**: None (read-only)

**FR Coverage**: FR-TMC-3.3, AC-TMC-10, AC-TMC-21

---

## UC-TMC-11: `/here` Bot Command — Showing the Bound CLI's Host and Working Directory

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- Chat 111 is bound to CLI "mira" (agent_id = "cli-1-id")
- CLI "mira"'s `AgentRow` in `agent_registry` has `host = "devbox"` and `cwd = "/home/operator/project"`

**Trigger**: Operator sends `/here` in chat 111

### Primary Flow (Happy Path)
1. Daemon receives `/here`, `chat.id = 111`.
2. Routing tree step 1: matches `/here`.
3. Resolve bound CLI: `active_cli_per_chat[111]` → `active_agent_id = "cli-1-id"`.
4. Look up `AgentRow` in `agent_registry` for `agent_id = "cli-1-id"`. Row found with `host = "devbox"`, `cwd = "/home/operator/project"`.
5. Send reply: "CLI 'mira' is running on devbox at /home/operator/project."
6. Routing terminates.

**Postconditions**:
- Operator receives host and cwd of the bound CLI.
- No state changes.

### Alternative Flows

- **UC-TMC-11-A1: No explicit binding** — fall back to `first_alive(prefer_role="orchestrator")` to resolve the default CLI, then look up its `host`/`cwd`. Reply notes that no explicit binding exists.
- **UC-TMC-11-A2: `host` or `cwd` absent in `agent_registry`** — metadata fields are absent or empty. Reply: "CLI 'mira' is online but host/working-directory information is unavailable."
- **UC-TMC-11-A3: Bound CLI not found in `agent_registry`** (e.g., was reaped between `/switch` and `/here`). Reply: "CLI 'mira' is no longer online. Use /switch to rebind or /agents to see alive CLIs."

### Error Flows

- **UC-TMC-11-E1: Database query fails** — daemon logs error and replies: "Error retrieving CLI location information."

### Edge Cases

- **UC-TMC-11-EC1**: `cwd` contains a very long path (e.g., 500 characters). Reply must not exceed Telegram's 4096-character message limit; truncation with `...` if needed.
- **UC-TMC-11-EC2**: `host` contains special Telegram markdown characters (e.g., `_`, `*`). Daemon must escape the reply appropriately for the parse mode in use, or send as plain text to avoid rendering issues.

### Data Requirements

- **Input**: Telegram `/here` message, `chat.id`
- **Output**: Telegram reply with `host:cwd`
- **Side Effects**: None (read-only)

**FR Coverage**: FR-TMC-3.4, AC-TMC-11, AC-TMC-21

---

## UC-TMC-12: Existing Bot Commands Preserved — `/start`, `/help`, `/status`

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- Existing `/start`, `/help`, `/status` handlers are present in `src/daemon/telegram.rs`

**Trigger**: Operator sends `/start`, `/help`, or `/status`

### Primary Flow (Happy Path)
1. Daemon receives one of `/start`, `/help`, `/status`.
2. Routing tree step 1 matches the command.
3. The pre-existing handler runs unchanged.
4. `/help` reply now includes the four new commands (`/agents`, `/switch`, `/whoami`, `/here`) and a note that `/switch` in group chats rebinds for all participants.
5. No CLI receives a channel notification.

**Postconditions**:
- Operator receives the existing response behaviour (unchanged, except `/help` text now lists new commands).
- No state changes.

### Alternative Flows

- **UC-TMC-12-A1: `/help` updated text** — the `/help` reply includes documentation of all seven commands: `/agents`, `/switch <name>`, `/whoami`, `/here`, `/start`, `/help`, `/status`. The entry for `/switch` explicitly notes group-chat rebind semantics.

### Error Flows

- **UC-TMC-12-E1: Handler for `/status` fails due to missing daemon sub-state** — preserves existing behavior: logs error, replies with partial status. This use case does not change error handling of pre-existing commands.

### Edge Cases

- **UC-TMC-12-EC1**: Operator sends `/help@botusername` (Telegram group command with bot name suffix). Must be matched as `/help`. Routing tree must strip or ignore the `@botusername` suffix.

### Data Requirements

- **Input**: Telegram command message
- **Output**: Telegram reply (pre-existing behavior + updated /help text)
- **Side Effects**: None

**FR Coverage**: FR-TMC-3.5, FR-TMC-3.6

---

## UC-TMC-13: `chat_ask` — Agent Sends a Multiple-Choice Question as Inline Keyboard Buttons

**Actor**: A bound CLI instance (specifically an agent like Mira in plan mode)
**Preconditions**:
- Daemon is running with Telegram enabled
- CLI-1 is alive and registered
- Chat 111 is bound to CLI-1
- `chat_ask` is in `TOOL_WHITELIST` in `src/plugin/mcp.rs`
- `chat_ask` appears in the daemon's `tools/list` response

**Trigger**: CLI-1's agent code calls the `chat_ask` MCP tool with `thread="telegram:111"`, `question="Which approach do you prefer?"`, `options=[{label:"A: Simple"}, {label:"B: Robust"}, {label:"C: Defer"}]`

### Primary Flow (Happy Path)
1. `chat_ask` MCP call arrives at daemon `server.rs` dispatch.
2. Daemon generates a compact `question_id` (e.g., a short UUID prefix or monotonic counter rendered as decimal; NOT the full question text).
3. Constructs `callback_data` strings: `"<qid>:0"`, `"<qid>:1"`, `"<qid>:2"`. Each MUST be no greater than 64 bytes in length.
4. Calls Telegram `sendMessage` to `chat_id = 111` with `text = "Which approach do you prefer?"` and `reply_markup.inline_keyboard = [[{text:"A: Simple", callback_data:"<qid>:0"}], [{text:"B: Robust", callback_data:"<qid>:1"}], [{text:"C: Defer", callback_data:"<qid>:2"}]]` (one button per row, or layout at implementer discretion).
5. `sendMessage` returns HTTP 200 with `result.message_id = 9002`. Daemon records `(9002, 111, "cli-1-id", <ts>)` in `tg_message_map`.
6. Daemon registers the pending `question_id → CLI-1 callback correlation` (internal mechanism, sync or async — architect decision; see UC-TMC-14 for the answer-arrival flow).
7. `chat_ask` MCP tool call is pending (or returns `question_id` if async).

**Postconditions**:
- Telegram chat 111 displays the question text with 3 inline keyboard buttons.
- A `tg_message_map` row exists for `tg_msg_id = 9002`.
- A pending correlation entry exists mapping `question_id → CLI-1`.

### Alternative Flows

- **UC-TMC-13-A1: `options` has the minimum count (2 options)** — 2 buttons are rendered. Primary flow unchanged.
- **UC-TMC-13-A2: `options` has a `description` field** — the `description` field may be rendered as additional context (tooltip or appended to button text, at implementer discretion). The `label` is the button text; `description` is supplemental. The PRD mandates `label` is required; `description` is optional.
- **UC-TMC-13-A3: `thread` resolves to a chat with no current binding** — `chat_id` is derived from the `thread` string (`"telegram:111"` → `chat_id = 111`). No binding check is needed for `chat_ask` delivery — the message goes to `chat_id 111` unconditionally. The correlation maps to CLI-1 (the caller), not to the chat's binding.

### Error Flows

- **UC-TMC-13-E1: `callback_data` would exceed 64 bytes** — compact `question_id` design must prevent this. If the implementer's `question_id` + `:` + `option_idx` string exceeds 64 bytes, the `chat_ask` call MUST fail with a clear internal error before any `sendMessage` is issued. The MCP call returns an error to the agent.
- **UC-TMC-13-E2: `sendMessage` API call fails** — daemon returns an error to CLI-1. No `tg_message_map` row is created. The agent may retry.
- **UC-TMC-13-E3: `options` has fewer than 2 items** — the `inputSchema` has `minItems: 2`; the daemon rejects the call with a tool-level validation error before any Telegram call is made.
- **UC-TMC-13-E4: `thread` is malformed (not `telegram:<chat_id>` pattern)** — daemon returns a validation error immediately.

### Edge Cases

- **UC-TMC-13-EC1**: Agent calls `chat_ask` while another `chat_ask` is still pending for the same chat. The daemon must either queue the second question or reject it with an "a question is already pending" error, depending on the correlation mechanism chosen by the architect. QA must verify that the second question does not corrupt the first correlation.
- **UC-TMC-13-EC2**: Option labels contain Telegram Markdown characters (e.g., `*bold*`). The `text` field of an `InlineKeyboardButton` is plain text; Markdown in button labels is not rendered. Daemon must sanitise or leave as-is (Telegram's API handles it as literal text).
- **UC-TMC-13-EC3**: Question text is very long (> 200 characters). Telegram imposes a `sendMessage` text limit of 4096 characters total; `question` text alone is unlikely to exceed it, but must be validated.

### Data Requirements

- **Input**: MCP call with `thread`, `question`, `options` array
- **Output**: Telegram `sendMessage` with `reply_markup.inline_keyboard`; `tg_message_map` row; pending correlation entry
- **Side Effects**: Telegram API call; `tg_message_map` write; internal correlation state

**FR Coverage**: FR-TMC-5.2, FR-TMC-5.3, FR-TMC-5.4, FR-TMC-5.6, FR-TMC-5.7, AC-TMC-14, AC-TMC-16, NFR-TMC-4

---

## UC-TMC-14: Operator Taps an Inline Keyboard Button — Answer Routed Back to the CLI

**Actor**: Operator on Telegram (taps one of the inline keyboard buttons rendered by UC-TMC-13)
**Preconditions**:
- UC-TMC-13 primary flow completed successfully: inline keyboard is displayed in chat 111
- `question_id = "q7a"` is pending; correlation maps `"q7a" → CLI-1 (cli-1-id)`
- CLI-1 is still alive

**Trigger**: Operator taps "B: Robust" (index 1)

### Primary Flow (Happy Path)
1. Telegram delivers a `callback_query` update to the daemon's `getUpdates` poll. The update contains:
   - `callback_query.id = "cq-xyz"` (the callback_query identifier)
   - `callback_query.data = "q7a:1"` (the encoded `question_id:option_index`)
   - `callback_query.message.chat.id = 111`
2. Daemon's `process_batch` handles the `callback_query` branch (new in this feature).
3. Daemon calls Telegram `answerCallbackQuery(callback_query_id="cq-xyz")` to dismiss the loading spinner on the operator's device.
4. Daemon decodes `callback_data`: `question_id = "q7a"`, `option_index = 1`.
5. Daemon looks up the correlation: `"q7a"` → `CLI-1 (cli-1-id)`.
6. Daemon routes the answer (`index=1, label="B: Robust"`) to CLI-1. For sync correlation: the blocked `chat_ask` MCP call returns `{index: 1, label: "B: Robust"}`. For async correlation: a channel notification carries the answer to CLI-1's subscription.
7. CLI-1 receives and processes the answer.

**Postconditions**:
- CLI-1 has received the answer `{index: 1, label: "B: Robust"}`.
- `answerCallbackQuery` was called; loading spinner is dismissed on the operator's device.
- The correlation entry for `"q7a"` is cleaned up (no longer pending).
- CLI-2 (if registered) did not receive the answer.

### Alternative Flows

- **UC-TMC-14-A1: Operator taps option A (index 0)** — `callback_data = "q7a:0"`; answer `{index: 0, label: "A: Simple"}` routed to CLI-1. Same flow with different data.

### Error Flows

- **UC-TMC-14-E1: `callback_query.data` is malformed (no colon, or non-numeric option_index)** — daemon logs the error and calls `answerCallbackQuery` with an error notification text ("Sorry, this response could not be processed."). Does not crash. The correlation remains pending (or times out).
- **UC-TMC-14-E2: `question_id` not found in correlation map (stale or already-answered question)** — daemon calls `answerCallbackQuery` with an informational text ("This question has already been answered or expired.") to dismiss the spinner. Does not route the answer to any CLI. See UC-TMC-14-EC1.
- **UC-TMC-14-E3: `answerCallbackQuery` Telegram API call fails** — spinner persists on operator's device. Daemon logs the error. Answer routing proceeds regardless (the answer delivery is independent of spinner dismissal).
- **UC-TMC-14-E4: CLI-1 dies between UC-TMC-13 and button tap** — correlation map holds `cli-1-id` but `is_alive("cli-1-id")` returns `false`. Daemon logs a diagnostic: "Answer arrived but target CLI is no longer alive; answer dropped." Calls `answerCallbackQuery` with a note. Answer is not delivered (no live recipient).

### Edge Cases

- **UC-TMC-14-EC1**: Operator taps the same button twice (network retry or double-tap). Second `callback_query` arrives with the same `callback_query.id` OR a different `callback_query.id` but the same `data`. On the second arrival: `question_id = "q7a"` is no longer in the correlation map (cleaned up in step 7 of primary flow). Falls into UC-TMC-14-E2 — spinner dismissed with "already answered." CLI-1 is NOT sent a duplicate answer.
- **UC-TMC-14-EC2**: `callback_data` string is exactly 64 bytes. Valid; no truncation. Primary flow handles it.
- **UC-TMC-14-EC3**: `callback_data` would be >64 bytes if it were generated — prevented at `chat_ask` time (UC-TMC-13-E1). This edge case cannot arise at answer time if the send was guarded.
- **UC-TMC-14-EC4**: Multiple inline keyboard questions are sent in the same chat (UC-TMC-13-EC1, if the architect allows queuing). Each `callback_query.data` encodes a distinct `question_id`; the daemon matches each to its own correlation. Answers are delivered independently and in the order received.

### Data Requirements

- **Input**: Telegram `callback_query` update (`id`, `data`, `message.chat.id`)
- **Output**: `answerCallbackQuery` API call; answer delivered to CLI-1 via MCP or channel notification
- **Side Effects**: Correlation entry cleaned up; Telegram `answerCallbackQuery` API call

**FR Coverage**: FR-TMC-5.1, FR-TMC-5.4, AC-TMC-15, NFR-TMC-4

---

## UC-TMC-15: Daemon Poller 409 Conflict — Legacy Plugin Still Running

**Actor**: The daemon (automated, on first `getUpdates` call after startup)
**Preconditions**:
- `[telegram] enabled = true` in `daemon.toml`
- The per-CLI plugin (`plugins/telegram-rs/`) is actively polling `getUpdates` and holds the bot token's single-consumer slot
- Daemon starts (or restarts) and attempts to poll

**Trigger**: Daemon calls `getUpdates` for the first time after startup

### Primary Flow (Happy Path — conflict detected and surfaced)
1. Daemon calls Telegram `getUpdates` (long-poll, 30-second timeout).
2. Telegram returns HTTP 409 Conflict (the per-CLI plugin holds the polling slot).
3. Daemon logs a clear operator-readable error message containing "409" and a description such as: "Telegram daemon poller received 409 Conflict: the legacy telegram-plugin-rs poller is still running. Stop it before enabling the daemon poller."
4. Daemon stops polling attempts (does NOT retry in a tight loop, does NOT crash).
5. Daemon process remains alive and responsive on the UDS socket for all other capabilities (MCP dispatch, `ChatBus`, `agent_registry`, etc.).
6. Telegram routing is inoperative until the conflict is resolved, but no other daemon functionality is impaired.

**Postconditions**:
- Daemon is running but Telegram poller is stopped.
- Operator can see a clear log message explaining the conflict.
- UDS socket is still accepting CLI connections and MCP calls.
- No silent dual-polling occurs.

### Alternative Flows

- **UC-TMC-15-A1: Plugin stops, daemon takes over cleanly** — after the 409, the operator stops the per-CLI plugin. Operator restarts the daemon (or the conflict gate allows retry on restart). Daemon polls `getUpdates` successfully; Telegram routing is operational. See AC-TMC-18.
- **UC-TMC-15-A2: 409 occurs on a subsequent poll (not just first)** — daemon log repeats the conflict message on each 409 response. Does not crash. Same stop-polling behavior.

### Error Flows

- **UC-TMC-15-E1: 409 response body is non-standard** — daemon still detects the HTTP 409 status code; the log message is produced. Body parsing failure is not fatal.
- **UC-TMC-15-E2: Network error (not 409)** — distinguished from 409. Daemon logs a generic network error; retry logic (if any) applies per existing `getUpdates` error handling. This is not a conflict-gate event.

### Edge Cases

- **UC-TMC-15-EC1**: Daemon starts before the plugin has fully started. `getUpdates` succeeds (daemon gets the slot first). Plugin then starts, receives 409, and must log its own conflict error. This is the plugin's responsibility, not the daemon's. The daemon continues polling normally.
- **UC-TMC-15-EC2**: Both daemon and plugin are stopped simultaneously, then both restart simultaneously. Race condition — whichever calls `getUpdates` first wins the slot; the other receives 409. Non-deterministic ordering is acceptable; the conflict gate in whichever loses will log the error.

### Data Requirements

- **Input**: HTTP 409 response from Telegram `getUpdates`
- **Output**: Operator-readable log entry; Telegram poller stopped
- **Side Effects**: None (poller ceases; UDS remains open)

**FR Coverage**: FR-TMC-6.1, FR-TMC-6.2, FR-TMC-6.3, AC-TMC-17, NFR-TMC-3, NFR-TMC-5

---

## UC-TMC-16: Plugin Cutover — Operator Migrates from Per-CLI Plugin to Daemon Poller

**Actor**: Operator (performs the cutover manually following documented steps)
**Preconditions**:
- The per-CLI plugin (`plugins/telegram-rs/`) is the active Telegram receiver
- Daemon is installed and started with `[telegram] enabled = true`
- Daemon is in the 409-conflict state (UC-TMC-15 primary flow occurred)

**Trigger**: Operator stops the per-CLI plugin and restarts the daemon

### Primary Flow (Happy Path)
1. Operator stops the per-CLI plugin (e.g., kills the plugin process or sets the CLI config to disable the Telegram plugin).
2. Operator restarts the daemon (or the daemon's conflict gate allows an automatic retry after the next startup).
3. Daemon calls `getUpdates`; no 409 is returned (plugin no longer holds the slot).
4. Daemon begins receiving and routing Telegram messages via the 5-step routing tree.
5. Operator verifies via a test message: the message arrives routed via the daemon (`source="claudebase"`), not the plugin (`source="plugin:telegram:telegram"`).
6. Daemon owns the `getUpdates` slot from this point forward.

**Postconditions**:
- Daemon is the sole Telegram receiver.
- All new Telegram messages are routed by the daemon's routing tree.
- The per-CLI plugin no longer receives Telegram updates (even if restarted, it receives 409).

### Alternative Flows

- **UC-TMC-16-A1: Revert — operator re-enables plugin path** — operator sets `[telegram] enabled = false` in `daemon.toml` and restarts the daemon. Daemon does NOT start the Telegram poller. Operator starts the per-CLI plugin. Plugin receives `getUpdates` slot and begins receiving messages. Source label on messages returns to `"plugin:telegram:telegram"`. No code changes required.

### Error Flows

- **UC-TMC-16-E1: Plugin cannot be stopped (process is stuck)** — operator must kill the plugin process. Daemon restarts with the plugin still running; 409 recurs. Operator must resolve the stuck plugin.
- **UC-TMC-16-E2: `[telegram] enabled` toggled incorrectly** — if `enabled` is set to `false` and the daemon is restarted, the daemon does NOT poll (correct revert). If `enabled` is set to `true` without stopping the plugin, 409 recurs (correct conflict detection).

### Edge Cases

- **UC-TMC-16-EC1**: Operator sets `[telegram] enabled = false` via `daemon.toml` while the daemon is running (hot-config-reload). Whether the daemon picks up the config change without restart is an implementation detail not mandated by the PRD. If hot-reload is NOT supported, the operator must restart the daemon for the flag to take effect.

### Data Requirements

- **Input**: `daemon.toml` `[telegram] enabled` flag; operator action to stop the plugin
- **Output**: Daemon owns the `getUpdates` slot; Telegram messages route via daemon
- **Side Effects**: Telegram polling slot ownership changes

**FR Coverage**: FR-TMC-6.1, FR-TMC-6.4, FR-TMC-6.5, AC-TMC-18, AC-TMC-19, NFR-TMC-5

---

## UC-TMC-17: Installer Stops Auto-Patching the Per-CLI Telegram Plugin

**Actor**: A new user (runs `install.sh` or `install.ps1` after this feature lands)
**Preconditions**:
- The user is performing a fresh install of claudebase
- The official Anthropic Telegram plugin is present or will be installed

**Trigger**: User runs `bash install.sh --yes`

### Primary Flow (Happy Path)
1. `install.sh` runs its normal installation steps.
2. The step that previously auto-patched the official Anthropic Telegram plugin with the claudebase `server-rs` variant is absent from the script (removed or gated).
3. A fresh install does NOT result in the per-CLI plugin being activated alongside the daemon poller.
4. The installer documents (or prints a note) that the Telegram path is now through the daemon; the operator should run the cutover steps from `RELEASING.md`.

**Postconditions**:
- Fresh install does NOT create a dual-poller situation.
- The per-CLI plugin is not automatically enabled by the installer.

### Alternative Flows

- **UC-TMC-17-A1: Upgrade install (claudebase already installed)** — existing user runs `install.sh --yes`. The auto-patch step was already absent (or is now removed). If the per-CLI plugin was previously installed and active, the installer MUST NOT restart it. The operator must follow the documented cutover steps.

### Error Flows

- **UC-TMC-17-E1: Installer detects a running per-CLI plugin** — installer prints a warning: "The per-CLI Telegram plugin appears to be running. Stop it before starting the daemon poller to avoid 409 Conflict." Installer does not forcibly kill the plugin.

### Edge Cases

- **UC-TMC-17-EC1**: `install.ps1` (Windows). Same behavior: the auto-patch step must be absent. The conflict gate in the daemon handles any residual plugin that the installer did not stop.

### Data Requirements

- **Input**: `install.sh` / `install.ps1` execution
- **Output**: No per-CLI plugin auto-patching; installer note about cutover steps
- **Side Effects**: File system (plugin configuration); installer output

**FR Coverage**: FR-TMC-6.5, NFR-TMC-5

---

## UC-TMC-18: `chat_ask` Plugin Whitelist — CLI Accesses Tool via Thin-Client Bridge

**Actor**: A bound CLI instance (accessing the daemon via the thin-client plugin bridge)
**Preconditions**:
- Daemon is running; CLI-1 is connected via `src/plugin/bridge.rs` (the thin-client STDIO↔UDS bridge)
- `chat_ask` is present in `TOOL_WHITELIST` in `src/plugin/mcp.rs`
- `chat_ask` appears in the daemon's `tools/list` response

**Trigger**: CLI-1's agent code calls `chat_ask` via the MCP protocol (through the plugin bridge)

### Primary Flow (Happy Path)
1. CLI-1 sends an MCP `tools/call` request for `chat_ask` via the thin-client STDIO↔UDS bridge.
2. `src/plugin/mcp.rs` receives the call; checks `TOOL_WHITELIST`.
3. `"chat_ask"` is found in `TOOL_WHITELIST` (the 10th entry).
4. Call is forwarded to the daemon's `server.rs` dispatch.
5. Daemon processes the `chat_ask` call per UC-TMC-13.
6. Result is returned to CLI-1 via the bridge.

**Postconditions**:
- CLI-1 successfully initiated a `chat_ask` questionnaire.
- The whitelist enforcement did not block the call.

### Alternative Flows

(none — this use case primarily verifies whitelist presence)

### Error Flows

- **UC-TMC-18-E1: `chat_ask` absent from `TOOL_WHITELIST`** — the plugin bridge rejects the `tools/call` with a tool-not-found error. CLI-1 receives an error response. No Telegram message is sent. This is the failure mode if FR-TMC-5.6 is not implemented.

### Edge Cases

- **UC-TMC-18-EC1**: The whitelist is checked at call time (not at connection time). A daemon restart that adds `chat_ask` to `tools/list` does not require CLI-1 to reconnect — the whitelist check passes from the first `tools/call` after the daemon is updated.

### Data Requirements

- **Input**: MCP `tools/call` for `chat_ask` arriving via the thin-client bridge
- **Output**: Call forwarded to daemon; result returned to CLI-1
- **Side Effects**: As per UC-TMC-13

**FR Coverage**: FR-TMC-5.6, AC-TMC-20

---

## UC-TMC-19: Daemon Not Running — Feature Is Inert

**Actor**: Operator on Telegram; a CLI instance
**Preconditions**:
- Daemon process is NOT running (e.g., service not started, or service manager registration not confirmed)
- `[telegram] enabled = true` in `daemon.toml` (would be the intended setting)

**Trigger**: Operator sends a Telegram message; a CLI calls `chat_ask`

### Primary Flow (Happy Path — inert state, no crash)
1. Telegram continues to deliver messages to whichever path holds the `getUpdates` slot (if the per-CLI plugin is running, it receives them; if no poller is running, messages queue on Telegram's server).
2. CLI calls `chat_ask` via the plugin bridge. Bridge attempts UDS connection. Connection refused (daemon not running). Plugin bridge returns an error to CLI-1 ("daemon not running" or equivalent sentinel message from §17 bridge behavior).
3. No routing occurs via the daemon.
4. No crash of the CLI or any other process.

**Postconditions**:
- No routing via the 5-step tree.
- CLI receives a clear error for any daemon MCP calls.
- The per-CLI plugin (if running) continues to handle Telegram messages via the legacy path.

### Alternative Flows

- **UC-TMC-19-A1: Operator starts the daemon** — daemon starts, polls `getUpdates` (if no plugin is running), begins routing. The feature becomes active without operator restart of the CLI (the bridge reconnects).

### Error Flows

(none beyond the inert state described above)

### Edge Cases

- **UC-TMC-19-EC1**: Daemon auto-start via launchd/systemd fails silently (service registered but not active). Operator is unaware the feature is inert. The documented operator action is to confirm the daemon is running before the cutover (`claudebase daemon status` or `pgrep`).

### Data Requirements

- **Input**: None (feature inert)
- **Output**: Error returned to CLI for MCP calls; Telegram messages handled by legacy path or queued
- **Side Effects**: None

**FR Coverage**: NFR-TMC-7

---

## UC-TMC-20: Duplicate Agent Name Registration — Registry Uniqueness

**Actor**: Two CLI instances attempting to register with the same `agent_name`
**Preconditions**:
- CLI-1 is registered with `agent_name = "mira"`, `agent_id = "cli-1-id"`
- CLI-2 attempts to register with `agent_name = "mira"`, `agent_id = "cli-2-id"`
- `validate_agent_name` is called before `register` (per §17 `agent_registry.rs`)

**Trigger**: CLI-2 calls `agent_register` with `agent_name = "mira"`

### Primary Flow (Happy Path — rejection)
1. `validate_agent_name("mira")` is called.
2. `agent_registry` already contains a row with `agent_name = "mira"` and a non-orphaned `agent_id = "cli-1-id"`.
3. Registration is rejected with a conflict error.
4. CLI-2 receives an error response from `agent_register`.
5. `/switch mira` continues to route to CLI-1.
6. `/agents` lists only CLI-1 as "mira".

**Postconditions**:
- `agent_registry` contains exactly one row with `agent_name = "mira"` (CLI-1's entry).
- CLI-2 is not registered under that name.
- Routing is not confused.

### Alternative Flows

- **UC-TMC-20-A1: CLI-1 unregisters first, then CLI-2 registers** — CLI-1 unregisters or is reaped (orphaned). CLI-2 registers with `agent_name = "mira"` successfully. `/switch mira` now routes to CLI-2. No conflict.

### Error Flows

- **UC-TMC-20-E1: Registry uniqueness is NOT enforced** — two rows with `agent_name = "mira"` exist. `list_alive` returns both. `/switch mira` is ambiguous (which `mira` to bind?). This is the failure mode if `validate_agent_name` is bypassed or broken. The router must handle this gracefully (e.g., bind the first result; log a warning about duplicate names).

### Edge Cases

- **UC-TMC-20-EC1**: CLI-1 crashes without calling `unregister` (orphaned). `is_alive("cli-1-id")` returns `false` (orphaned status). CLI-2 may or may not be able to register as "mira" depending on whether `validate_agent_name` checks only non-orphaned entries. The QA test must clarify this behavior explicitly.

### Data Requirements

- **Input**: `agent_register` MCP call with duplicate `agent_name`
- **Output**: Registration error returned to CLI-2
- **Side Effects**: None (registry unchanged)

**FR Coverage**: FR-TMC-1.4, FR-TMC-1.5 (registry helpers used in routing when uniqueness is assumed)

---

## UC-TMC-21: No Alive CLI — Routing Tree Step 5

**Actor**: Operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- No CLIs are registered in `agent_registry` (or all registered CLIs are orphaned/dead)

**Trigger**: Operator sends any free-text message in any chat

### Primary Flow (Happy Path)
1. Routing tree steps 1-4 all fail to resolve a live target.
2. Step 5: `first_alive` returns `None`.
3. Daemon sends Telegram reply to the originating `chat_id`: "No CLIs online. Spawn one with `claudebase run`."
4. No channel notification is published.

**Postconditions**:
- Operator receives the "No CLIs online" message.
- No routing to any agent occurs.
- Daemon remains alive; next message attempt after a CLI registers will route successfully.

### Alternative Flows

- **UC-TMC-21-A1: Bot command `/agents` when no CLIs** — step 5 is not reached; `/agents` is handled in step 1. Reply: "No CLIs currently online." (per UC-TMC-8-A1).

### Error Flows

- **UC-TMC-21-E1: Daemon's reply to "No CLIs online" fails** — daemon logs the error. No crash. The operator's message is effectively dropped but the daemon remains functional.

### Edge Cases

- **UC-TMC-21-EC1**: A CLI registers while a routing decision is in progress for a message. The message was evaluated when no CLI existed (step 5 fired) so the "No CLIs online" reply was sent. The newly registered CLI is not retroactively notified of the dropped message. Acceptable race condition.

### Data Requirements

- **Input**: Telegram `message` update; empty `agent_registry`
- **Output**: Telegram reply "No CLIs online. Spawn one with `claudebase run`."
- **Side Effects**: None

**FR Coverage**: FR-TMC-2.1 (step 5), AC-TMC-7

---

## UC-TMC-22: End-to-End CLI Subscription and Thread Binding Verification

**Actor**: A bound CLI instance; the daemon; the operator on Telegram
**Preconditions**:
- Daemon is running with Telegram enabled
- CLI-1 is alive, subscribed to thread `telegram:111` via `ChatBus` (verified via §17 `bridge.rs`)
- Chat 111 is bound to CLI-1

**Trigger**: Integration test or QA verification run

### Primary Flow (Happy Path — thin-client wiring verified)
1. Operator sends a Telegram message in chat 111.
2. Daemon's routing tree resolves to CLI-1 (step 4).
3. `ChatBus` publishes the channel notification to thread `telegram:111`.
4. CLI-1's plugin bridge (subscribed to `telegram:111`) receives the notification.
5. CLI-1 processes the message and sends a reply via `chat_reply`.
6. Daemon proxies the reply through `enqueue_outbound_tg`; Telegram delivers the reply to chat 111.
7. The reply's `tg_msg_id` is recorded in `tg_message_map`.
8. Operator reply-quotes the bot's message; daemon resolves via `tg_message_map` back to CLI-1.

**Postconditions**:
- Full round-trip verified: inbound Telegram → daemon routing → CLI-1 → outbound reply → `tg_message_map` → reply-quote back to CLI-1.
- The wiring is end-to-end functional.

### Alternative Flows

- **UC-TMC-22-A1: Daemon restart mid-test** — operator sends message 1, daemon restarts, operator sends message 2. Message 2 routing and reply-quote for message 1's `tg_msg_id` (persisted in `chat.db`) must both work after restart.

### Error Flows

(none — this is a verification use case; failures are caught by the QA test, not handled by application logic)

### Edge Cases

- **UC-TMC-22-EC1**: Two CLIs are active simultaneously. Integration test sends messages to both chats and verifies cross-chat isolation: chat 111 reaches only CLI-1, chat 222 reaches only CLI-2.

### Data Requirements

- **Input**: Live Telegram messages; running daemon; registered CLIs
- **Output**: Verified end-to-end routing; verified `tg_message_map` persistence
- **Side Effects**: Actual Telegram API calls (integration test scope)

**FR Coverage**: FR-TMC-7.1, AC-TMC-4, AC-TMC-5, AC-TMC-12

---

## Facts

### Verified facts

- PRD §19 read in full at lines 1112-1495 of `/Users/aleksandra/Documents/claude-code-sdlc/claudebase/docs/PRD.md` — this session, 2026-05-30. All FR-TMC-x.y references and AC-TMC-x values are sourced from this read. — salience: high
- `.claude/plan.md` read in full (198 lines) at `/Users/aleksandra/Documents/claude-code-sdlc/claudebase/.claude/plan.md` — this session. Plan confirms 7 slices, chat-as-id routing key, 5-step routing tree, `chat_ask` MCP tool as explicit call (not auto-mirroring), conflict gate, sync vs async correlation as open architect decision. — salience: high
- Three existing use-case files found in `docs/use-cases/`: `agent-chat-daemon_use_cases.md`, `agent-insights-base_use_cases.md`, `insights-hybrid-corpus_use_cases.md`. None covers Telegram multi-CLI routing. CREATE decision is correct. — salience: high
- `agent-chat-daemon_use_cases.md` covers §17 daemon install, registration, and bus flows (UC-1 through at least UC-5 read). The present file does NOT duplicate §17 flows; it only covers the §19 delta. — salience: medium
- Insights corpus query returned no results for `telegram-multi-cli` feature slug — no prior-session insights to cite. — salience: low
- Books corpus `doc_count = 0` — no overlap possible; topical queries skipped per corpus-scope-relevance protocol (verdict: No overlap). — salience: low
- `active_cli_per_chat` schema columns per PRD §19 FR-TMC-1.1: `chat_id INTEGER PRIMARY KEY`, `active_cli_name TEXT NOT NULL`, `active_agent_id TEXT NOT NULL`, `set_at INTEGER NOT NULL`, `set_by TEXT NOT NULL`. Source: PRD §19 line 1141-1151, read this session. — salience: high
- `tg_message_map` schema columns per PRD §19 FR-TMC-1.2: `tg_msg_id INTEGER NOT NULL`, `chat_id INTEGER NOT NULL`, `sender_agent_id TEXT NOT NULL`, `sent_at INTEGER NOT NULL`, `PRIMARY KEY (chat_id, tg_msg_id)`. Source: PRD §19 lines 1155-1166, read this session. — salience: high
- TTL for `tg_message_map` purge: 30 days = 2592000 seconds (30 × 86400). Source: PRD §19 FR-TMC-1.3, line 1169. — salience: high
- 5-step routing tree, per PRD §19 FR-TMC-2.1 (lines 1177-1183): step 1 = bot command, step 2 = reply-quote, step 3 = omitted (chat-as-id), step 4 = active binding, step 5 = no alive CLI. — salience: high
- Bot commands handled: `/agents`, `/switch`, `/whoami`, `/here`, `/start`, `/help`, `/status`. Source: PRD §19 FR-TMC-2.1 step 1 (line 1179). — salience: high
- `chat_ask` inputSchema `minItems: 2` for options array. Source: PRD §19 FR-TMC-5.3, line 1247. — salience: high
- `callback_data` max 64 bytes, format `<question_id>:<option_idx>`. Source: PRD §19 FR-TMC-5.2, line 1225. — salience: high
- `chat_ask` is the 10th entry in `TOOL_WHITELIST` (current count = 9 verified against `mcp.rs:56-71` in PRD Facts block, sourced from plan.md read this session). Source: PRD §19 FR-TMC-5.6, line 1263; plan.md Facts. — salience: high
- "No CLIs online. Spawn one with `claudebase run`." — exact reply text mandated by FR-TMC-2.1 step 5. Source: PRD §19 line 1183. — salience: high
- `/switch` uses `INSERT OR REPLACE` for atomic upsert of `active_cli_per_chat`. Source: PRD §19 FR-TMC-3.2, line 1195. — salience: high
- Conflict gate 409 log message must contain "409" and phrase "legacy telegram-plugin-rs poller still running". Source: PRD §19 AC-TMC-17, line 1337. — salience: high
- Chat-as-id routing key is `chat_id` alone (not `(user_id, chat_id)`). Operator decision 2026-05-30, OQ-3. Source: plan.md lines 48-50 and PRD §19 line 1127, both read this session. — salience: high
- Sync vs async `chat_ask` correlation mechanism is an open architect decision. Source: PRD §19 §19.10 risk #7 (line 1428); plan.md line 102. — salience: high

### External contracts

- **Telegram Bot API — `getUpdates`** — symbol: single-consumer-per-token rule; second concurrent caller receives HTTP 409 Conflict — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for UC-TMC-15. Implementer must verify the exact HTTP status and response body. — salience: high
- **Telegram Bot API — `sendMessage` with `reply_markup.inline_keyboard`** — symbol: `reply_markup` object containing `inline_keyboard` (array of arrays of `InlineKeyboardButton` with `text` and `callback_data` fields; `callback_data` max 64 bytes) — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for UC-TMC-13. — salience: high
- **Telegram Bot API — `callback_query` update** — symbol: top-level `callback_query` field in a Telegram Update object; contains `id` (for `answerCallbackQuery`), `data` (the `callback_data` string), `message.chat.id` (originating chat) — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for UC-TMC-14. — salience: high
- **Telegram Bot API — `answerCallbackQuery`** — symbol: POST method, required parameter `callback_query_id` (string); dismisses loading spinner on Telegram client — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for UC-TMC-14. — salience: high
- **teloxide v0.17** — symbols: `Bot::get_updates` (verified in use per plan.md Facts); `InlineKeyboardMarkup`, `InlineKeyboardButton::callback`, `CallbackQuery`, `answer_callback_query` (NOT verified against pinned Cargo.lock this session) — verified: yes for `get_updates`; no — assumption for inline-keyboard/callback symbols. Implementer must confirm symbol availability at the pinned version before Slice 5. Source: plan.md Facts block, read this session. — salience: high
- **MCP `tools/list` / `tools/call` dispatch** — symbol: daemon dispatch at `src/daemon/server.rs:632-727`; plugin `TOOL_WHITELIST` at `src/plugin/mcp.rs:56-71` — adding `chat_ask` extends both — verified: yes (sourced from plan.md Facts block, which cites direct file reads in its own session). — salience: medium
- **`agent_registry` `list_alive`, `validate_agent_name`** — symbols: `list_alive(conn, thread) -> Vec<AgentRow>`, `validate_agent_name(name) -> Result<()>` at `src/daemon/agent_registry.rs` — verified: yes (plan.md Facts, read this session, cites `grep "pub fn"` run in its session). — salience: high
- knowledge-base: corpus is empty (doc_count=0); task domain is Telegram Bot API + Rust daemon + SQLite; no overlap. Topical queries skipped per corpus-scope-relevance protocol. — salience: low

### Assumptions

- The architect's sync-vs-async `chat_ask` correlation decision (open at time of writing) does not change the observable behavior described in UC-TMC-13 and UC-TMC-14 — both the sync and async paths deliver the answer to CLI-1 and call `answerCallbackQuery`. Only the timing and internal wire differ. Risk: if the architect chooses a fundamentally different delivery mechanism (e.g., a separate MCP notification channel), some UC-TMC-14 postconditions may need updating. How to verify: architect review at bootstrap Step 3. — salience: high
- `validate_agent_name` in `agent_registry.rs` rejects duplicate non-orphaned agent names (UC-TMC-20 depends on this). Risk: if `validate_agent_name` only validates the name format (not uniqueness), two CLIs can register the same name, making `/switch <name>` ambiguous. How to verify: read `agent_registry.rs:102` — sourced from plan.md Facts as verified in its session, but the exact uniqueness-enforcement logic was not quoted. — salience: high
- The `first_alive` function's tiebreak for equal-priority agents (two agents both named "orchestrator") is deterministic (e.g., by registration time or `agent_id` sort order). Risk: non-deterministic routing makes UC-TMC-3-EC1 and UC-TMC-4-A1 behavior unpredictable. How to verify: verify at implementation of FR-TMC-1.5. — salience: medium
- Option label text in `InlineKeyboardButton.text` is rendered as plain text by Telegram (not parsed as Markdown/HTML). Risk: if Telegram parses it as Markdown, special characters in option labels could break the rendering. How to verify: Telegram Bot API docs for `InlineKeyboardButton.text` field type (NOT verified this session). — salience: medium
- `/whoami` response includes "last 3 messages from `tg_message_map`/chat.db" per plan.md line 89 ("the chat's bound CLI + last 3 messages"). The PRD FR-TMC-3.3 does not explicitly mandate the last-3-messages display; the plan does. Assumption: the QA test will verify binding name + agent_id at minimum; the last-3-messages display is a plan-level detail the implementer resolves. Risk: UC-TMC-10 underspecifies `/whoami` response content relative to the plan. How to verify: architect/planner confirmation. — salience: medium

### Open questions

- **Sync vs. async `chat_ask` correlation** — does the `chat_ask` MCP call block until the operator taps a button (sync), or does it return a `question_id` immediately and the answer arrives later as a channel notification (async)? Needs: architect decision at bootstrap Step 3. This affects UC-TMC-13 step 7 and UC-TMC-14 step 6. — salience: high
- **`validate_agent_name` uniqueness enforcement** — does it check for duplicate non-orphaned names, or only format validity? The exact behavior determines whether UC-TMC-20-E1 (no uniqueness guard) is a real risk. Needs: read `agent_registry.rs:102` this session or architect confirmation. — salience: high
- **`/whoami` last-3-messages display** — plan.md says `/whoami` shows "the chat's bound CLI + last 3 messages from tg_message_map/chat.db"; FR-TMC-3.3 does not mention last-3-messages. QA planner should clarify whether the last-3-messages display is a required AC. — salience: medium
- **`[telegram] enabled` default for the release** — remains `true` (daemon-on by default) or flips to `false` (opt-in migration)? Needs: operator/architect decision. Affects UC-TMC-15 (when the conflict gate fires on fresh install). — salience: medium

## Decisions

### Inbound validation

- Task received: author use-cases for `telegram-multi-cli` feature from PRD §19 and `.claude/plan.md`. Both inputs read in full this session. No contradictions between PRD and plan detected (both are consistent on chat-as-id, 5-step routing tree, conflict gate, `chat_ask` as explicit tool). No upstream error detected. — challenged: yes — outcome: proceeded as-is. — salience: high
- PRD §19 and plan.md both treat sync-vs-async `chat_ask` correlation as an OPEN question for the architect. Use cases are written to be correlation-mechanism-agnostic (both UC-TMC-13 and UC-TMC-14 note the open decision). This avoids encoding a pre-architect assumption into the test blueprint. — challenged: yes — outcome: use cases are correlation-agnostic. — salience: high

### Decisions made

- **CREATE new file** rather than UPDATE an existing use-case file. Three existing files cover §17, insights-base, and insights-hybrid corpus — none covers §19 Telegram multi-CLI routing. Q1 hack? no. Q2 sane? yes. Q3 alternatives? update `agent-chat-daemon_use_cases.md` (rejected: §19 is a distinct feature with its own UC namespace; mixing would confuse QA planners who consume by file). Q4 cause. Q5 n/a. — salience: high
- **UC-TMC-N numbering starting at UC-TMC-1** — dedicated prefix `TMC` to avoid collision with UC-1..UC-N in other use-case files. Q1 hack? no. Q2 sane? yes. Q3 alternatives? flat UC-N (rejected: ambiguous across files when QA planner cross-references). Q4 cause. Q5 n/a. — salience: medium
- **Correlation-mechanism-agnostic UC-TMC-13/14** — use cases describe inputs and outputs (button rendered; answer received by CLI; spinner dismissed) without prescribing whether the `chat_ask` call blocks or the answer arrives asynchronously. Q1 hack? no — the mechanism is genuinely unresolved. Q2 sane? yes — this is the correct level of abstraction for a use-case document. Q3 alternatives? pick sync (rejected: would encode an unverified assumption); pick async (rejected: same reason). Q4 cause — addresses the root of the uncertainty (open architect decision). Q5 tracked in Open questions. — salience: high
- **UC-TMC-20 (duplicate agent name) included** — the task brief mentions it as an edge case and the routing tree depends on `agent_name` uniqueness for `/switch`. Q1 hack? no. Q2 sane? yes. Q3 alternatives? omit (rejected: QA would have no test case for registry collision). Q4 cause. Q5 n/a. — salience: medium
- **Not duplicating §17 flows** — UC-TMC-22 (thin-client wiring verification) references the bridge and `ChatBus` as pre-conditions, not re-specifying them. Q1 hack? no. Q2 sane? yes. Q3 alternatives? re-specify §17 flows inline (rejected: creates maintenance debt; agent-chat-daemon_use_cases.md is the authoritative source). Q4 cause. Q5 n/a. — salience: low

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)
