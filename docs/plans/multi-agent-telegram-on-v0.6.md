# Plan: Multi-Agent Telegram Routing on v0.6 Foundation

**Feature slug:** `multi-agent-telegram-on-v0.6`
**Owner:** Mira (orchestrator)
**Branch strategy:** new branch `feat/multi-agent-on-v0.6` from tag `claudebase-v0.6.0`
**Date drafted:** 2026-06-02
**Plan version:** v2 (post Plan-Critic + post-operator architecture decisions C1/C2/C3)

## Context — why

In v0.7/v0.8 (released 2026-05-30 / 2026-05-31) the team built multi-CLI Telegram routing on top of the v0.6 stack, by moving Telegram polling ownership from the per-CLI plugin server to a shared daemon, changing access-file paths, changing `chat_id` serialization, adding `chat_ask` + callback handling, and rewiring the official Anthropic plugin to use a daemon bridge.

The operator's empirical verdict after extended debugging: «вся версия получилась косячной, точно неизвестно что пошло не так, отлаживали долго, психанули и откатываемся до 6 версии где работали нотификации из телеграма в cli клод». v0.7 and v0.8 are considered «ошибочными и мусорными» — root cause was not isolated, fix-forward cost exceeded rebuild cost.

This plan re-builds the multi-CLI flow **from the `claudebase-v0.6.0` tag**, **preserves v0.6's mixed TG library stack** (teloxide 0.17 in `src/daemon/telegram.rs`, frankenstein 0.49 in `plugins/telegram-rs/`), preserves the v0.6 CLI↔MCP wire shape, and **adds one minimal optional field** (`message_thread_id`) to the `reply` MCP tool params + inbound `channel` notification meta — the only contract delta — so KP1-KP3 (topic-aware routing) work.

The architecture (operator-decided C1/C2/C3):

- **C1: Mixed stack kept.** `src/daemon/telegram.rs` keeps teloxide 0.17; `plugins/telegram-rs/` keeps frankenstein 0.49. No migration either direction.
- **C2: Daemon owns the TG connection.** Daemon code does NOT write `loop { get_updates }`; instead it uses teloxide's high-level `Dispatcher` API which internally handles long-polling and dispatches events as callbacks. The plugin's frankenstein polling loop is DISABLED — the plugin becomes a thin MCP bridge: it receives notifications FROM the daemon via UDS, forwards outbound tool calls TO the daemon via UDS. **No CLI↔MCP method renames; no notification-method renames.** This eliminates v0.6's latent dual-poller 409 vulnerability without altering the contract Claude Code sees.
- **C3: `reply` gets an optional `message_thread_id` param.** When CLI replies, it can pass thread_id back; daemon adds it to teloxide's `SendMessageSetters`. Old `reply` calls without the field still work (reply lands in main thread / DM). Mirror: inbound `notifications/claude/channel` meta gets an optional `thread_id` field so the CLI knows which topic it's bound to.

## Reference material (READ-ONLY context, NOT source of truth)

Three pre-existing design docs in `docs/plans/` describe v0.7/v0.8-era thinking. **Per operator directive 2026-06-02:** these are reference material — useful to PEEK AT for vocabulary and earlier design rationale, but they are NOT instruction, NOT contract, NOT source of truth. The new plan is authored fresh against the v0.6 baseline; legacy decisions in those docs are not binding.

- `docs/plans/telegram-multi-cli-orchestration.md` — covers similar routing tree + bot-command surface; useful for vocabulary.
- `docs/plans/agent-registry-multi-cli.md` — proposes richer registry schema; useful for column-naming inspiration.
- `docs/plans/claudebase-server-foundation.md` — HTTP/WSS + Bearer auth + cross-machine. **Entirely out of scope** here.

This plan makes its own decisions; the legacy docs do not override.

## Success Criteria (operator-stated, for `/qa-cycle` protocol — KP1-KP3)

**The acceptance criterion the QA-cycle MUST verify with concrete evidence on a live Telegram bot:**

| ID | Scenario | Routing key | Expected CLI binding |
|---|---|---|---|
| **KP1** | Operator messages bot `@X` in **DM** | `(dm_chat_id, None)` | Routes to **CLI instance A** |
| **KP2** | Operator messages the **same bot `@X`** in a **group with forum topics enabled**, **topic α** | `(group_chat_id, Some(thread_id_α))` | Routes to **CLI instance B** (≠ A) |
| **KP3** | Operator messages the **same bot `@X`** in the **same group**, **topic β** (different topic in the same group) | `(group_chat_id, Some(thread_id_β))` | Routes to **CLI instance C** (≠ A, ≠ B) |

**Definition of "routes to CLI X":**

- The inbound TG message surfaces inside CLI X's live Claude Code session as a `<channel source="plugin:telegram:telegram" ...>` event (v0.6 wire shape preserved; new OPTIONAL `thread_id` meta field added — old consumers ignore unknown fields per JSON-RPC convention).
- The CLI's `reply` tool call (with `message_thread_id` echoed back where applicable) lands the response **in the same chat-and-topic** the operator sent from (DM, topic α, or topic β).
- CLI instances A, B, C are three independent Claude Code processes started by `claudebase run` from three different cwds (or with three different `--agent-id`s).
- All three are served by a **single bot token** (same `@X`) — no per-CLI bot.

**QA-cycle evidence required per case (concrete artifacts the qa-engineer must collect):**

| Case | Verification Class | Evidence Required |
|---|---|---|
| KP1 | **Mixed** (UI/UX + CLI + DB) | (a) Telegram Desktop OS-level screenshot `tc-kp1-dm-after.png` showing operator's DM with bot AND CLI A's reply text; (b) terminal screenshot `tc-kp1-cli-a-channel.png` of CLI A's session printing `<channel source="plugin:telegram:telegram" chat_id="..." thread_id="" user="...">`; (c) Bash `claudebase daemon logs --tail 50` literal stdout containing `routed (chat_id=<N>, thread_id=None) -> cli_id=A`; (d) SQL `SELECT cli_id, chat_id, thread_id FROM agent_registry WHERE state='alive'` literal output showing row `(A, <dm_chat_id>, NULL)`. |
| KP2 | **Mixed** | Same shape as KP1 but `tc-kp2-topicα-after.png` shows reply landing in topic α; daemon log line `routed (chat_id=<G>, thread_id=Some(α_id)) -> cli_id=B`; SQL row `(B, <G>, α_id)`. |
| KP3 | **Mixed** | Same shape but `tc-kp3-topicβ-after.png` shows reply in topic β; daemon log `routed (chat_id=<G>, thread_id=Some(β_id)) -> cli_id=C`; SQL row `(C, <G>, β_id)`. Three distinct CLIs visible in `agent_registry`. |

**Note on screenshot mechanism (Plan Critic finding #6):** Playwright MCP cannot capture native Telegram Desktop. Evidence captures are OS-level screenshots (PowerShell `Get-Screenshot` / `Snipping Tool` on Windows; `screencapture` on macOS; `gnome-screenshot` on Linux). The qa-engineer takes the screenshot manually during the live run; the file path is the evidence artifact. Telegram Web in a Playwright-driven browser is an acceptable alternate path if the operator can log into Telegram Web in the session.

KP1-KP3 are the load-bearing acceptance criteria. A build that fails any of them is NOT shippable.

## Deliverables Checklist (mandatory for `/bootstrap-feature`)

- [ ] **PRD section** in `docs/PRD.md` — Functional Requirements for: routing key (`chat_id, Option<thread_id>`), per-CLI registration semantics, `reply.message_thread_id` optional param, channel-meta `thread_id` optional field, bot-command dispatch (/agents /switch /whoami /here), the v0.6 mixed-library stance. Acceptance criteria = KP1-KP3 verbatim. PRD section MUST carry `## Facts` block with `### External contracts` citing teloxide 0.17 `Dispatcher` / frankenstein 0.49 `Message.message_thread_id` per cognitive-self-check.
- [ ] **Use cases** in `docs/use-cases/multi-agent-telegram-on-v0.6_use_cases.md` — 7+ scenarios: DM routing (KP1), topic α routing (KP2), topic β routing (KP3), `/agents` discovery, `/switch` rebinding, daemon-restart with active bindings, CLI process dies mid-conversation (orphan handling).
- [ ] **Architecture review** by `architect` agent on:
  - (a) teloxide `Dispatcher` invocation pattern for owned-connection mode + routing-key extraction from `Message`
  - (b) plugin-as-thin-bridge — how reply/react/edit_message/download_attachment forward to daemon over the existing UDS frame (length-prefixed JSON) without growing `src/plugin/mcp.rs` `TOOL_WHITELIST`
  - (c) the optional `message_thread_id` additive on `reply` params + `notifications/claude/channel` meta — confirm backward-compat per JSON-RPC convention
  - (d) the `agent_registry` additive schema migration on top of v0.6's `(agent_id, agent_name, connection_id, chat_thread_id, state, spawned_at, last_pinged_at)` — adding `routing_chat_id INTEGER` and `routing_thread_id INTEGER NULL`; new partial-unique index `(routing_chat_id, routing_thread_id, state) WHERE state='alive'`
- [ ] **QA test cases** in `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md` — KP1-KP3 as TC-1/TC-2/TC-3 with the Verification Class + Evidence Required columns shown above. Plus TC-4 `/agents` discovery, TC-5 `/switch` rebinding, TC-6 daemon-restart resilience, TC-7 orphan CLI handling, TC-8 visual: bot reply does NOT bleed into wrong topic (negative case), TC-9 visual: 3 CLIs visible side-by-side with distinct prompts.
- [ ] **Implementation plan** in `<project>/.claude/plan.md` — refined slices by `planner` agent. (Mira will copy this plan body there after ExitPlanMode.)
- [ ] **Plan Critic** pass on the refined `.claude/plan.md`.
- [ ] **CHANGELOG `[Unreleased]`** entry once implementation begins.

## Architectural Constraints (the «не менять контракт» fence, post-C3 amendment)

### Frozen — bit-for-bit v0.6 (CLI-observable surface)

1. **Inbound MCP notification method name** — `notifications/claude/channel`, exact string.
   *Source:* `git show claudebase-v0.6.0:plugins/telegram-rs/src/mcp/notification.rs:59` (line 59 carries the literal — Plan-Critic finding #12 correction).
2. **Plugin's outbound tool names** — `reply, react, edit_message, download_attachment` exposed by `plugins/telegram-rs/src/mcp/server.rs` + `plugins/telegram-rs/src/mcp/tools.rs`. These names are frozen; CLI calls them unchanged.
3. **Root-crate daemon's MCP tool names** — `chat_post, chat_subscribe, chat_reply, chat_list, chat_list_threads, claudebase_daemon_status, agent_register, agent_unregister, agent_list_alive, agent_reap` exposed by `src/plugin/mcp.rs` `TOOL_WHITELIST`. Frozen. (Plan-Critic finding #2 correction: this whitelist is in the daemon-bridge crate, NOT in the plugin — they're two different name-spaces.)
4. **Notification meta JSON shape** — existing field names and types unchanged. Adding NEW OPTIONAL fields IS allowed (per JSON-RPC additive-evolution convention) and is the mechanism for C3:
   - Inbound `notifications/claude/channel` meta: add optional `thread_id: Option<String>` (string per v0.6 ID-as-string discipline).
   - Outbound `reply` tool params: add optional `message_thread_id: Option<String>`.
   - Old consumers (CLI built against v0.6) ignore unknown fields — backward compatible.
5. **IPC framing** — length-prefixed 4-byte big-endian + UTF-8 body on UDS; newline-delimited JSON on STDIO. Unchanged.
6. **Deployment model** — `install.sh` patches the **official** `telegram@claude-plugins-official` plugin's `.mcp.json`; the patch wrapper at v0.6 (`install.sh:551-702` per Plan-Critic VERIFIED list) is preserved.
7. **`claudebase run` argv** — `claude --channels plugin:telegram@claude-plugins-official` (unchanged from v0.6, verified `src/main.rs:177`).
8. **Library pinning** — `frankenstein = "0.49"` (plugin), `teloxide = "0.17"` (daemon). Both kept (operator decision C1).

### Open for modification (daemon + plugin internals — NOT CLI-observable)

- `src/daemon/telegram.rs` — teloxide `Dispatcher`-based event handling; routing-key extraction; outbound send pipeline accepting daemon-internal `(chat_id, Option<thread_id>, text, reply_to, files)` from plugin-forwarded MCP tool calls.
- `src/daemon/agent_registry.rs` — additive schema columns + new partial-unique index.
- `src/daemon/server.rs` — UDS dispatch table grows daemon-internal envelopes for the plugin↔daemon forwarding of outbound tool calls.
- `src/daemon/chat.rs` — broadcast bus filter by `(chat_id, thread_id)` key.
- `plugins/telegram-rs/src/telegram/bot.rs` — frankenstein polling loop DISABLED (the `getUpdates` loop is removed or behind a `cfg(feature = "legacy-direct-poll")` opt-out); the plugin still owns access-gate and pairing UI but routes inbound from daemon.
- `plugins/telegram-rs/src/mcp/server.rs` `handle_reply`/`handle_react`/`handle_edit_message`/`handle_download_attachment` — INTERNAL implementation forwards to daemon over the plugin↔daemon UDS instead of calling frankenstein `send_message` directly. **Tool method names + param shapes unchanged from CLI's perspective** (only the new optional `message_thread_id` param added per C3).
- `src/plugin/bridge.rs` — preserved as v0.6 692-line baseline. NO session-cache, NO reconnect-replay, NO ensure_daemon_running. Those v0.8 additions are explicitly OUT.

## Preliminary Slices (planner refines at bootstrap)

Sequential, single Wave 1 — `src/daemon/telegram.rs` and `plugins/telegram-rs/src/mcp/server.rs` are touched by multiple slices; parallel waves would conflict.

**Slice 1 — Branch + baseline verification + external-contract check.**
- Create `feat/multi-agent-on-v0.6` from `claudebase-v0.6.0`.
- **Pre-flight verifications (Plan-Critic findings #10, #13):**
  - `cargo build --release` clean.
  - `cargo test --workspace` passing on v0.6 baseline.
  - `cargo run -- daemon serve --foreground` starts cleanly.
  - `cargo run -p telegram-plugin-rs` starts cleanly.
  - Architect verifies that teloxide 0.17's `Message` struct exposes `message_thread_id: Option<i32>` (or compatible type). If absent → BLOCKED, surface to operator.
  - Architect verifies that frankenstein 0.49's `Message` struct also exposes `message_thread_id` if the plugin ever needs it daemon-forwarded.
- SDLC scaffold: PRD section + use-cases + qa test-cases files seeded.
- Done when: above checks pass AND `docs/PRD.md` Feature section drafted by `prd-writer`.

**Slice 2 — Routing-key data model + `agent_registry` schema additive migration.**
- ARCHITECT pre-review.
- Extend `agent_registry` schema with nullable `routing_chat_id INTEGER` and `routing_thread_id INTEGER NULL`; new partial-unique index `(routing_chat_id, routing_thread_id) WHERE state='alive'` to enforce one-CLI-per-routing-key.
- **Migration discipline (Plan-Critic finding #7):** the migration is NOT green-field — operators with prior v0.7/v0.8 dev installs may have a populated `chat.db` with different columns. Migration script must (a) detect existing schema_version, (b) safely add columns with `ALTER TABLE ... ADD COLUMN ... DEFAULT NULL`, (c) leave existing v0.7/v0.8 columns alone (not drop), (d) include a forward-compatible rollback note (the new columns are nullable so existing inserts remain valid).
- New `register_routing(cli_id, chat_id, thread_id: Option<i64>)` API on `agent_registry.rs`.
- Done when: unit tests cover (a) insert with routing key, (b) UNIQUE-constraint violation on duplicate `(chat_id, thread_id)` while alive, (c) migration idempotent (running twice = no-op), (d) migration tolerates pre-existing v0.7/v0.8 columns.

**Slice 3 — teloxide `Dispatcher` in daemon + routing decision dispatcher.**
- ARCHITECT pre-review (concurrency / Dispatcher invocation pattern).
- SECURITY pre-review (no token leak in logs; daemon does not log full `TELEGRAM_BOT_TOKEN`).
- Replace v0.6 `src/daemon/telegram.rs::run_long_poll`'s manual `getUpdates` loop with teloxide's `Dispatcher::builder` wired to a `handler_tree!` macro routing every inbound `Message`.
- Extract `(chat_id, message_thread_id)` from each Update. Look up `agent_registry` for the bound CLI. If found → emit `notifications/claude/channel` (with new optional `thread_id` meta field) into the chat broadcast bus, scoped to the bound CLI's subscription. If no binding → fallback policy (see R3 + OQ3).
- **Error-handling coverage (Plan-Critic finding #9):** explicit handlers for:
  - daemon startup with no `TELEGRAM_BOT_TOKEN` env → start daemon WITHOUT TG support, log warning, do not crash.
  - teloxide 401 (invalid token) → log error, terminate Dispatcher gracefully, daemon stays alive.
  - teloxide 409 (another consumer holds the token) → log warning with operator-facing instruction ("kill any other claudebase daemon / TG plugin holding this token; the daemon is the sole owner in this build"); exponential backoff.
  - `message_thread_id` absent on inbound (DM case) → routing key is `(chat_id, None)`.
  - Orphan inbound (no CLI bound to the routing key) → log + fallback per OQ3.
- Done when: KP1-KP3 unit tests on synthetic `Message` payloads PASS (mock teloxide handler input), plus error-handling unit tests for all 5 cases above.

**Slice 4 — Plugin-side `reply/react/edit_message/download_attachment` forward to daemon.**
- ARCHITECT pre-review (plugin↔daemon UDS envelope shape for outbound forwarding).
- Disable frankenstein polling loop in `plugins/telegram-rs/src/telegram/bot.rs` (the `get_updates` poller — keep the access-gate + pairing UI).
- Replace direct `frankenstein::send_message` calls in `plugins/telegram-rs/src/mcp/server.rs::handle_reply` (and the 3 sibling handlers) with a daemon-forward envelope sent over the existing UDS (length-prefixed JSON frame, daemon-internal method name e.g. `internal.send_message`).
- The CLI-facing `reply` tool params gain the optional `message_thread_id` per C3 — plugin echoes it into the daemon-forward envelope; daemon translates to teloxide `SendMessageSetters::message_thread_id`.
- Done when: CLI's `reply { chat_id, text, message_thread_id: "X" }` lands in TG topic X verifiably (frankenstein-side smoke not required — teloxide-side smoke sufficient).

**Slice 5 — Bot commands `/agents`, `/switch`, `/whoami`, `/here`.**
- Daemon-internal handler dispatched in the Dispatcher handler_tree BEFORE routing.
- Topic-aware: `/agents` lists CLIs bound to the SAME `(chat_id, thread_id)` tuple; `/switch <cli>` rebinds the routing-key row in `agent_registry` to the named CLI; `/whoami` returns current binding; `/here` returns the CLI's host/cwd from registry.
- **Race-condition note (Plan-Critic finding #8):** two `/switch` taps for the same routing key landing in the same Dispatcher tick are serialized by SQLite's per-write transaction. The Dispatcher per-Update task spawns one DB transaction; UNIQUE-constraint on `(routing_chat_id, routing_thread_id) WHERE state='alive'` ensures only one binding persists. The second `/switch` either succeeds (overwrites — desired) or is rejected with a "rebinding conflict" reply to the user. Default plan: second tap wins (overwrites), with a TG reply confirming the rebind.
- RED-TEAM on `/switch` security in groups: per-tap user check — only the user whose `user_id` is the existing binding's `last_user_id` (or a chat admin) can `/switch` — see OQ3.
- Done when: each command returns the correct response per the use-case file; race test (concurrent `/switch` taps) leaves a single deterministic binding.

**Slice 6 — Pairing + access (v0.6-style, both files coexist).**
- Plan-Critic finding #11 correction: `src/daemon/channel_state.rs` ALREADY EXISTS at v0.6.0 alongside `permissions.rs`. The plan keeps **v0.6's existing pairing+access model unchanged** — both files stay where they were. No "migrate from permissions.rs to channel_state.rs" rewrite; both modules continue serving their v0.6 roles.
- Done when: pairing flow (operator approves a sender_id) works end-to-end; access gate denies unpaired senders.

**Slice 7 — Daemon-restart resilience.**
- Persist `agent_registry` rows across daemon restart (SQLite, already in v0.6).
- On daemon restart, re-pair active CLIs by their `connection_id` + `routing_chat_id` + `routing_thread_id`.
- **Edge case (Plan-Critic finding #9):** CLI starts BEFORE daemon is up → `src/plugin/bridge.rs`'s v0.6 fallback (sentinel tool `claudebase_daemon_status` returning `{status: "down"}`, retry connection in background, `notifications/tools/list_changed` once connected) handles this — verify v0.6 behavior is preserved.
- NO session-cache / ensure_daemon_running / reconnect-replay (those are v0.8 additions we explicitly avoid).
- Done when: daemon restart with 3 CLIs alive restores all 3 routing-key bindings; `/whoami` from each chat confirms the binding.

**Slice 8 — Docs + e2e smoke + gates.**
- README "v0.6+ fleet setup" section.
- CHANGELOG `[Unreleased]` entry (release-scribe).
- `docs/qa/multi-agent-telegram-on-v0.6_smoke_runbook.md` — step-by-step KP1-KP3 live run instructions.
- `/qa-cycle` → `/merge-ready` → eventually `/release` (version TBD per OQ4).
- Done when: all 9 quality gates pass at `/merge-ready`.

## Files Likely Affected

**Modified (daemon + plugin internals — open for change):**

- `src/daemon/telegram.rs` — teloxide Dispatcher rewrite + error handling (Slice 3, 5)
- `src/daemon/agent_registry.rs` — schema migration + routing-key API (Slice 2)
- `src/daemon/server.rs` — UDS dispatch grows daemon-internal envelopes for plugin-forwarded outbound (Slice 4)
- `src/daemon/chat.rs` — broadcast bus filter by routing key (Slice 3)
- `src/daemon/mod.rs` — wiring
- `src/daemon/migrations.rs` (or wherever daemon migrations live in v0.6) — additive schema migration (Slice 2)
- `plugins/telegram-rs/src/telegram/bot.rs` — disable polling, keep gate/pairing (Slice 4)
- `plugins/telegram-rs/src/mcp/server.rs` — handle_reply/react/edit/download forward to daemon (Slice 4)
- `plugins/telegram-rs/src/mcp/notification.rs` — emit `thread_id` optional meta (Slice 3)
- `plugins/telegram-rs/src/mcp/tools.rs` — `reply` tool schema gains optional `message_thread_id` (Slice 4)
- `Cargo.lock` — regen

**Preserved bit-for-bit (frozen contract):**

- `plugins/telegram-rs/src/mcp/protocol.rs`
- `src/plugin/bridge.rs` (v0.6 692-line version — NOT the v0.8 1066-line version)
- `src/plugin/mcp.rs` (v0.6 TOOL_WHITELIST exactly)
- `install.sh` Telegram plugin patching block at v0.6 lines 551-702 (verified via Plan Critic)

**Created:**

- `docs/use-cases/multi-agent-telegram-on-v0.6_use_cases.md`
- `docs/qa/multi-agent-telegram-on-v0.6_test_cases.md`
- `docs/qa/multi-agent-telegram-on-v0.6_smoke_runbook.md`
- New PRD section appended to `docs/PRD.md`

## Risks & Dependencies (post-revision)

- **R1 — Full v0.6 baseline includes loss of v0.7 quality-of-life (insights tag-filter, hooks, `/update-claudebase`, `prompts/` reorg).** Default plan: branch from `claudebase-v0.6.0` cleanly. Selective port-forward of v0.7 Bucket-A items (per Plan Critic Bucket-A triage from Explore agent 2) is **possible** but deferred to a follow-up: this plan only ships KP1-KP3 routing on the v0.6 baseline. Operator can later request port-forward of insights-corpus tag-filter etc. as a separate feature on top of the merged branch.
- **R2 — `reply` becoming an optional-additive contract change.** Strictly speaking adding `message_thread_id: Option<String>` IS a contract change. Operator approved (C3 option a). Backward-compat is guaranteed by the field being optional + old consumers ignoring unknown fields.
- **R3 — `/switch` security in groups (deferred decision OQ3).** Per Slice 5 default: per-tap-user-check (only the binding's last user_id or chat admin can `/switch`). Red-team at bootstrap may refine.
- **R4 — teloxide 0.17 `Message.message_thread_id` field existence.** Plan Critic finding #10 — verified at Slice 1 (architect call), NOT later. If teloxide 0.17 omits the field, options: (a) bump teloxide to 0.18+, (b) custom deser bypass, (c) downgrade to "topic routing unsupported, only DM works" → operator must approve scope cut.
- **R5 — Plugin's frankenstein polling disabled = no fallback if daemon dies.** v0.6 plugins could poll independently. Disabling that loses standalone-mode-as-fallback. Mitigation: clearly documented as a deliberate architectural choice (operator-stated C2); v0.6 standalone mode is preserved by a feature-flag `cfg(feature = "legacy-direct-poll")` for emergency-revert if needed.
- **R6 — v0.6's latent dual-poller 409 vulnerability** (daemon teloxide loop + plugin frankenstein loop both reading the same `TELEGRAM_BOT_TOKEN` in v0.6 base). Plan Critic finding #3. Resolved by Slice 4 disabling the plugin's poller; daemon (via teloxide Dispatcher) becomes sole owner.
- **R7 — `chat.db` migration on a dev box that has v0.7/v0.8 leftovers.** Plan Critic finding #7. Slice 2 migration is additive (`ADD COLUMN`) so existing rows survive; pre-existing v0.7/v0.8 columns are not dropped (they just become unused). New code reads only the new columns.
- **R8 — Topic-aware `/agents` in a group.** Bot command `/agents` in topic α should list CLIs bound to `(chat_id, α)`; in topic β should list CLIs bound to `(chat_id, β)`. Code must read `message.message_thread_id` from the COMMAND message itself when filtering. (Edge case: command sent in main group thread without a topic context — list all CLIs in that group.)

## Out of Scope (this plan)

- Multi-bot token store (`telegram addbot/listbots/removebot`) — v0.9 fleet plan, separate.
- `claudebase update` / `daemon update` / `daemon setup` — v0.9 fleet plan, separate.
- `chat_ask` MCQ via inline keyboard — additive-but-new MCP tool from v0.8; deferred.
- `--dangerously-skip-permissions` on `claudebase run` — v0.9, separate.
- HTTP/WSS cross-machine fleet — `claudebase-server-foundation.md` is reference-only.
- Selective port-forward of v0.7 Bucket-A items (insights tag-filter, hooks, `/update-claudebase`, `prompts/` reorg). Operator can request after KP1-KP3 ships.

## Open Questions (resolve at bootstrap)

- **OQ1 — Selective port-forward of v0.7 Bucket-A items?** Default plan: NO — strict v0.6 baseline + KP1-KP3 routing. Operator may green-light Bucket-A as follow-up.
- **OQ2 — RESOLVED by operator C3 (option a):** `reply` gets optional `message_thread_id`. CLOSED.
- **OQ3 — `/switch` security in groups.** Default plan: per-tap user check (last binding's user_id OR chat admin). Red-team at bootstrap may refine.
- **OQ4 — Release version after merge.** v0.9.0 conflicts with the fleet plan's target (Plan Critic finding #15). Recommend either (a) bump this to v0.7.0-rebuild (semver-recycle the broken v0.7 line), (b) bump to v0.10.0 (skip v0.9 entirely; v0.9 = fleet plan), (c) call it v0.6.1 (patch-level — though the additive `message_thread_id` argues for MINOR). My recommendation: **(b) v0.10.0** — gives the fleet plan room to be v0.9.0 and signals clearly that v0.7/v0.8 were ejected.

## Verification (how to test end-to-end)

After all slices land, the verification sequence is:

1. **Pre-flight:** `cargo build --release` clean; `cargo test --workspace` passing.
2. **Local-only smoke:** Start daemon (`claudebase daemon start`); register 3 CLI instances via `claudebase run` from 3 cwds; each registers with a distinct routing key — A=(dm_chat_id, None), B=(group_chat_id, α), C=(group_chat_id, β).
3. **Live TG runs (KP1-KP3):** see Success Criteria table for evidence required per case.
4. **Negative test:** Stop CLI B → operator sends another message in topic α → daemon either (a) routes to next bound CLI per fallback policy, or (b) replies in TG "no CLI bound" — architect call at bootstrap OQ3.
5. **Daemon restart:** `claudebase daemon restart` → all three bindings restored from SQLite → KP1-KP3 still pass.
6. **Dual-poller stress:** Start a 2nd daemon process trying to claim the same token → first daemon's teloxide Dispatcher continues serving; second daemon logs 409, backs off, eventually exits per Slice 3 error handling.

## Facts

### Verified facts
- v0.6 tag `claudebase-v0.6.0` exists; `Cargo.toml` workspace version `0.6.0`; members `[".", "plugins/telegram-rs"]`. — source: `git show claudebase-v0.6.0:Cargo.toml` this session — salience: high
- v0.6 `plugins/telegram-rs/Cargo.toml` pins `frankenstein = "0.49"` with `features = ["client-reqwest"]`. — source: `git show claudebase-v0.6.0:plugins/telegram-rs/Cargo.toml:23` this session — salience: high
- **v0.6 ROOT `Cargo.toml` pins `teloxide = "0.17"` for the daemon's TG client.** Plan-Critic finding #1 verification. — source: `git show claudebase-v0.6.0:Cargo.toml:90` (per Plan Critic verified citation) — salience: **high** (load-bearing for C1)
- v0.6 contains BOTH `src/daemon/telegram.rs` (teloxide) AND `plugins/telegram-rs/src/telegram/bot.rs` (frankenstein) — both with `getUpdates` polling logic reading `TELEGRAM_BOT_TOKEN` from env. Latent dual-poller; Plan-Critic finding #3. — source: `git show claudebase-v0.6.0:src/daemon/telegram.rs` (teloxide poll) + `git show claudebase-v0.6.0:plugins/telegram-rs/src/telegram/bot.rs:47` (frankenstein poll) — salience: high
- v0.6 already contains both `src/daemon/permissions.rs` AND `src/daemon/channel_state.rs` (Plan-Critic finding #11 correction). — source: `git ls-tree -r claudebase-v0.6.0` — salience: high
- v0.6 MCP protocol version `"2025-11-25"`; max frame size 1 MiB. — source: Explore agent 1, `plugins/telegram-rs/src/mcp/protocol.rs:6,10` — salience: high
- v0.6 notification literal `notifications/claude/channel` lives at `plugins/telegram-rs/src/mcp/notification.rs:59` (Plan-Critic finding #12 correction). — salience: medium
- v0.6 root-crate `src/plugin/mcp.rs` `TOOL_WHITELIST` contains only the 10 chat/agent/status tools (`chat_post, chat_subscribe, chat_reply, chat_list, chat_list_threads, claudebase_daemon_status, agent_register, agent_unregister, agent_list_alive, agent_reap`). The 4 plugin tools (`reply, react, edit_message, download_attachment`) are exposed by `plugins/telegram-rs/src/mcp/server.rs:201-204` + `mcp/tools.rs:15,41,54,72`, NOT by the root crate. Plan-Critic finding #2 correction. — source: `git show claudebase-v0.6.0:src/plugin/mcp.rs` + Plan Critic verified — salience: high
- v0.6 daemon IPC: length-prefixed 4-byte big-endian + UTF-8 JSON, 16 MiB cap. — source: Explore agent 1, `src/daemon/mod.rs:20`, `src/plugin/bridge.rs:37-43` — salience: high
- v0.6 `claudebase run` execs `claude --channels plugin:telegram@claude-plugins-official`. — source: `src/main.rs:177` (Plan Critic verified line) — salience: high
- v0.6 installer patches the **official** `telegram@claude-plugins-official` plugin's `.mcp.json` at `install.sh:551-702`. — source: Plan Critic VERIFIED — salience: high
- v0.6 `agent_registry` schema: `(agent_id, agent_name, connection_id, chat_thread_id: Option<String>, state, spawned_at, last_pinged_at)` with partial-unique index `(chat_thread_id, agent_name) WHERE state='alive'`. — source: Explore agent 1, `src/daemon/agent_registry.rs:92-138` — salience: high
- v0.6 PRD §17 does NOT mention forum-topic support in scope. — source: Explore agent 1 — salience: medium

### External contracts
- **teloxide 0.17** — symbol: `Dispatcher::builder`, `handler_tree`, `Message`, `Message.message_thread_id` (per Telegram API, expected to be `Option<i32>`) — source: `git show claudebase-v0.6.0:Cargo.toml:90` (pinned) — verified: PIN yes, `message_thread_id` field-existence verification deferred to Slice 1 architect call — salience: **high** (Slice 1 BLOCKING check)
- **frankenstein 0.49 + client-reqwest** — symbol: `Bot`, `Message`, `SendMessageParams`, `GetUpdatesParams`, `AsyncTelegramApi` — source: `git show claudebase-v0.6.0:plugins/telegram-rs/Cargo.toml:23` — verified: yes — salience: high
- **Telegram Bot API `message_thread_id`** — symbol: optional integer on `Message`; used in `sendMessage` outbound to target a specific forum topic — source: NOT opened this session — verified: no — assumption — salience: high (Slice 1 verifies)
- **Claude Code `--channels plugin:<id>` injection of `notifications/claude/channel`** — source: live-verified at v0.8.0 release-notes; v0.6 plugin already shipped this — verified: partial — salience: high

### Assumptions
- v0.6 worked «stably» for the operator because they ran ONE polling owner at a time in practice — either the daemon's teloxide loop OR the plugin's frankenstein loop, not both. The dual-poller is latent vulnerability v0.6 never hit in single-CLI use. Risk: my "v0.6 was working" claim is shorthand; the architecture has a known race. Mitigation: this plan (Slice 4) explicitly resolves it by making daemon the sole owner. — salience: high
- Adding optional fields to existing JSON-RPC notification meta is non-breaking per JSON-RPC convention (consumers MUST ignore unknown fields). Risk: a strict consumer might reject. Mitigation: documented in PRD; v0.6 Claude Code client is the only consumer and is known-tolerant. — salience: medium
- `chat.db` schema migration tolerates pre-existing v0.7/v0.8 columns left over on a dev box. Risk: SQLite `ALTER TABLE ADD COLUMN` only adds; it doesn't conflict with existing columns. Verification: Slice 2 unit tests exercise both fresh-DB and pre-populated-DB cases. — salience: medium

### Open questions
- OQ1, OQ3, OQ4 (see above). OQ2 closed by operator C3. — salience: high
- Slice 1 architect call: does teloxide 0.17 `Message` expose `message_thread_id`? If no → fallback options listed in R4. — salience: high
- Slice 6 architect call: do `permissions.rs` and `channel_state.rs` cooperate or contend on access-grant state in v0.6? — salience: medium

## Decisions

### Inbound validation
- Operator directive turn 1 (rollback Telegram to v0.6, additively extend with multi-agent routing + topic-as-id, freeze wire contracts) — challenged: yes — outcome: 3 clarifying questions surfaced (failure mode / branch strategy / architecture contradiction); operator answered concretely; proceeded. — salience: high
- Operator directive turn 2 (KP1-KP3 — one bot, three CLIs, DM + topic α + topic β) — challenged: no, concrete acceptance criterion accepted as load-bearing. Persisted verbatim. — salience: high
- Operator directive turn 3 (legacy plans in `docs/plans/` = reference only, NOT instruction, NOT source of truth) — challenged: no — outcome: Reference-material section softened in v2 of this plan; no LIFTED/REJECTED matrix; no obligation to honor legacy plan decisions. — salience: high
- Operator decisions C1/C2/C3 (mixed stack / daemon owns TG connection via teloxide Dispatcher / `reply` gains optional `message_thread_id`) — challenged: no — outcome: architecture sections in this v2 plan reflect these directly. — salience: high

### Decisions made
- Branch from `claudebase-v0.6.0` tag (operator turn 1). — salience: high
- Mixed library stack kept: teloxide (daemon) + frankenstein (plugin). Operator C1. Q1 hack? no | Q2 sane? yes | Q3 alternatives? options (b) daemon→frankenstein, (c) plugin→teloxide considered and rejected for migration cost. — salience: high
- Daemon owns TG via teloxide `Dispatcher` (high-level, no manual `loop { get_updates }`). Plugin's frankenstein polling DISABLED in Slice 4 (preserved behind `cfg(feature = "legacy-direct-poll")` for emergency revert). Operator C2. Q1 hack? no | Q2 sane? yes | Q3 alternatives? options (b) plugin owns, (c) leader-election considered and rejected for 409-safety / complexity. — salience: high
- `reply` MCP tool gains OPTIONAL `message_thread_id: Option<String>` param (additive, backward-compat); inbound `notifications/claude/channel` meta gains OPTIONAL `thread_id: Option<String>` field (additive). Operator C3 option a. Q1 hack? no | Q2 sane? yes | Q3 alternatives? options (b) daemon-resolves-from-binding (rejected — `reply` lives in plugin, requires daemon-back-channel; more invasive), (c) full-proxy reply (rejected — that's the v0.8 path the operator considers broken). — salience: high
- Legacy plans treated as reference only (operator turn 3). — salience: medium
- `permissions.rs` AND `channel_state.rs` BOTH preserved as-is from v0.6 (Plan-Critic finding #11 correction). Slice 6 does NOT migrate / consolidate; both modules continue serving their v0.6 roles. — salience: medium
- Slice count = 8 sequential, single wave. — salience: medium
- Out-of-scope: multi-bot, `claudebase update`, `chat_ask` MCQ, `--dangerously-skip-permissions`, HTTP server foundation, port-forward of v0.7 Bucket-A. All separately ticketable. — salience: medium

### Hacks / workarounds acknowledged
- `cfg(feature = "legacy-direct-poll")` flag on the plugin's frankenstein poller is a workaround for "what if daemon is unavailable but operator still wants TG to work in single-CLI mode" — it's a deliberate fallback escape hatch, not a long-term path. **Removal path:** drop the cfg gate once daemon-ownership has been proven stable across 2+ releases (track in PRD §Future Cleanup). — salience: medium

### Symptom-only patches (with root-cause links)
- The decision to rollback rather than fix-forward v0.8 IS a symptom-only patch at the meta-level: «v0.8 не работает, корень не нашли». Tracked at: this plan's Context section + an implicit obligation to log any specific v0.8 failure modes uncovered during the v0.6+ build into `docs/issues/`. Root cause of v0.8 brokenness is NOT pursued because operator decided forward-debug cost exceeds rebuild cost. Operator-acknowledged trade-off. — salience: high

## Review Notes

### Critic Findings (Plan Critic v1, on plan v1)
- **Total:** 15 findings (3 CRITICAL, 8 MAJOR, 4 MINOR)
- **All CRITICAL/MAJOR addressed in v2:** Yes

### Changes Made in v2
- **CRITICAL #1 (teloxide vs frankenstein):** Plan now explicitly documents mixed-stack reality, operator decision C1 kept both. R4 reframed to teloxide 0.17.
- **CRITICAL #2 (TOOL_WHITELIST conflation):** Architectural Constraint #3 split into two namespaces (plugin tools vs root-crate tools), each cited at correct file.
- **CRITICAL #3 (dual-poller in v0.6):** Surfaced as latent v0.6 vulnerability; Slice 4 explicitly disables plugin polling so daemon (via teloxide Dispatcher) is sole owner. Operator decision C2 reflected.
- **MAJOR #4 (`reply` in plugin, not daemon):** Slice 4 rewires plugin `handle_reply` to forward over UDS to daemon; CLI signature unchanged except for the optional `message_thread_id` added per C3.
- **MAJOR #5 (multi-CLI 409 storm via plugin pollers):** Same mitigation as #3.
- **MAJOR #6 (Playwright vs native TG Desktop):** Success Criteria evidence table now names OS-level screenshot tools + Telegram Web fallback.
- **MAJOR #7 (schema migration green-field assumption false):** Slice 2 migration discipline now handles pre-populated dev DBs.
- **MAJOR #8 (`/switch` race in groups):** Slice 5 race-condition note added; UNIQUE-constraint on the routing-key partial index serializes resolution.
- **MAJOR #9 (missing error handling):** Slice 3 error-handling subsection enumerates 5 cases (no token, 401, 409, no thread_id, orphan inbound).
- **MAJOR #10 (`message_thread_id` contract verification timing):** Moved to Slice 1 (pre-flight) instead of Slice 2.
- **MAJOR #11 (`channel_state.rs` was already in v0.6):** Slice 6 no longer talks about migration; both `permissions.rs` and `channel_state.rs` preserved as-is.
- **MINOR #12 (citation line drift):** notification.rs:59 corrected.
- **MINOR #13 (Slice 1 done-condition too weak):** Now requires `cargo build` + `cargo test --workspace` + daemon-starts + plugin-starts + teloxide-field-verification.
- **MINOR #14 (Inbound validation single entry for multiple sources):** Split into 4 entries (turn 1, turn 2, turn 3, C1/C2/C3).
- **MINOR #15 (OQ4 version collision with v0.9 fleet plan):** OQ4 amended with concrete options + recommendation v0.10.0.

### Acknowledged Minor Issues
- (none deferred — all addressed in v2 above)
