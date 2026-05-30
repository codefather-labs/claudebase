# Changelog

All notable user-facing changes to `claudebase` are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

User-facing means changes a developer using claudebase notices in day-to-day work — new commands, new behavior of existing commands, new install steps, fixed broken flows. Internal refactors and test-only changes do NOT belong here.

## [Unreleased]

### Changed / BREAKING
- **`insight create` now requires `--category <general|project>` and at least one `--tag`.** Missing either causes exit 2. `--category` is the routing key — `general` writes to the new global db at `~/.claude/knowledge/insights.db` (cross-project lessons); `project` writes to the per-project local `insights.db` (this-project insights, unchanged location). Tags are free-form (e.g. `#nginx`, `#mistakes`, the feature slug), normalized (leading `#` stripped, lowercased, deduped), and stored one row per tag in a new `insight_tags` table. External callers not in this repository must update before upgrading to v0.7.0; the 14 SDLC agent prompts + the in-tree reminder hook are already updated.

### Added
- **Hybrid Insights Corpus** — project insights stay in the per-project local `insights.db`; general (cross-project) insights collect in ONE global db at `~/.claude/knowledge/insights.db`. A project registry at `~/.claude/knowledge/projects.json` (atomic-rename write, populated at `claudebase run` startup) maps project name to path so insight tooling can resolve a different project's db on demand. Schema v5 migration is additive on top of v4 and converges v2/v3/v4 → v5 with backfill (`category='project'`, default tag from `feature_slug`); books-corpus rows are untouched (`category IS NULL`).
- **`insight tags` subcommand** — lists the distinct tag vocabulary with counts. Default merges the per-project local db + the global db (counts summed per tag). `--category general|project` narrows to one db; `--project <slug>` resolves a different project via the registry; `--json` emits `[{tag,count}]` sorted by count desc.
- **Tag and category filters on every read subcommand** — `insight search`, `insight list`, `insight random` gain `--tag <tag>` (repeatable, **OR / any-intersection** — an insight is returned if its tag set intersects the requested tags by at least one), `--category <general|project>`, `--project <slug>`, `--general-only`, `--project-only`. `insight gc` and `insight delete` gain `--category`. The in-project default read merges the local + global dbs; other projects are walled off unless explicitly named.
- **SessionStart read-on-new-context hook** (`claudebase-read-insights-reminder.{sh,ps1}`) reminds an agent entering a fresh context window to discover the tag vocabulary (`insight tags`) and pull only relevant insights by tag (`insight search --tag <t>`) rather than re-reading every message. ASCII-only `.ps1` (the established Windows PS 5.1 constraint). Wired idempotently into `~/.claude/settings.json` by `install.sh` / `install.ps1`.
- **Project registry** (`~/.claude/knowledge/projects.json`) — `claudebase run` startup atomically upserts the current project (canonical path basename, last_seen) so future cross-project lookups have a name→path map. Atomic write-temp-rename with a per-call `AtomicU64`-derived temp name; the upsert is non-fatal (never blocks `exec`).

### Fixed
- **Pre-existing path-traversal gap in `--project` registry lookup.** `resolve_registry_project_db` joined the user-supplied `--db-name` into the registry-resolved project path without validation; the canonical `cli::validate_db_name` gate (which `open_and_validate` already uses) now runs first and rejects path-traversal / separators / hidden-file prefixes with exit 2.
- **Windows PowerShell hook broke on parse** (`claudebase-selfcheck-reminder.ps1`). The script contained non-ASCII characters (em-dashes, bullets); Windows PowerShell 5.1 parses no-BOM scripts in the local code page, not UTF-8, so the multibyte sequences corrupted string literals and aborted with `Unexpected token` / `string is missing the terminator`. The `.ps1` hook is now ASCII-only (em-dash/bullet -> `-`), which parses identically under any code page; the `.sh` variant keeps UTF-8 (Unix is UTF-8 throughout). A convention note at the top of the `.ps1` documents the ASCII-only requirement.

### Changed
- Reorganized repository layout: toplevel `agents/`, `commands/`, `rules/` moved into `prompts/{agents,commands,rules}/` to disambiguate from Rust source dirs (`bench/`, `plugins/`, `src/`, `tests/`). `install.sh` / `install.ps1` updated to read from new paths. End-user effect: none — installed files at `~/.claude/{agents,commands,rules}/` unchanged.
- Repo cleanup: removed `whisper-build-test/` (one-off spike, superseded by `plugins/telegram-rs/src/whisper.rs`), `spikes/ipc_concurrent_accept/` (closed engineering spike), `examples/` (empty), and `.DS_Store`.

### Removed
- `.claude-plugin/` directory (plugin.json + marketplace.json) — claudebase is no longer distributed as a Claude Code plugin via `claude plugin install claudebase@claudebase-dev`. Operators install the CLI tool via `install.sh` / `install.ps1`; the Telegram channel plugin lives inside the claudebase repo and is installed via `install_telegram_plugin` on top of the upstream Anthropic plugin manifest.
- `register_claude_plugin()` function in `install.sh` and `Register-ClaudePlugin` in `install.ps1` — no longer needed after marketplace removal.
- Toplevel `skills/access/SKILL.md` + `skills/configure/SKILL.md` — were the `/claudebase:*` skill duplicates; the canonical `/telegram:*` skills come from the upstream Anthropic Telegram plugin install.

### Added
- **`/update-claudebase` command.** New `prompts/commands/update-claudebase.md` skill that updates the locally-installed claudebase to the latest version by **reading the current repository README** (the authoritative, never-stale install/update procedure) and executing the path that matches the machine — `git pull` + `install.sh --local` for a checkout, or the README's remote one-liner otherwise — then verifying the version delta and reporting what changed. Reads-the-README-first by design so the skill never drifts from how the installer actually works; honors operator opt-out env vars; never `git rebase`, never `--force`, never publishes.
- Repository `.github/` scaffolding: issue templates (bug report, feature request, plugin question), PR template, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md`.
- **`UserPromptSubmit` hook — self-check + insight-capture reminder.** New `hooks/claudebase-selfcheck-reminder.sh` / `.ps1`, wired into `~/.claude/settings.json` under `hooks.UserPromptSubmit`. Fires before the agent responds to each prompt and injects a SHORT agent-only `additionalContext` reminder covering two things: (1) the three cognitive-self-check protocols (Facts / Decisions / Inbound) so the agent doesn't drift over a long session, and (2) insight-capture — if the PREVIOUS turn produced a genuine insight (self-learning / prediction-reality-mismatch / operator-correction) persist exactly one via `claudebase insight create`, else skip silently. No `systemMessage` (per-prompt operator bubbles would be noise), no block, no extra turn — prompt-cache-friendly. **Insight-capture deliberately lives here, NOT on a Stop hook:** a Stop hook can only force reflection via `decision: block`, which Claude Code renders to the operator as `Stop hook error: …` (looks like a failure) and forces an extra turn every response. Folding it into `UserPromptSubmit` reflects on the previous turn at the start of the next one — trade-off: the very last turn of a session is not reflected on (acceptable). Ships from claudebase because the insights corpus, the `insight` subcommand, and `insights.db` are all claudebase features.
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
