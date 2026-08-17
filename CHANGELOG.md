# Changelog

All notable changes to claudebase will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.1] - 2026-06-07

### Fixed

- **Telegram voice-note transcription now works.** Whisper speech-to-text was shipped but never functional on any platform — the `asr-whisper` backend is an opt-in build feature and none of the release/installer build commands enabled it, so every binary contained a stub that reported the feature as not compiled in. Release builds and `install.sh --local` / `install.ps1 -Local` now build with `--features asr-whisper`, and a daemon with no `[asr]` config block defaults to the whisper backend. Send the bot a voice note → it transcribes (model auto-downloads on first use, or pre-fetch with `claudebase daemon warmup --asr`).

## [0.8.0] - 2026-06-07

### Added

- **Agent-to-agent (CLI-to-CLI) communication.** Multiple Claude Code CLIs now discover each other by a stable cross-clone `project_id` (resolved from the git remote-origin URL, a `.claudebase/config.json` override, or a path hash) and message one another directly through new MCP tools — `agent_send` (deliver a message to another live agent), `agent_describe` (publish/read what a peer is working on), and `agent_set_dnd` (do-not-disturb). `claudebase agent list-alive --project current` lists peers on the same repo. Two Claude Code hooks form the read/write boundary: `agent-routing-reminder` (PreToolUse:EnterPlanMode) surfaces who else is working on what before you plan, and `feature-describe` (PostToolUse:ExitPlanMode) publishes your decided plan via `agent_describe` and mirrors it to the scratchpad.

## [Unreleased]

### Changed

- **BREAKING — Telegram and peer messaging no longer use a Claude Code plugin.** The transport was a
  Rust binary swapped into the official Anthropic Telegram plugin's cache plus a rewritten
  `.mcp.json`, driven by `claude --channels plugin:telegram@claude-plugins-official`. Any plugin
  update silently reverted it (and had — the installed 0.0.7 cache carried upstream's `.mcp.json`
  with no trace of the patch), and Claude Code gates channel plugins behind a server-side allowlist
  no third party can join. `claudebase run` is now a **PTY supervisor**: it runs `claude` unmodified
  and writes inbound messages into its input the same way a keystroke arrives.
- **BREAKING — inbound message format.** Messages appear in the session's input as
  `[telegram_message]: <text>` and `[agent-to-agent:<nick>]: <text>` instead of `<channel …>` turns.
  The prefix marks the line as a message rather than operator input, and names the sender for peer
  traffic.
- **BREAKING — outbound is CLI, not MCP.** `claudebase telegram send --text "…"` replaces the
  `chat_reply` tool; `claudebase agent send "…" --agent_nick <nick>` replaces `agent_send`;
  `claudebase agent list` replaces `agent list-alive`; `claudebase agent describe` replaces the
  `agent_describe` tool. Sender identity is still enforced by the daemon — a session token minted at
  registration, never a caller-supplied `from`.
- **Bot tokens live in the daemon's database.** `claudebase telegram addbot <token>` verifies the
  token via `getMe`, then stores it in `telegram_bots` inside `chat.db` (0600, single owner).
  Re-adding the same bot rotates its token rather than duplicating the row. `~/.claude/channels/
  claudebase/.env` and `~/.config/claudebase/secrets.toml` remain fallbacks, closing the
  two-sources-of-truth split behind `docs/issues/004`.
- **Channel access is a command, not a skill.** `claudebase telegram pair|access|policy|allow|revoke`
  replace the `/claudebase-access` and `/claudebase-configure` skills, which were removed. Access
  decisions no longer pass through a model whose context contains messages from the very channel
  being gated.

### Added

- `claudebase telegram get_me` — raw Telegram `getMe` response for the registered bot, so a revoked
  token surfaces immediately instead of as silence in the channel.
- `claudebase agent list [--all] [--json]` — nick, agent id, online/offline and what each session is
  working on.
- `claudebase-channel-contract` SessionStart hook — teaches every session the message format and the
  reply commands before the first message arrives.
- Injection safety, measured rather than assumed (`docs/qa/evidence/pty-inject/`): inbound text is
  held while the operator has a half-typed line (it silently concatenated otherwise) and while a
  Claude Code modal is up (the message vanished and the submit key answered the dialog). Nothing is
  dropped — messages queue and land when the input is clear.
- **Nicks are remembered.** `claudebase agent rename "<nick>"` now records the choice against the
  working directory, so the next session started there comes back under the same name. This is what
  keeps a Telegram `/switch` binding alive across a restart: bindings are keyed by nick precisely so
  they outlive a process. A rename also carries that session's bindings with it, instead of
  stranding the chat on a name nobody answers to.
- `claudebase agent whoami` — this session's nick, id, and whether the nick was chosen or merely
  derived from the directory name. Read-only and daemon-free, so it is safe to call from a hook.
- The `claudebase-channel-contract` SessionStart hook asks a session to name itself, but only while
  the nick is still the directory default — once a name is chosen the prompt disappears. Renaming on
  every start would break the binding it is keyed by.

### Removed

- `claudebase plugin serve` and the MCP stdio bridge (`src/plugin/`), the `plugins/telegram-rs`
  crate, and the `telegram-plugin-rs` release artifact for all five platforms. The installers no
  longer install, patch or even mention Claude Code plugins; instead they **undo** the old patch,
  restoring the official plugin's `.mcp.json` from the backup taken at patch time and deleting the
  binary we had dropped into its cache. `daemon install` likewise removes the stale
  `~/.claude/plugins/claudebase/.mcp.json` rather than writing it.
- The `/claudebase-configure` and `/claudebase-access` skills. Access control moved to commands so
  that granting access is not a decision taken by a model whose context contains messages from the
  channel being gated.

### Fixed

- A daemon restart no longer ends message delivery: the supervisor owns the connection and
  re-registers on reconnect (root causes 1 and 3 of `docs/issues/006`, now closed).
- Each session now gets a unique `agent_id`, so concurrent sessions in one project are
  distinguishable and broadcasts stop landing on stale connections.
- An agent no longer receives its own outbound message back as inbound. The daemon broadcasts a
  reply to the thread it was sent on, which fed the sender's own words into its input — a
  self-sustaining loop, found on the first live round trip.
- Running the test suite no longer steals the operator's Telegram polling slot. Three daemon tests
  isolated the socket but not `$HOME`, so they opened the real `chat.db`, found the registered bot
  and started polling (`Conflict: terminated by other getUpdates request`).
- `tests/registry_test.rs` no longer flakes under parallel execution — it mutated process-global
  `$HOME` without serialising.
- `claudebase-selfcheck-reminder.ps1` is ASCII-only, matching the constraint the other PowerShell
  hooks follow because of the BOM/JSON failure in `docs/issues/003`.
- The pairing tests were exercising `claudebase daemon access …`, a subcommand that never existed;
  they had been red since they were written, and the rest asserted only on JSON they had written
  themselves. Rewritten against the real CLI.
- **Telegram messages were labelled as peer traffic.** A message from the operator arrived as
  `[agent-to-agent:<truncated-username>]`, and the reply it suggested was undeliverable. The daemon
  describes a thread three different ways — a Telegram notification carries a bare numeric
  `meta.chat_id`, peer notifications carry `agent:<id>`, and posts carry a prefixed `meta.thread` —
  and the subscriber only understood the last one, so real Telegram messages never matched and fell
  through to the peer branch. Classification now reads what the daemon actually emits, and its tests
  build frames with the real notification builders rather than fixtures.
- **A sender's nick was sometimes replaced by its agent id.** `chat.db` was the one database in the
  project with neither `busy_timeout` nor WAL, and `open_chat_db` ran a schema-ensuring write
  transaction on every open — so looking up a name competed with the daemon storing the very message
  being labelled, failed with `SQLITE_BUSY`, and the failure was swallowed into an id. Reads now use
  a read-only handle that opens no write transaction, the database waits for contended locks instead
  of failing, and an unresolved sender degrades to its **full** id, which `--agent_nick` accepts,
  rather than an 8-character stub that nothing could resolve.
- `install.sh` never installed the `claudebase-channel-contract` hook it advertised in its own help
  text, so a fresh install on Linux/macOS left sessions receiving `[telegram_message]:` lines they
  had not been told about. The PowerShell installer already wired it.

### Attribution

- `NOTICE` moved to the repository root. It shipped inside `plugins/telegram-rs/`, but the code
  derived from Anthropic's Apache-2.0 Telegram plugin (the pairing flow and access gate) now lives
  in `src/daemon/`, so the attribution follows it and says where it applies.


### Added

- Cross-session insights now work again — the insights corpus tracks categories and tags, survives migration from any prior schema version, and two new Claude Code hooks remind agents to query the corpus on every new context and every prompt.
- **`UserPromptSubmit` hook — cognitive-self-check reminder.** New `hooks/claudebase-selfcheck-reminder.sh` / `.ps1`, wired into `~/.claude/settings.json` under `hooks.UserPromptSubmit`. Fires before the agent responds to each prompt and injects a SHORT agent-only `additionalContext` reminder of the three cognitive-self-check protocols (Facts / Decisions / Inbound) so the agent doesn't silently drift over a long session. No `systemMessage` — per-prompt operator bubbles would be noise; the operator CLI stays clean.
- **`cognitive-self-check.md` ships from claudebase.** The three-protocol rule (Facts / Decisions / Inbound) lives in claudebase `prompts/rules/`, joining `knowledge-base.md` / `knowledge-base-tool.md` / `tool-limitations.md` as claudebase's cognitive-infrastructure layer. End-user effect: file still lands at `~/.claude/rules/cognitive-self-check.md` via the claudebase installer.
- **`/update-claudebase` slash-command skill.** New `prompts/commands/update-claudebase.md` skill that updates the locally-installed claudebase to the latest version by **reading the current repository README** (the authoritative, never-stale install/update procedure) and executing the path that matches the machine — `git pull` + `install.sh --local` for a checkout, or the README's remote one-liner otherwise — then verifying the version delta and reporting what changed. Reads-the-README-first by design so the skill never drifts from how the installer actually works; honors operator opt-out env vars; never `git rebase`, never `--force`, never publishes.

### Changed

### Fixed

### Deprecated

### Known Limitations

- `/update-claudebase` skill ships in v0.9 but its end-to-end upgrade path will be empirically verified only in v0.10 → v0.11; v0.7+v0.8 are deprecated paths and v0.6 has no skill to upgrade from (operator directive 2026-06-04).
- KP2/KP3 Telegram forum-topic routing is architecturally complete but live-evidence is pending v0.10 (deferred scope).

## [0.6.0] - 2026-05-24

### Added
- **`plugins/telegram-rs/` — Rust port of the official Anthropic Telegram channel plugin** at parity with the upstream TSX implementation.
- **`claudebase run [--no-telegram] [-- args...]` subcommand** — exec wrapper launching `claude` with the Telegram plugin channel preset preloaded.
- **`install_whisper_stack` + `install_telegram_plugin` in installers** — opt-out via `CLAUDEBASE_SKIP_WHISPER=1` / `CLAUDEBASE_SKIP_TELEGRAM=1`.
- **`.github/workflows/release.yml` extension** — builds `telegram-plugin-rs` binaries for all 5 platforms alongside `claudebase`.

### Changed
- Cargo workspace: repo root is now a workspace; `plugins/telegram-rs` is a workspace member.

## [0.5.0] - 2026-05-16

### Added
- **Insights corpus** + `claudebase insight {create,search,list,random,get,gc,delete}` subcommand tree.
- **Hybrid search across both corpora** via `claudebase search --corpus all`.
- **`/reflect` and `/consolidate` slash commands**.

## [0.4.0] - 2026-05-10

### Added
- **Hybrid retrieval backend** (BM25 + dense + RRF).
- **Per-page PDF navigation**.
- **`claudebase compare <query>` subcommand**.
- **Native Windows installer**.

### Changed
- Tool renamed from `claudeknows` to `claudebase`; install path moved to `~/.claude/tools/claudebase/`.

## [0.3.0] and earlier

Pre-extraction history lived in [claude-code-sdlc](https://github.com/codefather-labs/claude-code-sdlc) before `claudebase` was split into its own repo on 2026-05-10.
