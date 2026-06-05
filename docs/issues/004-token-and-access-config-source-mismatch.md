# Issue 004 — Telegram token + access config: three locations, confusing precedence

**Status:** OPEN — deferred (fix later, per operator 2026-06-06)
**Severity:** High (caused a multi-hour "push doesn't work" debugging spiral)
**Area:** daemon config loading + `configure` / `access` skills + installer

## Summary

The bot token and the access allowlist can each live in **multiple
locations**, and the daemon's read precedence does not match where the
skills (especially the *official* telegram-plugin skills) write. The result
is that "configure the token" can silently update a file the daemon never
reads, leaving it polling a stale/old bot.

This bit us live on 2026-06-06: a restored `secrets.toml` held an **old
bot** (`5298677359…`), `channels/claudebase/.env` was absent, so the daemon
fell back to the old token and polled the wrong bot — inbound Telegram
messages never reached the session. Re-running `/telegram:configure` with
the correct token did NOT fix it, because that skill writes a *third*
location the daemon ignores.

## The three token locations (precedence as implemented)

| # | Path | Who writes it | Daemon reads it? |
|---|------|---------------|------------------|
| 1 | `~/.claude/channels/claudebase/.env` (`TELEGRAM_BOT_TOKEN=`) | our `/claudebase:configure` skill | **YES — canonical, takes precedence** |
| 2 | `~/.config/claudebase/secrets.toml` (`[telegram] bot_token`) | manual / restore | YES — **legacy fallback only** |
| 3 | `~/.claude/channels/**telegram**/.env` | the **official** `/telegram:configure` skill | **NO — ignored** |

Daemon precedence: `server.rs` ≈ `let token = env_token.or(secrets_token_opt);`
— i.e. `channels/claudebase/.env` (1) wins; `secrets.toml` (2) is fallback;
`channels/telegram/.env` (3) is never consulted.

## The two access.json locations

| Path | Who writes it | Daemon reads it? |
|------|---------------|------------------|
| `~/.claude/channels/claudebase/access.json` | our `/claudebase:access` skill | **YES** (`channel_state::access_json_path` = `channel_state_dir/access.json`) |
| `~/.claude/channels/**telegram**/access.json` | the **official** `/telegram:access` skill | **NO — ignored** |

Our `access` skill is already correct (writes `channels/claudebase/`). The
*official* `/telegram:access` writes the wrong dir. Note access.json is
`$HOME`-based, so it has no Mac-vs-Windows path divergence (unlike the token).

## Why the operator hit this

The installer patches the **official** telegram plugin's `.mcp.json` to run
our daemon bridge, but does NOT replace the official plugin's `configure` /
`access` skills. So `/telegram:configure <token>` runs the *official* skill,
which writes location #3 — a dead file. The operator naturally assumed
"configure set the token", but the daemon kept its old token.

## Desired fix (operator preference — DEFERRED)

Operator wants **one** obvious token location: `secrets.toml`
(`~/.config/claudebase/secrets.toml` on Mac/Linux, `%APPDATA%\claudebase\secrets.toml`
on Windows — matches `config::user_level_config_dir()`).

To make that real and non-confusing, the deferred fix is all of:

1. **Daemon precedence flip** — make `secrets.toml` the canonical source
   (read it first), and either drop the `.env` reads or keep them as a
   clearly-deprecated fallback. Otherwise a stale `.env` keeps overriding a
   freshly-written `secrets.toml`. (`src/daemon/server.rs` token-resolve block.)
2. **`configure` skill rewrite** — write the token to `secrets.toml`
   platform-aware (TOML `[telegram] bot_token = "…"`, chmod 600), and READ
   from there for the status display. Mac/Linux: `~/.config/claudebase/secrets.toml`;
   Windows: `%APPDATA%\claudebase\secrets.toml`. (`prompts/skills/configure/SKILL.md`.)
3. **Installer skill override** — have `install.sh` / `install.ps1` deploy our
   `configure` / `access` skills OVER the official telegram plugin's skills
   (the way `.mcp.json` is already patched), so `/telegram:configure` runs the
   corrected logic instead of the official dead-file version. OR document that
   only `/claudebase:configure` is canonical and the official one is a no-op.
4. **`access` skill** — confirmed already writes the daemon path
   (`channels/claudebase/access.json`); only needs the same installer-override
   so the official `/telegram:access` cannot write the ignored dir.

## Workaround until fixed

Set the token directly where the daemon reads it now:
`~/.claude/channels/claudebase/.env` → `TELEGRAM_BOT_TOKEN=<token>` (canonical),
OR `~/.config/claudebase/secrets.toml` → `[telegram] bot_token = "<token>"`
(fallback), then `claudebase daemon restart`. Verify with the daemon log line
`telegram long-poll starting` + a `telegram batch persisted` after sending a
test DM.

## Facts

### Verified facts
- Daemon token precedence is `env_token.or(secrets_token_opt)` — `.env`
  (channels/claudebase) wins, `secrets.toml` is fallback — source:
  `src/daemon/server.rs:223-254` (read this session). — salience: high
- `channel_state::access_json_path()` = `channel_state_dir()/access.json`
  and `channel_state_dir()` = `$HOME/.claude/channels/claudebase` — source:
  `src/daemon/channel_state.rs:151-162`. — salience: high
- Our `/claudebase:access` skill writes `~/.claude/channels/claudebase/access.json`
  (the daemon path) — source: `prompts/skills/access/SKILL.md:22,31,60,72`. — salience: medium
- Our `/claudebase:configure` skill writes `~/.claude/channels/claudebase/.env`
  (the canonical token path) — source: `prompts/skills/configure/SKILL.md:14,27,78,80`. — salience: medium
- `user_level_config_dir()` = `$XDG_CONFIG_HOME|$HOME/.config/claudebase` on
  Unix, `%APPDATA%|%USERPROFILE%\claudebase` on Windows — source:
  `src/daemon/config.rs:203-221`. — salience: high
- Live repro 2026-06-06: restored `secrets.toml` had old bot `5298677359…`,
  `channels/claudebase/.env` absent → daemon polled the wrong bot; writing the
  correct token + `daemon restart` fixed it (`telegram batch persisted`). — salience: high

### External contracts
- Telegram Bot API `getUpdates` — single-consumer-per-token; a wrong/old token
  polls a different bot entirely (not an auth error if that old bot still
  exists) — source: Telegram Bot API (not opened this session) — verified: no — assumption. — salience: medium

### Assumptions
- The official `/telegram:configure` skill writes `~/.claude/channels/telegram/.env`
  — risk: exact path may differ by plugin version — how to verify: read the
  installed `claude-plugins-official/telegram/<ver>/skills/configure/SKILL.md`. — salience: medium

### Open questions
- Should the single canonical token source be `secrets.toml` (operator's
  stated preference) or `channels/claudebase/.env` (current daemon canonical)?
  — needs: operator/architect decision at fix time. — salience: high

## Decisions

### Inbound validation
- Operator asked to "update the configure skill to write secrets.toml". Protocol-1
  check found the daemon's canonical source is actually `.env` (secrets.toml is
  fallback), and that a prior statement of mine ("daemon reads secrets.toml") was
  incomplete. Surfaced the full 3-location picture rather than silently editing the
  skill to write the fallback. — challenged: yes — outcome: documented as a deferred
  issue per operator ("fix later"). — salience: high

### Decisions made
- Defer the fix and capture it as issue 004 rather than rushing a partial skill
  edit. Q1 hack? no. Q2 sane? yes — a multi-source precedence change spanning daemon
  + 2 skills + installer warrants a planned fix, not an ad-hoc edit mid-session.
  Q3 alternatives? edit just the skill now (rejected — a stale `.env` would override
  it, so it would appear fixed but not be). Q4 cause? yes — names the root (multi-source
  precedence mismatch). Q5 tracked? this doc. — salience: high

### Hacks / workarounds acknowledged
- (none)

### Symptom-only patches (with root-cause links)
- (none)
