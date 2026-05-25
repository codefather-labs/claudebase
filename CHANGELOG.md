# Changelog

All notable user-facing changes to `claudebase` are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

User-facing means changes a developer using claudebase notices in day-to-day work — new commands, new behavior of existing commands, new install steps, fixed broken flows. Internal refactors and test-only changes do NOT belong here.

## [Unreleased]

### Changed
- Reorganized repository layout: toplevel `agents/`, `commands/`, `rules/` moved into `prompts/{agents,commands,rules}/` to disambiguate from Rust source dirs (`bench/`, `plugins/`, `src/`, `tests/`). `install.sh` / `install.ps1` updated to read from new paths. End-user effect: none — installed files at `~/.claude/{agents,commands,rules}/` unchanged.
- Repo cleanup: removed `whisper-build-test/` (one-off spike, superseded by `plugins/telegram-rs/src/whisper.rs`), `spikes/ipc_concurrent_accept/` (closed engineering spike), `examples/` (empty), and `.DS_Store`.

### Removed
- `.claude-plugin/` directory (plugin.json + marketplace.json) — claudebase is no longer distributed as a Claude Code plugin via `claude plugin install claudebase@claudebase-dev`. Operators install the CLI tool via `install.sh` / `install.ps1`; the Telegram channel plugin lives inside the claudebase repo and is installed via `install_telegram_plugin` on top of the upstream Anthropic plugin manifest.
- `register_claude_plugin()` function in `install.sh` and `Register-ClaudePlugin` in `install.ps1` — no longer needed after marketplace removal.
- Toplevel `skills/access/SKILL.md` + `skills/configure/SKILL.md` — were the `/claudebase:*` skill duplicates; the canonical `/telegram:*` skills come from the upstream Anthropic Telegram plugin install.

### Added
- Repository `.github/` scaffolding: issue templates (bug report, feature request, plugin question), PR template, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md`.
- **`Stop` hook — insight-capture nudge.** New `hooks/claudebase-insight-capture.sh` / `.ps1`, deployed by `install.sh` / `install.ps1` into `~/.claude/hooks/` and wired into `~/.claude/settings.json` under `hooks.Stop`. Fires after every agent turn and prompts a reflection: did the agent learn something, catch a mistake, or have an assumption falsified? If yes and genuinely axis-worthy (self-learning / prediction-reality-mismatch / operator-correction), the agent persists exactly one insight via `claudebase insight create`; if not, it stops silently with no insight and no commentary. Loop-safe via the `stop_hook_active` payload flag (the forced reflection turn never re-triggers the hook). Operator-visible `🪝 claudebase insight-capture hook` bubble each fire. Installed as a claudebase hook (not SDLC) because the insights corpus, the `insight` subcommand, and `insights.db` are all claudebase features — the tool that owns insights owns the trigger that fills them. Cost note: blocking every turn adds one reflection turn per agent response (extra output + latency; input largely prompt-cache-warm).
- **`UserPromptSubmit` hook — cognitive-self-check reminder.** New `hooks/claudebase-selfcheck-reminder.sh` / `.ps1`, wired into `~/.claude/settings.json` under `hooks.UserPromptSubmit`. Fires before the agent responds to each prompt and injects a SHORT agent-only `additionalContext` reminder of the three cognitive-self-check protocols (Facts / Decisions / Inbound) so the agent doesn't silently drift over a long session. No `systemMessage` — per-prompt operator bubbles would be noise; the operator CLI stays clean. Cheap: no block, no extra turn, prompt-cache-friendly.
- **`cognitive-self-check.md` now ships from claudebase.** The three-protocol rule (Facts / Decisions / Inbound) moved from the SDLC repo's `src/rules/` into claudebase `prompts/rules/`, joining `knowledge-base.md` / `knowledge-base-tool.md` / `tool-limitations.md` as claudebase's cognitive-infrastructure layer. Rationale: the rule's `### External contracts` evidence discipline + salience tags are the foundation the books/insights corpora rest on, so the rule belongs with the tool that owns those corpora. End-user effect: none — the file still lands at `~/.claude/rules/cognitive-self-check.md` (now via the claudebase installer instead of the SDLC installer; the SDLC installer already chains claudebase, so downstream deployment is unchanged).

## [0.6.0] - 2026-05-24

### Added
- **`plugins/telegram-rs/` — Rust port of the official Anthropic Telegram channel plugin** at parity with the upstream TSX implementation. Features: MCP stdio server (hand-rolled), TG long-polling via `frankenstein`, gate / pairing / groups, all 8 inbound multimedia handlers (text / photo with download / document / voice / audio / video / video_note / sticker), voice transcription via `whisper-cli` subprocess, outbound tools (reply with chunking + files attachment, react, edit_message, download_attachment), permission-request flow with inline keyboard, `assert_allowed_chat` security gate, `/start` / `/help` / `/status` bot commands. Apache-2.0, source commit `3449c10cd1f254c2529a4a7e96a094ef118a00a5` of `anthropics/claude-plugins-official` preserved via `NOTICE`.
- **`claudebase run [--no-telegram] [-- args...]` subcommand** — exec wrapper launching `claude` with the Telegram plugin channel preset preloaded in one shot. Unix uses `CommandExt::exec` (zero-overhead, signal forwarding free); Windows uses spawn + wait + exit code forwarding.
- **`install_whisper_stack` in installers** — best-effort install of `ffmpeg` + `whisper-cli` via brew / apt / dnf / pacman (Unix) or winget / choco / scoop (Windows). Opt-out via `CLAUDEBASE_SKIP_WHISPER=1`.
- **`install_telegram_plugin` in installers** — installs the official Anthropic Telegram plugin via `claude plugin install`, then downloads our pre-built `server-rs` binary from this release's GH assets and patches the plugin's `.mcp.json` with a bash toggle (default Rust, fallback bun via `TELEGRAM_USE_TSX_SERVER=1`). Opt-out via `CLAUDEBASE_SKIP_TELEGRAM=1`.
- **`.github/workflows/release.yml` extension** — builds, smoke-tests, and uploads `telegram-plugin-rs` binaries for all 5 platforms (mac arm64 / x64, linux x64 / arm64, windows-x64) alongside `claudebase` to the GH release.

### Changed
- Cargo workspace: repo root is now a workspace; `plugins/telegram-rs` is a workspace member. `cargo build --release -p telegram-plugin-rs` works from the root.

## [0.5.0] - 2026-05-16

### Added
- **Insights corpus** (`<project>/.claude/knowledge/insights.db`) — write-side parallel to the books corpus that lets agents persist their own cognitive observations across sessions. Hippocampal-replay analogue for cross-session agent memory.
- **`claudebase insight {create,search,list,random,get,gc,delete}` subcommand tree** — full CRUD over insights with deduplication (exact-sha + semantic via cosine > 0.92), salience tags (high / medium / low) driving TTL retention (∞ / 365d / 90d), and metadata filters (type / agent / feature / since).
- **Hybrid search across both corpora** via `claudebase search --corpus all` — RRF-fuses hits from books and insights DBs.
- **`/reflect` and `/consolidate` slash commands** (Drift + Mnem agent personas) — DMN unfocused observation pass + hippocampal-replay drift detection.

## [0.4.0] - 2026-05-10

### Added
- **Hybrid retrieval backend** — BM25 (FTS5) + dense (384-dim e5-multilingual-small via sqlite-vec) + Reciprocal Rank Fusion (k=60) all in the same SQLite file. Default search mode is `hybrid`; `--mode lexical` and `--mode dense` for ablation.
- **Per-page PDF navigation** — every chunk tagged with its 1-indexed page number; `claudebase page <doc> <N>` fetches full extracted text of any cited page in O(1).
- **`claudebase compare <query>` subcommand** — runs the same query through all three search modes side-by-side so operator can pick what works best on their corpus.
- **Native Windows installer** — `install.ps1` for PowerShell + `install.bat` cmd.exe wrapper.

### Changed
- Tool renamed from `claudeknows` to `claudebase`; install path moved from `~/.claude/tools/sdlc-knowledge/` to `~/.claude/tools/claudebase/`. Existing installations are auto-migrated by `install.sh` on next run.

## [0.3.0] and earlier

Pre-extraction history lived in [claude-code-sdlc](https://github.com/codefather-labs/claude-code-sdlc) before `claudebase` was split into its own repo on 2026-05-10. See that repo's git log for archival changelog.
