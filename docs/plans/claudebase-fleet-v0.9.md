# Plan: claudebase Telegram Fleet + Project Config + Lifecycle CLI (v0.9)

> Future scoping document (authored 2026-05-31, post-v0.8.0). NOT yet
> bootstrapped. Feeds `/bootstrap-feature` when picked up. Builds directly on
> the v0.8.0 telegram-multi-cli + daemon-bridge + channel-push stack that is
> live on `main` (tag `claudebase-v0.8.0`).

## Context — why

v0.8.0 made ONE Telegram bot, served by the daemon, push routed messages into
a live Claude Code session. The operator wants the **turnkey fleet** experience:
add any number of bot tokens once, scaffold a project, and `claudebase run`
"just works" — a single always-on daemon polls every registered bot, the CLI
launches with permissions pre-granted, and the bot itself offers a tappable
`/start` menu. This plan turns the v0.8 plumbing into a product an operator can
drive without manual `agent_register` / `secrets.toml` editing / `--channels`
incantations.

## Requirements (operator-stated, with current-state delta)

### R1 — `claudebase telegram addbot "<bot_token>"` (multi-bot token store)
- Save a bot token into a **global** claudebase store so the daemon can serve
  many bots. **Current:** a single token lives in `secrets.toml`
  (`[telegram] bot_token`, 0600, SEC-15 — `src/daemon/config.rs`). **Delta:**
  a multi-bot secret store (e.g. `~/.config/claudebase/bots.toml` 0600, or a
  `bots` table in a global DB) holding N `(bot_id/username, token)` rows; the
  daemon polls every stored bot. `addbot` validates the token via `getMe`
  before storing (reject invalid), is idempotent on the same token, and never
  logs the token (NFR-VR-style redaction).
- Companion verbs to scope properly: `telegram listbots` (masked), `telegram
  removebot <id>`.

### R2 — `claudebase startproject [path]` (project `.claudebase` config)
- Scaffold a `.claudebase` config in the project at `path` (default = cwd).
  **Current:** `claudebase run` upserts a project into the registry
  (`registry::upsert_project`, `src/main.rs:174`); the `claudebase-project-dir.md`
  plan in `docs/plans/` already designed a per-project `.claudebase/`
  directory + `identity.local`. **Delta:** a `startproject` command that
  materialises `.claudebase` (project name/slug, default agent name, optional
  default bot binding) so a project is self-describing. Reuse / supersede the
  `claudebase-project-dir.md` design. Idempotent; refuses to clobber an
  existing `.claudebase` without `--force`.

### R3 — `claudebase run` upgrades
- **R3a — dangerous-skip-permissions mode.** `run` launches `claude` with the
  permission-bypass flag so the CLI does not re-prompt on every action.
  **Current:** `run_claude_with_preset` (`src/main.rs:163`) execs `claude
  --channels plugin:telegram@claude-plugins-official`. **Delta:** add
  `--dangerously-skip-permissions` to the exec argv (EXTERNAL CONTRACT —
  verify the exact flag against `claude --help` at implementation; the
  `run --no-ask` backlog item maps here). Consider gating behind an explicit
  `--no-ask` / `--yolo` opt-in vs default-on (operator wants default-on; the
  bootstrap red-team should weigh the safety of default-on skip-permissions).
- **R3b — ensure-daemon-running.** `run` MUST start a fresh daemon OR verify
  the current one is up before launching. **Current:** the v0.8 plugin bridge
  has `ensure_daemon_running` (`src/plugin/bridge.rs`, commit a328d43);
  `claudebase run` does NOT ensure the daemon. **Delta:** `run` calls an
  ensure-daemon helper (connect-probe the UDS socket; spawn detached
  `claudebase daemon serve` if down; fslock backstops dups). Idempotent — never
  a second poller.

### R4 — daemon connects to ALL bots + listens for commands
- The single always-on daemon polls every token from R1's store and routes/
  serves each. **Current:** the daemon's long-poll (`src/daemon/telegram.rs`
  `run_long_poll`) polls ONE bot (one `getUpdates` loop). **Delta:** N
  concurrent long-poll loops (one tokio task per bot token), each tagging its
  thread/notifications with the originating bot so chat-as-id routing,
  access.json gating, and `chat_ask` all stay correct per-bot. The 409
  conflict-gate (v0.8) applies per-bot. NFR: a bad/duplicate token for one bot
  must not crash the others (per-loop isolation).

### R5 — daemon lifecycle CLI: `claudebase daemon {restart,stop,start,update,setup}`
- **Current:** `start` / `stop` / `restart` / `install` / `uninstall` /
  `status` / `logs` / `doctor` already exist (`src/main.rs:139-143`). **Delta:**
  add `daemon update` (pull + swap the daemon binary, restart the service) and
  `daemon setup` (one-shot: install the service unit + ensure it auto-starts at
  boot — Windows SCM / launchd / systemd; the `setup daemon` backlog item).
  `daemon setup` is the turnkey "make it permanent" command.

### R6 — `claudebase update` (self-update)
- A top-level `claudebase update` that downloads the latest release binary from
  GitHub (the `claudebase-v*` release matrix), verifies it, and swaps the
  installed `~/.claude/tools/claudebase/claudebase`. **Current:** an
  `/update-claudebase` slash-command skill exists; there is no `claudebase
  update` CLI subcommand. **Delta:** the CLI subcommand mirrors the installer's
  download path (dynamic latest-tag resolution, sha-verify, atomic swap) and
  optionally chains `daemon restart` so the running daemon picks up the new
  binary.

### R7 — in-Telegram `/start` inline menu (pairing / switch / help)
- `/start` opens a Telegram message with inline-keyboard buttons: **Pairing**,
  **Switch**, **Help**. **Current:** `/start` returns `None` — no reply
  (`src/daemon/telegram.rs:488`, by design in v0.8). **Delta:** `/start`
  renders an `InlineKeyboardMarkup` (reuse the `chat_ask` callback plumbing
  from v0.8 Slice 5). Button taps drive sub-flows: **Pairing** → start/continue
  the pairing approval flow; **Switch** → list alive CLIs as buttons → tap to
  `/switch`; **Help** → the `/help` text. Each callback re-uses the 4-step
  callback validation + the access allowlist gate added in v0.8 (the
  callback-allowlist defense-in-depth). SECURITY: in a group, the per-tap
  owner check (deferred F-4) becomes relevant — gate menu actions by the
  tapping user.

## Preliminary slices (planner refines at bootstrap)

1. **Multi-bot secret store + `telegram addbot/listbots/removebot`** (R1) — store schema + CLI + `getMe` validation + redaction. SECURITY pre-review (secret handling).
2. **Daemon multi-bot long-poll** (R4) — N per-bot poll loops, per-bot thread/notification tagging, per-loop isolation, per-bot 409 gate. ARCHITECT pre-review (concurrency + routing correctness).
3. **`claudebase run` skip-permissions + ensure-daemon** (R3) — argv flag (verify vs `claude --help`) + ensure-daemon helper. RED-TEAM the default-on skip-permissions safety.
4. **`claudebase startproject` + `.claudebase` config** (R2) — scaffold + idempotency; reconcile with `claudebase-project-dir.md`.
5. **`daemon update` + `daemon setup` + `claudebase update`** (R5, R6) — binary download/swap + service auto-start; reuse installer download logic.
6. **`/start` inline menu (pairing/switch/help)** (R7) — InlineKeyboardMarkup + callback sub-flows reusing v0.8 callback validation + allowlist gate. SECURITY pre-review (group per-tap owner check).
7. **Docs + e2e + gates** — README "fleet setup", CHANGELOG, e2e for multi-bot routing + the /start menu round-trip; `/qa-cycle` (live multi-bot smoke) → `/merge-ready` → `/release` v0.9.0.

## Files likely affected

- `src/cli.rs` — new subcommands: `telegram addbot/listbots/removebot`, `startproject`, `daemon update/setup`, top-level `update`.
- `src/main.rs` — dispatch + `run_claude_with_preset` (skip-permissions + ensure-daemon).
- `src/daemon/config.rs` — multi-bot secret store (alongside / replacing single `secrets.toml` token).
- `src/daemon/telegram.rs` — N per-bot long-poll loops; `/start` inline menu + callback sub-flows (reuse `validate_callback`).
- `src/daemon/server.rs` — per-bot notification tagging if needed.
- `src/plugin/bridge.rs` — already has `ensure_daemon_running` (a328d43); reuse for `run`.
- new: project-config scaffolder (`.claudebase`); self-update module.
- `install.sh` / `install.ps1` — `daemon setup` may share the service-install + download logic.
- `docs/PRD.md`, `README.md`, `RELEASING.md`, `CHANGELOG.md`, `docs/use-cases/`, `docs/qa/`.

## Risks & dependencies

- **`--dangerously-skip-permissions` default-on** is a real safety trade-off — a prompt-injected channel message reaching a skip-permissions CLI is higher-blast-radius. Red-team the default; consider opt-in or a scoped allowlist. (Ties to the v0.8 channel prompt-injection warning already in the MCP instructions.)
- **Multi-bot token store is secret material** — must match `secrets.toml`'s 0600 + lstat + no-log discipline (SEC-9/15). One store, N tokens.
- **Per-bot poll-loop isolation** — one bad token must not take down the fleet; each loop owns its 409/401/429 handling (v0.8 conflict-gate per bot).
- **`claudebase update` atomicity** — swapping a running binary; do it atomically (download to temp, verify, rename) and restart the daemon after.
- **`/start` menu in groups** — the deferred per-tap owner check (F-4) is now load-bearing; menu actions must gate by the tapping user, not just chat membership.
- **Reconcile `claudebase-project-dir.md`** — `startproject` likely supersedes or implements it; check for overlap before bootstrapping.
- **Session-id auto-register (v0.8.1)** ideally lands first — so `run` → fleet → push is fully seamless without manual `agent_register`.

## Out of scope (this plan)

- HTTP/WSS cross-machine fleet (the `claudebase-server-foundation.md` plan) — single-machine daemon only.
- topic-as-id (forum-group topics = per-CLI) — separate deferred feature (insight doc#44).
- The v0.8.1 polish items (session-id auto-register, intermittent pair-reply bug) — tracked separately; this plan assumes they land in v0.8.1 first.

## Facts

### Verified facts
- Daemon subcommands `serve/config/access/doctor/warmup/install/uninstall/start/stop/restart/status/logs` exist; `update`/`setup` do NOT — source: `src/main.rs:126-143` (read this session). — salience: high
- `claudebase run` = `run_claude_with_preset` execs `claude --channels plugin:telegram@claude-plugins-official` + upserts the project registry; no skip-permissions, no ensure-daemon — source: `src/main.rs:163-191` (read this session). — salience: high
- Token storage is a SINGLE `[telegram] bot_token` in `secrets.toml` (0600, SEC-15) — source: `src/daemon/config.rs:1-12` (read this session). Multi-bot is net-new. — salience: high
- `/start` returns `None` (no reply) by design in v0.8 — source: `src/daemon/telegram.rs:488` (read this session). The inline menu is net-new. — salience: high
- The v0.8 plugin bridge already has `ensure_daemon_running` (commit a328d43) — reusable for R3b. — salience: medium
- `docs/plans/claudebase-project-dir.md` already exists (per-project `.claudebase/` design) — `startproject` should reconcile with it. — salience: medium

### External contracts
- **Claude Code CLI `--dangerously-skip-permissions`** — symbol: the permission-bypass launch flag for `claude` — source: NOT opened this session — verified: no — assumption. The implementer MUST confirm the exact flag against `claude --help` before R3a. — salience: high
- **Telegram Bot API `getMe`** — symbol: token-validation endpoint used by `addbot` to reject invalid tokens before storing — source: Telegram Bot API docs (used live this session via curl on `@my_dev_remote_bot`) — verified: yes. — salience: medium
- **Telegram `InlineKeyboardMarkup` / `callback_query`** — symbol: the v0.8 `chat_ask` button plumbing (`validate_callback`, `build_channel_notification`) reused for the `/start` menu — source: v0.8 Slice 5 (`src/daemon/telegram.rs`, shipped) — verified: yes. — salience: high

### Assumptions
- Multi-bot store as `~/.config/claudebase/bots.toml` (0600) is the natural extension of the existing `secrets.toml` pattern — risk: a DB table may be preferred for `listbots` metadata — how to verify: architect call at bootstrap. — salience: medium
- `startproject` supersedes `claudebase-project-dir.md` rather than coexisting — risk: duplicated `.claudebase` designs — how to verify: read `claudebase-project-dir.md` at bootstrap and merge. — salience: medium

### Open questions
- Default-on vs opt-in for `claudebase run --dangerously-skip-permissions` — needs: operator/red-team decision (safety vs convenience). — salience: high
- Where the multi-bot tokens live (bots.toml vs global DB) and how a bot is named/identified for `switch`/`removebot` — needs: architect call. — salience: high

## Decisions

### Inbound validation
- Operator requested a 7-part fleet feature for "the future" (a plan, not implementation). Challenged: is it coherent + proportional? Yes — it is the natural productisation of the v0.8 plumbing; each part maps to an existing seam (run preset, daemon subcommands, secrets store, /start handler). Outcome: authored as a scoping plan doc; flagged the default-on skip-permissions safety + the multi-bot-store location as the two load-bearing open questions for the bootstrap architect/red-team. — salience: high

### Decisions made
- Wrote this as a future-plan doc in `docs/plans/` (the home of the other claudebase feature plans) rather than `.claude/plan.md` (reserved for the ACTIVE bootstrap cycle). Q1 hack? no | Q2 sane? yes | Q3 alternatives? `.claude/plan.md` rejected (would collide with the live v0.8 plan). — salience: medium
- Ordered the slices so SECURITY/ARCHITECT/RED-TEAM pre-reviews land on the three risk-bearing slices (multi-bot secrets, multi-bot concurrency, default-on skip-permissions). — salience: medium

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
