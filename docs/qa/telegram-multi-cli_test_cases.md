# Test Cases: Telegram Multi-CLI Orchestration

> Based on [PRD §19](../PRD.md#19-telegram-multi-cli-orchestration--chat-as-id-routing-bot-commands-inline-keyboard-questionnaires-and-plugin-cutover) and [Use Cases](../use-cases/telegram-multi-cli_use_cases.md)

---

## Facts

### Verified facts

- PRD §19 (FR-TMC-1.1..FR-TMC-7.3, AC-TMC-1..AC-TMC-21, NFR-TMC-1..NFR-TMC-7) read in full at `/Users/aleksandra/Documents/claude-code-sdlc/claudebase/docs/PRD.md` lines 1112–1495 this session 2026-05-30. All FR/AC references in these test cases are sourced from this read. — salience: high
- Use-case file `docs/use-cases/telegram-multi-cli_use_cases.md` read in full this session (1103 lines, UC-TMC-1..UC-TMC-22, including all alternative flows, error flows, and edge cases). — salience: high
- Format reference `docs/qa/agent-chat-daemon_test_cases.md` read this session (first 60 lines). Column order, Verification Class, and Evidence Required conventions confirmed. — salience: high
- `active_cli_per_chat` schema: `chat_id INTEGER PRIMARY KEY`, `active_cli_name TEXT NOT NULL`, `active_agent_id TEXT NOT NULL`, `set_at INTEGER NOT NULL`, `set_by TEXT NOT NULL`. Source: PRD §19 FR-TMC-1.1 lines 1141–1151. — salience: high
- `tg_message_map` schema: `tg_msg_id INTEGER NOT NULL`, `chat_id INTEGER NOT NULL`, `sender_agent_id TEXT NOT NULL`, `sent_at INTEGER NOT NULL`, `PRIMARY KEY (chat_id, tg_msg_id)`. Source: PRD §19 FR-TMC-1.2 lines 1155–1166. — salience: high
- TTL constant for `tg_message_map` purge: 2592000 seconds (30 × 86400). Source: PRD §19 FR-TMC-1.3 line 1169. — salience: high
- 5-step routing tree: step 1 = bot command, step 2 = reply-quote, step 3 = omitted under chat-as-id, step 4 = active binding / first_alive, step 5 = no alive CLI → "No CLIs online. Spawn one with `claudebase run`." Source: PRD §19 FR-TMC-2.1 lines 1179–1183. — salience: high
- `chat_ask` `callback_data` format: `"<question_id>:<option_index>"`, max 64 bytes. Source: PRD §19 FR-TMC-5.2 line 1225. — salience: high
- `TOOL_WHITELIST` currently contains 9 tools; `chat_ask` is the 10th. Source: PRD §19 FR-TMC-5.6 line 1263. — salience: high
- Conflict gate 409 log must contain "409" AND "legacy telegram-plugin-rs poller still running". Source: PRD §19 AC-TMC-17 line 1337. — salience: high
- Chat-as-id routing key is `chat_id` alone. Operator decision 2026-05-30. Source: PRD §19 line 1127. — salience: high
- `[telegram] enabled = true` in `daemon.toml` activates the daemon poller; `false` leaves the plugin path active. Source: PRD §19 FR-TMC-6.1 line 1269. — salience: high
- `chat.db` location: `~/.claude/knowledge/chat.db` (user-level, per OQ-ACD-4 resolution in §17 architect verdict; carried forward by use-cases file). — salience: high
- No prior-session insights found for `telegram-multi-cli` feature slug via `claudebase insight search` — insights.db exists but returned zero hits for this feature. — salience: low
- Books corpus `doc_count = 0`; scope verdict: **No overlap**. Topical queries skipped per corpus-scope-relevance protocol. — salience: low

### External contracts

- **Telegram Bot API — `getUpdates`** — symbol: single-consumer-per-token rule; second concurrent caller receives HTTP 409 Conflict — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for TC-TMC-15.x. — salience: high
- **Telegram Bot API — `sendMessage` + `reply_markup.inline_keyboard`** — symbol: `reply_markup.inline_keyboard` array-of-arrays of `{text, callback_data}` objects; `callback_data` max 64 bytes hard limit — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for TC-TMC-13.x. — salience: high
- **Telegram Bot API — `callback_query` update** — symbol: `callback_query.id`, `callback_query.data`, `callback_query.message.chat.id` in a Telegram Update — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. Load-bearing for TC-TMC-14.x. — salience: high
- **Telegram Bot API — `answerCallbackQuery`** — symbol: POST, required `callback_query_id` (string); dismisses loading spinner — source: Telegram Bot API docs (NOT opened this session) — verified: no — assumption. — salience: high
- **teloxide (pinned version in Cargo.lock)** — symbols: `Bot::get_updates` (verified in use per PRD Facts); `InlineKeyboardMarkup`, `InlineKeyboardButton::callback`, `CallbackQuery`, `answer_callback_query` — verified: yes for `get_updates`; no — assumption for inline-keyboard/callback symbols. Source: PRD §19 Facts block lines 1434–1458. — salience: high
- **MCP `tools/list` / `tools/call` dispatch** — symbol: daemon dispatch at `src/daemon/server.rs:632-727`; plugin `TOOL_WHITELIST` at `src/plugin/mcp.rs:56-71` — verified: yes (sourced from PRD §19 Facts). — salience: medium
- **`agent_registry` `list_alive`, `validate_agent_name`, `is_alive` (new), `first_alive` (new)** — symbols: `list_alive(conn, thread) -> Vec<AgentRow>`, `validate_agent_name` at `agent_registry.rs:102`, `is_alive` and `first_alive` are NEW functions mandated by FR-TMC-1.4/1.5 — verified: yes for existing symbols (PRD Facts); no — assumption that `is_alive` and `first_alive` will exist with the exact signatures in FR-TMC-1.4/1.5. — salience: high

### Assumptions

- The `chat_ask` sync-vs-async correlation mechanism is an open architect decision (PRD §19 §19.10 risk #7). Test cases for UC-TMC-13 and UC-TMC-14 are **correlation-agnostic**: they assert observable I/O only (button rendered, `answerCallbackQuery` called, answer received by CLI) and do not prescribe the wire mechanism. Risk: if the architect chooses a radically different delivery path the evidence format for TC-TMC-14.1 may need one field updated. — salience: high
- `validate_agent_name` in `agent_registry.rs:102` enforces uniqueness of non-orphaned names (assumed by TC-TMC-20.1). Risk: if it only validates format, duplicate-name rejection is not guaranteed. How to verify: read `agent_registry.rs:102` at implementation time. — salience: high
- `first_alive` tiebreak for two equally-preferred agents is deterministic (registration order or `agent_id` sort). Risk: non-deterministic routing makes TC-TMC-3.3 assertions fragile. — salience: medium
- Test harness for daemon behavior can mock or stub the Telegram Bot API (no real Telegram calls required for the daemon test suite). API-level assertions verify the outbound payload constructed by the daemon, not a live Telegram response. — salience: high

### Open questions

- **Sync vs. async `chat_ask` correlation** — architect decision pending (PRD §19 §19.10 risk #7). TC-TMC-13 and TC-TMC-14 are written mechanism-agnostic; the QA engineer adapts the evidence format once the decision lands. — salience: high
- **`validate_agent_name` uniqueness check scope** — does it reject duplicate non-orphaned names only, or all duplicate names including orphaned? Clarification needed before TC-TMC-20.2 can be executed definitively. — salience: medium
- **`/whoami` last-3-messages display** — plan.md mentions "last 3 messages from tg_message_map/chat.db" as part of `/whoami`; PRD FR-TMC-3.3 does not mandate it. TC-TMC-10.1 tests binding name + agent_id only (the PRD-mandated minimum). — salience: low

---

## Decisions

### Inbound validation

- Task: author test cases for `telegram-multi-cli` from PRD §19 and 22 use cases (UC-TMC-1..22). Both inputs read in full this session. No contradictions between PRD and use-cases detected. Correlation-agnostic framing is correct per PRD §19 §19.10 risk #7. The task explicitly requires CLI/DB/FS/Mixed classes only (no UI/UX / Playwright). Inbound task is coherent and well-specified. — challenged: yes — outcome: proceeded. — salience: high
- The task brief mandates explicit security TCs for forged `callback_data` (invalid qid, out-of-range idx, >64-byte data, stale question). These are covered under Section 5 (Security — Forged `callback_data`). None of the 22 use cases captures all four attack vectors explicitly; TC-TMC-S1..S4 are added as security-coverage cases derived from AC-TMC-16, FR-TMC-5.1/5.2, NFR-TMC-4, and the architect HIGH security risk implicit in routing arbitrary callback strings to a CLI. — challenged: yes — outcome: 4 dedicated security TCs added. — salience: high

### Decisions made

- **TC-ID scheme: TC-TMC-{UC-number}.{flow-number}** — aligns with the use-case UC-TMC-N numbering so every row traces back to exactly one use-case scenario. Alternative: slice-grouped (TC-1.x..TC-7.x like the §17 file) — rejected because the UC-to-TC trace is more valuable for the QA engineer than implementation-slice grouping when there are 22 UCs across 7 slices. Q1: not a hack. Q2: sane. Q3: alternatives considered. Q4: cause. Q5: n/a. — salience: medium
- **Security TCs as a dedicated Section 5** — not folded into UC-TMC-14's error flows, to make the security surface visible as a first-class concern the QA engineer cannot miss. Q1: not a hack. Q2: sane — 4 TCs is proportionate. Q3: inline (rejected: buried in table row 14.x, easy to skip). Q4: cause. Q5: n/a. — salience: high
- **Correlation-agnostic evidence for TC-TMC-13.1 and TC-TMC-14.1** — evidence is stated as observable outputs (outbound sendMessage payload shape, `answerCallbackQuery` called, answer delivered to CLI) without prescribing sync/async timing. Q1: not a hack — genuinely unresolved. Q2: sane. Q3: pick sync or async (rejected: would encode unverified architect assumption). Q4: cause. Q5: tracked in Open questions. — salience: high

### Hacks acknowledged

(none)

### Symptom-only patches (with root-cause links)

(none)

---

## 1. Schema v7 Migration

### 1.1 Happy Path and Additive Behavior

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-1.1 | UC-TMC-1 | DB | Open a `chat.db` at schema v6 (has `agent_registry`, `messages`, `chat_sessions` with existing rows); start the daemon binary (or call `ensure_chat_db_schema` directly). | (a) `PRAGMA table_info(active_cli_per_chat)` returns exactly 5 rows: `chat_id`, `active_cli_name`, `active_agent_id`, `set_at`, `set_by`; (b) `PRAGMA table_info(tg_message_map)` returns exactly 4 rows: `tg_msg_id`, `chat_id`, `sender_agent_id`, `sent_at`; (c) pre-existing row counts in `agent_registry` and `messages` are unchanged. | `sqlite3 ~/.claude/knowledge/chat.db "PRAGMA table_info(active_cli_per_chat)"` output shows exactly `0\|chat_id\|INTEGER\|0\|\|1`, `1\|active_cli_name\|TEXT\|1\|\|0`, `2\|active_agent_id\|TEXT\|1\|\|0`, `3\|set_at\|INTEGER\|1\|\|0`, `4\|set_by\|TEXT\|1\|\|0`; `PRAGMA table_info(tg_message_map)` shows `tg_msg_id`, `chat_id`, `sender_agent_id`, `sent_at` columns with composite PK; `SELECT COUNT(*) FROM agent_registry` = same value as before migration. |
| TC-TMC-1.2 | UC-TMC-1 | DB | Run `ensure_chat_db_schema` a second time on the same already-v7 `chat.db`. | Both `CREATE TABLE IF NOT EXISTS` statements succeed without error; zero rows dropped, zero rows altered; daemon startup completes normally. | `cargo test --test schema_migration_idempotent -- --nocapture` exits 0 with output `0 tables created (already at v7)`; OR `sqlite3 chat.db "PRAGMA table_info(active_cli_per_chat)"` still shows 5 columns and `SELECT COUNT(*) FROM active_cli_per_chat` = same value before and after second call. |
| TC-TMC-1.3 | UC-TMC-1-A2 | DB | Start the daemon with NO pre-existing `chat.db`. | `chat.db` is created; both `active_cli_per_chat` and `tg_message_map` tables exist; zero rows in both tables (fresh DB). | `ls -la ~/.claude/knowledge/chat.db` exists; `sqlite3 chat.db "SELECT COUNT(*) FROM active_cli_per_chat"` = `0`; `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map"` = `0`. |

### 1.2 Error Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-1.4 | UC-TMC-1-EC2 | CLI | Start daemon against a `chat.db` that exists but is missing the `agent_registry` table (corrupt or pre-v5 schema). | Daemon exits non-zero with a clear diagnostic message naming the missing table and schema step; does NOT leave `chat.db` in a partially-migrated state. | Daemon process exits with code `!= 0`; stderr contains a string matching `"ensure_chat_db_schema"` or `"agent_registry"` or `"schema migration"`; `sqlite3 chat.db "SELECT name FROM sqlite_master WHERE type='table'"` shows NO `active_cli_per_chat` (no partial v7 state). |

---

## 2. Registry Helpers — `is_alive` and `first_alive`

### 2.1 `is_alive` — Happy Path

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-2.1 | UC-TMC-2 | Mixed | Unit-test: insert an alive (non-orphaned) agent row into `agent_registry`; call `is_alive(conn, "agent-id-abc")`. | Returns `true`. | `cargo test --test is_alive_returns_true_for_registered -- --nocapture` exits 0, output confirms `is_alive("agent-id-abc") = true`. |
| TC-TMC-2.2 | UC-TMC-2-A1 | Mixed | Unit-test: call `is_alive(conn, "nonexistent-id")` with no such row in `agent_registry`. | Returns `false`. | `cargo test --test is_alive_returns_false_for_unknown -- --nocapture` exits 0; output shows `is_alive("nonexistent-id") = false`. |
| TC-TMC-2.3 | UC-TMC-2-A2 | Mixed | Unit-test: insert agent row with `state = 'orphaned'`; call `is_alive(conn, "agent-id-orphaned")`. | Returns `false`. | `cargo test --test is_alive_returns_false_for_orphaned -- --nocapture` exits 0; output shows `false`. |
| TC-TMC-2.4 | UC-TMC-2-EC1 | Mixed | Unit-test: call `is_alive(conn, "")` (empty string) and `is_alive(conn, "'; DROP TABLE agent_registry;--")` (injection attempt). | Both return `false` without panicking or modifying `agent_registry`. | `cargo test --test is_alive_rejects_malformed_ids -- --nocapture` exits 0; `sqlite3 chat.db "SELECT COUNT(*) FROM agent_registry"` unchanged before and after. |

### 2.2 `first_alive` — Happy Path and Fallback

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-3.1 | UC-TMC-3 | Mixed | Unit-test: two alive agents — one named "orchestrator-main", one named "worker"; call `first_alive(conn, thread=None, prefer_role=Some("orchestrator"))`. | Returns `Some(AgentRow)` where `agent_name` contains "orchestrator". | `cargo test --test first_alive_prefers_role -- --nocapture` exits 0; output shows `first_alive = Some(AgentRow { agent_name: "orchestrator-main", .. })`. |
| TC-TMC-3.2 | UC-TMC-3-A1 | Mixed | Unit-test: one alive agent named "worker" (no "orchestrator"); call `first_alive(conn, None, Some("orchestrator"))`. | Returns `Some(AgentRow)` for "worker" (fallback to any alive). | `cargo test --test first_alive_fallback_no_role_match -- --nocapture` exits 0; output shows `first_alive = Some(AgentRow { agent_name: "worker", .. })`. |
| TC-TMC-3.3 | UC-TMC-3-A2 | Mixed | Unit-test: `agent_registry` has zero non-orphaned rows; call `first_alive(conn, None, Some("orchestrator"))`. | Returns `None`. | `cargo test --test first_alive_returns_none_when_empty -- --nocapture` exits 0; output shows `first_alive = None`. |
| TC-TMC-3.4 | UC-TMC-3-EC1 | Mixed | Unit-test: two alive agents both named "orchestrator-a" and "orchestrator-b"; call `first_alive(conn, None, Some("orchestrator"))` twice. | Returns the SAME `AgentRow` on both calls (deterministic tiebreak). | `cargo test --test first_alive_deterministic_tiebreak -- --nocapture` exits 0; two consecutive calls return identical `agent_id`. |

---

## 3. Routing Tree — Chat-as-ID Isolation and Fallback

### 3.1 Bound Chat Routes to the Correct CLI (Happy Path)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-4.1 | UC-TMC-4 | Mixed | Integration test: CLI-1 registered (agent_id="cli-1-id"); `active_cli_per_chat` has row `(chat_id=111, active_agent_id="cli-1-id")`; CLI-2 registered (different chat or no binding). Inject a mock inbound Telegram `message` update with `chat.id=111`, text="hello" (not a command, not a reply). | (a) `ChatBus` publishes a channel notification with `meta.target_agent_id = "cli-1-id"`; (b) no notification is published with `meta.target_agent_id = "cli-2-id"`; (c) `active_cli_per_chat` row is not modified. | `cargo test --test routing_bound_chat_reaches_cli1 -- --nocapture` exits 0; test asserts `notification.meta.target_agent_id == "cli-1-id"` AND no second notification emitted. |
| TC-TMC-4.2 | UC-TMC-4-A3 | Mixed | Same setup as TC-TMC-4.1. Inject a message with `chat.id=222` bound to CLI-2. | Notification has `meta.target_agent_id = "cli-2-id"`; no notification to "cli-1-id". | `cargo test --test routing_chat_isolation_222_to_cli2 -- --nocapture` exits 0; assertions confirm CLI-2 receives message, CLI-1 does not. |
| TC-TMC-4.3 | UC-TMC-4-EC1 | Mixed | Three alive CLIs; chats 111 → CLI-1, 222 → CLI-2 (bindings set); chat 333 has no binding. Inject message on chat 333. | Routing falls back to `first_alive(prefer_role="orchestrator")`; message reaches the first-alive CLI; neither CLI-1 nor CLI-2 receives a notification via their chat-specific binding; `active_cli_per_chat` row for 333 is NOT created by routing. | `cargo test --test routing_unbound_chat_falls_to_first_alive -- --nocapture` exits 0; target_agent_id is the `first_alive` result (not chat-111 or chat-222's binding). |
| TC-TMC-4.4 | UC-TMC-4-EC3 | Mixed | `active_cli_per_chat[111]` = "cli-1-id"; inject a message with `chat.id=111`, text="@botname what's up?" (no reply_to; contains @-mention). | Routing ignores the @-mention and routes to "cli-1-id" per the active binding (chat-as-id; `extract_first_mention` precursor is replaced). | `cargo test --test routing_at_mention_ignored_under_chat_as_id -- --nocapture` exits 0; `meta.target_agent_id = "cli-1-id"`. |

### 3.2 Fallback Flows When Binding Is Dead or Absent

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-4.5 | UC-TMC-4-A2 | Mixed | `active_cli_per_chat[111]` = "cli-dead-id"; `is_alive("cli-dead-id")` returns `false`; CLI-2 "cli-2-id" is alive. Inject message on chat 111. | Daemon falls through to `first_alive`; notification sent to "cli-2-id"; daemon log contains a note about dead-binding fallback. | `cargo test --test routing_dead_binding_falls_to_first_alive -- --nocapture` exits 0; `meta.target_agent_id = "cli-2-id"`; test asserts log output contains "dead" or "fallback". |
| TC-TMC-4.6 | UC-TMC-4-EC4 | Mixed | `active_cli_per_chat[111]` row exists with `active_agent_id = ""` (empty string — data corruption). Inject message on chat 111. | `is_alive("")` returns `false`; routing falls through to `first_alive`; daemon logs a warning about malformed binding row. | `cargo test --test routing_malformed_empty_agent_id_warning -- --nocapture` exits 0; log output contains "malformed" or "empty" or "active_agent_id"; target is the `first_alive` result. |

### 3.3 No Alive CLI — Step 5

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-21.1 | UC-TMC-21 | Mixed | `agent_registry` contains zero non-orphaned rows; inject a free-text message on any `chat_id`. | Daemon sends a Telegram reply to the originating `chat_id` with the EXACT text: "No CLIs online. Spawn one with `claudebase run`." No channel notification is published to any CLI. | `cargo test --test routing_no_alive_cli_step5_reply -- --nocapture` exits 0; test asserts outbound sendMessage payload `text = "No CLIs online. Spawn one with \`claudebase run\`."` AND zero ChatBus notifications emitted. |

---

## 4. Reply-Quote Routing (`tg_message_map`)

### 4.1 Outbound Tracking (Recording)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-6.1 | UC-TMC-6 | DB | CLI-1 (agent_id="cli-1-id") calls `chat_reply` with `thread="telegram:111"`. Mock Telegram API returns `sendMessage` HTTP 200 with `result.message_id = 9001`. | Row `(tg_msg_id=9001, chat_id=111, sender_agent_id="cli-1-id", sent_at=<within 5s of now>)` exists in `tg_message_map`. | `sqlite3 ~/.claude/knowledge/chat.db "SELECT sender_agent_id, chat_id FROM tg_message_map WHERE tg_msg_id=9001 AND chat_id=111"` returns exactly `cli-1-id|111`; `sent_at` value is within 5 seconds of current Unix time. |
| TC-TMC-6.2 | UC-TMC-6-A2 | DB | CLI-1 sends message; `sendMessage` succeeds (msg_id=9001 recorded). On retry (transient failure re-sends), `sendMessage` succeeds again and the daemon attempts to INSERT the same row. | `tg_message_map` contains EXACTLY one row for `(chat_id=111, tg_msg_id=9001)` — no duplicate. | `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map WHERE chat_id=111 AND tg_msg_id=9001"` = `1`. |
| TC-TMC-6.3 | UC-TMC-6-E1 | Mixed | Mock Telegram API returns HTTP 500 for `sendMessage`. | No row is inserted into `tg_message_map`; CLI-1 receives an error response from `chat_reply`. | `cargo test --test outbound_tracking_no_row_on_sendmessage_failure -- --nocapture` exits 0; `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map"` = same count as before the failed call. |
| TC-TMC-6.4 | UC-TMC-6-A1 | DB | CLI-1 sends msg 9001; CLI-2 sends msg 9002 — both to same chat_id=111. | Two distinct rows exist: `(9001, 111, "cli-1-id")` and `(9002, 111, "cli-2-id")`. | `sqlite3 chat.db "SELECT tg_msg_id, sender_agent_id FROM tg_message_map WHERE chat_id=111 ORDER BY tg_msg_id"` returns two rows with correct agent IDs. |

### 4.2 Reply-Quote Routing

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-5.1 | UC-TMC-5 | Mixed | `tg_message_map` row: `(9001, 111, "cli-1-id", <recent>)`; CLI-1 is alive. Inject inbound Telegram message with `reply_to_message.message_id=9001`, `chat.id=111`. | Routing tree step 2 matches; notification sent with `meta.target_agent_id = "cli-1-id"`. | `cargo test --test reply_quote_routes_to_originating_cli -- --nocapture` exits 0; `meta.target_agent_id == "cli-1-id"`. |
| TC-TMC-5.2 | UC-TMC-5-A1 | Mixed | `tg_message_map` row: `(9001, 111, "cli-dead-id")`; `is_alive("cli-dead-id")` = `false`; CLI-2 is alive and bound to chat 111. Inject reply-to 9001. | Daemon logs "original sender CLI is no longer alive; falling through to active binding"; routing falls to step 4; notification sent to CLI-2. | `cargo test --test reply_quote_dead_cli_fallback -- --nocapture` exits 0; log contains "no longer alive" or "falling through"; `meta.target_agent_id = "cli-2-id"`. |
| TC-TMC-5.3 | UC-TMC-5-A2 | Mixed | `tg_message_map` has no row for `(chat_id=111, tg_msg_id=8000)`. Inject reply-to 8000. | Step 2 finds no row; falls through to step 4 (active binding). Routes to bound CLI as if free-text. | `cargo test --test reply_quote_unknown_msg_falls_to_binding -- --nocapture` exits 0; routing proceeds to step 4 result. |
| TC-TMC-5.4 | UC-TMC-5-A3 | Mixed | `tg_message_map` contains a row from before a daemon restart (persisted in `chat.db`). Restart daemon, then inject reply-to message. | Reply-quote routing uses the persisted row; routes to the original sender CLI (alive). | `cargo test --test reply_quote_survives_daemon_restart -- --nocapture` exits 0; `meta.target_agent_id = "cli-1-id"`. |
| TC-TMC-5.5 | UC-TMC-5-EC1 | Mixed | `tg_message_map` row: `(9002, 222, "cli-2-id")`. Inject reply-to 9002 on chat 222. | Routing uses `chat_id=222` key correctly; notification to "cli-2-id"; CLI-1 (bound to chat 111) unaffected. | `cargo test --test reply_quote_chat_isolation -- --nocapture` exits 0; `meta.target_agent_id = "cli-2-id"`; no notification to "cli-1-id". |
| TC-TMC-5.6 | UC-TMC-5-EC2 | Mixed | Both CLI-1 (msg 9001) and CLI-2 (msg 9002) sent messages in chat 111. Operator reply-quotes msg 9002. | Routes to CLI-2 (not CLI-1, even if CLI-1 is the active binding). | `cargo test --test reply_quote_overrides_active_binding -- --nocapture` exits 0; `meta.target_agent_id = "cli-2-id"`. |

### 4.3 TTL Purge

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-7.1 | UC-TMC-7 | DB | Insert rows with `sent_at = now - 2592001` (31 days old) and `sent_at = now - 86400` (1 day old). Trigger the TTL purge task. | Old row deleted; recent row present. | `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map WHERE sent_at < (strftime('%s','now') - 2592000)"` = `0` after purge; `SELECT COUNT(*) FROM tg_message_map` >= 1 (recent row survives). |
| TC-TMC-7.2 | UC-TMC-7-EC1 | DB | Insert row with `sent_at` exactly equal to `now - 2592000` (boundary). Trigger purge. | Row is NOT deleted (strictly less-than condition `sent_at < cutoff`). | `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map WHERE sent_at = (strftime('%s','now') - 2592000)"` = `1` after purge run. |

---

## 5. Security — Forged `callback_data` (SECURITY — ALL HIGH)

> Architect-designated HIGH security risk: an arbitrary string in `callback_data` MUST NOT be routed to a CLI without validation of `question_id` and `option_index`.

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-S1 | UC-TMC-14-E1 | Mixed | Inject a `callback_query` update with `callback_data = "INVALID_NO_COLON"` (no colon separator — does not match `<qid>:<idx>` format). | Daemon calls `answerCallbackQuery` with an error notification text; NO answer is routed to any CLI; daemon does NOT crash; correlation map is unchanged. | `cargo test --test forged_callback_no_colon -- --nocapture` exits 0; test asserts `answerCallbackQuery` called with non-empty `text` error message; zero ChatBus notifications emitted for this callback. |
| TC-TMC-S2 | UC-TMC-14-E1 variant | Mixed | Inject `callback_query` with `callback_data = "q7a:999"` (valid format but option index 999 is out of range for a 3-option question). | Daemon calls `answerCallbackQuery` with error text; does NOT route index 999 to any CLI. | `cargo test --test forged_callback_out_of_range_idx -- --nocapture` exits 0; zero routing notifications; `answerCallbackQuery` called with error body. |
| TC-TMC-S3 | UC-TMC-13-E1 | Mixed | Force a `callback_data` string that would exceed 64 bytes (e.g., a `question_id` of 60 characters plus `:0`). | `chat_ask` call fails with a clear error BEFORE any `sendMessage` is issued; no Telegram message is sent; MCP call returns error to agent. | `cargo test --test chat_ask_rejects_oversized_callback_data -- --nocapture` exits 0; test asserts no outbound `sendMessage` call made; MCP response contains error indicating size violation. |
| TC-TMC-S4 | UC-TMC-14-E2 | Mixed | Inject `callback_query` with `callback_data = "stale-qid:0"` — a `question_id` that is NOT in the current correlation map (stale or already-answered). | Daemon calls `answerCallbackQuery` with informational text ("already answered or expired"); does NOT route the answer to any CLI. | `cargo test --test forged_callback_unknown_qid -- --nocapture` exits 0; `answerCallbackQuery` called; zero ChatBus notifications; correlation map size unchanged. |

---

## 6. Bot Commands

### 6.1 `/agents` Command

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-8.1 | UC-TMC-8 | Mixed | Two alive CLIs registered: "mira" and "worker". Inject Telegram message `text="/agents"`, `chat.id=111`. | (a) Daemon sends `sendMessage` to `chat_id=111` containing both "mira" and "worker" agent names; (b) no channel notification is published to any CLI. | `cargo test --test bot_cmd_agents_lists_alive -- --nocapture` exits 0; outbound `sendMessage` payload `text` contains "mira" AND "worker"; zero ChatBus notifications emitted. |
| TC-TMC-8.2 | UC-TMC-8-A1 | Mixed | Zero alive CLIs. Inject `/agents`. | Reply text is exactly "No CLIs currently online." (or substring); no notification to any CLI. | `cargo test --test bot_cmd_agents_empty -- --nocapture` exits 0; `sendMessage` payload `text` contains "No CLIs currently online"; zero notifications. |
| TC-TMC-8.3 | UC-TMC-8-EC2 | Mixed | Inject message `text="/agents "` (trailing space). | Matched and handled identically to `/agents`; same reply. | `cargo test --test bot_cmd_agents_trailing_space -- --nocapture` exits 0; same reply as TC-TMC-8.1. |
| TC-TMC-8.4 | UC-TMC-8 / AC-TMC-21 | Mixed | `/agents` in chat 111 with two alive CLIs. | No CLI receives a channel notification (bot commands do NOT leak to CLIs). | `cargo test --test bot_cmd_does_not_leak_to_cli -- --nocapture` exits 0; ChatBus publish call count = 0 for this command. |

### 6.2 `/switch` Command

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-9.1 | UC-TMC-9 | DB | CLI "mira" (agent_id="cli-1-id") is alive. Inject `/switch mira` in chat 111 from Telegram user_id=99999. | (a) `active_cli_per_chat` row `(111, "mira", "cli-1-id", <ts>, "99999")` upserted; (b) reply sent to chat 111 containing "mira" and a group-rebind note; (c) no CLI notification. | `sqlite3 chat.db "SELECT active_cli_name, active_agent_id, set_by FROM active_cli_per_chat WHERE chat_id=111"` returns `mira|cli-1-id|99999`; outbound `sendMessage` payload contains "mira"; ChatBus publish count = 0. |
| TC-TMC-9.2 | UC-TMC-9-A1 | DB | Chat 111 previously bound to "worker". Inject `/switch mira`. | `active_cli_per_chat[111]` now has `active_cli_name="mira"`; old "worker" binding is gone (no second row with `chat_id=111`). | `sqlite3 chat.db "SELECT COUNT(*) FROM active_cli_per_chat WHERE chat_id=111"` = `1`; `SELECT active_cli_name FROM active_cli_per_chat WHERE chat_id=111` = `mira`. |
| TC-TMC-9.3 | UC-TMC-9-E1 | Mixed | Inject `/switch nonexistent` where "nonexistent" is not in `list_alive`. | Reply contains "Unknown CLI" or "nonexistent" and lists available CLIs; NO row inserted or updated in `active_cli_per_chat`. | `cargo test --test bot_cmd_switch_unknown_name_rejected -- --nocapture` exits 0; `sendMessage` payload contains "Unknown" or "nonexistent"; `sqlite3 chat.db "SELECT COUNT(*) FROM active_cli_per_chat WHERE chat_id=111"` unchanged. |
| TC-TMC-9.4 | UC-TMC-9-A3 | Mixed | Inject `/switch` with no argument (no `<name>` after the command). | Reply contains usage hint; no DB write. | `cargo test --test bot_cmd_switch_no_arg -- --nocapture` exits 0; reply contains "Usage: /switch" or equivalent; `SELECT COUNT(*) FROM active_cli_per_chat` unchanged. |
| TC-TMC-9.5 | UC-TMC-9-EC2 | Mixed | Inject `/switch mir` where the only alive agent is named "mira" (partial match). | Rejected with "Unknown CLI: 'mir'"; "mira" listed in available names; no DB write. | `cargo test --test bot_cmd_switch_partial_name_rejected -- --nocapture` exits 0; reply contains "mir" AND "mira"; no `active_cli_per_chat` row updated. |
| TC-TMC-9.6 | UC-TMC-9-A2 | Mixed | `/switch mira` in a group chat (negative chat_id, e.g. -100111). | Binding set for `chat_id=-100111`; reply explicitly mentions group-chat rebind semantics. | `sqlite3 chat.db "SELECT chat_id FROM active_cli_per_chat WHERE chat_id=-100111"` = `-100111`; reply text contains "group" or "all participants". |

### 6.3 `/whoami` Command

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-10.1 | UC-TMC-10 | Mixed | `active_cli_per_chat[111]` = `(active_cli_name="mira", active_agent_id="cli-1-id")`. Inject `/whoami` in chat 111. | Reply contains "mira" and "cli-1-id"; no CLI notification; no DB write. | `cargo test --test bot_cmd_whoami_bound -- --nocapture` exits 0; `sendMessage` payload text contains "mira" and "cli-1-id"; ChatBus publish count = 0. |
| TC-TMC-10.2 | UC-TMC-10-A1 | Mixed | No binding for chat 111. Inject `/whoami`. | Reply names the `first_alive` CLI and states "no explicit binding set". | `cargo test --test bot_cmd_whoami_no_binding -- --nocapture` exits 0; reply text contains "no explicit binding" or "default"; names the first_alive agent. |
| TC-TMC-10.3 | UC-TMC-10-A2 | Mixed | Binding exists but `is_alive(bound_agent_id)` = `false`. Inject `/whoami`. | Reply states the bound CLI is offline and suggests `/switch`. | `cargo test --test bot_cmd_whoami_dead_binding -- --nocapture` exits 0; reply contains "offline" or "no longer" and "/switch". |

### 6.4 `/here` Command

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-11.1 | UC-TMC-11 | Mixed | `active_cli_per_chat[111]` = "cli-1-id"; `agent_registry` row for "cli-1-id" has `host="devbox"`, `cwd="/home/operator/project"`. Inject `/here` in chat 111. | Reply contains "devbox" and "/home/operator/project". | `cargo test --test bot_cmd_here_shows_host_cwd -- --nocapture` exits 0; `sendMessage` payload text contains "devbox" and "/home/operator/project". |
| TC-TMC-11.2 | UC-TMC-11-A2 | Mixed | `agent_registry` row for bound CLI has absent (NULL or empty) `host` and `cwd`. Inject `/here`. | Reply indicates host/cwd "unavailable" or "information is unavailable". | `cargo test --test bot_cmd_here_missing_metadata -- --nocapture` exits 0; reply text contains "unavailable". |
| TC-TMC-11.3 | UC-TMC-11-A3 | Mixed | Bound CLI's agent_registry row was removed between `/switch` and `/here`. Inject `/here`. | Reply states CLI "no longer online"; suggests `/switch` or `/agents`. | `cargo test --test bot_cmd_here_reaped_cli -- --nocapture` exits 0; reply contains "no longer" and "/switch" or "/agents". |

### 6.5 Existing Commands Preserved

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-12.1 | UC-TMC-12 | Mixed | Inject `/help` command. | Reply includes documentation for all 7 commands: `/agents`, `/switch <name>`, `/whoami`, `/here`, `/start`, `/help`, `/status`; the `/switch` entry explicitly mentions group-chat rebind semantics. | `cargo test --test bot_cmd_help_lists_all_commands -- --nocapture` exits 0; `sendMessage` payload text contains each of: "agents", "switch", "whoami", "here", "start", "help", "status", and "group". |
| TC-TMC-12.2 | UC-TMC-12-EC1 | Mixed | Inject `/help@botusername` (group-chat form with bot-name suffix). | Matched and handled as `/help`; same reply as TC-TMC-12.1. | `cargo test --test bot_cmd_help_with_botname_suffix -- --nocapture` exits 0; reply identical to TC-TMC-12.1. |
| TC-TMC-12.3 | UC-TMC-12 / AC-TMC-21 | Mixed | Inject `/start`, `/help`, `/status` in succession. | None of the three commands produces a channel notification to any CLI. | `cargo test --test existing_commands_no_cli_notification -- --nocapture` exits 0; ChatBus publish count = 0 across all three commands. |

---

## 7. `chat_ask` MCP Tool — Outbound Keyboard Rendering

### 7.1 Happy Path — Sending the Inline Keyboard

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-13.1 | UC-TMC-13 | Mixed | CLI-1 calls `chat_ask` MCP tool with `thread="telegram:111"`, `question="Pick one"`, `options=[{label:"A"}, {label:"B"}, {label:"C"}]`. Mock Telegram API intercepts the call. | (a) Daemon sends `sendMessage` to `chat_id=111` with `text="Pick one"` and `reply_markup.inline_keyboard` containing exactly 3 button entries; (b) each button's `callback_data` matches pattern `^[^:]+:[012]$` and is ≤ 64 bytes; (c) `tg_message_map` row inserted with `sender_agent_id="cli-1-id"` for the returned `message_id`; (d) a pending correlation entry exists mapping the generated `question_id` to CLI-1. | `cargo test --test chat_ask_sends_inline_keyboard -- --nocapture` exits 0; outbound `sendMessage` JSON captured by mock contains `reply_markup.inline_keyboard` array of length 3; each element has `callback_data` matching regex `^[^:]+:[012]$` with `len(bytes) <= 64`; `sqlite3 chat.db "SELECT COUNT(*) FROM tg_message_map WHERE sender_agent_id='cli-1-id'"` = 1. |
| TC-TMC-13.2 | UC-TMC-13 / AC-TMC-16 | Mixed | Call `chat_ask` with 3 options. Inspect the generated `callback_data` strings. | ALL `callback_data` strings are strictly ≤ 64 bytes. | `cargo test --test chat_ask_callback_data_size_limit -- --nocapture` exits 0; test asserts `callback_data.len() <= 64` for all 3 button entries. |
| TC-TMC-13.3 | UC-TMC-13-A1 | Mixed | Call `chat_ask` with exactly 2 options (the minimum). | Sends `sendMessage` with `reply_markup.inline_keyboard` containing exactly 2 button entries; `callback_data` format valid. | `cargo test --test chat_ask_minimum_two_options -- --nocapture` exits 0; `inline_keyboard` length = 2; each `callback_data` ≤ 64 bytes. |

### 7.2 Input Validation Errors

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-13.4 | UC-TMC-13-E3 | Mixed | Call `chat_ask` with `options` array containing only 1 item (fewer than `minItems: 2`). | MCP call returns validation error before any `sendMessage` is issued; no Telegram call made; error body describes the schema violation. | `cargo test --test chat_ask_rejects_one_option -- --nocapture` exits 0; mock asserts zero `sendMessage` calls; MCP response is an error. |
| TC-TMC-13.5 | UC-TMC-13-E4 | Mixed | Call `chat_ask` with malformed `thread = "nottelogram:111"` (wrong prefix). | MCP returns validation error; no Telegram call made. | `cargo test --test chat_ask_rejects_malformed_thread -- --nocapture` exits 0; zero outbound calls; MCP error response. |

---

## 8. `chat_ask` MCP Tool — Answer Routing via `callback_query`

### 8.1 Happy Path — Operator Taps a Button

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-14.1 | UC-TMC-14 | Mixed | Setup: `chat_ask` pending with `question_id="q7a"`, options A/B/C; correlation maps "q7a" → CLI-1. Inject `callback_query` update: `id="cq-xyz"`, `data="q7a:1"`, `message.chat.id=111`. | (a) Daemon calls `answerCallbackQuery(callback_query_id="cq-xyz")`; (b) answer `{index: 1, label: "B"}` delivered to CLI-1 (via MCP return or channel notification — mechanism-agnostic); (c) correlation entry "q7a" cleaned up (no longer pending); (d) CLI-2 (if registered) receives NO answer. | `cargo test --test callback_query_routes_answer_to_cli -- --nocapture` exits 0; mock asserts `answerCallbackQuery` called with `callback_query_id="cq-xyz"`; CLI-1 receives `{index:1, label:"B"}`; correlation map size decremented. |

### 8.2 Error and Idempotency Flows

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-14.2 | UC-TMC-14-EC1 | Mixed | Same `callback_query` injected twice (double-tap / network retry). | First injection: answer delivered (TC-TMC-14.1). Second injection: correlation entry "q7a" is already cleaned up → daemon calls `answerCallbackQuery` with informational text ("already answered or expired"); NO duplicate answer delivered to CLI-1. | `cargo test --test callback_query_double_tap_idempotent -- --nocapture` exits 0; second injection: `answerCallbackQuery` called with non-empty `text`; no second answer delivered. |
| TC-TMC-14.3 | UC-TMC-14-E4 | Mixed | CLI-1 dies between `chat_ask` send and button tap; `is_alive("cli-1-id")` = `false`. Operator taps a button. | Daemon calls `answerCallbackQuery` with a note; logs "CLI is no longer alive"; answer NOT delivered to CLI-1 (no live recipient). | `cargo test --test callback_query_dead_cli_answer_dropped -- --nocapture` exits 0; `answerCallbackQuery` called; log contains "no longer alive" or "answer dropped"; zero ChatBus notifications. |
| TC-TMC-14.4 | UC-TMC-14-EC4 | Mixed | Two concurrent `chat_ask` calls pending ("q7a" → CLI-1, "q8b" → CLI-2). Inject `callback_query` for "q7a:0". | Answer for "q7a" routed to CLI-1 only; "q8b" correlation untouched; CLI-2 receives no spurious answer. | `cargo test --test callback_query_two_concurrent_questions -- --nocapture` exits 0; CLI-1 gets answer; CLI-2 correlation intact; `correlation_map.contains_key("q8b") == true`. |

---

## 9. Plugin Whitelist — `chat_ask` via Thin-Client Bridge

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-18.1 | UC-TMC-18 / AC-TMC-20 | FS | Read `src/plugin/mcp.rs` and grep `TOOL_WHITELIST`. | `"chat_ask"` appears in `TOOL_WHITELIST` as a string literal; the whitelist contains exactly 10 entries (was 9 + `chat_ask`). | `grep -n "chat_ask" /Users/aleksandra/Documents/claude-code-sdlc/claudebase/src/plugin/mcp.rs` returns at least one match within a `TOOL_WHITELIST` array or constant context; `grep -c '"[a-z_]*"' <whitelist_block>` = 10. |
| TC-TMC-18.2 | UC-TMC-18 | Mixed | Integration test: CLI-1 connected via the thin-client bridge sends MCP `tools/call` for `chat_ask`. | Call is NOT rejected with tool-not-found; it reaches the daemon `server.rs` dispatch (even if the downstream Telegram call is mocked to succeed or fail for other reasons). | `cargo test --test plugin_bridge_chat_ask_whitelisted -- --nocapture` exits 0; test asserts the bridge does not emit a "tool not allowed" error; the call reaches the daemon dispatch layer. |
| TC-TMC-18.3 | UC-TMC-18-E1 | Mixed | Remove `"chat_ask"` from `TOOL_WHITELIST` (deliberate regression test). CLI calls `chat_ask` via bridge. | Bridge returns a tool-not-found or not-allowed error; no Telegram call made. | `cargo test --test plugin_bridge_chat_ask_not_whitelisted_blocked -- --nocapture` exits 0; MCP response error; zero outbound Telegram calls. (NOTE: run this test in isolation with modified source or a test double — do not modify production source permanently.) |

---

## 10. Conflict Gate — 409 Detection and Clean Cutover

### 10.1 409 Detection (Legacy Plugin Still Running)

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-15.1 | UC-TMC-15 / AC-TMC-17 | Mixed | Mock Telegram `getUpdates` endpoint to return HTTP 409 Conflict. Start daemon with `[telegram] enabled = true`. | (a) Daemon log contains a message with both the string "409" AND the phrase "legacy telegram-plugin-rs poller" (or "still running"); (b) daemon process remains alive (does NOT exit); (c) UDS socket is still accepting connections; (d) Telegram poller stops (no further `getUpdates` calls after the 409). | `cargo test --test conflict_gate_409_detected -- --nocapture` exits 0; log output captured by test contains "409" AND "legacy telegram-plugin-rs"; daemon `is_running()` = true after 409; mock asserts `getUpdates` call count does not increase after first 409. |
| TC-TMC-15.2 | UC-TMC-15 / NFR-TMC-3 | Mixed | After a 409, verify that non-Telegram daemon capabilities remain functional. | MCP `tools/list` call over UDS succeeds; `agent_registry` queries succeed; `ChatBus` publish succeeds. | `cargo test --test conflict_gate_uds_stays_alive_after_409 -- --nocapture` exits 0; test asserts MCP `tools/list` returns a valid response after the 409 event. |
| TC-TMC-15.3 | UC-TMC-15-A2 | Mixed | Mock `getUpdates` to return 409 on the 5th call (not just the first). | Daemon logs the conflict message on each 409; does not crash; does not resume polling. | `cargo test --test conflict_gate_subsequent_409_handled -- --nocapture` exits 0; log contains "409" at least once per 409 response; process stays alive. |

### 10.2 Clean Cutover — Daemon Takes Over After Plugin Stops

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-16.1 | UC-TMC-16 / AC-TMC-18 | Mixed | Simulate: first `getUpdates` returns 409; plugin "stops" (mock flips to return 200 with empty update list on restart). Daemon is restarted. | Daemon polls `getUpdates` successfully; routing tree is operational; no 409 logged. | `cargo test --test cutover_daemon_takes_over_after_plugin_stops -- --nocapture` exits 0; `getUpdates` call returns 200; test message injected via mock routes successfully via daemon routing tree. |
| TC-TMC-16.2 | UC-TMC-16-A1 / AC-TMC-19 | FS | Set `[telegram] enabled = false` in `daemon.toml`; restart daemon. | Daemon does NOT start the Telegram poller; UDS socket is up; mock Telegram `getUpdates` is NOT called. | `cargo test --test revert_path_enabled_false_no_poller -- --nocapture` exits 0; mock asserts `getUpdates` call count = 0; `claudebase daemon status --json` field `tg_bot_state = "disconnected"` or `"not-configured"`. |

---

## 11. Installer — Plugin Auto-Patch Removed

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-17.1 | UC-TMC-17 / FR-TMC-6.5 | FS | Read `install.sh` and `install.ps1`; search for the auto-patch step that previously activated the `server-rs` Telegram plugin variant. | No auto-patch step is present in either installer; neither script activates the per-CLI telegram plugin automatically. | `grep -n "server-rs\|telegram.*patch\|patch.*telegram" /Users/aleksandra/Documents/claude-code-sdlc/claudebase/install.sh` returns zero matches; same grep on `install.ps1` returns zero matches. |
| TC-TMC-17.2 | UC-TMC-17 / FR-TMC-6.5 | FS | Verify that `install.sh` contains a note or instruction about the Telegram daemon cutover path. | `install.sh` output or a `README.md` / `RELEASING.md` section documents the cutover steps. | `grep -n "cutover\|daemon.*poller\|telegram.*enabled\|RELEASING" /Users/aleksandra/Documents/claude-code-sdlc/claudebase/install.sh` returns at least one match; OR `grep -rn "Telegram Multi-CLI" /Users/aleksandra/Documents/claude-code-sdlc/claudebase/README.md` returns a match. |

---

## 12. Daemon Not Running — Feature Is Inert

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-19.1 | UC-TMC-19 / NFR-TMC-7 | Mixed | Stop the daemon. CLI-1 calls `chat_ask` via the plugin bridge. | Bridge returns an error to CLI-1 indicating "daemon not running" or "connection refused"; no crash of CLI-1. | `cargo test --test daemon_not_running_chat_ask_returns_error -- --nocapture` exits 0; MCP response is an error containing "daemon" or "connection" or "refused"; CLI process exits cleanly. |

---

## 13. Registry Uniqueness — Duplicate Agent Name

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-20.1 | UC-TMC-20 | Mixed | CLI-1 registered as "mira" (non-orphaned). CLI-2 attempts `agent_register` with `agent_name="mira"`. | CLI-2 receives a conflict or uniqueness error; `agent_registry` still has exactly one row with `agent_name="mira"` (CLI-1's). | `cargo test --test registry_duplicate_name_rejected -- --nocapture` exits 0; MCP response to CLI-2 is an error; `sqlite3 chat.db "SELECT COUNT(*) FROM agent_registry WHERE agent_name='mira'"` = `1`. |
| TC-TMC-20.2 | UC-TMC-20-EC1 | Mixed | CLI-1 registered as "mira" but crashes (orphaned state). CLI-2 attempts `agent_register` with `agent_name="mira"`. | Documents observed behavior: either CLI-2 succeeds (if `validate_agent_name` only checks non-orphaned rows) or fails (if it checks all rows). Test asserts the actual behavior and confirms it is consistent with how `/switch mira` resolves routing. | `cargo test --test registry_duplicate_name_after_orphan -- --nocapture` exits 0; test output explicitly logs "ALLOWED" or "REJECTED"; if ALLOWED: `sqlite3 chat.db "SELECT COUNT(*), state FROM agent_registry WHERE agent_name='mira' GROUP BY state"` shows exactly one non-orphaned row; if REJECTED: count of ALL "mira" rows = 1. |
| TC-TMC-20.3 | UC-TMC-20-A1 | DB | CLI-1 "mira" cleanly unregisters via `agent_unregister`. CLI-2 then registers as "mira". | CLI-2 registration succeeds; `/switch mira` now resolves to CLI-2; `/agents` lists CLI-2 as "mira". | `sqlite3 chat.db "SELECT agent_id FROM agent_registry WHERE agent_name='mira' AND state='alive'"` returns CLI-2's agent_id; `sqlite3 chat.db "SELECT active_agent_id FROM active_cli_per_chat WHERE active_cli_name='mira'"` = CLI-2's agent_id (after a `/switch mira` call). |

---

## 14. End-to-End — Full Round-Trip Verification

| # | Use Case | Verification Class | Test Case | Expected Result | Evidence Required |
|---|----------|--------------------|-----------|-----------------|--------------------|
| TC-TMC-22.1 | UC-TMC-22 / FR-TMC-7.1 | Mixed | Integration test: daemon running; CLI-1 alive and subscribed to `telegram:111`; chat 111 bound to CLI-1. (1) Inject inbound Telegram message on chat 111. (2) CLI-1 replies via `chat_reply`. (3) Mock Telegram returns `message_id=9001`. (4) Inject reply-quote referencing msg 9001. | (a) Step 1: `ChatBus` notification with `meta.target_agent_id="cli-1-id"` received by CLI-1's bridge subscription; (b) Step 3: `tg_message_map` row `(9001, 111, "cli-1-id")` inserted; (c) Step 4: reply-quote routing resolves to CLI-1 (not any other CLI). | `cargo test --test e2e_full_roundtrip_routing -- --nocapture` exits 0; asserts all three postconditions in order: (a) bridge received notification; (b) `sqlite3 chat.db "SELECT sender_agent_id FROM tg_message_map WHERE tg_msg_id=9001"` = `cli-1-id`; (c) second notification `meta.target_agent_id = "cli-1-id"`. |
| TC-TMC-22.2 | UC-TMC-22-EC1 / AC-TMC-4 | Mixed | Two CLIs alive; chat 111 → CLI-1, chat 222 → CLI-2. Inject a message on chat 111 and independently on chat 222. | Chat 111 message reaches CLI-1 only; chat 222 message reaches CLI-2 only. Cross-chat isolation holds under concurrent routing. | `cargo test --test e2e_cross_chat_isolation -- --nocapture` exits 0; two notification assertions: `(chat_id=111).target = "cli-1-id"` AND `(chat_id=222).target = "cli-2-id"`; no cross-contamination. |
| TC-TMC-22.3 | UC-TMC-22-A1 | Mixed | E2E test with daemon restart mid-flow. Inject msg on chat 111 (CLI-1 replies, tg_msg_id=9001 recorded). Restart daemon. Inject message on chat 111 and inject reply-quote for msg 9001. | After restart: (a) new message routes to CLI-1 via `active_cli_per_chat` (persisted in `chat.db`); (b) reply-quote for msg 9001 routes to CLI-1 via `tg_message_map` (also persisted). | `cargo test --test e2e_routing_survives_daemon_restart -- --nocapture` exits 0; both routing assertions pass after daemon restart without re-seeding `chat.db`. |

---

## Coverage Summary

| Section | Use Cases Mapped | Test Cases |
|---------|-----------------|------------|
| 1. Schema v7 Migration | UC-TMC-1 (primary, A2, EC2) | TC-TMC-1.1..1.4 (4 TCs) |
| 2. Registry Helpers | UC-TMC-2, UC-TMC-3 | TC-TMC-2.1..3.4 (7 TCs) |
| 3. Routing Tree | UC-TMC-4, UC-TMC-21 | TC-TMC-4.1..4.6, TC-TMC-21.1 (7 TCs) |
| 4. Reply-Quote / Outbound Tracking / TTL | UC-TMC-5, UC-TMC-6, UC-TMC-7 | TC-TMC-5.1..7.2 (11 TCs) |
| 5. Security — Forged callback_data | UC-TMC-13-E1, UC-TMC-14-E1/E2 | TC-TMC-S1..S4 (4 TCs) |
| 6. Bot Commands | UC-TMC-8..UC-TMC-12 | TC-TMC-8.1..12.3 (17 TCs) |
| 7. chat_ask Outbound | UC-TMC-13 | TC-TMC-13.1..13.5 (5 TCs) |
| 8. chat_ask Answer Routing | UC-TMC-14 | TC-TMC-14.1..14.4 (4 TCs) |
| 9. Plugin Whitelist | UC-TMC-18 | TC-TMC-18.1..18.3 (3 TCs) |
| 10. Conflict Gate | UC-TMC-15, UC-TMC-16 | TC-TMC-15.1..16.2 (5 TCs) |
| 11. Installer | UC-TMC-17 | TC-TMC-17.1..17.2 (2 TCs) |
| 12. Daemon Not Running | UC-TMC-19 | TC-TMC-19.1 (1 TC) |
| 13. Registry Uniqueness | UC-TMC-20 | TC-TMC-20.1..20.3 (3 TCs) |
| 14. E2E Verification | UC-TMC-22 | TC-TMC-22.1..22.3 (3 TCs) |
| **Total** | **22 of 22 UCs** | **76 test cases** |

**UC coverage:** All 22 use-case scenarios (UC-TMC-1..UC-TMC-22) are mapped. Every primary flow, alternative flow, error flow, and selected edge case from the use-cases file is represented by at least one test case.
