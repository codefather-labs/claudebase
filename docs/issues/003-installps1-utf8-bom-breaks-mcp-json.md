# Issue 003: install.ps1 writes UTF-8 with BOM on Windows PowerShell 5.1, breaking .mcp.json + settings.json

## Status

**Open** — diagnosed 2026-06-03, live `.mcp.json` BOM-stripped manually on the affected dev box; install.ps1 source not yet fixed.

## Severity

**Major** — silent plugin-load failure. The Telegram channel plugin appears installed (binary present, `.mcp.json` present, `/telegram:configure` skill works) but Claude Code never spawns `server-rs.exe` on session start, so the bot doesn't react and no error surfaces. Confused operators waste hours on misdiagnosis (which is exactly what happened this session).

## Reproduction

Windows 11 Home, Windows PowerShell 5.1 (`powershell.exe`, version 5.1.x — the default on Win 10/11 unless PS 7+ is explicitly installed):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -Local -Yes
```

The script's `Install-TelegramPlugin` function eventually calls (at `install.ps1:513`):

```powershell
$cfg | ConvertTo-Json -Depth 6 | Set-Content -Path $mcpJson -Encoding UTF8
```

On PS 5.1, `Set-Content -Encoding UTF8` writes **UTF-8 with BOM** (the `EF BB BF` byte sequence) at the start of the file. Verification this session:

```
$ head -c 6 ~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/.mcp.json | xxd
00000000: efbb bf7b 0d0a                           ...{..
```

Claude Code's MCP `.mcp.json` JSON parser rejects (or silently skips) files beginning with the BOM, so the `mcpServers.telegram` entry never registers. The session boots without the plugin and `server-rs.exe` is never spawned as a child of `claude.exe`.

On PS 7+ (`pwsh.exe`), `Set-Content -Encoding UTF8` writes UTF-8 **without** BOM, so the same install.ps1 works correctly. This is why the bug doesn't reproduce for operators on PS 7 dev boxes or on macOS / Linux.

## Affected call sites in install.ps1

| Line | File written | Impact |
|---|---|---|
| 214 | `$env:USERPROFILE\.claude\settings.json` | BOM may affect Claude Code's settings load; impact not yet tested |
| 226 | `$env:USERPROFILE\.claude\settings.json` (idempotent rewrite) | same as 214 |
| 513 | `<plugin-dir>\.mcp.json` | **Load-bearing — silently breaks Telegram channel plugin** |

Two other `Set-Content -Encoding ASCII` call sites (lines 190 + 296) are unaffected because ASCII has no BOM by definition.

## Diagnostic symptoms (for future operators hitting this)

All of these together on Windows 11 + PS 5.1, after a clean install:

1. `/telegram:configure <token>` skill runs and confirms token saved
2. `~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/server-rs.exe` exists and is the correct v0.6 binary
3. `~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/.mcp.json` exists and looks correct when viewed with most editors (BOM is invisible)
4. Operator DMs the bot → bot does not respond
5. `tasklist | grep server-rs` returns nothing — plugin process is NOT a child of `claude.exe`
6. `curl https://api.telegram.org/bot<TOKEN>/getUpdates` shows queued Updates (nobody is consuming them)
7. `head -c 6 .mcp.json | xxd` shows `efbb bf7b ...` (BOM present)

Step 7 is the load-bearing diagnostic. Steps 1-3 falsely reassure the operator that everything is configured.

## Fix

Replace each load-bearing `Set-Content -Encoding UTF8` call with an explicit `[System.IO.File]::WriteAllText` that writes UTF-8 **without** BOM:

```powershell
# Before (writes BOM on PS 5.1):
$cfg | ConvertTo-Json -Depth 6 | Set-Content -Path $mcpJson -Encoding UTF8

# After (works identically on PS 5.1 and PS 7+):
$json = $cfg | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText($mcpJson, $json, [System.Text.UTF8Encoding]::new($false))
```

The `[System.Text.UTF8Encoding]::new($false)` constructor explicitly disables the BOM. `[System.IO.File]::WriteAllText` is a .NET BCL call available on both PS 5.1 (.NET Framework) and PS 7+ (.NET 6+).

Apply the same pattern at lines 214, 226, and 513.

## Workaround (for operators who already hit it)

Strip the BOM from the affected file directly:

```bash
# Bash / Git Bash
FILE=~/.claude/plugins/cache/claude-plugins-official/telegram/0.0.6/.mcp.json
tail -c +4 "$FILE" > "$FILE.tmp" && mv "$FILE.tmp" "$FILE"

# PowerShell 5.1 alternative
$path = "$env:USERPROFILE\.claude\plugins\cache\claude-plugins-official\telegram\0.0.6\.mcp.json"
$content = (Get-Content -Raw $path) -replace "\xEF\xBB\xBF", ""
[System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))
```

After stripping, restart Claude Code via `claudebase run`. The plugin should then load and the bot should react.

## Why this wasn't caught earlier

- The original v0.6 installer was authored on macOS / Linux where PS 5.1 doesn't exist
- v0.7 explicitly fixed a related class of bug (em-dashes in `.ps1` files corrupting the PS 5.1 parser — see CHANGELOG v0.7.0 line 47) but did NOT fix the BOM-write issue, presumably because `.mcp.json` rejection is silent (no parser error, just an absent plugin)
- On PS 7+ dev boxes (most Windows-using devs) the same code path writes a clean file, so the bug never surfaces in dev testing

This is the same class of bug as v0.7's em-dash fix — a Windows PS 5.1 vs PS 7+ encoding divergence that is invisible until it strikes.

## Tracked under

- Feature: `multi-agent-telegram-on-v0.6` (uncovered while operator was setting up Slice 0 baseline pairing on Windows)
- Related: `docs/issues/002-channel-surface-not-firing-2.1.144.md` (separate Claude Code 2.1.144 channel issue, not BOM-related — keep both open)
- Related v0.7 fix: CHANGELOG v0.7.0 line 47 (em-dash parser corruption, same PS 5.1 vs PS 7+ class)

## Facts

### Verified facts
- `install.ps1:513` writes `.mcp.json` via `Set-Content -Path $mcpJson -Encoding UTF8` — source: `install.ps1:513` this session — salience: high
- On the affected dev box, the live `.mcp.json` was confirmed to start with bytes `EF BB BF` (UTF-8 BOM) — source: `head -c 6 .mcp.json | xxd` this session showed `efbb bf7b 0d0a` — salience: high
- After stripping the BOM via `tail -c +4`, the file starts with the literal `7B` (`{`) — source: same xxd post-strip — salience: high
- `Set-Content -Encoding ASCII` calls at install.ps1:190 and install.ps1:296 are unaffected (ASCII has no BOM) — source: file grep this session — salience: low

### External contracts
- **PowerShell 5.1 `Set-Content -Encoding UTF8` behavior** — symbol: writes UTF-8 WITH BOM — source: not opened this session — verified: no — recall (Microsoft docs known behavior; empirically confirmed by xxd output above). Salience: high.
- **PowerShell 7+ `Set-Content -Encoding UTF8` behavior** — symbol: writes UTF-8 WITHOUT BOM — source: not opened this session — verified: no — recall. PS 7 default encoding changed in PS Core to UTF-8 no-BOM. Salience: medium.
- **`[System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))`** — symbol: .NET BCL primitive available on .NET Framework 4.x (PS 5.1) and .NET 6+ (PS 7+) — source: not opened this session — verified: no — recall. Salience: high.
- **Claude Code MCP `.mcp.json` parser BOM tolerance** — symbol: behavior on UTF-8 BOM-prefixed `.mcp.json` — source: not opened this session — verified: no — assumption. Empirical evidence this session: plugin loads after BOM strip; plugin doesn't load with BOM present (no `server-rs.exe` child of `claude.exe`). Salience: high (load-bearing for the diagnosis).

### Assumptions
- The `settings.json` writes at lines 214 + 226 may or may not cause a parser issue for Claude Code's settings load. Not tested this session. The settings file may have been BOM'd already from an earlier run, with no obvious symptom — but a subtle behavior change is possible. Verification: `head -c 4 ~/.claude/settings.json | xxd` on the affected box, then test if removing the BOM changes any observed Claude Code behavior.

### Open questions
- Should Claude Code's JSON parser tolerate BOM? Submitting a bug upstream might be warranted (BOM is technically valid in UTF-8 JSON per RFC 8259 §8.1 which says parsers MAY ignore it, MAY interpret it).

## Decisions

### Inbound validation
- Operator request: record this as a separate issue file in `docs/issues/`. Q1 nonsensical? no | Q2 upstream error? no — diagnosis is solid this session | Q3 justification? operator wants follow-up tracked rather than buried in commits | Q4 amplify upstream error? no — surfacing increases visibility. Outcome: proceeded. Salience: high.

### Decisions made
- File numbered `003` per the next-integer convention from `001-` and `002-`. — salience: low.
- Issue format mirrors `002-channel-surface-not-firing-2.1.144.md` (heading + Context + tables + Facts/Decisions). — salience: low.
- Fix in install.ps1 NOT applied in this commit — operator wanted the issue **recorded** as separate from the plan. Apply fix in a later pass. — salience: medium.

### Hacks acknowledged
- (none — this is a diagnostic doc; the live-file BOM strip applied separately is a workaround, not a hack.)

### Symptom-only patches
- The live `.mcp.json` BOM-strip applied earlier this session IS symptom-only (treats the affected file, leaves install.ps1 source intact so next install reproduces the bug). Root cause = install.ps1 source. Tracked: THIS file. — salience: high.
