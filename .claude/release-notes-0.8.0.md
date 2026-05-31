### Added

- **Telegram Multi-CLI — one bot, many CLIs.** The daemon owns the Telegram bot and routes each chat to a single bound CLI instance (chat-as-id: one chat ID = one CLI). Operators running multiple Claude Code sessions share a single bot token without 409 conflicts.
- **Chat-as-id binding.** Each Telegram chat (DM or group) is bound to one CLI instance at a time. All users in a group share that chat's binding; `/switch` rebinds the whole chat for everyone.
- **Bot commands: `/agents`, `/switch`, `/whoami`, `/here`.** List online CLIs, rebind the active chat, show the current binding, and show the bound CLI's host/cwd (best-effort).
- **`chat_ask` — multiple-choice questions as Telegram inline keyboard buttons.** Agents present a question as tappable buttons; the operator taps and the answer routes back to the calling agent. (DM chats, single-select in v1.)
- **Conflict gate — 409 detection without crashing.** If a legacy poller still holds the bot token's slot, the daemon logs a clear warning and backs off 60 s per cycle instead of crashing.
- **Real-time Telegram → Claude Code channel push.** A Telegram message routed to your CLI is injected into the live session automatically (no polling) as a `<channel source="plugin:telegram:telegram" ...>` event; reply with the `chat_reply` tool. Launch with `claudebase run` (= `claude --channels plugin:telegram@claude-plugins-official`); the installer wires the claudebase daemon bridge behind the approved Telegram channel slot, which is the only path that receives Claude Code's channel injection.

### Changed

- **The installer wires the Telegram channel to the claudebase daemon bridge.** `install.sh` / `install.ps1` patch the official Telegram plugin's `.mcp.json` so its channel MCP server is the claudebase daemon bridge (`claudebase plugin serve`) instead of a per-CLI direct poller. The bridge only relays the single daemon — no dual-poll; the daemon stays the sole `getUpdates` consumer. The upstream `.mcp.json` is backed up to `.mcp.json.upstream-backup`.
- **`install.sh` + `install.ps1` resolve the install-target version dynamically** from origin's latest `claudebase-v*` git tag (semver-sorted `git ls-remote`, no GitHub API, no `jq`). The baked `CLAUDEBASE_FALLBACK_VERSION` is used only when the remote lookup fails. Pin via `CLAUDEBASE_VERSION=<x.y.z>`.
- **`darwin-x64` (Intel Mac) binary dropped from the release matrix** — `ort 2.0.0-rc.12` no longer ships prebuilt x86_64-apple-darwin binaries. Intel-Mac users build from source (`cargo install --path .`); the installer emits clear instructions on detection.

### Fixed

- **Telegram access-control split-brain fixed.** The daemon's inbound gating and the `claudebase daemon access pair` CLI now share one canonical access file (`~/.claude/channels/claudebase/access.json`); a one-shot boot-time migration carries any legacy `~/.config/claudebase/access.json` grants forward (union-only). Previously approvals never reached the gate and Telegram onboarding deadlocked.
- **Channel `chat_id` serialized as a string** so Claude Code's channel-surface parser accepts it — a numeric `chat_id` was silently dropped, which is why routed inbound messages never reached the live session before this fix.
- **`windows-x64` release binary cap raised 35 MB → 40 MB** (the v0.7.0 insights corpus pushed the binary past the old cap, failing the release build).
- **`linux-arm64` build runner bumped `ubuntu-22.04-arm` → `ubuntu-24.04-arm`** for `glibc >= 2.38` (the prebuilt `ort-sys` static lib needs `__isoc23_strtoull`).

## Facts

### Verified facts
- Real-time channel push live-verified 2026-05-31: a Telegram message surfaced automatically (no polling) as `<channel source="plugin:telegram:telegram" chat_id="434566766" target_agent_id="mira-live">` — source: this session's live test against `@my_dev_remote_bot`. — salience: high
- Channel injection requires a `--channels`-registered plugin; the installer wires the official Telegram plugin's `.mcp.json` to run `claudebase plugin serve` (the bridge relays the single daemon, no dual-poll) — source: `install.sh` `install_telegram_channel_bridge`, `src/main.rs:191-192` (`claudebase run` launches `--channels plugin:telegram@claude-plugins-official`). — salience: high
- Version 0.7.0 → 0.8.0 (MINOR; `Added` non-empty). Tag scheme `claudebase-v*` per `.github/workflows/release.yml:16`. — salience: medium

### External contracts
- **Claude Code `--channels` + `notifications/claude/channel`** — symbol: a `--channels plugin:<id>` registered plugin's `notifications/claude/channel` is injected as `<channel ...>`; a plain `mcpServers` entry is NOT — source: live-verified this session — verified: yes. — salience: high
- **`softprops/action-gh-release@v2`** — symbol: `body_path: .claude/release-notes-<version>.md` — source: `.github/workflows/release.yml:354-365` — verified: yes. — salience: medium

### Assumptions
- The GHA release matrix (linux-x64, linux-arm64, darwin-arm64, windows-x64) builds clean for 0.8.0 — risk: a build failure would block the published binaries — how to verify: watch the `claudebase-v0.8.0` GHA run after the tag push. — salience: high

### Open questions
- (none) — session-id auto-register + the intermittent pair-reply bug are tracked for v0.8.1 (see `.claude/CHECKPOINT.md`).
