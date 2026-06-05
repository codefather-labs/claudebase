# claudebase v0.9 — Product Plan (skip-v0.7-v0.8 port-forward catalog)

**Document type:** Product plan (research + scoping, NOT implementation plan)
**Owner:** Mira (orchestrator) for tech lead review
**Date drafted:** 2026-06-04
**Branch context:** current work on `feat/multi-agent-on-v0.6` (26 commits beyond `claudebase-v0.6.0`); operator declared v0.7 and v0.8 broken and chose to ship the next release as **v0.9** directly, bypassing both.

---

## 1. Context

The v0.6→v0.7→v0.8 trajectory shipped two layers of work the operator considers «ошибочной и мусорной»:

- **v0.7** (29 commits): insights-corpus refactor (schema v5 + category/project_slug/tags + dual-DB reads + SessionStart read-insights hook + UserPromptSubmit self-check hook), project registry + claudebase run integration, /update-claudebase skill, ASCII-only PowerShell hook fix, repo presentation polish.
- **v0.8** (19 commits): chat_ask MCP tool + callback_query inline keyboards, chat.db schema v7 (chat-as-id routing), outbound `tg_message_map` for reply-quote routing, bot commands `/agents` `/switch` `/whoami` `/here`, bridge reconnect-replay cache + ensure-daemon-running, access.json File-A→File-B migration, channel-meta `chat_id`-as-string fix, installer wiring CC plugin slot to daemon bridge.

The operator's empirical verdict after extended debugging: «вся версия получилась косячной, точно неизвестно что пошло не так… откатываемся до 6 версии». The cause was not isolated; forward-debug cost exceeded rebuild cost. Mira's `feat/multi-agent-on-v0.6` branch re-implemented the v0.7/v0.8 product surface from scratch on the v0.6 baseline with empirical step-by-step verification at each delta.

The fleet-v0.9 plan upstream (`https://github.com/codefather-labs/claudebase/blob/main/docs/plans/claudebase-fleet-v0.9.md`) was authored against the v0.8 baseline and proposes a 7-slice "multi-bot fleet" feature on top. Since v0.9 will be cut from our v0.6+ branch (not v0.8), the fleet plan's slice dependencies need re-stating in our terms.

This document catalogues what is **already shipped on `feat/multi-agent-on-v0.6`**, what should be **port-forwarded from v0.7 / v0.8 / fleet-v0.9**, and what should be **explicitly out of scope**.

---

## 2. Already shipped on `feat/multi-agent-on-v0.6` (the v0.9 baseline)

These 26 commits ARE what v0.9 inherits — operator's «v0.6 with updates»:

| Theme | Commits | Status |
|---|---|---|
| Slice 1: `agent_registry` additive routing-key columns + COALESCE index | `da44b58` (earlier — already in tree) | live ✅ |
| Slice 2: routing-key extraction in `run_long_poll` | `a88bff0` | KP1 LIVE PASS ✅ |
| Slice 3: `chat_reply` gains `message_thread_id`; outbound topic propagation | `d8a8def` | live ✅ |
| Slice 4a/b/c: bot commands `/agents` `/switch` `/whoami` | `bfdfef3` + `3f83b4e` + `7ae7ea0` | live ✅ |
| Slice 4d/e: host/cwd/pid registration + `/here` | partial via cwd-basename auto-register | partial 🟡 |
| chat_id-as-string contract restore (KP1 root cause fix) | `4b29e6e` | live ✅ |
| Installer wires CC plugin slot to `claudebase plugin serve` daemon bridge | `6153f72` + `4ac910d` | live ✅ |
| `claudebase run` auto-spawns daemon; `daemon status` pipe-probe fallback | `c2b75a7` | live ✅ |
| Bridge self-bootstrap: auto-register + auto-subscribe at handshake | `333bdd1` | live ✅ |
| Per-project `.claudebase/config.json` (session_id + name persistence) | `25189bc` | live ✅ |
| Daemon-side rename-as-cleanup in `agent_registry::register` | `01c3a01` | live ✅ |
| Bridge re-fires self-bootstrap on `try_reconnect` | `4bb0c92` | live (with bug #2 — see §6) 🟡 |
| Remove orphaned `daemon access pair/list` CLI surface | `b507434` + `af43285` | live ✅ |
| `install.ps1` spawns daemon as current user (no SCM/LocalService) | `a615d9c` | live ✅ |
| HOME/USERPROFILE env injection into detached daemon | `1e337c7` | live ✅ |
| Bridge filters `<channel>` notifications by `meta.target_agent_id` | `0ba2c41` | live ✅ |
| `/agents` lists ALL alive CLIs with `(current)` marker | `87d5463` | live ✅ |
| **Slice 8a/b/c — `chat_ask` MCP tool + inline keyboard** | `5dfcf8d` + `86ab5ff` + `4a65819` | LIVE PASS (single + multi, both ask_ids verified) ✅ |
| Slice 8 AR-9 doc amendment | `02160cf` | docs in sync ✅ |
| Daemon `start/stop/restart/status` migrated SCM→Start-Process | `ffda4a9` | live (status verified) ✅ |

**KP1 (DM routing) LIVE-VERIFIED.** KP2/KP3 (forum-topic routing) pending live evidence — requires operator to set up group with forum topics + 3 CC sessions. Architecturally complete; gating only on evidence capture.

**Open bugs from this branch's session work** (carried into v0.9):
- **#2 — bridge auto-reconnect insufficient.** After daemon bounce, existing bridges fail to re-establish; operator must `/exit` + `claudebase run` for each CC. `try_reconnect` is finite-attempt one-shot, no proactive periodic retry.
- **#8 — daemon accept-loop hang after multi-hour uptime.** PID 4168 stopped accepting new UDS connections after ~2h though existing connections worked. Tokio runtime starvation or tracing-subscriber blocking suspected. Preserved evidence at `~/.claude/tools/claudebase/daemon-pre-bounce-2026-06-04.log`.
- **#6 (closed) — `daemon install`/`uninstall` still SCM-aware** (out of scope of this commit; dormant landmine).
- **#7 — `insights.db` corrupt** in current project (`error: index database invalid; re-ingest required` on every `insight create`).

---

## 3. Port-forward catalogue — what to TAKE from v0.7 / v0.8 / fleet-v0.9

### 3.1 From v0.7 — MUST-HAVE

#### A1 — Insights corpus schema v5 (`category` + `project_slug` + `insight_tags`)
**Commits:** `1161570` + `ff30d9f` + `c0eebca` + `2719e25` + `afddf71` + `6fbc7cf` + `9664dcd`
**Effort:** ~2 days · **Risk:** low (additive migration over v0.6 schema v1)

**What it does:** Today's `claudebase insight create` accepts only `--type`, `--agent`, `--feature` and writes to a single project-local `insights.db`. v0.7 makes the corpus dual-DB and tag-aware:
- **Two corpora** — one local (`<project>/.claude/knowledge/insights.db`) AND one global (`~/.claude/knowledge/insights.db`). The `--category project` flag routes the insight into local DB; `--category general` routes into global. `insight search` reads BOTH and fuses via RRF, so a Mira searching "this feature" finds cross-project lessons too.
- **First-class tags** — `--tags <list>` is required at create time (≥1 tag, comma-separated, normalised to lowercase, dedup'd, stored one row per tag in new `insight_tags(doc_id, tag)` table). `insight search` adds `--tag <t>` (repeatable, OR-semantics intersection per operator decision 2026-05-27). New subcommand `claudebase insight tags` lists distinct tags with counts merged across local + global.
- **Schema migration** — additive v2→v3→v4→v5 path; v4→v5 backfills existing agent rows with `category='project'` + derived `project_slug` + default tag from feature slug. Books-corpus rows untouched.

**Why port:** The hook reminders in this session (every UserPromptSubmit) literally diktirovat `claudebase insight create ... --category <general|project> --tags <tag> --salience <...>` but the deployed binary's `--help` does NOT have `--category` / `--tags` flags AND the corpus DB returns `error: index database invalid; re-ingest required`. **Cross-session learning is broken today.** Insights v5 is the contract those hook reminders were designed against.

**Side-effects:** Operator gets cross-project insight discovery + a tag vocabulary they can browse via `insight tags`. SDLC subagents stop re-discovering the same lessons each session.

**🔒 MANDATORY constraint (operator directive 2026-06-04): backward compatibility with existing insights.db files.** Any insights.db on operator's box that survived from a v0.6 (schema v1) install MUST continue to function after the v0.9 upgrade — without data loss, without `error: index database invalid` lockouts, without requiring `claudebase ingest --reset`. The v0.7 migration body (`1161570`) is already additive and backfills existing agent rows with `category='project'` + derived `project_slug` + default tag — that path covers the v0.6→v0.9 jump. **Implementer responsibility:** verify on a CORRUPT-state DB (the current followup #7 state on operator's box) that the migration's recovery path either repairs it OR exits with a clean repair-required message (NOT a silent data-loss event). Add a regression test: open a fresh v0.6-shaped DB → run v9 migration → assert all original rows survive with correct category + project_slug + at least one tag.

---

#### A2 — Project registry (`~/.claude/knowledge/projects.json`)
**Commits:** `cccef44` (+ Slice 4 inline read it replaces) **Effort:** ~1 day · **Risk:** low

**What it does:** Adds a centralised file `~/.claude/knowledge/projects.json` mapping `project_slug → {name, path, last_seen_iso}`. The `claudebase run` command (the launcher operator already uses) upserts the current cwd into the registry on every invocation — slug = basename of cwd (canonicalised, never from user input — security). Atomic write-temp-then-rename closes the same-PID concurrent-rename race. Adds `resolve_project_path(slug)` API so downstream tools can answer "where does project `tactics-trade` live?" without scanning disk.

**Why port:** Composes with **A1 insights v5**: `claudebase insight tags --project <slug>` needs `resolve_project_path(slug)` to find the project's local insights.db. Composes with **fleet-v0.9 Slice 4 startproject**: that command scaffolds new projects and the registry tracks them. Today the same lookup is done via inline `projects.json` read scattered across 3 callsites — registry centralises.

**v0.6+ overlap:** our `25189bc` adds `.claudebase/config.json` (per-project session_id + name). The registry is the cross-cutting INDEX of all those per-project configs — they don't conflict, they layer.

---

#### A3 — `/update-claudebase` slash-command skill
**Commit:** `4bc9a9c` **Effort:** ~0.5 day · **Risk:** low

**What it does:** A new SDLC skill at `prompts/commands/update-claudebase.md` that, when invoked (e.g. `/update-claudebase` from CC), updates the installed claudebase binary to latest by **reading the project's README FIRST** then running whichever install path matches the local box: `git pull && bash install.sh --local` for a checkout, or the README's remote one-liner curl/wget command for end-users. Verifies version delta after (`claudebase --version` before vs after) and reports what changed.

**Design principle:** Reads-README-first by design — never hardcodes install commands that could drift from the actual installer. Honours `CLAUDEBASE_SKIP_*` env opt-outs. Never `git rebase`, never `--force`, never publishes (updating is one-way consumption).

**Why port:** Operator-quality-of-life. Today the only way to update binary is to `cargo build --release` + `cp` manually OR run `bash install.sh --yes` from a fresh git checkout. The skill makes it one slash-command. Fleet-v0.9 Slice 5 references this skill as the binary-update mechanism.

---

#### A4 — SessionStart read-insights-on-new-context hook
**Commit:** `385efff` **Effort:** ~0.5 day · **Risk:** low

**What it does:** A new shell/PowerShell hook at `~/.claude/hooks/claudebase-read-insights-reminder.{sh,ps1}` wired into the SDLC `SessionStart` hook event (fires on session start, resume, and post-compact). It injects an `additionalContext` reminder telling the agent entering a fresh context to:
1. Discover the tag vocabulary: `claudebase insight tags --project <current-project>`
2. Load relevant insights by tag: `claudebase insight search "<keywords>" --tag <tag>`

Idempotent settings.json wiring added to install.sh (jq) and install.ps1 (ConvertFrom-Json) — installer doesn't double-register.

**Why port:** Compounds the value of **A1+A2**. Without this hook the corpus exists but agents never spontaneously query it on context resume — they only query when explicit `claudebase insight search` calls are needed mid-task. With the hook, every fresh context starts already aware of the relevant prior lessons.

**Current state:** the SessionStart hook IS deployed on operator's box (we see its output every session resume — long onboarding text dumped at the start of each conversation). But the read-insights-reminder companion hook is NOT wired — it's the missing piece.

---

#### A5 — UserPromptSubmit self-check hook + cognitive-self-check.md migration
**Commits:** `cb45b4d` + `0b92384` **Effort:** ~0.25 day · **Risk:** low

**What it does:** Two cognitive-infra changes bundled:
1. A `UserPromptSubmit` hook (`claudebase-selfcheck-reminder.{sh,ps1}`) that fires before every agent response and injects a short reminder of the three cognitive-self-check protocols (Facts / Decisions / Inbound). No systemMessage bubble (avoids per-prompt noise) — just `additionalContext`. **This session uses it** — we see "pre-response reminder" injected every turn.
2. `cognitive-self-check.md` rule moves into claudebase prompts/rules/ alongside `knowledge-base.md` + `knowledge-base-tool.md` + `tool-limitations.md`. Reason: the rule's External-contracts evidence discipline + salience tags are the foundation the books/insights corpora rest on, so it belongs with the tool that owns those corpora.

**Why port:** Already enabled in the SDLC config on operator's box (system-reminder confirms every prompt). But the HOOK SCRIPTS aren't necessarily on operator's box — verify before declaring "shipped". Likely needs only a small wiring patch to install.sh / install.ps1.

---

#### A6 — ASCII-only PowerShell hooks bugfix
**Commit:** `e43ca12` **Effort:** ~0.25 day · **Risk:** low

**What it does:** Both `.ps1` hooks (claudebase-insight-capture.ps1 + claudebase-selfcheck-reminder.ps1) contained non-ASCII glyphs (hook emoji, em-dashes, bullets). Windows PowerShell 5.1 parses no-BOM scripts in the local code page (NOT UTF-8), so multi-byte UTF-8 bytes corrupted string literals — mangled em-dash introduced stray quotes, parser aborted with `Unexpected token 'event=Stop'`. Fix: replace emoji with `[hook]`, em-dash/bullet with `-`. ASCII bytes are identical across UTF-8 / ANSI / any code page so scripts parse correctly regardless of how PS reads them. `.sh` variants keep emoji (Unix is UTF-8 native).

**Why port:** Operator is on Windows. This bug WILL hit immediately on a fresh install once A4/A5 hooks land. Bundle the fix with A4+A5.

### 3.2 From v0.7 — SKIP

| Item | Reason |
|---|---|
| Repo presentation (`.github` scaffolding + README hero) — `f8f9ea5` + `e6401df` + `4049eee` | Cosmetic; operator can do separately when convenient. |
| Stop-hook → UserPromptSubmit refactor — `8159cbc` + `a683875` | Internal SDLC plumbing; behavioural delta is captured by UserPromptSubmit hook (item above). |
| code-graph concept docs — `75af60d` | Planning-only doc; no code shipped. |
| Multi-CLI orchestration plan quartet — `4d6aa65` + `fd3b19f` | Reference material already used as basis for our work; don't re-import. |
| pre-/qa-cycle QA + PRD corrections — `7d07018` + `3a4be61` | SDLC pipeline doc updates; port only if `/qa-cycle` regressions surface during v0.9 QA. |
| TC-IHC parallel-safe test fix — `3c9966a` | Test-infra detail; port only if the failing test condition recurs. |
| Insight CLI BREAKING docs — `389ac4a` + `03647db` | Naturally absorbed when schema v5 lands. |

### 3.3 From v0.8 — MUST-HAVE

#### B1 — `/here` bot command (display CLI host + cwd + pid)
**Commit:** `0c4dc77` **Effort:** ~0.5 day · **Risk:** low

**What it does:** Adds `/here` to the bot-command surface alongside the existing `/agents` `/switch` `/whoami` we already ship. When operator types `/here` in TG, the bound CLI replies with a line showing **its host machine + cwd + pid** — answers "where is this CLI actually running?". Useful when operator has 3 CC sessions across 2 boxes and forgets which one mira is on.

**v0.8's hardening points** the port should preserve:
- **Cross-chat isolation** — `/here` scoped to THIS chat's bound CLI only (no cross-chat host:cwd leak). v0.8's red-team finding F-6.
- **Graceful "unavailable"** — when `agent_registry.metadata` lacks host/cwd (the v0.6+ rows don't populate it yet), reply with `host: unavailable cwd: unavailable` instead of error.

**Why port:** Closes Slice 4d/4e from scratchpad ("host/cwd/pid registration + live evidence"). Bug surface is small; high UX value once operator runs >2 sessions across machines.

**Side-effect required:** `agent_register` MCP tool gains optional `host` + `cwd` + `pid` fields; bridge populates them from `gethostname()` + current cwd + `getpid()` at register time. Stored in `agent_registry.metadata` JSON column.

---

#### B2 — e2e routing tests (Rust integration suite)
**Commit:** `70de8cf` (tests/e2e_routing.rs) **Effort:** ~1.5 days · **Risk:** medium (TG-fake harness stability)

**What it does:** A Rust integration test file `tests/e2e_routing.rs` covering 4 scenarios end-to-end (daemon process + bridge process + mocked teloxide):

- **TC-TMC-22.1** — chat-as-id routing resolves to bound CLI only (positive case: KP1-style routing through the real daemon)
- **TC-TMC-22.2** — cross-chat isolation: a planted third-chat binding is never reached when the inbound message belongs to chat A (KP2/KP3 negative case)
- **TC-TMC-22.3** — `active_cli_per_chat` + `tg_message_map` persist across a chat.db reopen (daemon-restart durability)
- **TC-TMC-19.1** — daemon-down sentinel: bridge returns clean `{status: "down"}` instead of hanging when daemon is unreachable

**Cases requiring real TG HTTP** (409 conflict, inline keyboard round-trip, live bus delivery) are explicitly routed to `/qa-cycle` (manual live-run) — the e2e suite stays hermetic.

**Why port:** Today we have unit tests + live KP1 evidence. Pre-requisite for `/merge-ready` Gate 6 (test coverage). Also formalises KP2/KP3 verification.

**Adapt to v0.6+:** v0.8's tests use the `tg_message_map` table; since we struck that, the corresponding test (TC-TMC-22.3) drops the `tg_message_map` assertion and tests only `active_cli_per_chat`.

---

#### B3 — Live smoke runbook (manual TG verification)
**Commit:** `5a316c0` (`docs/qa/.../smoke_runbook.md`) **Effort:** ~0.5 day · **Risk:** low

**What it does:** A step-by-step markdown runbook the operator (OR qa-engineer) follows to verify a fresh install end-to-end **with a live bot token + real button taps**. Each case lists: action operator performs → exact evidence to capture (daemon log substring, daemon status field, SQL row, OS screenshot). Cases covered:
- Daemon-online ping (bot reaches operator's DM)
- Live routing KP1 / KP2 / KP3
- Bot commands `/agents` `/switch` `/whoami` `/here`
- `chat_ask` button round-trip (single + multi + Done)
- 409 conflict gate (start a second daemon-or-plugin polling same token; first daemon stays alive, logs ONE backoff line)
- Cutover takeover (kill plugin polling, daemon takes over)

**Why port:** The Rust e2e suite (B2) is hermetic — cannot catch behaviour that requires the real Telegram HTTP boundary. Runbook is the load-bearing manual pass before `/merge-ready` calls MERGE READY. Pre-requisite for `/merge-ready` Gate 7 (docs).

---

#### B4 — Telegram daemon conflict gate (per-bot 409 protection)
**Commit:** `b8be4a5` **Effort:** ~0.5 day audit · **Risk:** low

**What it does:** A dedicated 409 `terminated by other getUpdates` branch in `run_long_poll` (Rust file in our `src/daemon/telegram.rs`). When the daemon's getUpdates hits Telegram's "another consumer is polling" error (e.g. legacy plugin still running, or operator started a second daemon), the branch:
- Logs ONE operator-actionable line (fixed literal — no `err_str` interpolation, so the bot token in teloxide error URL can never leak via logs — security)
- Backs off **60 seconds** (not the generic 5s that would spam log)
- Continues — daemon NEVER crashes (preserves NFR-3 from v0.8 plan)
- `conflict_logged` flag suppresses repeats — 10 consecutive 409s produce exactly 1 log line; recovery edge logs ONCE when a later poll succeeds so operator sees both "conflict-started" and "conflict-cleared"
- Detection via `is_conflict_error()` string-match, consistent with existing 401/429 handling

**Plus migration flag** `[telegram] enabled` in daemon.toml (default `true`, SEC-15-hardened TOML loader). `enabled=false` → daemon skips spawning the poller (the revert path operator can use to fall back to legacy plugin). Missing/malformed toml defaults to enabled (a config error must never silently kill Telegram).

**Why port:** We already have parts of this — daemon-only ownership eliminates the dual-poller case via architecture. But the **per-bot 60s backoff with deduplicated log** is missing — without it, if any external process accidentally polls the same token (a leftover plugin process, a typo'd `claudebase run` in a wrong cwd), daemon would log every 5s forever. Audit-and-port: ~0.5 day if our current code already has the structure (just need to add the dedup + 60s backoff); ~1.5 days if we need to add from scratch.

### 3.4 From v0.8 — ALREADY SHIPPED on our branch

Do NOT re-port:

- chat_ask MCP tool + callback handling — `9fe22cc` (we did Slice 8a/8b/8c cleaner with AR-9 amendment)
- bot commands `/agents` `/switch` `/whoami` — `0c4dc77` (we did Slice 4b/4c)
- chat-as-id 5-step routing decision tree — `d39599b` (our Slice 2 + 4c does the equivalent)
- chat.db schema v7 routing tables — `6e67d8b` (our additive migration covers it)
- Telegram daemon conflict gate + migration flag — `b8be4a5` (our `4b29e6e` + daemon-only ownership covers)
- channel-meta `chat_id`-as-string — `f69c634` (our `4b29e6e` is the same fix)
- installer wires CC plugin slot to daemon bridge — `4c02b98` (our `6153f72` + `4ac910d`)

### 3.5 From v0.8 — SKIP

| Item | Reason |
|---|---|
| Bridge reconnect-replay cache + ensure-daemon-running — `a328d43` (WIP) | v0.8 itself shipped this as WIP-partial. Our `4bb0c92` + `c2b75a7` covers ensure-daemon-running; replay cache is open question. Don't re-import a partial; rebuild if needed when bug #2 is properly fixed. |
| access.json File A→B migration — `3549743` + `de23205` + `ca5fcff` | Our branch was built on v0.6 baseline which uses the canonical path already; no legacy A-path to migrate. |
| v0.7.1 release-matrix fix — `6aee5cc` | Infra patch already absorbed into our release tooling. |
| Telegram-multi-cli bootstrap docs — `f8bc0ed` | Superseded by our `multi-agent-telegram-on-v0.6` plan + PRD §18. |

### 3.6 From fleet-v0.9 plan — MUST-HAVE (new features)

These are the **net-new** features the fleet plan adds beyond v0.6+ baseline:

#### C1 — Multi-bot secret store + CLI commands
**Effort:** ~3 days · **Risk:** medium (auth surface — SECURITY pre-review required per fleet plan)

**What it does:** Operator can register MULTIPLE Telegram bot tokens against the same daemon. New CLI surface:
- `claudebase telegram addbot <name> <token>` — validates the token via `getMe` API call (rejects invalid before storage), stores token + bot username + `getMe` user_id under operator-chosen name.
- `claudebase telegram listbots` — lists registered bots with token MASKED (`1234567890:****..xyz`); shows bot username + status.
- `claudebase telegram removebot <name>` — drops the entry; idempotent (no-op on unknown name).

**Storage choice (open question OQ-2 in §5):** TOML file `~/.config/claudebase/telegram-bots.toml` (human-editable, simple) vs DB rows in `claudebase.db` (queryable, transactional). Recommend TOML for v0.9 — smaller surface, easier to back up.

**Why include:** Direct operator request. Today the daemon reads a SINGLE `TELEGRAM_BOT_TOKEN` env var (or `.env` file). This forces operator into one-bot-per-machine — can't run `@mira_bot` and `@fbscout_bot` from the same install. Multi-bot is the foundation feature of "fleet". Existing v0.6+ single-bot path stays intact (the env var becomes the implicit default-bot).

**Security pre-review focus:** token redaction in logs, token-at-rest file permissions (0600 on Unix; ACL on Windows), `getMe` validation must not echo token in error messages.

---

#### C2 — Daemon multi-bot long-poll (concurrent async tasks)
**Effort:** ~3 days · **Risk:** high (concurrency + isolation — ARCHITECT pre-review required)

**What it does:** Extends `run_long_poll` (currently single instance polling one token) to spawn ONE async task per registered bot. Each task has its own teloxide `Bot` instance and its own state (offset, conflict-gate dedup flag from B4). The chat broadcast bus stays shared — all bots publish to the same `<channel>` consumer surface, but inbound messages get tagged with `meta.bot_name` so consumers can filter.

**Isolation guarantee:** "one bad token must not take down the fleet" (fleet plan acceptance). If bot `@foo` hits 401 (token revoked), only foo's task exits gracefully; bots `@bar` and `@baz` keep polling. If bot `@foo` hits 409 (conflict), only foo's task backs off (via B4 logic per-task).

**Architectural note:** Single shared `daemon.toml` `[telegram]` section becomes a `[[bots]]` array (one entry per bot). The C1 TOML store IS this array — `addbot` appends, `removebot` removes, daemon hot-reloads on file change OR requires restart (TBD — operator preference).

**Why include:** Pre-requisite for C1's user-visible value. Architecturally significant — needs concurrency review.

**Architect pre-review focus:** task lifecycle (spawn / cancel / re-spawn on hot-reload), bus arc-sharing correctness, tokio task supervision (panic in one task must not panic siblings — `JoinHandle::abort` correctness).

---

#### C3 — `claudebase run --dangerously-skip-permissions` + daemon-ensure
**Effort:** ~1 day · **Risk:** medium (RED-TEAM-gated per fleet plan + operator-stated principle)

**What it does:** Adds `--dangerously-skip-permissions` flag to `claudebase run`. When set, the launcher passes through equivalent flags to the spawned `claude` CLI so the operator doesn't get permission prompts for routine read-only tool calls (bash `git status`, Read on common paths, etc.). Daemon-ensure-running is already shipped on our branch (`c2b75a7`).

**Critical UX detail:** the flag is DANGEROUS — named accordingly per Claude Code convention. Skipping permissions means the agent can execute ANY bash command without operator confirmation, which is the right trade-off for trusted demo / automation contexts but the WRONG trade-off for ad-hoc exploration. Default MUST be OFF; flag is opt-in per-invocation; loud warning printed when flag is in effect.

**Why include:** Operator-stated quality-of-life — eliminates the constant permission-prompt friction during multi-CC fleet operation. Fleet plan flagged the safety-vs-convenience trade-off as load-bearing OQ.

**Red-team focus:** can the flag leak from one CC into another? Can a malicious project's `.claude/settings.local.json` flip the default? Should it require explicit `--yes-i-know-this-is-dangerous` double-confirm?

**Open question OQ-1 in §5:** default-on or opt-in. Recommend opt-in (operator types the flag explicitly).

---

#### C4 — `claudebase startproject` scaffold command
**Effort:** ~1.5 days · **Risk:** low

**What it does:** New CLI `claudebase startproject [<name>]`. When invoked in a directory:
1. Creates `<cwd>/.claudebase/config.json` with `{session_id: <new uuid>, name: <provided OR cwd basename>}`
2. Adds the project to the registry (`~/.claude/knowledge/projects.json` from A2) so insight tooling finds it
3. Optionally binds a default agent_id (operator-chosen at scaffold time)
4. Prints a "next steps" banner showing how to `claudebase run` from this directory

**Idempotent:** running on an already-initialized directory is a no-op + helpful message.

**Why include:** Closes the gap between today's ad-hoc `.claudebase/` creation (which only happens implicitly when `claudebase run` is first called from a new cwd) and a proper init flow. Composes cleanly with A2 (project registry) and A1 (`insight create --project` lookup).

**Architectural decision:** `startproject` is a CONVENIENCE command, not the only path. Existing flow (run from a fresh cwd → auto-create config) keeps working. `startproject` is for operators who want to set up the binding BEFORE running.

---

#### C5 — `claudebase update` binary self-update + `claudebase daemon setup` per-platform boot integration
**Effort:** ~2 days · **Risk:** medium (install footprint differs per OS)

**What it does:** Two related commands:

**`claudebase update`** — CLI-level binary self-update. Fetches latest release from GitHub releases (the canonical install source per install.sh/install.ps1), verifies checksum if released signed, replaces the installed binary atomically (write-temp + rename, exits if daemon is running so the operator must stop daemon first), reports the version delta. Differs from A3 (`/update-claudebase` skill) by being CLI-level (anyone can run; doesn't require CC session) — the skill is the higher-level "do all the right things" interface; the CLI is the primitive.

**`claudebase daemon setup`** — wires daemon-on-boot per-platform:
- **Windows** — Scheduled Task via `schtasks` (NOT SCM service — we deprecated SCM via `a615d9c` + `ffda4a9` for the LocalService-profile-redirect reason). Task runs as current user at logon; daemon survives logout to next logon. Replaces today's "install.ps1 does Start-Process once but daemon dies on reboot" gap.
- **macOS** — launchd plist at `~/Library/LaunchAgents/com.claudebase.daemon.plist` so daemon auto-starts on user login.
- **Linux** — systemd user unit at `~/.config/systemd/user/claudebase-daemon.service` so daemon survives reboots via `systemctl --user enable`.

`claudebase daemon teardown` reverses each (removes the scheduled-task / launchd / systemd entry).

**Why include:** Closes followup #6 dormant `daemon install/uninstall` surface (we deprecated SCM but didn't replace with anything; teardown just leaves the daemon manual). Operator's box loses daemon on logout currently; `daemon setup` fixes for fleet operation.

---

#### C6 — `/start` Telegram inline-menu (operator's first interaction with bot)
**Effort:** ~1.5 days · **Risk:** low (security-gated — only paired operators can hit menu actions)

**What it does (per operator spec 2026-06-04):** When operator sends `/start` to the bot, daemon responds with a message + inline keyboard containing two buttons:
- **`agents`** — tap → emits a new push message (text reply) listing every alive CLI from `agent_registry` (same payload as the `/agents` bot command); no further button interaction needed.
- **`switch`** — tap → emits a SECOND push message with a NEW inline keyboard, where each button is one of the currently-alive CLIs (one button per agent_name). Tapping one of those CLI-buttons rebinds the routing key `(this_chat_id, this_thread_id)` to that CLI (same effect as `/switch <name>` typed manually). Operator sees the binding update reflected in the reply.

**Implementation:** Built on top of our shipped **Slice 8 `chat_ask` AR-9 pattern** — both stages are `chat_ask` calls with `multi=false` (single-tap finalisation). The two-stage flow is:

1. `/start` handler builds a `chat_ask` with `options=[{label:"agents",value:"agents"},{label:"switch",value:"switch"}]`, sends it as a new keyboard message
2. Operator taps a button → daemon receives CallbackQuery with `<ask_id>:agents` or `<ask_id>:switch`
3. If `agents` → daemon replies in plain text with the `list_alive` bullet list (NO further keyboard)
4. If `switch` → daemon queries `list_alive` AT TAP TIME (fresh snapshot — important: if CLIs come/go between `/start` and the tap, the second keyboard reflects current reality), builds a SECOND `chat_ask` with one option per alive agent_name, sends as new push message
5. Operator taps a CLI-button → daemon receives `<ask_id_2>:<agent_name>` → calls `handle_switch(...)` (the existing security-validated function) → replies with `Switched to <agent_name>` OR security-denied reply per existing `/switch` rules

**Future menu items (deferred to v0.10):** Pair / Help / multi-bot Switch — kept out of v0.9 scope per operator request to keep the initial menu minimal.

**Why include:** First operator interaction with a fresh bot today is a silent void or a dry "Hello!" — operator must already KNOW typed commands. `/start` becomes the discoverability surface AND the most-common-action shortcut. Switch-via-button removes the need to remember CLI names.

**Security focus:** the second-stage CLI-keyboard is generated from `list_alive` AT TAP TIME (NOT cached from `/start` time) — prevents stale-binding attacks where a CLI dies between menu render and tap. The CLI-tap callback enforces the same FR-MAT-8.6 `last_user_id` security gate the typed `/switch` already uses (only the prior binder OR a chat admin may rebind).

**UX edge cases:**
- No alive CLIs at switch-tap time → second keyboard collapses to single button "no CLIs alive — try `/agents` later"
- Operator taps the CLI already bound to this chat → no-op success ("already switched to X")
- Operator's `last_user_id` doesn't match → security denial reply (same as typed `/switch`)

---

#### C7 — Docs + e2e + 9 quality gates + release workflow
**Effort:** ~2 days · **Risk:** low

**What it does:** The standard release tail. README updates (v0.9 fleet setup section + multi-bot + `startproject` + `daemon setup`), `/qa-cycle` for live runbook (composes with B3 above), `/merge-ready` 9 gates, `/release` cut `claudebase-v0.9.0` tag + GitHub release with changelog + asset matrix.

**Why include:** Required to ship.

### 3.7 From fleet-v0.9 plan — OUT OF SCOPE for v0.9 (per fleet plan itself)

| Item | Reason |
|---|---|
| Cross-machine HTTP/WSS fleet | Separate `claudebase-server-foundation.md` plan |
| Topic-as-id for forum-group threading | We ALREADY DID this in v0.6+ (Slice 2) — `routing_thread_id` column + extraction. The fleet plan listed it as out-of-scope because v0.8 hadn't done it; we did. |
| v0.8.1 polish (session auto-register, pair-reply bugs) | Our branch already absorbed via `333bdd1` (bridge self-bootstrap) + `01c3a01` (rename-cleanup) + other slices |

---

## 4. Proposed v0.9 slice structure

Synthesising §2 + §3 above:

```
v0.9 = current feat/multi-agent-on-v0.6 (26 commits)
     + port-forward Wave A: insights v5 + project registry + UpdateClaudebase skill + SessionStart-read-insights + ASCII-PS-hook fix
     + port-forward Wave B: tg_message_map + /here + e2e routing tests + smoke runbook
     + fleet plan Wave C: multi-bot store + multi-bot long-poll + startproject + /start inline menu
     + fleet plan Wave D: daemon setup (Windows/macOS/Linux) + binary update + permissions bypass + docs/gates/release
     + bug-fix Wave E: bridge proactive-retry (bug #2) + accept-loop survival (bug #8) + insights.db re-ingest (followup #7)
```

**Operator decision 2026-06-04:** scope of v0.9 narrowed to **Wave A + Wave D ONLY**. Waves B + C are deferred to v0.10. The reasoning: the v0.6+ branch's TG product surface (KP1 LIVE-verified + Slice 8 chat_ask live-verified) is good enough to ship as-is; insights corpus restoration is the actual immediate blocker. Multi-bot fleet, /start menu, KP2/KP3 evidence, and bug #2 / #8 fixes are valuable but not load-bearing for shipping v0.9.

| Wave | Slices | Estimated effort | Pre-review gates | Status |
|---|---|---|---|---|
| **A — Insights + project registry** | A1: insights v5 schema migration · A2: project registry · A3: `/update-claudebase` skill · A4: SessionStart read-insights hook · A5: UserPromptSubmit + cognitive-self-check.md migration · A6: ASCII-PS hooks bugfix | ~4 days | ARCHITECT on dual-DB fusion; SECURITY on tag injection + token-redaction | **v0.9 scope** ✅ |
| ~~B — Telegram polish (v0.8 ports + bug-fixes + KP2/KP3 verify)~~ | B1-B6 (`/here`, e2e tests, smoke runbook, conflict-gate, bug #2 bridge proactive-retry, bug #8 daemon accept-loop survival) | ~5 days | — | **Deferred to v0.10** 🟡 |
| ~~C — Multi-bot fleet (fleet plan core)~~ | C1-C6 (multi-bot store, multi-bot long-poll, --dangerously-skip-permissions, startproject, update+setup, /start inline menu) | ~11 days | — | **Deferred to v0.10** 🟡 |
| **D — Release infra** | D1: 9-gate `/merge-ready` · D2: CHANGELOG `[Unreleased]` finalisation · D3: `/release` cut claudebase-v0.9.0 | ~2 days | (none) | **v0.9 scope** ✅ |

**Total v0.9 effort:** ~6 days (A + D only) — focused release fixing the broken insights corpus + cutting a tag for the v0.6+ TG work that's already shipped on the branch.

**v0.9 implementation directive (operator 2026-06-04):** Wave A IS a code REUSE exercise, NOT a re-architecture. The implementer should **cherry-pick or port the actual v0.7 source code** (commits `1161570` + `ff30d9f` + `afddf71` + `c0eebca` + `2719e25` + `cccef44` + `4bc9a9c` + `385efff` + `cb45b4d` + `0b92384` + `e43ca12` + supporting tests + helper files) rather than re-implementing the schema migration / dual-DB reads / project registry / hooks from scratch. Architect pre-review focuses on **compatibility with v0.6+ baseline** (do the v0.7 commits cherry-pick cleanly on top of our 26 commits, or are there merge conflicts?), NOT on re-deriving the architecture. The v0.7 architecture was already approved + shipped in upstream; we are bringing it forward, not reinventing.

`tg_message_map` (originally proposed in §3.3) removed per operator decision 2026-06-04 (no longer interesting).

---

## 5. Open questions for tech lead / operator before bootstrap

1. **Default-on or opt-in for `--dangerously-skip-permissions` (Slice D3)?** Fleet plan flagged this as load-bearing OQ. Operator's safety-vs-convenience preference is a binary call.
2. **Multi-bot secret store location:** TOML file (`~/.config/claudebase/telegram-bots.toml`) vs DB rows (`claudebase.db` registry table)? TOML is simpler + human-editable; DB is queryable + transactional. Recommend TOML for v0.9 (smaller surface).
3. **Bot naming scheme** (C1): bot1/bot2/bot3 numeric vs operator-named (`personal`, `support`, etc.)? Recommend operator-named — matches how `/switch <name>` already works for CLI sessions.
4. **Should Wave A (insights v5) ship in v0.9, or is v0.9 strictly the multi-agent-TG + fleet feature?** Insights v5 is desirable but a different product surface. Operator may prefer to keep v0.9 focused and ship insights v5 in v0.10. **Recommendation:** include in v0.9 because the corrupt-insights.db landmine blocks every cross-session learning attempt today.
5. **`/here` command — what does it print?** v0.8's version showed cwd + host + pid. Confirm minimal surface vs full (e.g., uptime, last-active-thread, etc.).
6. **e2e routing tests (B3):** TG-fake (mock teloxide) vs live-TG (requires bot token in CI)? Live-TG is more honest but harder to make hermetic in CI. Recommend TG-fake for hermetic gate + live-TG for pre-release verification only.
7. **Open bug #2 (bridge reconnect) handling order:** fix as part of Wave B (before v0.9 ships) OR ship v0.9 with operator-known workaround (`/exit` + `claudebase run` after daemon bounce)? Recommend fix in Wave B — without it Slice D3's "turnkey daemon ensure" is half-functional.

---

## 6. Reuse / Reject summary table

| Source | MUST-HAVE | NICE | REJECT |
|---|---|---|---|
| **v0.6+ branch (already on `feat/multi-agent-on-v0.6`)** | 26 commits — Slice 1–8 multi-agent-TG | n/a | n/a |
| **v0.7** | insights v5 schema · dual-DB reads · project registry · `/update-claudebase` skill · SessionStart-read-insights hook · ASCII-PS hook fix | UserPromptSubmit self-check (verify already on) | repo presentation · Stop-hook refactor · code-graph docs · TC-IHC test fix |
| **v0.8** | `/here` command (B1) · e2e routing tests (B2) · live smoke runbook (B3) · daemon conflict-gate audit (B4) | n/a | `tg_message_map` reply-quote routing (operator-rejected as no-longer-interesting) · bridge reconnect-replay cache (WIP) · access.json A→B migration · v0.7.1 release-matrix fix · multi-cli bootstrap docs (already superseded) |
| **fleet-v0.9 plan** | Slice 1 multi-bot store · Slice 2 multi-bot long-poll · Slice 3 `--dangerously-skip-permissions` · Slice 4 startproject · Slice 5 binary update + daemon setup · Slice 6 `/start` inline menu · Slice 7 docs/gates/release | n/a | cross-machine HTTP/WSS · topic-as-id (already done) · v0.8.1 polish (already absorbed) |

---

## 7. Risks specific to v0.9 cut

| Risk | Mitigation |
|---|---|
| **R-V9-1: Skipping v0.7 + v0.8 tags creates ecosystem confusion.** Downstream consumers tracking GitHub tags may not realise v0.7/v0.8 are dead branches. | Ship release notes that explicitly state v0.7 + v0.8 are deprecated; v0.9 is the next supported version. CHANGELOG `[0.9.0]` block enumerates v0.7+v0.8 ports for context. |
| **R-V9-2: Insights corpus migration on dev boxes with the corrupt-state followup #7.** | A1 must handle "corpus DB invalid; recreate" as a recovery path in addition to the v0.6→v0.7 fresh-DB path. |
| **R-V9-3: Multi-bot long-poll concurrency bugs** (Wave C2). | Per fleet plan ARCHITECT pre-review gate. Add chaos-test in B3 e2e suite: kill one bot's polling task and verify others stay alive. |
| **R-V9-4: bug #2 bridge reconnect fix is harder than estimated.** | If proactive-retry redesign exceeds 2 days, ship v0.9 with the workaround documented; cut bug-fix in v0.9.1. |
| **R-V9-5: TG strict-meta-schema (AR-9) generalises to all CC channel surfaces.** | Document the constraint in `docs/architecture/cc-channel-meta-shape.md` as an architectural invariant any future feature must respect. |
| **R-V9-6: v0.7 `cccef44` project registry conflicts with v0.6+ `25189bc` per-project `.claudebase/config.json`.** | A3 must reconcile: confirm the registry is a centralised index of all projects, while `.claudebase/config.json` stays as the per-project source of truth. |

---

## 8. Recommended next step

If approved, the orchestrator should:

1. Spawn `prd-writer` to draft `docs/PRD.md §19 claudebase-v0.9` from this product plan
2. Spawn `ba-analyst` for use-cases (one per Wave A/B/C/D feature)
3. Spawn `architect` on the load-bearing pre-reviews per the fleet plan's gating (SECURITY on C1+C4, ARCHITECT on C2+B5+B6, RED-TEAM on D3)
4. Spawn `qa-planner` for test cases (B3 e2e + KP2/KP3 evidence runbook)
5. Spawn `planner` to convert Waves A/B/C/D into 4–7-slice executable plan with wave parallelism where files don't overlap

Estimated bootstrap cycle: ~1 day (PRD + use-cases + QA + arch + planner with red-team). Then ~22 days implementation across 4 waves.

---

## Facts

### Verified facts

- 5 release tags exist locally: `claudebase-v0.4.0` … `claudebase-v0.8.0` — verified via `git tag -l 'claudebase-v*'` in this session — salience: high.
- v0.6.0→v0.7.0 diff: 29 non-merge commits, 70 files changed (~9436 insertions / ~1691 deletions) — verified via `git log --no-merges` + `git diff --stat` this session — salience: high.
- v0.7.0→v0.8.0 diff: 19 non-merge commits, 30 files changed (~8253 insertions / ~1244 deletions) — verified via same — salience: high.
- `feat/multi-agent-on-v0.6` has 26 commits beyond `claudebase-v0.6.0`, 30 files changed (~7560 insertions / ~1390 deletions) — verified via `git log claudebase-v0.6.0..HEAD` this session — salience: high.
- Fleet-v0.9 plan structure (7 slices, multi-bot focus, explicit out-of-scope list) — verified via WebFetch of `https://raw.githubusercontent.com/codefather-labs/claudebase/main/docs/plans/claudebase-fleet-v0.9.md` this session — salience: high.
- Local plan `docs/plans/multi-agent-telegram-on-v0.6.md` exists with v2 architecture decisions C1/C2/C3 — verified via Read of lines 1-80 this session — salience: medium.

### External contracts

- **GitHub release tags `claudebase-vX.Y.Z`** — symbol: tag-naming convention — source: `git tag -l` output this session — verified: yes — salience: medium.
- **fleet-v0.9 plan upstream** — symbol: 7 ordered slices, dependency on v0.8.0 — source: raw.githubusercontent.com this session — verified: yes (via WebFetch summary; full file not opened) — salience: high.

### Assumptions

- Effort estimates (days) are rough Mira-estimates with no calibration against operator's actual velocity. Risk: estimates off by 2–4×. How to verify: planner refines per-slice during bootstrap. Salience: medium.
- "Already shipped on `feat/multi-agent-on-v0.6` covers the same ground as v0.8 chat_ask + bot commands + chat-as-id routing" — based on commit-subject correspondence; the IMPLEMENTATIONS differ. Risk: subtle behavioural deltas. Mitigation: e2e routing tests (B3) catch them. Salience: medium.
- Fleet plan's Slice 1 "TOML vs DB store" recommendation is Mira's preference for v0.9 — not yet operator-confirmed. Salience: low.

### Open questions

- All seven OQ items in §5 await operator/tech lead decision before bootstrap. Salience: high.
- The "skipping v0.7+v0.8 tags" semver narrative: do we want a `v0.9.0` cut at all, or is `v0.7.0-rebuild` / `v0.10.0` more honest? Recommendation in §7 R-V9-1 is to keep `v0.9.0` and explain via CHANGELOG. Operator decision. Salience: medium.

## Decisions

### Inbound validation

- Operator's task: «выясни что ушло вперед и собери продуктовый план того что мы можем перенести из 7 и 8 версии в текущую 6ю с последними ее апдейтами. мы сделаем ее 9той в обход 7 и 8». Plus instruction to consult local plan + upstream fleet plan. Challenged: no — straightforward research-and-synthesis task. Outcome: proceeded as instructed. Salience: high.
- Recommendation that Wave A (insights v5) ship in v0.9 rather than v0.10 — pushed back on the strict-scope reading because insights.db corruption blocks every cross-session lesson today. Surfaced under §5 OQ-4. Operator decides. Salience: medium.

### Decisions made

- Skip v0.7 / v0.8 SCM-related work entirely — superseded by our `a615d9c` + `ffda4a9` Start-Process migration. Q1 hack? no | Q2 sane? yes | Q3 alternatives? cherry-pick the v0.8 SCM-aware bits — rejected (operator already chose the post-SCM path) | Q4 cause | Q5 n/a. Salience: medium.
- Skip v0.7's repo-presentation commits (`.github` scaffolding) — not blocking; operator can do separately when convenient. Q1-Q5 trivial. Salience: low.
- Recommend `tg_message_map` (v0.8 `3965292`) as MUST-HAVE port — single concrete v0.8 feature we did not re-implement, low-risk additive table, high UX value. Q1 hack? no | Q2 sane? yes | Q3 alternatives? skip — rejected (gap in our branch) | Q4 cause | Q5 n/a. Salience: high.
- Recommend keeping `v0.9.0` as the release number rather than skipping further to `v0.10.0` — minimises ecosystem confusion (one-step skip is the smaller surprise than two-step). Q1-Q5 trivial. Salience: low.

### Hacks acknowledged

(none — research/planning task; no shortcuts taken)

### Symptom-only patches (with root-cause links)

- Estimating ports based on commit-subject correspondence rather than file-by-file diff is symptom-only: I'm assuming "v0.8 chat_ask commit `9fe22cc`" and our Slice 8a/b/c are equivalent without diffing the actual implementations. Root cause that remains: behavioural-equivalence verification. Tracked at: §5 OQ + §3.4 ALREADY-SHIPPED list + Wave B's e2e tests will surface delta if any. Salience: medium.
