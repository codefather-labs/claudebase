# Use Cases: CLI-to-CLI Routing — Agent-to-Agent Communication via Daemon

> Based on [PRD §20](../PRD.md#20-cli-to-cli-routing--agent-to-agent-communication-via-daemon) and [.claude/plan.md](../../.claude/plan.md).
>
> Feature slug: `cli-to-cli-routing`. Branch: `feat/multi-agent-on-v0.6`.
> Date: 2026-06-05.
>
> **Updated 2026-06-05**: Architect PASS-WITH-CONDITIONS amendments applied.
> - Factual corrections: `chat_messages.thread` → `thread_id`; `src/agent_registry.rs` → `src/daemon/agent_registry.rs`; migration file `src/store.rs` → `src/daemon/chat.rs`; `agent_registry` v5+routing columns corrected to actual schema (verified `src/daemon/chat.rs:443-552`).
> - UC-C2C-1: added UC-C2C-1-EC3 (legacy `cwd IS NULL` rows under `--project all` only).
> - UC-C2C-3: added precondition on connection-bound identity; added UC-C2C-3-E2 (caller-supplied `from` ignored).
> - UC-C2C-4: postcondition updated with 10-msg/30s drain rate-limit; added UC-C2C-4-EC3 (rate-limit drain schedule); UC-C2C-4-A updated with `i64::MAX` encoding.
> - UC-C2C-5: added pre-flight assumption on hook event existence; added UC-C2C-5-C (fallback hook mode).
> - UC-C2C-15-EC1: clarified fallthrough semantics (any non-`"agent-to-agent"` kind value → `<channel>`).
> - UC-C2C-17: updated preconditions and flow to reference `src/daemon/chat.rs::apply_agent_registry_c2c_migration` and `ensure_chat_db_schema` (not `src/store.rs`).
> - Facts and Decisions blocks updated.
>
> **Scope frame:** One daemon, single machine, single user. Two or more Claude Code (CC) instances running in separate terminals, each in a clone or worktree of the same git project. Each CC registers with the daemon via MCP and is assigned an `agent_id`. The daemon routes `notifications/claude/channel` events to the correct CC session via the existing `should_relay_channel_notification(target_agent_id)` bridge filter (frozen). Agent-to-agent messages are distinct from Telegram inbound: they use `meta.kind="agent-to-agent"` and render as `<agent-message>` (not `<channel>`). Trust model is single-box single-user; no prompt-injection guard in MVP.
>
> **Actor glossary:**
> - **Operator** — the human using one or more CC windows.
> - **Agent A / CC #1** — the sending CC session; has `agent_id` "A" registered in `agent_registry`.
> - **Agent B / CC #2** — the receiving CC session; has `agent_id` "B" registered in `agent_registry`.
> - **Daemon** — the `claudebase daemon` process owning the UDS/named-pipe socket and SQLite database.
> - **Bridge** — `src/plugin/bridge.rs` — the MCP bridge that runs inside each CC process and connects to the daemon.
> - **Hook** — the `PostToolUse:ExitPlanMode` shell/PowerShell script installed by the installer.
>
> **Verification-class hint for downstream qa-planner:** primary flows are `CLI + DB` (Mixed). DND flows are `CLI + DB`. Hook flow is `FS + CLI`. Error flows are `CLI`. Edge cases are `DB + CLI`.

---

## UC-C2C-1: Operator Lists All Alive Agents in the Same Project

**Actor**: Operator (from CC #1 terminal or any shell on the same machine).

**Preconditions**:
- Daemon is running; UDS/named-pipe socket is up.
- At least two CC sessions are alive: CC #1 in clone A (cwd = `C:\Users\madwh\Documents\claudebase`, branch `feat/multi-agent-on-v0.6`) and CC #2 in clone B (cwd = `C:\Users\madwh\Documents\claudebase-mirror`, branch `main`). Both are registered in `agent_registry` with `state = 'alive'` and `last_pinged_at` within the 30-second alive window.
- Both clones have the SAME `git remote origin URL` (i.e., the same logical project). Both have `project_id` = normalized `github.com/codefather-labs/claudebase` (or equivalent) resolved at register time per FR-C2C-2.2 Step 1.
- Schema is at v6 (all new columns present: `project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts`).
- The base `agent_registry` v5 schema (before the C2C migration) has columns: `agent_id PK`, `agent_name`, `connection_id`, `chat_thread_id`, `permission_relayer`, `spawned_at`, `last_pinged_at`, `state CHECK('alive'/'orphaned'/'dead')`, `metadata`. The C2C-routing migration (in `src/daemon/chat.rs::apply_agent_registry_c2c_migration`) adds: `routing_chat_id`, `routing_thread_id`, `last_user_id`, `host`, `cwd`, `pid`.

**Trigger**: Operator runs `claudebase agent list-alive --project current` from within clone A's cwd (or any cwd whose `project_id` normalizes to the same value).

### Primary Flow (Happy Path)

1. CLI parses `agent list-alive --project current`.
2. `run_agent_list_alive` calls `resolve_project_id(cwd())` — git remote URL normalization succeeds; returns `github.com/codefather-labs/claudebase` (lowercase, `.git` stripped).
3. Daemon receives the request; executes `SELECT agent_id, cwd, routing_chat_id, feature_description, last_pinged_at, dnd_until_ts FROM agent_registry WHERE cwd LIKE <project_cwd_prefix>` (or scoped via `project_id` lookup) with the resolved value AND `last_pinged_at > now() - 30s` filter AND `state = 'alive'`.
4. Result set contains two rows: one for agent A (branch `feat/multi-agent-on-v0.6`, working_dir clone A path, feature_description possibly NULL), one for agent B (branch `main`, working_dir clone B path).
5. With `--json` flag: daemon returns a JSON array; CLI prints it to stdout and exits 0.
6. Without `--json`: CLI renders a human-readable table with columns `AGENT_ID | CWD | ROUTING_CHAT_ID | FEATURE_DESCRIPTION | LAST_PINGED_AT | DND_UNTIL_TS | STATE`.

**Postconditions**:
- stdout contains a JSON array (or table) with exactly the two agents whose `project_id` (or `cwd`-based scope) matches and whose `state = 'alive'` and `last_pinged_at > now() - 30s`; no agents from unrelated projects appear.
- `feature_description` is present for any agent that has called `agent_describe` since last register.
- Exit code 0.

**Data Requirements**:
- Input: resolved `project_id` string; `agent_registry` rows.
- Output: JSON array or table of alive agents scoped to the same project.
- Side Effects: none (read-only query).

**FR Coverage**: FR-C2C-6.1, FR-C2C-6.2, FR-C2C-6.3, FR-C2C-6.4, FR-C2C-6.5, FR-C2C-2.2 (Step 1). **Acceptance: AC-C2C-1**.

### Alternative Flows

- **UC-C2C-1-A: Operator passes `--project all`** — Step 3 omits the `project_id` filter; all alive agents across all projects appear in the output. Useful when the operator has CC sessions in unrelated projects and wants a global roster.
- **UC-C2C-1-B: Operator passes `--project github.com/codefather-labs/claudebase` (literal slug)** — Step 2 is skipped; the literal slug is used directly as the filter. Result is identical to UC-C2C-1 in the shared setup.

### Error Flows

- **UC-C2C-1-E1: No alive agents in the current project** — `SELECT` returns zero rows; CLI prints `[]` (JSON) or an empty table with a "No alive agents found for project: <id>" note; exits 0. Not an error state.
- **UC-C2C-1-E2: Daemon is not running** — CLI connection to UDS/named-pipe fails; CLI prints a structured error `{"error": "daemon not running"}` to stderr; exits 1.

### Edge Cases

- **UC-C2C-1-EC1**: One of the two agents has `dnd_until_ts` set to a future timestamp. The agent still appears in the list — DND does not remove an agent from `list-alive`. The `dnd_until_ts` column shows the future timestamp so the operator knows that agent is in DND.
- **UC-C2C-1-EC2**: An agent's `last_pinged_at` is stale (CC process crashed without calling `agent_unregister`, or `state` was set to `'dead'`/`'orphaned'`). The agent is filtered out by the 30-second alive window and/or the `state = 'alive'` filter. It does NOT appear in output. No error is raised.
- **UC-C2C-1-EC3 (legacy `cwd IS NULL` backfill semantics)**: Legacy `agent_registry` rows that predate the C2C routing migration have `cwd IS NULL` (the column did not exist and was backfilled to NULL by `ALTER TABLE ADD COLUMN ... DEFAULT NULL`). Such rows can only appear in output when the operator passes `--project all` — they are NEVER returned under `--project current` (which scopes by cwd match). The presence of `cwd IS NULL` rows does not cause an error; the `--project all` output includes them with `CWD = NULL` displayed as `—` or `(legacy)` in the human-readable table.

---

## UC-C2C-2: Operator Publishes Feature Description; Peer Sees Update Within 5s

**Actor**: Operator / Agent A (in CC #1).

**Preconditions**:
- Daemon running; CC #1 (agent A) and CC #2 (agent B) are registered (`state = 'alive'`), both with same cwd-derived project scope.
- CC #1 has `agent_describe` in its MCP TOOL_WHITELIST.
- Agent A's current `feature_description` in `agent_registry` may be NULL or stale. (`feature_description` is NOT a column in the base v5 `agent_registry` — it is added by the C2C routing migration per FR-C2C-1.1; see precondition: schema at v6.)

**Trigger**: Agent A (responding to an operator request in CC #1) calls MCP tool `agent_describe` with `{ feature_id: "cli-to-cli-routing", branch: "feat/multi-agent-on-v0.6", description: "Wiring agent-to-agent comms via daemon" }`.

### Primary Flow (Happy Path)

1. CC #1's bridge sends the `agent_describe` MCP tool call to the daemon over the UDS/named-pipe.
2. Daemon's `handle_agent_describe` handler executes `UPDATE agent_registry SET feature_description = ?, branch = ? WHERE agent_id = ?` using the calling agent's `agent_id` (derived from the connection's registered `agent_id`).
3. Daemon returns `{ "ok": true }` to CC #1. The MCP tool call resolves successfully in under 1s.
4. Within 5 seconds, operator in CC #2 runs `claudebase agent list-alive --project current --json`.
5. Result contains agent A's row with `feature_description = "Wiring agent-to-agent comms via daemon"`.

**Postconditions**:
- `agent_registry` row for agent A has `feature_description = "Wiring agent-to-agent comms via daemon"` and `branch = "feat/multi-agent-on-v0.6"`.
- `claudebase agent list-alive --project current` from any cwd in the same project reflects the update.

**Data Requirements**:
- Input: `{ feature_id: string, branch: string, description: string }` per FR-C2C-3.2.
- Output: `{ "ok": true }`.
- Side Effects: `agent_registry` row updated (feature_description, branch columns).

**FR Coverage**: FR-C2C-3.2, FR-C2C-3.3, FR-C2C-3.4. **Acceptance: AC-C2C-2**.

### Alternative Flows

- **UC-C2C-2-A: Agent A calls `agent_describe` a second time with a different description** — the UPDATE is idempotent; the row is overwritten. The previous description is not preserved. `list-alive` shows only the latest description.
- **UC-C2C-2-B: `agent_describe` called with an empty description string** — handler accepts it (no NOT NULL constraint on the column per FR-C2C-1.1 `TEXT NULL`). Row is updated with empty string. List-alive shows an empty `feature_description`. This is valid behavior; the description may be intentionally cleared.

### Error Flows

- **UC-C2C-2-E1: Calling agent's `agent_id` is not found in `agent_registry`** (e.g., agent was unregistered between bridge connect and `agent_describe` call) — handler returns structured error `{ "error": "agent not found" }`; no DB write. CC #1 receives an MCP tool error response. The operator must re-register before calling `agent_describe`.

### Edge Cases

- **UC-C2C-2-EC1**: Concurrent `agent_describe` calls from two CC sessions registered as the same `agent_id` (R-C2C-3 race). The `UPDATE` is on the same row; SQLite serializes writes via its write lock. Last-write-wins. The surviving description is whichever write lands last. No data corruption. Documented as acceptable MVP behavior (plan.md §R-C2C-3).

---

## UC-C2C-3: Agent A Sends a Direct Message to Agent B; B Receives Within 2s

**Actor**: Agent A (in CC #1) sends; Daemon routes; Agent B (in CC #2) receives.

**Preconditions**:
- Daemon running; both CC #1 (agent A) and CC #2 (agent B) are alive in `agent_registry` with `state = 'alive'`.
- CC #2's bridge has auto-subscribed to thread `agent:<B_agent_id>` at connect time (FR-C2C-4.4).
- Agent B is NOT in DND (`dnd_until_ts IS NULL` or `dnd_until_ts < now()`).
- `agent_send` is in the TOOL_WHITELIST for CC #1.
- **Sender identity is connection-bound**: `from_agent_id` is resolved by the daemon from `agent_registry WHERE connection_id = <caller's connection_id>`, NOT from any caller-supplied `from` field in the request. This is enforced per FR-C2C-4.6 — the caller cannot spoof a different `agent_id` as the sender.

**Trigger**: Agent A (in CC #1) calls MCP tool `agent_send` with `{ to_agent_id: "<B_agent_id>", content: "I am about to touch bridge.rs in slice 4 — please hold off on that file." }`.

### Primary Flow (Happy Path)

1. CC #1's bridge sends the `agent_send` MCP tool call to the daemon.
2. Daemon's `handle_agent_send` handler verifies `to_agent_id` exists in `agent_registry`: `SELECT agent_id, dnd_until_ts FROM agent_registry WHERE agent_id = ?`. Row found; `dnd_until_ts` is NULL.
3. Daemon calls `chat_post` machinery to write a row to `chat_messages` with: `thread_id = 'agent:<B_agent_id>'`, `content = <message>`, `from_agent = <A_agent_id>`, `delivered_at = NULL` (pre-delivery). The `from_agent` is the daemon-resolved identity bound to the caller's `connection_id` — NOT a caller-supplied value.
4. Daemon emits a `notifications/claude/channel` notification with: `source = "claudebase"`, `chat_id = "<A_agent_id>"`, `thread = "agent:<B_agent_id>"`, `target_agent_id = "<B_agent_id>"`, `meta.kind = "agent-to-agent"`. The notification is emitted into the broadcast bus. (`thread` here is the notification envelope field — the DB column storing the same value is `chat_messages.thread_id`.)
5. Daemon updates `chat_messages.delivered_at = now()` for the message row (reusing the `delivered_at` tracking from commit `ccdf538`).
6. Daemon returns `{ "delivered": true }` to CC #1. Tool call resolves in under 2s (NFR-C2C-1).
7. CC #2's bridge receives the `notifications/claude/channel` event; `should_relay_channel_notification(target_agent_id)` returns true (target is `<B_agent_id>`, matching CC #2's own `agent_id`).
8. CC #2's bridge branches on `meta.kind`: value is `"agent-to-agent"` → renders the notification as `<agent-message from="<A_agent_id>" thread="agent:<B_agent_id>" ts="<ISO_timestamp>">I am about to touch bridge.rs in slice 4 — please hold off on that file.</agent-message>`. The `from` attribute is the daemon-bound identity, confirming sender cannot be spoofed.
9. Agent B (Claude Code in CC #2) reads the `<agent-message>` context; can acknowledge or act on it.

**Postconditions**:
- `chat_messages` row exists with `from_agent = '<A_agent_id>'` (daemon-bound, not caller-supplied), `thread_id = 'agent:<B_agent_id>'`, `delivered_at` non-NULL.
- CC #2 transcript shows the `<agent-message>` block with correct `from`, `thread`, and `ts` attributes.
- CC #1 received `{ "delivered": true }` from the tool call.
- Total round-trip wall time ≤ 2s on a local machine (NFR-C2C-1).

**Data Requirements**:
- Input: `{ to_agent_id: string, content: string, urgent?: boolean }` per FR-C2C-4.1.
- Output: `{ "delivered": true }`.
- Side Effects: one `chat_messages` row inserted (`thread_id = 'agent:<B_agent_id>'`, `from_agent` = daemon-bound caller identity, `delivered_at` set); one `notifications/claude/channel` emitted to CC #2's bridge subscription.

**FR Coverage**: FR-C2C-4.1 (a)(b)(c), FR-C2C-4.3, FR-C2C-4.4, FR-C2C-4.5, FR-C2C-8.1, FR-C2C-8.3. **Acceptance: AC-C2C-3**.

### Alternative Flows

- **UC-C2C-3-A: Agent A sends with `urgent: true`** — In MVP, `urgent` has no effect (FR-C2C-4.1 does not define urgent-override of DND). The message is delivered exactly as in the primary flow. The `urgent` field is accepted without error and silently ignored. Deferred per §20.8 Out of Scope.
- **UC-C2C-3-B: Agent A sends to itself (`to_agent_id = <A_agent_id>`)** — the primary flow executes identically; the message is posted to thread `agent:<A>` and the notification targets agent A. CC #1's own bridge receives and renders it. Unusual but not invalid — useful for self-messaging tests.

### Error Flows

- **UC-C2C-3-E1: `to_agent_id` not found in `agent_registry`** — handler returns structured error per FR-C2C-4.2: `{ "error": "agent not found", "agent_id": "<unknown_id>" }`. No message row is inserted; no notification emitted. CC #1 receives an MCP tool error. The operator sees the error in the CC #1 transcript. (Implements OQ-C2C-2 MVP default: fail loudly.)
- **UC-C2C-3-E2: Caller-supplied `from` field is silently overridden** — if the caller's `agent_send` request includes an explicit `from` field (e.g., `{ "to_agent_id": "B", "content": "...", "from": "spoofed-agent" }`), the daemon ignores the caller-supplied value entirely. The `from_agent` written to `chat_messages` and the `chat_id` in the outbound notification are ALWAYS resolved from `agent_registry WHERE connection_id = <caller's connection_id>`. The call succeeds normally (no error returned for the unexpected field); the spoofed `from` is silently discarded. CC #1 receives `{ "delivered": true }` but the `from` attribute in CC #2's `<agent-message>` tag reflects the daemon-verified identity, not the spoofed value. Per FR-C2C-4.6 (new).

### Edge Cases

- **UC-C2C-3-EC1**: CC #2's bridge subscription to `agent:<B_agent_id>` dropped between the auto-subscribe at connect time and the notification emit (e.g., due to a transient reconnect). Daemon emits the notification; no live subscription consumes it. The message IS persisted in `chat_messages` with `delivered_at` set (daemon set it after emit, not after confirmed receipt; `thread_id = 'agent:<B_agent_id>'`). CC #2 does not see the message in real time. On next bridge reconnect, CC #2 auto-subscribes again; pending messages are NOT replayed automatically (no session-cache replay per the frozen `chat_messages` drain semantics). Mitigation: operator can call `chat_list --thread agent:<B_id>` to retrieve backlog manually.

---

## UC-C2C-4: Agent B Sets DND; Messages Queue; DND Off Drains Queue to B

**Actor**: Agent B (in CC #2) sets DND; Agent A (in CC #1) sends; Daemon queues; DND expiry or explicit "off" triggers drain.

**Preconditions**:
- Daemon running; agents A and B alive and registered.
- `agent_set_dnd` is in TOOL_WHITELIST for CC #2.
- DND background drain task is running (30s poll interval; FR-C2C-5.2).

**Trigger**: Agent B (in CC #2) calls MCP tool `agent_set_dnd` with `{ state: "30m" }`.

### Primary Flow (Happy Path)

1. CC #2's bridge sends `agent_set_dnd` to daemon.
2. Daemon's `handle_agent_set_dnd` parses `"30m"`: computes `dnd_until_ts = now() + 1800` (Unix epoch integer).
3. Daemon executes `UPDATE agent_registry SET dnd_until_ts = ? WHERE agent_id = ?`. Returns `{ "ok": true, "dnd_until": "<ISO-8601 timestamp>" }` to CC #2. Resolves in under 2s (NFR-C2C-1).
4. Agent A (in CC #1) calls `agent_send` with `{ to_agent_id: "<B_agent_id>", content: "Q: are you free to review the store.rs PR?" }`.
5. Daemon's `handle_agent_send` checks `dnd_until_ts` for agent B: `dnd_until_ts > now()` → DND is active.
6. Daemon writes `chat_messages` row with `thread_id = 'agent:<B_agent_id>'`, `delivered_at = NULL` (message queued, per FR-C2C-4.1d).
7. Daemon does NOT emit a `notifications/claude/channel` notification. CC #2 receives nothing during the DND window.
8. Daemon returns `{ "queued": true, "delivered_when": "<B_dnd_until_ts as ISO-8601>" }` to CC #1. Tool call resolves in under 2s (FR-C2C-5.3).
9. [30m later, or earlier if `agent_set_dnd("off")` is called] — DND expires: either (a) the 30s background drain task polls and finds `dnd_until_ts < now()` for agent B, or (b) agent B explicitly calls `agent_set_dnd("off")` which clears `dnd_until_ts = NULL`.
10. Drain task: clears `dnd_until_ts` to NULL for agent B; queries `chat_messages WHERE thread_id = 'agent:<B_agent_id>' AND delivered_at IS NULL`; finds the queued message.
11. For each queued message: emits `notifications/claude/channel` with `target_agent_id = <B_agent_id>` and `meta.kind = "agent-to-agent"`; updates `delivered_at = now()`.
12. CC #2's bridge receives and renders `<agent-message from="<A>" ...>Q: are you free to review...</agent-message>`. Agent B sees the queued message.

**Postconditions**:
- `chat_messages` row has `delivered_at` non-NULL after drain.
- `agent_registry` row for B has `dnd_until_ts = NULL` after drain or explicit "off".
- CC #2 transcript shows the queued message only AFTER DND lifted, not during the DND window.
- Drain latency from DND expiry to notification delivery ≤ 30s for the first batch (NFR-C2C-2).
- **Rate-limit applies**: the drain task emits at most **10 channel notifications per 30s tick** (FR-C2C-5.5). With a single queued message this is immaterial; see UC-C2C-4-EC3 for the multi-message case.

**Data Requirements**:
- Input (set DND): `{ state: "30m" }`.
- Input (send while DND): `{ to_agent_id: "<B>", content: "..." }`.
- Output (send while DND): `{ "queued": true, "delivered_when": "<ISO-8601>" }`.
- Side Effects: `dnd_until_ts` written; `chat_messages` row with `delivered_at = NULL`; on drain, notification emitted and `delivered_at` set.

**FR Coverage**: FR-C2C-5.1, FR-C2C-5.2, FR-C2C-5.3, FR-C2C-5.4, FR-C2C-4.1 (d). **Acceptance: AC-C2C-4**.

### Alternative Flows

- **UC-C2C-4-A: Agent B calls `agent_set_dnd("off")` explicitly before the 30m expire** — handler executes `UPDATE agent_registry SET dnd_until_ts = NULL WHERE agent_id = ?`. Drain task picks up the cleared state on its next 30s tick and drains queued messages (subject to 10-msg/tick rate-limit; see UC-C2C-4-EC3). Latency = up to 30s after the "off" call.
- **UC-C2C-4-B: Multiple messages queued during DND window** — each `agent_send` adds a `chat_messages` row (`thread_id = 'agent:<B>'`) with `delivered_at = NULL`. On drain, queued messages are delivered as individual `notifications/claude/channel` events in insertion order, subject to the 10-msg/30s-tick rate-limit (FR-C2C-5.5). If count ≤ 10 they all arrive in the first tick. If count > 10, see UC-C2C-4-EC3.
- **UC-C2C-4-C: Agent B sets DND to `"on"` (indefinite)** — `dnd_until_ts` is set to `i64::MAX` (per OQ-UC-C2C-1 resolution). `NULL` means "no DND" — it is never the indefinite-DND encoding. Background drain task computes `dnd_until_ts < now()` which is always false for `i64::MAX`; the task therefore naturally never drains indefinite DND rows. Postcondition: `dnd_until_ts = 9223372036854775807` in `agent_registry`. Only explicit `agent_set_dnd("off")` (which writes `dnd_until_ts = NULL`) clears it.

### Error Flows

- **UC-C2C-4-E1: `agent_set_dnd` called with an unrecognized state string** (e.g., `"5d"` — days not in the spec) — handler returns structured error `{ "error": "invalid state", "state": "5d", "accepted_values": ["on", "off", "<N>m", "<N>h", "until HH:MM"] }`. No DB write. CC #2 receives an MCP tool error.

### Edge Cases

- **UC-C2C-4-EC1**: DND expires while the daemon is restarting (background drain task is not running). After daemon restart, the drain task re-initializes and runs its first 30s tick. It finds agent B's `dnd_until_ts < now()` and drains the queue. Queued messages are delivered with up to 30s delay after daemon restart — acceptable per NFR-C2C-2.
- **UC-C2C-4-EC3 (rate-limit honoured on large queued backlog)**: When agent B has 100 messages queued in `chat_messages` with `thread_id = 'agent:<B>' AND delivered_at IS NULL` and DND turns off, the drain task delivers at most 10 notifications in the first 30s tick. The next 10 are delivered in the second tick (30s later), and so on. Total drain time for 100 messages = 30s × ceil(100/10) = 300s (5 minutes). The `delivered_at` timestamp on each row is set when that specific message is emitted, so partially-delivered batches have a mix of NULL and non-NULL `delivered_at` values mid-drain. No message is dropped — drain continues until all `delivered_at IS NULL` rows for agent B are cleared.

---

## UC-C2C-5: ExitPlanMode Hook Fires; Agent Calls `agent_describe` and Updates Scratchpad

**Actor**: Operator exits plan mode in CC #1; Hook fires; Agent A (CC #1) calls `agent_describe` and updates scratchpad.

**Preconditions**:
- Installer has wired `hooks/claudebase-feature-describe.{sh,ps1}` into `~/.claude/settings.json` under `hooks.PostToolUse` with matcher `ExitPlanMode` (FR-C2C-7.3).
- `.claude/plan.md` exists in the project root and contains a non-blank first heading (e.g., `# Plan: claudebase cli-to-cli routing — agent-to-agent communication via daemon`).
- Agent A is registered with the daemon (`state = 'alive'`).
- CC #1 is in plan mode and has authored a feature plan.
- **Slice 7 pre-flight assumption**: The existence of the `PostToolUse:ExitPlanMode` hook event in the Claude Code hook schema MUST be verified against the actual `~/.claude/settings.json` schema BEFORE implementing Slice 7. If the `PostToolUse:ExitPlanMode` event does NOT exist in practice, the implementer MUST choose one of the documented fallbacks: (a) `UserPromptSubmit` with prev-turn-ExitPlanMode detection, (b) `Stop` hook with content-marker check, or (c) operator-driven (degraded mode). The fallback choice is recorded under UC-C2C-5-C.

**Trigger**: Operator calls `ExitPlanMode` in CC #1 (either directly or via the Claude Code plan-mode exit UI).

### Primary Flow (Happy Path)

1. Claude Code fires the `PostToolUse` event with tool name `ExitPlanMode`.
2. The `PostToolUse` hook script (`claudebase-feature-describe.sh` on Unix / `claudebase-feature-describe.ps1` on Windows) executes.
3. Hook reads `.claude/plan.md`; extracts the first heading line (strips the `#` prefix and leading whitespace) → feature title `"Plan: claudebase cli-to-cli routing — agent-to-agent communication via daemon"`.
4. Hook injects `additionalContext` into the CC #1 session: a system reminder mandating the agent to (a) call `agent_describe(feature_id, branch, description)` and (b) update `.claude/scratchpad.md` `## Feature:` line to match — BOTH in the same turn (FR-C2C-7.2).
5. Agent A (Claude Code in CC #1) receives the `additionalContext`; calls `agent_describe` via MCP with the extracted feature title and current branch.
6. Daemon updates `agent_registry` row for agent A: `feature_description = <extracted title>`, `branch = <current branch>`.
7. Agent A edits `.claude/scratchpad.md`, updating the `## Feature:` line to match.
8. Both writes succeed; agent A reports completion in the CC #1 turn.

**Postconditions**:
- `agent_registry.feature_description` for agent A equals the extracted plan title.
- `.claude/scratchpad.md` `## Feature:` line matches the daemon's `feature_description`.
- Hook executed without error; session transcript contains evidence of both writes in the same agent turn.

**Data Requirements**:
- Input: `.claude/plan.md` first heading.
- Output: `additionalContext` injection; `agent_describe` MCP call; scratchpad update.
- Side Effects: `agent_registry` row updated; `.claude/scratchpad.md` written.

**FR Coverage**: FR-C2C-7.1, FR-C2C-7.2, FR-C2C-7.3, FR-C2C-7.4, FR-C2C-3.2. **Acceptance: AC-C2C-5**.

### Alternative Flows

- **UC-C2C-5-A: `.claude/plan.md` does not exist or has no heading** — per FR-C2C-7.4, the hook emits an empty `additionalContext` (or a minimal "no plan found" note). No `agent_describe` is mandated. No error or crash. The agent turn proceeds normally without the feature-description update. This is a graceful no-op.
- **UC-C2C-5-B: Re-running installer (idempotency)** — installer's dedup-by-command-string logic (FR-C2C-7.3) detects that the hook script command is already wired in `~/.claude/settings.json`. No new entry is added. File is unchanged. Installer exits 0.
- **UC-C2C-5-C: Fallback hook mode (primary `PostToolUse:ExitPlanMode` unavailable)** — If the pre-flight check at Slice 7 start determines that the `PostToolUse:ExitPlanMode` event does not fire reliably in the target CC version, the implementer switches to the verified fallback. Steps 1–2 differ: instead of a `PostToolUse` event, the fallback hook event (e.g., `UserPromptSubmit` with a prev-turn detector, or a `Stop` hook with a content-marker check) fires instead. Steps 3–8 are IDENTICAL to the primary flow — the hook reads `.claude/plan.md`, injects `additionalContext`, agent A calls `agent_describe`, scratchpad is updated. Postconditions are identical to the primary flow. This alternative flow documents that the observable outcome (feature description updated in daemon + scratchpad) is invariant regardless of which hook event is the trigger.

### Error Flows

- **UC-C2C-5-E1: `agent_describe` MCP call fails inside the hook-triggered turn** (e.g., daemon is not running) — agent A receives an MCP tool error in the CC #1 turn. The scratchpad may or may not have been updated (depends on ordering). Agent A should surface the failure to the operator. The hook itself does not retry; the operator must re-run `agent_describe` manually.

### Edge Cases

- **UC-C2C-5-EC1**: Operator exits plan mode multiple times in the same session (edits plan, re-enters, exits again). The hook fires each time. Each firing may update `feature_description` with the latest plan title. Idempotent — no cumulative side effect.
- **UC-C2C-5-EC2**: `.claude/plan.md` first heading is very long (> 200 characters). Hook truncates or passes the full string to `agent_describe`. `feature_description` column is `TEXT` — no length limit in SQLite. No truncation needed; full string is stored.

---

## UC-C2C-6: Operator in a Non-Git Folder — `project_id` Fallback Chain

**Actor**: Operator (CC session or CLI invocation) in a cwd with no `.git` directory and no `.claudebase/config.json`.

**Preconditions**:
- The cwd has no `.git` directory (or `git config --get remote.origin.url` exits non-zero).
- There is no `.claudebase/config.json` file in the cwd.
- `src/project_id.rs` `resolve_project_id` module is compiled and available.

**Trigger**: `resolve_project_id(cwd)` is called — either at `agent_register` time (daemon receives a register call from a CC in this cwd) or at `claudebase agent list-alive --project current` time.

### Primary Flow (Happy Path)

1. `resolve_project_id(cwd)` invokes Step 1: runs `git -C <cwd> config --get remote.origin.url`. Command exits non-zero (not a git repo or no `origin` remote). Step 1 fails.
2. Step 2: attempts to read `<cwd>/.claudebase/config.json`. File does not exist. Step 2 fails.
3. Step 3: computes `sha256(canonical_absolute_path(cwd))[..16]` as a hex string; prefixes with `local:`. Returns e.g. `local:a3b4c5d6e7f80123`.
4. This value is used as `project_id` for the agent's `agent_registry` row or for the `list-alive` scope filter.

**Postconditions**:
- `project_id` is a 22-character string matching the pattern `local:[0-9a-f]{16}`.
- Two agents in the SAME non-git cwd (same canonical absolute path) produce the SAME `project_id` and will discover each other via `list-alive --project current`.
- Two agents in DIFFERENT non-git cwds produce DIFFERENT `project_id` values and will NOT see each other.

**FR Coverage**: FR-C2C-2.2 (Step 3), FR-C2C-2.3 (no-git-repo test case).

### Alternative Flows

- **UC-C2C-6-A: No git repo but `.claudebase/config.json` exists with `project_id` field** — Step 1 fails; Step 2 succeeds: reads `project_id` from the JSON. Returns the config-specified value. Operator can use this to force two unrelated directories to share a project scope (e.g., a monorepo split into two top-level folders).
- **UC-C2C-6-B: `.claudebase/config.json` exists but `project_id` field is absent or empty** — Step 2 fails (field absent or empty string). Falls through to Step 3. Returns the path-hash `local:*` string.

### Error Flows

- **UC-C2C-6-E1: `canonical_absolute_path(cwd)` fails** (e.g., cwd was deleted between call time and resolution) — `resolve_project_id` returns a deterministic fallback (e.g., `local:unknown`) rather than panicking. Caller logs a warning. The agent registers but with an unreliable `project_id`. No crash.

### Edge Cases

- **UC-C2C-6-EC1**: Two different cwds that normalize to the same canonical absolute path (symlink resolution). `sha256(canonical_absolute_path(cwd))` resolves symlinks before hashing. Both cwds produce the same `project_id`. Agents in both paths see each other in `list-alive`. This is correct and intended behavior.

---

## UC-C2C-7: Multiple Agents in Same Cwd — Last-Write-Wins on `agent_id` Collision

**Actor**: Two CC sessions (CC #1 and CC #2) both launched from the exact same cwd, both calling `agent_register` with the same default `agent_id` (derived from cwd basename per existing `derive_agent_id` convention).

**Preconditions**:
- Daemon running.
- CC #1 has registered as agent `claudebase` (cwd basename = `claudebase`).
- CC #2 is also launched from the SAME cwd (`C:\Users\madwh\Documents\claudebase`) and calls `agent_register("claudebase", ...)`.
- `agent_register` uses `ON CONFLICT(agent_id) DO UPDATE` semantics (existing behavior).

**Trigger**: CC #2 calls `agent_register` with `agent_id = "claudebase"`.

### Primary Flow (Happy Path)

1. CC #2's bridge sends `agent_register("claudebase", ...)` to daemon.
2. Daemon's `handle_agent_register` executes `INSERT INTO agent_registry ... ON CONFLICT(agent_id) DO UPDATE SET connection_id = excluded.connection_id, last_pinged_at = excluded.last_pinged_at, state = excluded.state, cwd = excluded.cwd, routing_chat_id = excluded.routing_chat_id, pid = excluded.pid`.
3. CC #1's row is overwritten in place with CC #2's connection details. CC #1 effectively loses its registration — notifications for `agent_id = "claudebase"` will route to CC #2's `connection_id`.
4. Daemon returns `{ "ok": true }` to CC #2.
5. `agent_list_alive` shows one row for `agent_id = "claudebase"` with `state = 'alive'` (CC #2's details).

**Postconditions**:
- Single row in `agent_registry` for `agent_id = "claudebase"` with `state = 'alive'`.
- `connection_id` and `last_pinged_at` are CC #2's values. CC #1's routing is now broken (notifications addressed to `target_agent_id = "claudebase"` go to CC #2's `connection_id`).
- This is documented acceptable MVP behavior (plan.md §R-C2C-3, last-write-wins).

**FR Coverage**: FR-C2C-3.1 (register extension). Risk: **R-C2C-3**.

### Alternative Flows

- **UC-C2C-7-A: Operator explicitly differentiates the two sessions** — operator forces each CC to use a unique `agent_id` (e.g., `claudebase-1` and `claudebase-2`) by setting `CLAUDEBASE_AGENT_ID` or by calling `agent_register` with a custom name. Both register as distinct rows. No collision; both visible in `list-alive`.

### Edge Cases

- **UC-C2C-7-EC1**: CC #1 calls `agent_describe` AFTER CC #2 has overwritten the registration. CC #1's description update lands on the row, which is now "owned" by CC #2's connection details. CC #2 sees the updated `feature_description` as if it had published it. No crash; semantically confusing to the operator but not a correctness failure.

---

## UC-C2C-8: Git Worktree — Same `project_id`, Different `branch` and `working_dir`

**Actor**: Operator running CC #1 in the main clone (`C:\…\claudebase`, branch `main`) and CC #2 in a git worktree of the same repo (`C:\…\claudebase-wt-feat`, branch `feat/multi-agent-on-v0.6`).

**Preconditions**:
- Both directories share the same `git remote origin URL` (same logical repo).
- Both have `agent_register` called; `resolve_project_id` runs successfully for each.

**Trigger**: Both CC sessions call `agent_register` (at bridge connect time); `resolve_project_id` is invoked for each cwd.

### Primary Flow (Happy Path)

1. CC #1 in `C:\…\claudebase`: `git config --get remote.origin.url` returns `git@github.com:codefather-labs/claudebase.git`. Normalizes to `github.com/codefather-labs/claudebase`.
2. CC #2 in `C:\…\claudebase-wt-feat`: worktrees share the same `.git` directory and the same `remote.origin.url`. Same normalization result: `github.com/codefather-labs/claudebase`.
3. Both agents registered in `agent_registry` with `project_id = "github.com/codefather-labs/claudebase"`, but `working_dir` differs (clone path vs. worktree path) and `branch` differs (`main` vs. `feat/multi-agent-on-v0.6`).
4. `claudebase agent list-alive --project current` from either cwd returns BOTH agents.

**Postconditions**:
- Both agents visible under the same `project_id`.
- `working_dir` and `branch` columns differentiate which agent is in which worktree.

**FR Coverage**: FR-C2C-2.2 (Step 1), FR-C2C-2.3 (git worktree test case), FR-C2C-6.4, FR-C2C-6.5.

### Edge Cases

- **UC-C2C-8-EC1**: Operator has a worktree for a FORK (different origin URL). `resolve_project_id` returns a different `project_id` than the canonical clone. The two agents do NOT see each other via `list-alive --project current`. Mitigation: operator uses `.claudebase/config.json::project_id` override to force the same scope. Documented risk R-C2C-1.

---

## UC-C2C-9: `agent_send` to Non-Existent Agent ID — Fails Loudly

**Actor**: Agent A (in CC #1) attempts to send a message to an agent that does not exist.

**Preconditions**:
- Daemon running; agent A registered.
- No agent with `agent_id = "ghost-agent"` exists in `agent_registry` (or it was unregistered before the send).

**Trigger**: Agent A calls `agent_send` with `{ to_agent_id: "ghost-agent", content: "hello?" }`.

### Primary Flow

1. Daemon's `handle_agent_send` executes `SELECT agent_id, dnd_until_ts FROM agent_registry WHERE agent_id = ?` with `"ghost-agent"`. Result: empty set.
2. Handler returns structured error per FR-C2C-4.2: `{ "error": "agent not found", "agent_id": "ghost-agent" }`. HTTP/MCP error response.
3. No `chat_messages` row is inserted; no notification emitted.
4. CC #1 receives the MCP tool error in the `agent_send` tool call response.
5. Agent A surfaces the error to the operator: "agent_send failed — ghost-agent is not registered."

**Postconditions**:
- No side effects in `chat_messages` or notification bus.
- CC #1 error response contains `"agent not found"`.
- Daemon log records the failed lookup.

**FR Coverage**: FR-C2C-4.2. Implements **OQ-C2C-2** MVP default (fail loudly).

### Edge Cases

- **UC-C2C-9-EC1**: Agent B was alive when agent A composed the message, but deregistered (crashed or `agent_unregister` called) in the milliseconds between the compose and the `agent_send` call. Same error flow. The message is lost — no retry queue for unknown agents. The operator must re-send after agent B re-registers.

---

## UC-C2C-10: Bridge Auto-Subscribe Failure — Log and Continue; Reconnect Retries

**Actor**: CC #1's bridge (`src/plugin/bridge.rs`) attempts auto-subscribe to thread `agent:<A_agent_id>` at connect time.

**Preconditions**:
- CC #1 is connecting to the daemon (initial connect or reconnect).
- `chat.db` is temporarily unavailable (e.g., locked by a concurrent process, corrupt read, or file-system issue) when the subscribe handler runs.

**Trigger**: Bridge initial connect completes; bridge auto-subscribe logic calls the daemon's `chat_subscribe` handler for thread `agent:<A_agent_id>`.

### Primary Flow

1. Bridge sends `chat_subscribe(thread: "agent:<A_agent_id>")` to daemon.
2. Daemon's subscribe handler attempts to access `chat.db`; encounters an error (lock timeout / IO error).
3. Daemon logs the error: `bridge auto-subscribe failed for thread agent:<A_agent_id>: <error message>`. Returns error response to bridge.
4. Bridge receives the error; logs `warn: auto-subscribe for agent-inbox failed; inbound agent-to-agent messages will not be delivered until reconnect`. Bridge does NOT abort the connection — it continues to function for other MCP calls (Telegram, CLI, etc.).
5. Bridge's existing reconnect logic (`try_reconnect`) eventually re-fires. On reconnect, auto-subscribe is attempted again (per FR-C2C-4.4 which requires auto-subscribe at "initial bridge connection"). If the second attempt succeeds, agent-to-agent messages resume.

**Postconditions**:
- CC #1 remains connected and functional for non-agent-message flows.
- Agent-to-agent messages sent to agent A during the window of failed subscribe are NOT delivered to CC #1 (they may be queued in `chat_messages` if the sender used `agent_send` — but without an active subscription, CC #1 won't receive the notification).
- Reconnect attempt eventually restores the subscription.

**FR Coverage**: FR-C2C-4.4. Risk: **R-C2C-7** (bridge auto-subscribe wiring).

### Error Flows

- **UC-C2C-10-E1: Daemon is entirely unreachable** (not just `chat.db`) — bridge's entire connect attempt fails at the transport layer. Existing `try_reconnect` loop handles this (pre-existing behavior, not new). Agent-to-agent subscribe is not separately visible in this case.

---

## UC-C2C-11: DND Drain Background Task Encounters `chat.db` Error — Log-and-Swallow; Retry Next Tick

**Actor**: Daemon's DND drain background task.

**Preconditions**:
- Daemon running; DND drain task is active (30s poll interval per FR-C2C-5.2).
- Agent B's `dnd_until_ts < now()` — DND has expired; queued messages await delivery.
- During the drain task's tick, `chat.db` encounters a transient I/O error (lock contention, momentary unavailability).

**Trigger**: DND drain background task fires its 30s tick; attempts to drain queued messages for agent B.

### Primary Flow

1. Task executes `SELECT agent_id FROM agent_registry WHERE dnd_until_ts IS NOT NULL AND dnd_until_ts < now()`. Returns agent B.
2. Task attempts to update `dnd_until_ts = NULL` for agent B and drain `chat_messages WHERE thread_id = 'agent:<B>' AND delivered_at IS NULL`. `chat.db` write fails with an I/O error.
3. Task logs: `warn: DND drain tick failed for agent <B>: <error>`. Swallows the error (does NOT propagate or crash the background task).
4. Task continues with any other expired DND agents in the same tick (if any). Subsequent agents in the tick may succeed.
5. Agent B's `dnd_until_ts` remains set (was NOT cleared because the update failed).
6. Next 30s tick: task retries. If `chat.db` has recovered, the drain proceeds normally: clears `dnd_until_ts`, emits queued notifications.

**Postconditions**:
- Background task remains alive across the transient error.
- Agent B's queued messages are drained on the next successful tick.
- No crash, no daemon restart required.
- Drain latency from DND expiry may exceed 30s by exactly one tick (up to 60s total) in the transient-error case. Still within NFR-C2C-2's intent ("at most 30s under normal conditions").

**FR Coverage**: FR-C2C-5.2 (drain pattern). Analogous to `drain_pending_outbound_tg` behavior from commit `ccdf538`.

### Edge Cases

- **UC-C2C-11-EC1**: Repeated `chat.db` errors across multiple ticks. Agent B's `dnd_until_ts` remains set indefinitely. Queued messages accumulate. Operator may notice the DND state is stuck via `list-alive` output. Recovery: operator calls `agent_set_dnd("off")` explicitly (which writes `dnd_until_ts = NULL` directly) — this write may also fail if `chat.db` is persistently corrupt, in which case the operator must inspect and repair `chat.db` manually.

---

## UC-C2C-12: SSH and HTTPS URLs for the Same Repo Normalize to the Same `project_id`

**Actor**: Two operators (or one operator with two clone methods) — CC #1 cloned via HTTPS, CC #2 cloned via SSH.

**Preconditions**:
- Clone A: `git config --get remote.origin.url` returns `https://github.com/codefather-labs/claudebase.git`.
- Clone B: `git config --get remote.origin.url` returns `git@github.com:codefather-labs/claudebase.git`.
- Both are the same logical repo.

**Trigger**: `resolve_project_id(cwd)` called for each clone at `agent_register` time.

### Primary Flow

1. Clone A: Step 1 strips `https://` prefix → `github.com/codefather-labs/claudebase.git`; strips `.git` suffix → `github.com/codefather-labs/claudebase`; lowercases → `github.com/codefather-labs/claudebase`.
2. Clone B: Step 1 strips `git@` prefix → `github.com:codefather-labs/claudebase.git`; replaces `:` with `/` → `github.com/codefather-labs/claudebase.git`; strips `.git` suffix → `github.com/codefather-labs/claudebase`; lowercases → `github.com/codefather-labs/claudebase`.
3. Both resolve to identical `project_id = "github.com/codefather-labs/claudebase"`.
4. `agent list-alive --project current` from either clone shows BOTH agents.

**Postconditions**:
- `project_id` is identical for both clones regardless of URL protocol.
- Cross-protocol discovery works without operator configuration.

**FR Coverage**: FR-C2C-2.2 (HTTPS normalization + SSH colon-separator normalization), FR-C2C-2.3 (HTTPS URL test, SSH URL test, `.git` strip test, case normalization test).

### Edge Cases

- **UC-C2C-12-EC1**: Mixed-case URL components (e.g., `git@GitHub.COM:OWNER/Repo.git`). Full lowercase normalization ensures `project_id = "github.com/owner/repo"`. Matches the lower-case form from a standard HTTPS clone. Discovery works correctly.

---

## UC-C2C-13: No `origin` Remote — Fallback Chain Engaged

**Actor**: Operator running CC in a git repo that has no `origin` remote (e.g., a local-only repo initialized with `git init` but no `git remote add origin ...`).

**Preconditions**:
- `git init` was run; no `git remote add origin` was run.
- No `.claudebase/config.json` in the cwd.

**Trigger**: `resolve_project_id(cwd)` called (at `agent_register` or `list-alive` time).

### Primary Flow

1. Step 1: `git -C <cwd> config --get remote.origin.url` — exits with code 1 (no origin remote). Step 1 fails.
2. Step 2: reads `<cwd>/.claudebase/config.json`. File absent. Step 2 fails.
3. Step 3: computes `sha256(canonical_absolute_path(cwd))[..16]`; returns `local:<16-hex-chars>`.

**Postconditions**:
- Agent registers with a path-hash `project_id`.
- Two CC sessions in the SAME cwd-path (no origin) will discover each other.
- Any CC session in a DIFFERENT cwd will NOT see this agent via `--project current`.
- No error, no crash.

**FR Coverage**: FR-C2C-2.2 (Steps 1, 2, 3), FR-C2C-2.3 (no-git-repo fallback test case).

### Alternative Flows

- **UC-C2C-13-A: Git repo exists with a remote named something other than `origin`** (e.g., `upstream`) — `git config --get remote.origin.url` still fails (it specifically queries `remote.origin.url`). Falls through to Step 2, then Step 3. This is a known limitation; documented in R-C2C-1 / FR-C2C-2.3.

---

## UC-C2C-14: DND `"until HH:MM"` — Parser Handles Local Timezone Correctly

**Actor**: Agent B (in CC #2) sets DND using a wall-clock time string.

**Preconditions**:
- Current local time is known (e.g., 14:30 local).
- `agent_set_dnd` is in TOOL_WHITELIST.

**Trigger**: Agent B calls `agent_set_dnd` with `{ state: "until 17:00" }`.

### Primary Flow

1. Daemon's `handle_agent_set_dnd` receives `state = "until 17:00"`.
2. Parser identifies the `"until HH:MM"` pattern.
3. Parser resolves the wall-clock time in the DAEMON'S local timezone (the machine's local timezone setting). Computes the next occurrence of 17:00 local time:
   - If current local time < 17:00 today: target = today at 17:00 local.
   - If current local time >= 17:00 today: target = tomorrow at 17:00 local (midnight rollover case).
4. Converts the target local time to a Unix epoch integer: `dnd_until_ts`.
5. Writes to `agent_registry`.
6. Returns `{ "ok": true, "dnd_until": "2026-06-05T17:00:00+02:00" }` (ISO-8601 with offset).

**Postconditions**:
- `dnd_until_ts` in `agent_registry` represents 17:00 local on the correct calendar day.
- DND drain task will expire the DND at or after 17:00 local (within 30s).

**FR Coverage**: FR-C2C-5.1 (`"until HH:MM"` variant).

### Edge Cases

- **UC-C2C-14-EC1: `"until HH:MM"` crosses midnight** — operator at 23:45 local sets `"until 01:00"`. The target is 01:00 the NEXT day (not 01:00 today, which has already passed). Parser MUST detect that `01:00 today < current time` and roll over to tomorrow's 01:00. Incorrect handling would compute a `dnd_until_ts` 22h45m in the PAST, causing immediate DND expiry.
- **UC-C2C-14-EC2: Daemon's local timezone differs from operator's local timezone** — the daemon uses its own process timezone. If the daemon is running in UTC (e.g., a server), "until 17:00" is interpreted as 17:00 UTC, not 17:00 in the operator's local timezone. This is a known limitation. Mitigation: document that `"until HH:MM"` is the daemon's local time; operators on machines where the daemon timezone matches theirs (typical single-user local-machine scenario) are unaffected.
- **UC-C2C-14-EC3: Daylight saving time boundary** — `"until 02:30"` on a day when DST spring-forward skips 02:00–03:00 locally. The wall clock never reaches 02:30 — it jumps from 02:00 to 03:00. Parser behavior at this boundary is implementation-specific. Flagged as an open question; must be tested explicitly in `tests/agent_dnd_test.rs`.

---

## UC-C2C-15: Telegram Inbound Rendering Is Unaffected After Feature Lands (Regression Safety)

**Actor**: Operator (Telegram); Bot `@X`; Daemon; CC #1 with its bridge.

**Preconditions**:
- All Slices 1-8 of cli-to-cli-routing have landed.
- The Telegram bridge integration is intact; bot is polling.
- An inbound Telegram DM arrives at the daemon.

**Trigger**: Operator sends "hello" via Telegram DM to bot `@X`.

### Primary Flow

1. Daemon receives Telegram `Update::Message`; routes to the correct CLI session via the existing `should_relay_channel_notification(target_agent_id)` filter (unchanged).
2. Bridge receives the `notifications/claude/channel` notification.
3. Bridge branches on `meta.kind`: field is ABSENT (Telegram messages do not set `meta.kind`) → falls through to Telegram `<channel>` rendering path (FR-C2C-8.2 regression requirement).
4. Bridge renders: `<channel source="plugin:telegram:telegram" chat_id="..." user="..." message_id="...">hello</channel>`.
5. CC #1 renders the `<channel>` block normally.

**Postconditions**:
- Telegram inbound renders as `<channel>` — NOT as `<agent-message>`.
- All existing Telegram functionality (chat_post, chat_reply, chat_subscribe, `/start`, `/agents`, `/switch`) remains fully functional.
- Existing 178+ tests continue to pass (NFR-C2C-5).

**FR Coverage**: FR-C2C-8.2, FR-C2C-8.4 (regression test cases a and d). **NFR-C2C-5 compliance**.

### Edge Cases

- **UC-C2C-15-EC1**: Inbound notification with `meta.kind` set to ANY value other than the literal string `"agent-to-agent"` — including unknown future values, empty string, or any unrecognized string — falls through to `<channel>` rendering. The implementer MUST NOT treat unknown `meta.kind` values as a discriminated-union error (no `panic!`, no structured error response, no dropped notification). The check is a simple equality test: `if meta.kind == "agent-to-agent" { render as <agent-message> } else { render as <channel> }`. This ensures forward compatibility with future `meta.kind` values this version of the bridge does not yet know about (FR-C2C-8.4d).

---

## UC-C2C-16: Register-Time Identity Capture — `project_id`, `branch`, `working_dir` Persist on Register

**Actor**: Agent A (CC #1) calling `agent_register` at bridge connect time.

**Preconditions**:
- Schema is at v6 (C2C routing migration applied — columns `routing_chat_id`, `routing_thread_id`, `last_user_id`, `host`, `cwd`, `pid` are present in `agent_registry`; plus the C2C extension columns `project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts` per FR-C2C-1.1).
- CC #1 is in cwd `C:\Users\madwh\Documents\claudebase`, branch `feat/multi-agent-on-v0.6`.
- `git remote origin URL` resolves to `github.com/codefather-labs/claudebase`.

**Trigger**: CC #1's bridge fires `agent_register` on initial connect.

### Primary Flow

1. `handle_agent_register` receives the call.
2. Handler calls `resolve_project_id(cwd)` — returns `github.com/codefather-labs/claudebase`.
3. Handler calls `git rev-parse --abbrev-ref HEAD` in the cwd — returns `feat/multi-agent-on-v0.6`.
4. Handler writes to `agent_registry`: `cwd = "C:\Users\madwh\Documents\claudebase"` (routing migration column), `project_id = "github.com/codefather-labs/claudebase"` (C2C extension column), `branch = "feat/multi-agent-on-v0.6"` (C2C extension column), `working_dir = "C:\Users\madwh\Documents\claudebase"` (C2C extension column — same value as `cwd`). `connection_id` and `last_pinged_at` are set from the live connection. `state = 'alive'`.
5. Returns `{ "ok": true }` to CC #1.

**Postconditions**:
- `agent_registry` row contains non-NULL `cwd`, `connection_id`, `last_pinged_at`, `state = 'alive'` for agent A.
- C2C extension columns non-NULL: `project_id`, `branch`, `working_dir` (set from register-time context).
- `feature_description` is NULL (set later by `agent_describe`).
- `dnd_until_ts` is NULL.

**FR Coverage**: FR-C2C-3.1, FR-C2C-3.4 (round-trip test precondition).

### Error Flows

- **UC-C2C-16-E1: `git rev-parse --abbrev-ref HEAD` returns `HEAD`** (detached HEAD state) — handler stores `branch = "HEAD"` literally. No error. `list-alive` shows the agent with `branch = "HEAD"`. Operator can interpret this.

---

## UC-C2C-17: Schema Migration v5→v6 (C2C routing columns) Is Idempotent

**Actor**: Daemon (startup migration logic in `src/daemon/chat.rs::apply_agent_registry_c2c_migration`, called from `ensure_chat_db_schema`).

**Preconditions**:
- Daemon starts against a `chat.db` that already has the base `agent_registry` table at v5 (columns per `src/daemon/chat.rs:443-453`: `agent_id PK`, `agent_name`, `connection_id`, `chat_thread_id`, `permission_relayer`, `spawned_at`, `last_pinged_at`, `state CHECK('alive'/'orphaned'/'dead')`, `metadata`). The columns `routing_key`, `last_seen_at`, `registered_at` do NOT exist in the actual v5 schema.
- The routing migration has NOT yet been applied (columns `routing_chat_id`, `routing_thread_id`, `last_user_id`, `host`, `cwd`, `pid` are absent from `agent_registry`).

**Trigger**: Daemon startup; `ensure_chat_db_schema(conn)` in `src/daemon/chat.rs` runs, which calls `apply_routing_migration(conn)` (this is the C2C routing migration, i.e., `apply_agent_registry_c2c_migration` per task naming).

### Primary Flow

1. `ensure_chat_db_schema` runs its `BEGIN/COMMIT` DDL block creating `chat_threads`, `chat_messages`, `daemon_state`, and the base `agent_registry` (all `IF NOT EXISTS` — idempotent). Then calls `apply_routing_migration(conn)`.
2. `apply_routing_migration` iterates over six column definitions: `routing_chat_id INTEGER`, `routing_thread_id INTEGER CHECK(...)`, `last_user_id INTEGER`, `host TEXT`, `cwd TEXT`, `pid INTEGER`. For each, probes `pragma_table_info('agent_registry')` to check if the column already exists.
3. For each absent column: executes `ALTER TABLE agent_registry ADD COLUMN {col} {decl}` within a wrapping `BEGIN/COMMIT` transaction. Guards ensure no partial-state if daemon crashes mid-ALTER.
4. Creates `CREATE UNIQUE INDEX IF NOT EXISTS agent_registry_routing_alive_uniq_idx ON agent_registry(routing_chat_id, COALESCE(routing_thread_id, -1)) WHERE state = 'alive' AND routing_chat_id IS NOT NULL`.
5. Migration commits; daemon proceeds normally.

**Postconditions**:
- All six routing columns exist: `routing_chat_id`, `routing_thread_id`, `last_user_id`, `host`, `cwd`, `pid`.
- Index `agent_registry_routing_alive_uniq_idx` exists.
- Existing rows have `NULL` for all six new columns (SQLite `ALTER TABLE ADD COLUMN DEFAULT NULL` semantics).
- No `src/store.rs` involved — migration runs entirely within `src/daemon/chat.rs`.

**FR Coverage**: FR-C2C-1.1, FR-C2C-1.2, FR-C2C-1.3, FR-C2C-1.4, FR-C2C-1.5.

**Note on C2C extension columns** (`project_id`, `branch`, `working_dir`, `feature_description`, `dnd_until_ts`): these are separate from the routing migration columns. They are added by a SEPARATE migration step also in `src/daemon/chat.rs` (the C2C feature extension, FR-C2C-1.1). UC-C2C-17 covers the routing-column migration (`apply_routing_migration`); the C2C extension migration follows the same probe-then-ALTER pattern and is also idempotent.

### Alternative Flows

- **UC-C2C-17-A: Migration runs a second time against an already-migrated schema** — each `ALTER TABLE ADD COLUMN` is guarded by a `pragma_table_info` probe; existing columns are detected; no DDL is executed. The `CREATE UNIQUE INDEX IF NOT EXISTS` is also idempotent. `apply_routing_migration` exits cleanly. No error, no duplicate columns. This is the idempotency requirement per FR-C2C-1.1. Source-level evidence: `src/daemon/chat.rs:525-539` (exists-probe before every ALTER).

---

## UC-C2C-18: `<agent-message>` Tag Contains Correct Attributes

**Actor**: CC #2's bridge receives an agent-to-agent notification.

**Preconditions**:
- Agent A has sent a message to agent B via `agent_send` (UC-C2C-3 primary flow completed up to step 4).
- Notification reaches CC #2's bridge; `meta.kind = "agent-to-agent"`.

**Trigger**: Bridge's notification handler processes the `notifications/claude/channel` event.

### Primary Flow

1. Bridge reads the notification envelope; checks `meta.kind` = `"agent-to-agent"`.
2. Bridge renders: `<agent-message from="<chat_id>" thread="<thread>" ts="<timestamp>">CONTENT</agent-message>`.
3. Attributes populated from notification meta: `from` = `meta.chat_id`, `thread` = `meta.thread`, `ts` = message timestamp.

**Postconditions**:
- `<agent-message>` tag has all three required attributes (`from`, `thread`, `ts`) per FR-C2C-8.3.
- Content is the original message text.
- CC #2's agent (Claude Code model) receives the tagged block and can read the `from` attribute to identify the sender.

**FR Coverage**: FR-C2C-8.1, FR-C2C-8.3, FR-C2C-8.4 (cases b and c).

### Edge Cases

- **UC-C2C-18-EC1**: `meta.ts` timestamp is absent from the notification (e.g., daemon omitted it). Bridge renders `ts=""` (empty string attribute) rather than omitting the attribute or crashing. Downstream model handles empty timestamp gracefully — it reads `from` for sender provenance and ignores the empty timestamp.

---

## Facts

### Verified facts

- PRD §20 lines 1604–1857 read in full this session via Read tool, offset 1604 limit 254 — source: Read tool invocation this session — salience: high.
- `.claude/plan.md` lines 1–233 read in full this session via Read tool — source: Read tool invocation this session — salience: high.
- `multi-agent-telegram-on-v0.6_use_cases.md` format inspected (lines 1–120) — confirms: slug-prefixed UC IDs (UC-MAT-N), FR Coverage + Acceptance lines, Data Requirements section, Alternative/Error/Edge Case subsections — source: Read tool invocation this session — salience: medium.
- Existing use-case files in `docs/use-cases/`: `agent-chat-daemon_use_cases.md`, `agent-insights-base_use_cases.md`, `multi-agent-telegram-on-v0.6_use_cases.md`, `claudebase-v0.9-cut_use_cases.md` — source: Glob tool this session — salience: low.
- Knowledge base for this project has 0 documents (`doc_count: 0, chunk_count: 0`) — source: `claudebase status --json` this session — salience: low.
- Insights corpus query returned exit 1 with a vector search error (`no such column: to`); insights.db may have a schema regression — no prior-session insights cited; corpus was not relied upon — source: Bash tool invocation this session — salience: medium.
- PRD FR-C2C-1.1: v5→v6 migration adds `project_id TEXT`, `branch TEXT`, `working_dir TEXT`, `feature_description TEXT NULL`, `dnd_until_ts INTEGER NULL` — guarded by `PRAGMA table_info` probe, idempotent — source: PRD §20 line 1629–1633 read this session — salience: high.
- PRD FR-C2C-2.2: three-step fallback chain: Step 1 git remote URL normalize, Step 2 `.claudebase/config.json::project_id`, Step 3 `sha256(cwd)[..16]` hex with `local:` prefix — source: PRD §20 line 1638–1641 read this session — salience: high.
- PRD FR-C2C-4.2: `agent_send` to non-existent agent_id MUST fail with structured error (OQ-C2C-2 MVP default: fail loudly) — source: PRD §20 line 1659 read this session — salience: high.
- PRD FR-C2C-5.1: `agent_set_dnd` state values: `"on"`, `"off"`, `"<N>m"`, `"<N>h"`, `"until HH:MM"` — local timezone — source: PRD §20 line 1666 read this session — salience: high.
- PRD FR-C2C-5.2: DND drain polls every 30s; reuses `drain_pending_outbound_tg` pattern from commit `ccdf538` — source: PRD §20 line 1667 read this session — salience: high.
- PRD FR-C2C-8.2: Telegram `<channel>` rendering MUST remain unchanged; `<agent-message>` shape only when `meta.kind = "agent-to-agent"` — source: PRD §20 line 1689 read this session — salience: high.
- PRD §20.8 Out of Scope: urgent-override of DND (`agent_send --urgent`) deferred for MVP — source: PRD §20 line 1776 read this session — salience: medium.
- Plan.md trust model: single-box single-user; no prompt-injection guard; operator-confirmed 2026-06-05 — source: plan.md line 67–68 read this session — salience: high.
- AC-C2C-1 through AC-C2C-5 exact text verified against plan.md lines 38–42 and PRD §20 lines 1707–1713 — source: both Read tool invocations this session — salience: high.
- Corpus scope relevance: `claudebase list --json` returned 0 documents; corpus is absent for this project. No topical queries executed. Task domain is SDLC pipeline + daemon MCP routing + SQLite schema; corpus does not cover this domain — salience: low.
- **[Amendment 2026-06-05] Actual `agent_registry` v5 base schema verified** — `src/daemon/chat.rs:443-453` (Read this session): columns are `agent_id PK`, `agent_name NOT NULL`, `connection_id NOT NULL`, `chat_thread_id`, `permission_relayer`, `spawned_at NOT NULL`, `last_pinged_at NOT NULL`, `state CHECK('alive'/'orphaned'/'dead')`, `metadata`. Columns `routing_key`, `last_seen_at`, `registered_at` do NOT exist — these were factual errors in the original use-cases document — salience: high.
- **[Amendment 2026-06-05] Routing migration columns verified** — `src/daemon/chat.rs:505-518` (Read this session via `apply_routing_migration`): six columns added: `routing_chat_id INTEGER`, `routing_thread_id INTEGER CHECK(...)`, `last_user_id INTEGER`, `host TEXT`, `cwd TEXT`, `pid INTEGER`. Migration uses probe-before-ALTER for idempotency, wrapped in `BEGIN/COMMIT`. Source function: `apply_routing_migration(conn)` at line 493 — salience: high.
- **[Amendment 2026-06-05] `chat_messages` schema verified** — `src/daemon/chat.rs:426-434` (Read this session): `chat_messages` has column `thread_id TEXT NOT NULL` (NOT `thread`). The original use-cases incorrectly referenced `chat_messages.thread` — all instances corrected to `thread_id` — salience: high.
- **[Amendment 2026-06-05] Migration file location verified** — `src/daemon/chat.rs:418-552` (Read this session): `ensure_chat_db_schema` is the entry point; it calls `apply_routing_migration`. No migration logic exists in `src/store.rs` (or `src/agent_registry.rs`) — the original use-cases' references to `src/store.rs` as the migration host were factual errors — salience: high.
- **[Amendment 2026-06-05] `src/daemon/agent_registry.rs` path not verified this session** — task directive states `src/agent_registry.rs` is wrong and the correct path is `src/daemon/agent_registry.rs`. This is accepted from the architect's evidence chain (the architect Read the file and found it at `src/daemon/agent_registry.rs`). Applied as a rename throughout — salience: medium.

### External contracts

- **`git config --get remote.origin.url`** — symbol: returns remote URL string or exits non-zero when `origin` remote is not configured — source: git documentation (not opened this session) — verified: no — assumption. Risk: non-standard remote name (`upstream`, `mine`) or bare repo without remotes returns error; mitigated by fallback chain. Salience: medium.
- **`git rev-parse --abbrev-ref HEAD`** — symbol: returns current branch name; returns literal string `HEAD` in detached-HEAD state — source: git documentation (not opened this session) — verified: no — assumption. Salience: low.
- **`PostToolUse` Claude Code hook event with `ExitPlanMode` matcher** — symbol: `hooks.PostToolUse` array entry in `~/.claude/settings.json`; `matchers: ["ExitPlanMode"]` fires only on that tool name — source: Claude Code hook documentation (not opened this session) — verified: no — assumption. Risk: matcher field syntax or event name may differ from assumption; validate in Slice 7 pre-flight check (per UC-C2C-5 precondition amendment). Salience: high.
- **`notifications/claude/channel` wire format** — symbol: meta fields `source`, `chat_id` (string), `target_agent_id` (string), `thread` (string, notification envelope field — distinct from `chat_messages.thread_id` DB column), optional `meta.kind` (string) — source: PRD §18 contract (PRD §20 line 1660–1661 references it as frozen); commit `ccdf538` live-tested pattern — verified: yes (PRD §20 read this session) — salience: high.
- **`chat_messages` table** — symbol: `thread_id TEXT NOT NULL` (NOT `thread`), `delivered_at INTEGER NULL`, `from_agent TEXT NOT NULL` columns — source: `src/daemon/chat.rs:426-434` Read this session — verified: yes — salience: high. [Amendment 2026-06-05: corrected from `thread TEXT` to `thread_id TEXT NOT NULL`.]
- **`agent_registry` base table (v5)** — symbol: `agent_id PK`, `agent_name NOT NULL`, `connection_id NOT NULL`, `chat_thread_id`, `permission_relayer`, `spawned_at NOT NULL`, `last_pinged_at NOT NULL`, `state CHECK('alive'/'orphaned'/'dead')`, `metadata` — source: `src/daemon/chat.rs:443-453` Read this session — verified: yes — salience: high. [Amendment 2026-06-05: replaces prior unverified assumption of `routing_key`/`last_seen_at`/`registered_at` columns.]
- **`agent_registry` routing-migration columns** — symbol: `routing_chat_id INTEGER`, `routing_thread_id INTEGER CHECK(IS NULL OR > 0)`, `last_user_id INTEGER`, `host TEXT`, `cwd TEXT`, `pid INTEGER` — source: `src/daemon/chat.rs:508-518` Read this session — verified: yes — salience: high.
- **`apply_routing_migration` function** — symbol: `fn apply_routing_migration(conn: &Connection) -> rusqlite::Result<()>` in `src/daemon/chat.rs:493` — uses probe-before-ALTER + `BEGIN/COMMIT` wrapping — source: `src/daemon/chat.rs:493-552` Read this session — verified: yes — salience: high.
- **SQLite `ON CONFLICT(agent_id) DO UPDATE`** — symbol: UPSERT semantics; last-write-wins on same primary key — source: SQLite documentation (not opened this session) — verified: no — assumption (well-established SQLite behavior; no version-specific risk). Salience: low.

### Assumptions

- Bridge auto-subscribe to `agent:<my-id>` can reuse the existing self-bootstrap pattern at bridge init (currently hardcoded for `telegram:*` threads). Risk: `agent_id` may not be available at bridge init time if registration happens after init, making auto-subscribe impossible without modification. How to verify: implementer reads `src/plugin/bridge.rs` at Slice 4 start. Tracked: R-C2C-7, plan.md §Risks. Salience: high.
- `chat_messages.thread_id` column has no CHECK constraint restricting values to `telegram:%` prefixes (verified: `src/daemon/chat.rs:426-434` shows `thread_id TEXT NOT NULL` with no CHECK on prefix — constraint is only `NOT NULL`). Risk level reduced by direct schema read. Salience: medium. [Amendment 2026-06-05: original assumption used wrong column name `thread`; corrected to `thread_id`; CHECK-absence now verified directly from source.]
- The `"on"` state for `agent_set_dnd` (indefinite DND) is stored as `i64::MAX` in `dnd_until_ts` — this is the OQ-UC-C2C-1 resolution. `NULL` = no DND (not indefinite DND). The drain task's `dnd_until_ts < now()` comparison is never true for `i64::MAX`, so indefinite DND rows are naturally skipped. Risk: if the implementation uses a different sentinel (e.g., 0 or a very-large-but-not-MAX integer), the drain might fire prematurely. How to verify: `tests/agent_dnd_test.rs` sentinel handling test. Salience: medium. [Amendment 2026-06-05: OQ-UC-C2C-1 resolved with `i64::MAX` per architect decision.]
- `"until HH:MM"` midnight rollover behavior (UC-C2C-14-EC1) is correctly handled by the parser. Risk: naive implementation may compute a `dnd_until_ts` in the past on midnight-crossing inputs. How to verify: `tests/agent_dnd_test.rs` DST + midnight rollover test cases. Salience: medium.
- DST (daylight saving time) edge cases for `"until HH:MM"` are handled gracefully (UC-C2C-14-EC3). The PRD does not specify behavior at DST transition boundaries. Risk: parser may panic or produce an incorrect timestamp on DST spring-forward gaps. How to verify: explicit DST boundary test in `tests/agent_dnd_test.rs`. Salience: low.
- `src/daemon/agent_registry.rs` path accepted from architect evidence (not directly verified via Read this session). Risk: if the file is at a different path or does not exist, references in this document would be stale. How to verify: `Glob src/daemon/agent_registry.rs` before Slice 2 implementation. Salience: medium.

### Open questions

- **OQ-UC-C2C-1 (RESOLVED 2026-06-05)**: Indefinite DND (`"on"` state) is encoded as `i64::MAX` in `dnd_until_ts`. `NULL` = no DND. Drain task naturally never matches `i64::MAX < now()`. This resolution was accepted from the architect's decision per PASS-WITH-CONDITIONS verdict. Implementer MUST use `i64::MAX`; test in `tests/agent_dnd_test.rs` — salience: high.
- **OQ-UC-C2C-2 (= OQ-C2C-3)**: Should the `PostToolUse:ExitPlanMode` hook ALSO fire on `UserPromptSubmit` to catch mid-session feature-description drift (R-C2C-2)? MVP deferred. If deferred, UC-C2C-5 is the only automated update path. Needs: operator decision — salience: medium.
- **OQ-UC-C2C-3**: Insight corpus query failed with `error: search failed: no such column: to` — suggests a schema regression in `insights.db`. Prior-session insights for `cli-to-cli-routing` could not be retrieved. Consider running `claudebase insight gc` or re-initializing `insights.db` before the next ba-analyst invocation. Needs: operator decision — salience: medium.
- **OQ-UC-C2C-4 (NEW)**: The `PostToolUse:ExitPlanMode` hook event existence must be validated against the live `~/.claude/settings.json` schema before Slice 7 implementation. If the event does not fire reliably in the installed CC version, UC-C2C-5-C fallback must be selected. Needs: implementer pre-flight check at Slice 7 start — salience: high.

## Decisions

### Inbound validation

- **Original authoring (2026-06-05)**: Inbound task: author use-cases for cli-to-cli-routing from PRD §20 and plan.md. Challenged: yes (Protocol 3). Q1: task is coherent — PRD §20 is well-formed with 8 FRs, 5 ACs, 7 NFRs. Q2: no upstream errors detected at that time. Q4: no amplification — no contradictions between PRD and plan observed. Outcome: proceeded. Salience: high.
- **Amendment (2026-06-05)**: Architect PASS-WITH-CONDITIONS verdict triggered this amendment pass. Architect's findings constitute upstream corrections; they are NOT errors in the architect's review but corrections of factual errors IN the original use-cases. Q1: amendment task is coherent and scoped. Q2: architect's evidence is grounded in `src/daemon/chat.rs:443-552` (Read by architect this session). Q4: applying the amendments removes factual errors that would have propagated to qa-planner and test-writer — non-application would amplify the upstream errors. Outcome: all architect-directed amendments applied. Salience: high.
- Task prompt listed UC-1 through UC-14 as a guide with specific scenario descriptions. I verified each scenario maps to a real PRD FR or risk entry. Additions UC-C2C-15..18 cover Slices 1, 3, and 8 which have no dedicated primary-flow use case in the task prompt, but ARE load-bearing for qa-planner coverage. Salience: medium.

### Decisions made

- Added UC-C2C-15 (Telegram regression), UC-C2C-16 (register-time identity), UC-C2C-17 (schema migration idempotency), UC-C2C-18 (`<agent-message>` tag attributes) beyond the task prompt's UC-1..UC-14 enumeration. Q1 hack? no. Q2 sane? yes. Q3 alternatives? stop at UC-14 — rejected because qa-planner reads this document as the single source of truth. Salience: high.
- UC IDs use the `UC-C2C-N` format — consistent with the feature slug `cli-to-cli-routing` and the plan's `C2C` FR numbering scheme. Q1 hack? no. Q2 sane? yes. Salience: low.
- UC-C2C-7 (last-write-wins collision) framed as a documented-acceptable-behavior use case rather than an error flow — per R-C2C-3 (plan.md). Q1 hack? no. Q2 sane? yes. Salience: low.
- **[Amendment 2026-06-05] OQ-UC-C2C-1 resolved**: `i64::MAX` is the indefinite-DND sentinel encoding for `dnd_until_ts`. `NULL` = no DND. This closes the open question. Q1 hack? no — `i64::MAX` is a principled sentinel for "never expires" in Unix-epoch arithmetic. Q2 sane? yes. Q3 alternatives? considered: (a) separate boolean flag column — rejected: requires additional schema column; (b) NULL for indefinite — rejected: NULL already means "no DND"; (c) `i64::MAX` — selected: single INTEGER column, no ambiguity with drain's `< now()` test. Salience: high.
- **[Amendment 2026-06-05] DND drain rate-limit = 10 msgs/tick (FR-C2C-5.5)**: UC-C2C-4 postcondition and UC-C2C-4-EC3 document this limit. Q1 hack? no — rate-limiting drain prevents flooding CC #2 with hundreds of notifications at once. Q2 sane? yes. Q3 alternatives? unlimited drain — rejected: could overwhelm CC #2 context with large queued backlog. Salience: medium.
- **[Amendment 2026-06-05] UC-C2C-15-EC1 fallthrough semantics confirmed**: Any `meta.kind` value that is NOT the exact literal string `"agent-to-agent"` renders as `<channel>`. No error, no panic, no dropped notification. Q1 hack? no. Q2 sane? yes — forward-compat behavior per FR-C2C-8.4d. Salience: medium.

### Hacks / workarounds acknowledged

(none)

### Symptom-only patches (with root-cause links)

- UC-C2C-5 (hook on ExitPlanMode only) treats the mid-session feature drift symptom. Root cause that remains: no CC lifecycle event fires reliably on task change outside plan mode. Tracked at: R-C2C-2 in PRD §20.9, OQ-UC-C2C-2, plan.md §Symptom-only patches. Salience: medium.
