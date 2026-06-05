# Changelog

All notable changes to claudebase will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
