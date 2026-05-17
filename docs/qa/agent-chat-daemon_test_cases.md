# Test Cases: Agent Chat Daemon + Telegram Bridge + ASR Pipeline + Claude Code Plugin

> Based on [PRD §17](../PRD.md#17-agent-chat-daemon--telegram-bridge--asr-pipeline--claude-code-plugin) and [Use Cases](../use-cases/agent-chat-daemon_use_cases.md)

---

## Facts

### Verified facts

- PRD §17 (FR-ACD-1 through FR-ACD-13, NFR-ACD-1 through NFR-ACD-12, AC-ACD-1 through AC-ACD-15) read in full this session from `docs/PRD.md` lines 407–666. — salience: high.
- Use-case file `docs/use-cases/agent-chat-daemon_use_cases.md` read in full this session: 11 primary UCs (UC-1 through UC-11), 18 alternative flows, 16 error flows, 14 edge cases, totalling 59 scenarios. — salience: high.
- Plan `.claude/plan.md` (374 lines, 7 slices, Waves 1–6) read in full this session. — salience: high.
- Architect verdict: PASS WITH 5 [STRUCTURAL] action items. Slices 1/2/4 → security pre-review. Slices 1/5/6/7 → architect pre-review. `claudebase chat purge` dropped from v1 scope per architect [STRUCTURAL] #3. — salience: high.
- OQ-ACD-4 resolved by architect: `chat.db` location is `~/.claude/knowledge/chat.db` (user-level, NOT per-project). All DB evidence commands use this path. — salience: high.
- `daemon status --json` field names verified against PRD FR-ACD-1.6: `state` (`"running"` | `"stopped"` | `"not-installed"`), `pid` (int|null), `uptime` (sec|null), `socket_path` (str|null), `subscriber_count` (int), `tg_bot_state` (`"connected"` | `"disconnected"` | `"not-configured"`), `asr_backend` (`"whisper"` | `"sherpa-nemo"` | `"nim"` | `"none"`). — salience: high.
- `agent_registry` state CHECK constraint: `('alive', 'orphaned', 'dead')`. Source: PRD §17.7 schema block. — salience: high.
- UDS socket path: `$XDG_RUNTIME_DIR/claudebase/daemon.sock` (Unix) / `\\.\pipe\claudebase-daemon` (Windows). Source: PRD FR-ACD-2.1. — salience: high.
- Whisper model auto-download URL: `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-<size>.bin`. Source: PRD FR-ACD-7.3. — salience: medium.
- Knowledge base corpus is empty (`doc_count: 0, chunk_count: 0`). Corpus scope relevance: No overlap. Topical queries skipped. — salience: low.
- insights-base: doc#1 sha=d0626a76 agent=prd-writer type=agent-learned — query: "agent chat daemon telegram asr" — verified: yes — salience: high (OQ-ACD-4 chat.db path resolved to user-level).
- Existing QA format reference: `docs/qa/agent-insights-base_test_cases.md` read this session — confirms column order and Evidence Required conventions. — salience: medium.

### External contracts

- `claudebase daemon status --json` — symbol: `state`, `pid`, `uptime`, `socket_path`, `subscriber_count`, `tg_bot_state`, `asr_backend` — source: PRD FR-ACD-1.6 (`docs/PRD.md` line 437) — verified: yes (read this session) — salience: high.
- `agent_registry` table schema — symbol: `agent_id`, `agent_name`, `connection_id`, `chat_thread_id`, `state CHECK ('alive','orphaned','dead')` — source: PRD §17.7 (`docs/PRD.md` lines 611–627) — verified: yes — salience: high.
- `chat_messages` table schema — symbol: `id`, `thread_id`, `from_agent`, `content`, `reply_to`, `created_at`, `delivered_at` — source: PRD §17.7 (`docs/PRD.md` lines 586–601) — verified: yes — salience: high.
- MCP JSON-RPC 2.0 wire format — symbol: `initialize`, `tools/list`, `tools/call`, `notifications/claude/channel`, `notifications/tools/list_changed` — source: PRD FR-ACD-3.1 (`docs/PRD.md` line 451); plan risk 2 flags `notifications/claude/channel` as Anthropic-internal spec — verified: no — assumption. Risk: spec may drift or restrict plugin usage. — salience: high.
- NVIDIA NIM endpoint — symbol: `POST https://integrate.api.nvidia.com/v1/audio/transcriptions`, `Authorization: Bearer $NVIDIA_API_KEY` — source: PRD FR-ACD-7.5 (`docs/PRD.md` line 495); plan risk 3 explicitly flags endpoint as 404'd at planning time — verified: no — assumption. Salience: high.
- Telegram Bot API — symbol: `getUpdates` (long-poll), `sendMessage`, voice-note file download — source: PRD FR-ACD-6.1 (`docs/PRD.md` line 479) — verified: no — assumption (standard API; not re-verified this session) — salience: medium.
- `systemd` user unit directives — symbol: `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths`, `NoNewPrivileges=true`, `PrivateTmp=true` — source: PRD FR-ACD-8.1 (`docs/PRD.md` line 503) — verified: yes (read this session) — salience: high.

### Assumptions

- Pairing code expiry window is 1 hour, inferred from UC-6-E1. PRD FR-ACD-6.5 does not state the duration explicitly. Risk: implementation may choose a different window. How to verify: TC-4.6 checks the 1-hour boundary. — salience: medium.
- `daemon stop` sends SIGTERM with 10-second timeout before SIGKILL (UC-8-E1 references "within 10 seconds"). Not explicitly specified in PRD. Risk: different timeout. How to verify: Slice 2 implementation. — salience: low.
- `claudebase chat purge` is NOT in scope for v1 per architect [STRUCTURAL] #3. Any reference to purge is excluded from these test cases. — salience: high.

### Open questions

- OQ-ACD-4 (chat.db location) — resolved to `~/.claude/knowledge/chat.db` per architect verdict. All DB evidence commands use this path. — salience: high.
- OQ-NIM-1 (NIM endpoint shape) — NVIDIA NIM endpoint could not be verified at planning time (plan risk 3). If the endpoint is gRPC-only or uses a different path, TC-6.4, TC-6.5, and TC-6.12 may need path adjustment. The whisper-backend TCs are unaffected. Needs: verification at Slice 6 implementation. — salience: medium.
- OQ-ACD-UC-2 (chat.db growth management) — `claudebase chat purge` is dropped from v1. DB size management is out of scope. No TCs covering purge. — salience: low.

---

## Decisions

### Inbound validation

- Task: write 80–120 TCs for agent-chat-daemon mapped to all 59 UC scenarios, per slice grouping (TC-1.x through TC-7.x). Challenged: yes — verified all inputs (PRD, use-cases, plan, architect verdict) are consistent. The `claudebase chat purge` exclusion (architect [STRUCTURAL] #3) is correctly applied. OQ-ACD-4 resolution (`~/.claude/knowledge/chat.db`) is load-bearing for all DB evidence commands. No incoherence found. Outcome: proceed. — salience: high.

### Decisions made

- Slice grouping for TC-IDs follows the 7 implementation slices (TC-1.x = Slice 1 UDS + plugin bridge; TC-2.x = Slice 2 install; TC-3.x = Slice 3 chat backend; TC-4.x = Slice 4 Telegram; TC-5.x = Slice 5 agent_registry; TC-6.x = Slice 6 ASR; TC-7.x = Slice 7 subagent routing). Edge cases that span slices are placed in the slice most relevant to the failure mode. Q1: not a hack. Q2: sane — matches the test-writer's slice-by-slice workflow. — salience: medium.
- DB verification class is used for standalone SQL checks against `chat.db` or `agent_registry`. Mixed is used when a test requires both a CLI action AND a DB state verification (e.g., send message and verify row count). This avoids inflating DB-only TCs to Mixed when the SQL verification is a single confirming query following a clear CLI precondition. — salience: medium.
- Cross-platform annotation `[platform: linux|macos|windows|all]` appears in the TC-ID column suffix for TCs that are OS-specific. Platform-agnostic TCs have no suffix. — salience: medium.
- `claudebase chat purge` references are explicitly excluded per architect [STRUCTURAL] #3. No TCs reference this subcommand. — salience: high.

### Hacks / workarounds acknowledged

- (none)

### Symptom-only patches (with root-cause links)

- (none)

---

## Use Case Coverage Map

| Use Case | Test Case(s) |
|----------|--------------|
| UC-1 (primary) | TC-2.1, TC-2.2 |
| UC-1-A | TC-2.3 |
| UC-1-B | TC-2.4 |
| UC-1-C | TC-2.5 |
| UC-1-E1 | TC-2.6 |
| UC-1-E2 | TC-2.7 |
| UC-1-E3 | TC-2.8 |
| UC-1-EC1 | TC-2.9 |
| UC-1-EC2 | TC-2.10 |
| UC-2 (primary) | TC-2.11 |
| UC-2-A | TC-2.12 |
| UC-2-B | TC-2.13 |
| UC-2-E1 | TC-2.14 |
| UC-2-E2 | TC-2.15 |
| UC-2-E3 | TC-2.16 |
| UC-2-EC1 | TC-2.17 |
| UC-2-EC2 | TC-2.18 |
| UC-3 (primary) | TC-3.1, TC-3.2 |
| UC-3-A | TC-3.3 |
| UC-3-B | TC-3.4 |
| UC-3-C | TC-3.5 |
| UC-3-E1 | TC-4.7 |
| UC-3-E2 | TC-4.8 |
| UC-3-EC1 | TC-3.6 |
| UC-3-EC2 | TC-3.7 |
| UC-4 (primary) | TC-6.1 |
| UC-4-A | TC-6.2 |
| UC-4-B | TC-6.4 |
| UC-4-C | TC-6.5 |
| UC-4-E1 | TC-6.6 |
| UC-4-E2 | TC-6.7 |
| UC-4-E3 | TC-6.8 |
| UC-4-EC1 | TC-6.9 |
| UC-4-EC2 | TC-6.10 |
| UC-4-EC3 | TC-6.11 |
| UC-5 (primary) | TC-7.1, TC-7.2 |
| UC-5-A | TC-7.3 |
| UC-5-B | TC-7.4 |
| UC-5-C | TC-7.5 |
| UC-5-E1 | TC-7.6 |
| UC-5-EC1 | TC-7.7 |
| UC-5-EC2 | TC-5.7 |
| UC-6 (primary) | TC-4.1, TC-4.2 |
| UC-6-A | TC-4.3 |
| UC-6-B | TC-4.4 |
| UC-6-E1 | TC-4.5 |
| UC-6-E2 | TC-4.6 |
| UC-6-EC1 | TC-4.9 |
| UC-6-EC2 | TC-4.10 |
| UC-7 (primary) | TC-6.3, TC-6.12 |
| UC-7-A | TC-6.13 |
| UC-7-B | TC-6.14 |
| UC-7-E1 | TC-6.15 |
| UC-8 (primary) | TC-2.19, TC-1.9 |
| UC-8-A | TC-2.20 |
| UC-8-B | TC-2.21 |
| UC-8-E1 | TC-2.22 |
| UC-9 (primary) | TC-2.23 |
| UC-9-A | TC-2.24 |
| UC-10 (primary) | TC-2.25 |
| UC-11 (primary) | TC-6.16, TC-6.17, TC-6.18 |
| UC-EC-1 (daemon-down) | TC-1.7, TC-1.8 |

---

## 1. Slice 1 — Daemon Skeleton + UDS Server + STDIO Plugin Bridge

### 1.1 Happy Path — UDS Server and Plugin Bridge

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-1.1 [platform: linux,macos] | UC-2 (primary), UC-2-B | FR-ACD-2.1, FR-ACD-1.8 | CLI | `claudebase` binary on PATH; no daemon running; `$XDG_RUNTIME_DIR` set | 1. Run `claudebase daemon serve &` 2. Wait 1 s 3. Run `ls $XDG_RUNTIME_DIR/claudebase/daemon.sock` | Socket file exists at `$XDG_RUNTIME_DIR/claudebase/daemon.sock`; daemon process is alive | `ls -la $XDG_RUNTIME_DIR/claudebase/daemon.sock` output showing the socket file; `pgrep -a claudebase` output showing process alive |
| TC-1.2 [platform: windows] | UC-2 (primary) | FR-ACD-2.1 | CLI | `claudebase` binary on PATH; no daemon running | 1. Run `claudebase daemon serve` in background 2. After 1 s check named pipe | Named pipe `\\.\pipe\claudebase-daemon` is present | PowerShell `[System.IO.File]::Exists('\\.\pipe\claudebase-daemon')` returns `True` |
| TC-1.3 [platform: linux,macos] | UC-2 (primary) | FR-ACD-3.1, FR-ACD-3.2 | API | Daemon running at `$XDG_RUNTIME_DIR/claudebase/daemon.sock` | 1. Run `echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}' \| claudebase plugin serve` | Plugin completes MCP `initialize` handshake; stdout JSON contains `"result"` with `"protocolVersion"` field and method `initialize`; exit 0 | stdout JSON literal showing `"jsonrpc":"2.0"`, `"id":1`, `"result"` containing `"serverInfo"` with `"name"` field; `jq .result.protocolVersion` returns a non-empty string |
| TC-1.4 [platform: linux,macos] | UC-2 (primary) | FR-ACD-3.1, AC-ACD-3 | API | Daemon running; plugin bridge connected | 1. Send `{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}` to `claudebase plugin serve` stdin | Response JSON `result.tools` array is non-empty; contains at least one entry with `name` field | stdout JSON `jq '.result.tools \| length'` returns integer ≥ 1; `jq '.result.tools[].name'` lists tool names; captured in `tc-1.4-tools-list.json` |
| TC-1.5 [platform: linux,macos] | UC-2-B | FR-ACD-2.2, FR-ACD-2.3 | API | Daemon running | 1. Connect second `claudebase plugin serve` process 2. Both send `initialize` | Both connections receive valid `initialize` responses with distinct `connection_id` values in daemon logs | Two stdout captures (`tc-1.5-session-a.json`, `tc-1.5-session-b.json`) each showing valid `initialize` result; `journalctl --user -u claudebase -n 20` (Linux) shows two distinct `connection_id` UUID v4 values |
| TC-1.6 [platform: linux,macos] | UC-2 (primary) | FR-ACD-2.5 | Mixed | Daemon running; one plugin connected | 1. Connect `claudebase plugin serve`; note connection_id from daemon logs 2. Kill plugin process (SIGKILL) 3. After 1 s query daemon logs | Daemon detects EOF; logs event; all `agent_registry` rows for that `connection_id` marked `state = 'orphaned'` (Slice 5 precondition) | `journalctl --user -u claudebase -n 20 \| grep "EOF"` shows connection-close event with `connection_id`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT state FROM agent_registry WHERE connection_id = '<id>'"` returns `orphaned` for any rows (if Slice 5 is already landed) |

### 1.2 Daemon-Down Graceful Degradation (UC-EC-1)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-1.7 [platform: linux,macos] | UC-EC-1, UC-8 | FR-ACD-10.1, FR-ACD-10.2, AC-ACD-7 | API | No daemon running (not installed or stopped) | 1. Run `claudebase plugin serve` (daemon intentionally absent) 2. Send MCP `initialize` 3. Send `tools/list` | (a) Plugin completes `initialize` handshake without crashing; (b) `tools/list` returns exactly ONE tool: `claudebase_daemon_status` with empty schema `{}`; (c) exit code of plugin is 0 (not crashed) | stdout JSON `tc-1.7-tools-list.json`: `jq '.result.tools \| length'` returns `1`; `jq '.result.tools[0].name'` returns literal `"claudebase_daemon_status"`; `jq '.result.tools[0].inputSchema'` returns `{}`; plugin process still alive after 3 s (checked via `pgrep`) |
| TC-1.8 [platform: linux,macos] | UC-EC-1, UC-8 | FR-ACD-10.1, FR-ACD-10.3, AC-ACD-7 | Mixed | Plugin running with daemon down (TC-1.7 state); daemon is installed but stopped | 1. With plugin connected in daemon-down mode, run `claudebase daemon start` 2. Wait 2 s 3. From plugin: send `tools/list` again | (a) Daemon starts; (b) plugin automatically reconnects; (c) plugin sends `notifications/tools/list_changed` to Claude Code; (d) `tools/list` now returns full tool list (≥1 tool beyond `claudebase_daemon_status`) | Daemon logs `journalctl --user -u claudebase -n 10` show new connection accepted; second `tools/list` stdout `tc-1.8-tools-list-after.json`: `jq '.result.tools \| length'` returns value > 1; plugin stdout stream contains `notifications/tools/list_changed` notification before the updated `tools/list` response |
| TC-1.9 [platform: linux,macos] | UC-8 | FR-ACD-9.3, FR-ACD-1.4 | FS | Daemon running; UDS socket file exists; PID file exists | 1. Run `claudebase daemon stop` 2. After stop completes: check socket and PID file | Socket file `$XDG_RUNTIME_DIR/claudebase/daemon.sock` does NOT exist; PID file `$XDG_RUNTIME_DIR/claudebase/daemon.pid` does NOT exist; `daemon status --json` returns `{"state":"stopped"}` | `ls $XDG_RUNTIME_DIR/claudebase/daemon.sock` exits non-zero (file absent); `ls $XDG_RUNTIME_DIR/claudebase/daemon.pid` exits non-zero; `claudebase daemon status --json \| jq .state` returns literal `"stopped"` |

### 1.3 Single-Instance Enforcement

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-1.10 [platform: linux,macos] | — | FR-ACD-9.1, FR-ACD-9.2, AC-ACD-12 | CLI | Daemon running (first instance); PID file held | 1. Run second `claudebase daemon serve` in a separate terminal | Second invocation exits 1 within 1 second; stderr contains literal `claudebase daemon: already running (pid ` followed by an integer PID | Second invocation exit code captured: `echo $?` returns `1`; stderr captured in `tc-1.10-stderr.txt` containing substring `claudebase daemon: already running (pid` and an integer N; timing: second invocation completes in ≤ 1 s (measured via `time` prefix) |

---

## 2. Slice 2 — Service Install (All Three OSes)

### 2.1 First-Time Install (UC-1)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-2.1 [platform: linux] | UC-1 (primary) | FR-ACD-1.1, FR-ACD-8.1, AC-ACD-1 | FS | Fresh Linux system; `claudebase` on PATH; no prior install | 1. Run `claudebase daemon install --yes` | (a) `~/.config/systemd/user/claudebase.service` exists; (b) `~/.claude/plugins/claudebase/.mcp.json` exists; (c) `systemctl --user is-enabled claudebase` returns `enabled`; (d) exit 0; (e) stdout contains `claudebase daemon installed` | `cat ~/.config/systemd/user/claudebase.service` output (non-empty); `cat ~/.claude/plugins/claudebase/.mcp.json \| jq .command` returns `"claudebase"` and `jq .args` returns `["plugin","serve"]`; `systemctl --user is-enabled claudebase` output literal `enabled`; `echo $?` returns `0` |
| TC-2.2 [platform: macos] | UC-1 (primary) | FR-ACD-1.1, FR-ACD-8.2, AC-ACD-1 | FS | Fresh macOS; `claudebase` on PATH; no prior install | 1. Run `claudebase daemon install --yes` | (a) `~/Library/LaunchAgents/dev.codefather.claudebase.plist` exists; (b) `~/.claude/plugins/claudebase/.mcp.json` exists with correct content; (c) exit 0 | `ls -la ~/Library/LaunchAgents/dev.codefather.claudebase.plist` (non-empty file); `cat ~/.claude/plugins/claudebase/.mcp.json \| jq '{command,args}'` returns `{"command":"claudebase","args":["plugin","serve"]}`; exit code `0` |
| TC-2.3 [platform: linux] | UC-1-A | FR-ACD-1.1, FR-ACD-8.4 | CLI | Fresh Linux system; no prior install | 1. Run `claudebase daemon install --yes --no-start` 2. Check service state | Service unit written and enabled; daemon NOT started immediately; `daemon status --json` returns `{"state":"stopped"}` or `{"state":"not-installed"}`; stdout mentions `To start now: claudebase daemon start` | `systemctl --user is-active claudebase` returns `inactive` (not `active`); `claudebase daemon status --json \| jq .state` returns `"stopped"` or `"not-installed"`; stdout contains substring `claudebase daemon start` |
| TC-2.4 [platform: linux] | UC-1-B | FR-ACD-1.1, AC-ACD-1 | CLI | Daemon already installed (TC-2.1 complete) | 1. Run `claudebase daemon install --yes` a second time | Exit 0; stdout contains `already installed (no changes)`; service unit file content unchanged (checksum identical) | `echo $?` returns `0`; stdout contains `already installed`; `sha256sum ~/.config/systemd/user/claudebase.service` matches pre-run checksum captured before step 1 |
| TC-2.5 [platform: linux] | UC-1-C | FR-ACD-8.4 | CLI | Fresh Linux; `install.sh` present | 1. Run `CLAUDEBASE_INSTALL_DAEMON=1 bash install.sh --yes` | Post-install hook calls `claudebase daemon install --no-start`; service unit written silently; no `daemon start` invocation; exit 0 | `ls ~/.config/systemd/user/claudebase.service` exits 0 (file exists); `systemctl --user is-active claudebase` returns `inactive`; install.sh stdout does NOT contain error messages |

### 2.2 Service Unit Hardening (Security)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-2.6 [platform: linux] | — | FR-ACD-8.1, NFR-ACD-8, AC-ACD-11 | FS | `claudebase daemon install --yes` completed | 1. Read service unit file | Unit contains ALL of: `ProtectSystem=strict`, `NoNewPrivileges=true`, `PrivateTmp=true`, `ProtectHome=read-only`, `ReadWritePaths` including `%h/.claude` and `%h/.config/claudebase`; does NOT contain `User=root` | `grep 'ProtectSystem=strict' ~/.config/systemd/user/claudebase.service` exits 0; `grep 'NoNewPrivileges=true' ~/.config/systemd/user/claudebase.service` exits 0; `grep 'PrivateTmp=true' ~/.config/systemd/user/claudebase.service` exits 0; `grep 'ProtectHome=read-only' ~/.config/systemd/user/claudebase.service` exits 0; `grep 'User=root' ~/.config/systemd/user/claudebase.service` exits non-zero (absent) |
| TC-2.7 [platform: windows] | — | FR-ACD-8.3, NFR-ACD-8 | CLI | Windows system; `claudebase` on PATH | 1. Run `claudebase daemon install --yes` 2. Check service account | Windows Service registered as current user (NOT LocalSystem); `sc qc claudebase` shows `SERVICE_START_NAME` is current user, NOT `LocalSystem` | PowerShell `(Get-Service claudebase).StartType` returns `Automatic`; PowerShell `(sc.exe qc claudebase) -match "LocalSystem"` is `False`; `sc qc claudebase` stdout captured in `tc-2.7-service-config.txt` |

### 2.3 Install Error Flows

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-2.8 [platform: linux] | UC-1-E3 | FR-ACD-6.4 | CLI | `claudebase daemon install --yes` complete; `secrets.toml` NOT present | 1. Run `claudebase daemon start` | Daemon starts; `tg_bot_state` is `"not-configured"` (not an error); UDS socket available | `claudebase daemon status --json \| jq .tg_bot_state` returns `"not-configured"`; `claudebase daemon status --json \| jq .state` returns `"running"`; exit 0 |
| TC-2.9 [platform: linux] | UC-1-EC1 | FR-ACD-1.1 | CLI | Linux environment where `systemctl --user` is not available (WSL without systemd, or container) | 1. Run `claudebase daemon install --yes` | Exit code is non-zero OR exit 0 with clear warning; stdout/stderr contains `systemd user units not supported` or equivalent warning; no crash | stderr or stdout contains `Warning: systemd user units not supported` (case-insensitive substring match); process exits without panic (exit 0 or 1 with clean message); no Rust backtrace in output |
| TC-2.10 [platform: linux] | UC-1-EC2 | FR-ACD-8.1 | CLI | Running as root (`$UID == 0`) | 1. Run `sudo claudebase daemon install --yes` | Exit 1; stderr contains `do not run 'daemon install' as root` | `echo $?` returns `1`; stderr captured contains literal substring `do not run 'daemon install' as root`; no service unit written (`ls ~/.config/systemd/user/claudebase.service` exits non-zero) |

### 2.4 Daemon Start / Stop Lifecycle

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-2.11 [platform: linux] | UC-2 (primary) | FR-ACD-1.3, FR-ACD-1.6, AC-ACD-2 | CLI | Daemon installed (TC-2.1); daemon stopped | 1. Run `claudebase daemon start` 2. Run `claudebase daemon status --json` | (a) `daemon start` exits 0 with `claudebase daemon started (pid NNNN)` in stdout; (b) status JSON fields: `state == "running"`, `pid` is integer > 0, `uptime` is integer ≥ 0, `socket_path` is non-empty string, `subscriber_count == 0`, `tg_bot_state` is one of `"connected"/"disconnected"/"not-configured"`, `asr_backend` is one of `"whisper"/"sherpa-nemo"/"nim"/"none"` | `claudebase daemon start` stdout contains `pid`; `claudebase daemon status --json` output captured in `tc-2.11-status.json`; `jq '{state,pid,uptime,socket_path,subscriber_count,tg_bot_state,asr_backend}' tc-2.11-status.json` all fields present and correctly typed |
| TC-2.12 [platform: linux] | UC-2-A | FR-ACD-1.3 | CLI | Daemon already running | 1. Run `claudebase daemon start` again | Exit 0; stdout contains `already running (pid NNNN)`; no second process launched | `echo $?` returns `0`; stdout contains `already running`; `pgrep -c claudebase-daemon` returns `1` (only one instance) |
| TC-2.13 [platform: linux] | UC-2-B | FR-ACD-1.8 | CLI | Daemon installed; no daemon currently running | 1. Run `claudebase daemon serve` directly in foreground 2. Press Ctrl-C | Daemon runs in foreground; PID file acquired; Ctrl-C triggers graceful shutdown; socket and PID file removed | `pgrep claudebase` returns daemon PID while running; after Ctrl-C: `ls $XDG_RUNTIME_DIR/claudebase/daemon.pid` exits non-zero (removed); `ls $XDG_RUNTIME_DIR/claudebase/daemon.sock` exits non-zero (unlinked) |
| TC-2.14 [platform: linux] | UC-2-E1 | FR-ACD-1.3 | CLI | Service unit manually corrupted (e.g., `ExecStart` points to wrong binary path) | 1. Corrupt service unit 2. Run `claudebase daemon start` | Exit non-zero; stderr contains `service start failed:` and includes systemctl error output | `echo $?` returns non-zero; stderr captured contains `service start failed`; stderr contains `daemon doctor` or `reinstall` suggestion |
| TC-2.15 [platform: linux] | UC-2-E2 | FR-ACD-6.4, NFR-ACD-7, AC-ACD-13 | Mixed | `secrets.toml` present but with permissions `0644` | 1. Run `claudebase daemon start` | Daemon starts (UDS socket available); `tg_bot_state` is `"not-configured"` (Telegram NOT started); error logged about permissions | `claudebase daemon status --json \| jq .state` returns `"running"`; `claudebase daemon status --json \| jq .tg_bot_state` returns `"not-configured"`; `journalctl --user -u claudebase -n 20` (Linux) contains `secrets.toml must have permissions 0600` or `0644` reference |
| TC-2.16 [platform: linux] | UC-2-E3 | FR-ACD-1.8 | CLI | `$XDG_RUNTIME_DIR` is unset | 1. Run `unset XDG_RUNTIME_DIR && claudebase daemon serve &` 2. Check fallback socket path | Daemon falls back to `~/.claude/run/claudebase/daemon.sock`; warning in logs | `ls ~/.claude/run/claudebase/daemon.sock` exits 0; daemon logs contain `XDG_RUNTIME_DIR not set`; socket is functional (plugin can connect) |
| TC-2.17 [platform: all] | UC-2-EC1 | NFR-ACD-12 | CLI | Two OS users on same machine (or simulated with different `$HOME`) | 1. User A starts daemon 2. User B starts daemon 3. Check isolation | User A and B have separate UDS socket paths, PID files, and `chat.db` files; no socket conflict | User A: `$XDG_RUNTIME_DIR/claudebase/daemon.sock` is user-A-owned; User B: their own path; `ls -la` output for both showing different ownership; no file-not-found or permission errors on either side |
| TC-2.18 [platform: linux] | UC-2-EC2 | FR-ACD-12.3 | CLI | `daemon.toml` with `[asr] backend = "deepgram"` (unknown value) | 1. Run `claudebase daemon serve` | Exit 1; stderr contains `unknown ASR backend "deepgram"` and lists allowed values `whisper`, `sherpa-nemo`, `nim` | `echo $?` returns `1`; stderr contains `unknown ASR backend "deepgram"` and `Allowed values` |
| TC-2.19 [platform: linux] | UC-8 (primary) | FR-ACD-1.4, FR-ACD-9.3 | CLI | Daemon running | 1. Run `claudebase daemon stop` 2. Run `claudebase daemon status --json` | (a) stop exits 0; (b) status returns `{"state":"stopped"}`; (c) PID file and socket file absent | `echo $?` after stop returns `0`; `claudebase daemon status --json \| jq .state` returns `"stopped"`; `ls $XDG_RUNTIME_DIR/claudebase/daemon.pid` exits non-zero; `ls $XDG_RUNTIME_DIR/claudebase/daemon.sock` exits non-zero |
| TC-2.20 [platform: linux] | UC-8-A | FR-ACD-1.5 | CLI | Daemon running | 1. Run `claudebase daemon restart` | Exit 0; daemon process has a new PID after restart; `daemon status` returns `running` | New PID ≠ old PID: capture old PID via `claudebase daemon status --json \| jq .pid` before; capture new PID after restart; `claudebase daemon status --json \| jq .state` returns `"running"` |
| TC-2.21 [platform: linux] | UC-8-B | FR-ACD-1.4 | CLI | Daemon NOT running | 1. Run `claudebase daemon stop` | Exit 0; stdout contains `already stopped`; no error | `echo $?` returns `0`; stdout contains `already stopped`; no Rust backtrace |
| TC-2.22 [platform: linux] | UC-8-E1 | FR-ACD-1.4 | CLI | Daemon running; intentionally make it unresponsive to SIGTERM (e.g., block signal in test stub) | 1. Run `claudebase daemon stop` 2. Wait > 10 s | OS sends SIGKILL after timeout; daemon process no longer exists; `daemon status` returns `stopped`; WARN in logs | `pgrep claudebase` exits non-zero after SIGKILL; `journalctl --user -u claudebase -n 20` contains `kill` or `SIGKILL` reference; `claudebase daemon status --json \| jq .state` returns `"stopped"` |

### 2.5 Uninstall

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-2.23 [platform: linux] | UC-9 (primary) | FR-ACD-1.2 | FS | Daemon installed and running; `chat.db` exists | 1. Run `claudebase daemon uninstall --keep-data` | (a) Service unit removed; (b) `.mcp.json` removed; (c) `chat.db` preserved; (d) `secrets.toml` preserved; (e) exit 0 | `ls ~/.config/systemd/user/claudebase.service` exits non-zero; `ls ~/.claude/plugins/claudebase/.mcp.json` exits non-zero; `ls ~/.claude/knowledge/chat.db` exits 0 (preserved); `ls ~/.config/claudebase/secrets.toml` exits 0 (preserved) |
| TC-2.24 [platform: linux] | UC-9-A | FR-ACD-1.2 | FS | Daemon installed; `chat.db` and `secrets.toml` exist | 1. Run `claudebase daemon uninstall` (no `--keep-data`) | Service unit, `.mcp.json`, `chat.db`, `secrets.toml`, `daemon.toml`, `access.json` all removed; exit 0 | `ls ~/.config/systemd/user/claudebase.service` exits non-zero; `ls ~/.claude/knowledge/chat.db` exits non-zero; `ls ~/.config/claudebase/secrets.toml` exits non-zero |
| TC-2.25 [platform: linux] | UC-10 (primary) | FR-ACD-1.7 | CLI | Daemon installed and running; at least one log entry exists | 1. Run `claudebase daemon logs --lines 10` | Exits 0; stdout contains ≥1 log line; log lines are from the claudebase daemon process | `echo $?` returns `0`; stdout non-empty; stdout contains `claudebase` in each line (journalctl format includes process name); `claudebase daemon logs --follow` streams new lines when a new log event fires (verified by running `daemon status` in a second terminal and observing new log output) |

---

## 3. Slice 3 — Chat Backend + MCP Tools

### 3.1 Schema Migration (DB)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-3.1-schema [platform: all] | UC-3 (primary) | FR-ACD-4.1, §17.7 schema v5 | DB | Daemon started for the first time (no `chat.db` yet) | 1. Start daemon 2. Stop daemon 3. Inspect `~/.claude/knowledge/chat.db` | `chat.db` contains tables: `chat_threads` and `chat_messages`; `chat_messages` has index `chat_messages_thread_time_idx` | `sqlite3 ~/.claude/knowledge/chat.db ".tables"` output contains `chat_threads` and `chat_messages`; `sqlite3 ~/.claude/knowledge/chat.db ".indexes"` output contains `chat_messages_thread_time_idx` |

### 3.2 Chat Tools — Happy Path

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-3.1 [platform: linux,macos] | UC-3 (primary), UC-3-B | FR-ACD-4.1, FR-ACD-4.2, FR-ACD-4.5, AC-ACD-4 | Mixed | Daemon running; plugin connected | 1. Call `chat_subscribe { "thread": "telegram:99999" }` via `tools/call` 2. Call `chat_post { "thread": "telegram:99999", "content": "hello", "from": "test-agent" }` via `tools/call` 3. Observe plugin stdout for notification 4. Query DB | (a) `chat_subscribe` returns undelivered backlog (empty on fresh thread); (b) `chat_post` returns success response within 100 ms; (c) plugin receives `notifications/claude/channel` notification with `thread == "telegram:99999"` and `content == "hello"`; (d) DB row inserted | MCP `tools/call` response for `chat_post` captured in `tc-3.1-post-resp.json`; plugin stdout contains `notifications/claude/channel` notification with literal `"content":"hello"`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT from_agent, content FROM chat_messages WHERE thread_id='telegram:99999'"` returns exactly 1 row with `from_agent='test-agent'` and `content='hello'` |
| TC-3.2 [platform: linux,macos] | UC-3 (primary) | FR-ACD-4.3, FR-ACD-4.5, AC-ACD-5, NFR-ACD-1 | Mixed | Daemon running; plugin connected; one message posted (TC-3.1 complete) | 1. Call `chat_reply { "thread": "telegram:99999", "content": "reply-text", "reply_to": "<message_id from TC-3.1>" }` via `tools/call` 2. Query DB | (a) Reply persisted with `reply_to` linked to original message id; (b) reply broadcast to subscribed connection; (c) round-trip completes within 1 second from post to notification receipt | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content, reply_to FROM chat_messages WHERE content='reply-text'"` returns 1 row with non-null `reply_to`; daemon trace log timestamp delta between inbound message and outbound notification ≤ 1 s (check `journalctl --user -u claudebase -n 50` timestamps) |
| TC-3.3 [platform: linux,macos] | UC-3-A | FR-ACD-2.4, FR-ACD-4.6, NFR-ACD-6, AC-ACD-8 | Mixed | Daemon running; TWO plugin instances connected; both subscribed to same thread | 1. From plugin A: call `chat_post { "thread": "telegram:99999", "content": "broadcast-test", "from": "mira" }` 2. Observe both plugin A and B stdout | Both plugins receive `notifications/claude/channel` with identical payload; `subscriber_count` in `daemon status --json` reflects 2 | Plugin A stdout AND plugin B stdout each contain `notifications/claude/channel` with `"content":"broadcast-test"`; `claudebase daemon status --json \| jq .subscriber_count` returns `2` |
| TC-3.4 [platform: linux,macos] | UC-3-B | FR-ACD-4.2, AC-ACD-4 | Mixed | Daemon running; NO plugin connected; one message posted to `telegram:77777` (persisted but not delivered) | 1. Connect new plugin 2. Call `chat_subscribe { "thread": "telegram:77777" }` | `chat_subscribe` returns backlog containing the undelivered message with `content` matching the earlier post | MCP `tools/call` response for `chat_subscribe` captured in `tc-3.4-subscribe.json`; `jq '.result.messages \| length'` returns `1`; `jq '.result.messages[0].content'` returns the posted message text; `sqlite3 ~/.claude/knowledge/chat.db "SELECT delivered_at FROM chat_messages WHERE thread_id='telegram:77777'"` has `delivered_at` populated after subscribe |
| TC-3.5 [platform: linux,macos] | UC-3-C | FR-ACD-4.3 | Mixed | Daemon running; plugin connected | 1. Call `chat_reply { "thread": "telegram:99999", "content": "stale-reply", "reply_to": "nonexistent-uuid-1234" }` | Reply persisted with `reply_to = NULL` (graceful degradation); no error returned; warning in daemon logs | MCP response for `chat_reply` is success (no error field); `sqlite3 ~/.claude/knowledge/chat.db "SELECT reply_to FROM chat_messages WHERE content='stale-reply'"` returns `NULL`; `journalctl --user -u claudebase -n 20` contains `warn` and `reply_to` |
| TC-3.6 [platform: linux,macos] | UC-3-EC1 | FR-ACD-4.1 | Mixed | Daemon running; plugin subscribed to thread | 1. Call `chat_post { "thread": "telegram:55555", "content": "", "from": "test" }` (empty content) | Post succeeds; empty-content row in DB; notification broadcast to subscribers | MCP response for `chat_post` has no error field; `sqlite3 ~/.claude/knowledge/chat.db "SELECT length(content) FROM chat_messages WHERE thread_id='telegram:55555'"` returns `0` |
| TC-3.7 [platform: linux,macos] | UC-3-EC2 | FR-ACD-4.1, FR-ACD-4.6, NFR-ACD-3 | Mixed | Daemon running; plugin subscribed | 1. Call `chat_post` twice within 50 ms for same thread | Both messages persisted in insertion order; both broadcast; no coalescing; broadcast latency ≤ 10 ms each | `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM chat_messages WHERE thread_id='telegram:55556'"` returns `2`; daemon trace logs show two separate notification dispatch events with timestamps ≤ 10 ms apart |

### 3.3 Broadcast Latency (NFR-ACD-3)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-3.8 [platform: linux,macos] | — | NFR-ACD-3, FR-ACD-4.6 | Mixed | Daemon running; plugin subscribed to thread | 1. Record timestamp T1; call `chat_post` 2. Record timestamp T2 when `notifications/claude/channel` received by plugin | T2 - T1 ≤ 10 ms | Daemon trace log lines for the post event and notification dispatch event; timestamps extracted and delta computed: `δ = T2 - T1 ≤ 10 ms`; or plugin stdout timestamp vs. post invocation timestamp (shell `date +%s%N` before and after) |

### 3.4 CLI Chat Introspection

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-3.9 [platform: all] | — | FR-ACD-13.1, AC-ACD-14 | CLI | `chat.db` exists with messages in thread `telegram:12345`; daemon NOT running | 1. Run `claudebase chat list --thread telegram:12345` | Exit 0; messages printed in chronological order; no daemon connection required | `echo $?` returns `0`; stdout contains message content from thread `telegram:12345`; verified against `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages WHERE thread_id='telegram:12345' ORDER BY created_at"` which matches the CLI output order |
| TC-3.10 [platform: all] | — | FR-ACD-13.2 | CLI | `chat.db` exists with multiple threads | 1. Run `claudebase chat threads` | Exit 0; all thread ids listed with message counts and last-message timestamps | `echo $?` returns `0`; stdout contains all thread ids visible via `sqlite3 ~/.claude/knowledge/chat.db "SELECT id FROM chat_threads"`; each thread id accompanied by a count and timestamp |

---

## 4. Slice 4 — Telegram Bot Integration

### 4.1 Permission Pairing Flow (UC-6)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-4.1 [platform: linux,macos] | UC-6 (primary) | FR-ACD-6.5, FR-ACD-6.6, AC-ACD-13 | Mixed | Daemon running with valid bot token; `dmPolicy = "pairing"` in `daemon.toml`; unknown Telegram user sends first message | 1. Unknown user sends `/start` to bot 2. Bot replies with inline keyboard showing pairing code (e.g., `X7K2Q9`) 3. User runs `claudebase daemon access pair X7K2Q9` | (a) Bot sends Telegram message with inline keyboard containing `claudebase daemon access pair X7K2Q9`; (b) `access pair` exits 0; (c) `access.json` contains user in `allowFrom`; (d) bot sends Telegram confirmation `"You are now authorized."` | Telegram screenshot `tc-4.1-pairing-keyboard.png` showing inline keyboard with pairing code text; `echo $?` after `access pair` returns `0`; `cat ~/.config/claudebase/access.json \| jq '.allowFrom \| length'` returns ≥ 1; `cat ~/.config/claudebase/access.json \| jq '.pending \| length'` returns `0` (pending cleared) |
| TC-4.2 [platform: linux,macos] | UC-6 (primary) | FR-ACD-6.7 | CLI | Daemon running; at least one authorized user in `access.json` | 1. Run `claudebase daemon access list` | Exit 0; table/list output with `telegram_user_id`, `username`, `authorized_at` columns for each authorized user | `echo $?` returns `0`; stdout contains column headers `telegram_user_id` / `username` / `authorized_at`; data rows match entries in `cat ~/.config/claudebase/access.json \| jq '.allowFrom'` |
| TC-4.3 [platform: linux,macos] | UC-6-A | FR-ACD-6.5 | Mixed | Daemon running; `dmPolicy = "allowlist"` in `daemon.toml`; unknown user sends message | 1. Unknown user sends any message to bot | No pairing code sent; message silently discarded; audit log entry made; no `chat_messages` row inserted | Telegram UI: NO reply from bot (screenshot `tc-4.3-no-reply.png` showing absence of bot response after 5 s); `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM chat_messages WHERE from_agent LIKE 'telegram:%'"` returns count unchanged from before the message was sent; daemon logs contain `discarded` or `allowlist` entry |
| TC-4.4 [platform: linux,macos] | UC-6-B | FR-ACD-6.5 | Mixed | Daemon running; `dmPolicy = "disabled"` in `daemon.toml`; unknown user sends message | 1. Unknown user sends message to bot | Message accepted without pairing; `chat_messages` row inserted for the unknown user | `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM chat_messages"` increases by 1; no pairing code sent (Telegram screenshot `tc-4.4-no-pairing.png` shows message NOT blocked) |
| TC-4.5 [platform: linux,macos] | UC-6-E1 | FR-ACD-6.5, AC-ACD-13 | CLI | Daemon running; pairing code `X7K2Q9` issued > 1 hour ago (simulate by manually setting `expires_at` to past timestamp in `access.json`) | 1. Run `claudebase daemon access pair X7K2Q9` | Exit 1; stderr contains `pairing code X7K2Q9 has expired`; bot sends Telegram message `"Your pairing code has expired."` | `echo $?` returns `1`; stderr contains `has expired` and `X7K2Q9`; Telegram screenshot `tc-4.5-expired-tg.png` showing bot's expiry message |
| TC-4.6 [platform: linux,macos] | UC-6-E2 (wrong code variant) | FR-ACD-6.5 | CLI | Daemon running; no code `XXXXXXXX` issued | 1. Run `claudebase daemon access pair XXXXXXXX` | Exit 1; stderr contains `unknown pairing code` | `echo $?` returns `1`; stderr contains `unknown pairing code`; `Codes are case-sensitive` in stderr |
| TC-4.9 [platform: linux,macos] | UC-6-EC1 | FR-ACD-6.5 | Mixed | Daemon running; user has pending pairing code already | 1. Same user sends a second message before pairing | Bot resends the SAME pairing code (does NOT generate a new one) | Telegram screenshot `tc-4.9-resend-code.png` showing second bot reply with identical pairing code text; `cat ~/.config/claudebase/access.json \| jq '.pending \| length'` returns `1` (still one pending entry, not two) |
| TC-4.10 [platform: linux,macos] | UC-6-EC2 | FR-ACD-6.5, FR-ACD-6.6 | CLI | Two different Telegram users both have pending pairing codes simultaneously | 1. User A runs `claudebase daemon access pair <code-A>` 2. User B runs `claudebase daemon access pair <code-B>` | Both `access pair` calls exit 0; both users appear in `allowFrom`; no collision | `cat ~/.config/claudebase/access.json \| jq '.allowFrom \| length'` returns `2`; both user IDs present in `allowFrom` array; `jq '.pending \| length'` returns `0` |

### 4.2 Telegram Message Delivery

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-4.11 [platform: linux,macos] | UC-3 (full E2E) | FR-ACD-6.2, FR-ACD-4.1, NFR-ACD-1, AC-ACD-5 | Mixed | Daemon running with valid bot token; authorized user; plugin bridge connected and subscribed to `telegram:<chat_id>` | 1. Record timestamp T1 2. Authorized user sends text message `"integration-test-message"` to bot 3. Observe plugin stdout for notification 4. Record timestamp T2 when notification received | (a) Plugin receives `notifications/claude/channel` with `content == "integration-test-message"` within 1 second (T2 - T1 ≤ 1000 ms); (b) `chat_messages` row persisted | Plugin stdout notification captured in `tc-4.11-notification.json`: `jq .params.content` returns `"integration-test-message"`; timestamp delta T2-T1 ≤ 1000 ms; `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages WHERE content='integration-test-message'"` returns 1 row |
| TC-4.7 [platform: linux,macos] | UC-3-E1 | FR-ACD-6.1, FR-ACD-1.6 | Mixed | Daemon running with Telegram; simulate 401 response from Telegram (mock teloxide to return 401) | 1. Trigger simulated 401 from Telegram long-poll | (a) Daemon does NOT crash; (b) `tg_bot_state` set to `"disconnected"`; (c) error logged at ERROR level | `claudebase daemon status --json \| jq .tg_bot_state` returns `"disconnected"`; daemon process still alive: `pgrep claudebase` exits 0; daemon logs contain `401 Unauthorized` and `tg_bot_state` reference |
| TC-4.8 [platform: linux,macos] | UC-3-E2 | FR-ACD-6.2 | Mixed | Daemon running; plugin connected; mock Telegram outbound API to return HTTP 429 with `retry_after: 2` | 1. Trigger `chat_reply` that would send Telegram message 2. Mock returns 429 on first attempt | (a) Daemon backs off and retries after `retry_after` seconds; (b) if retry fails, logs WARN; (c) tool response contains `{ "error": "telegram_rate_limited", "retry_after": 2 }`; (d) daemon stays alive | MCP `tools/call` response for `chat_reply` captured in `tc-4.8-rate-limited.json`: `jq .result.error` returns `"telegram_rate_limited"`; daemon logs contain `429` and `retry_after`; daemon alive: `pgrep claudebase` exits 0 |

### 4.3 Config Management (UC-7)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-4.12 [platform: linux,macos] | UC-7-B | FR-ACD-6.9 | CLI | Daemon running; `secrets.toml` contains bot token; `daemon.toml` exists | 1. Run `claudebase daemon config show --json` | Exit 0; JSON output contains `telegram` section with bot token masked as `"***"`; `NVIDIA_API_KEY` if present also masked | `echo $?` returns `0`; `claudebase daemon config show --json \| jq '.telegram.bot_token'` returns `"***"` (literal three asterisks, not the actual token) |
| TC-4.13 [platform: linux,macos] | UC-7-E1 | FR-ACD-12.3 | CLI | Daemon running; `$EDITOR` set to a script that writes invalid TOML | 1. Run `claudebase daemon config edit` 2. Editor writes malformed TOML to `daemon.toml` 3. Editor exits | `daemon config edit` exits non-zero; stderr contains `daemon.toml is invalid TOML at line`; daemon continues running with previous config | `echo $?` returns non-zero; stderr contains `invalid TOML`; `claudebase daemon status --json \| jq .state` still returns `"running"` (daemon not restarted) |

### 4.4 Security: Bot-Token File Permissions

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-4.14 [platform: linux,macos] | — | FR-ACD-6.4, NFR-ACD-7, AC-ACD-13 | Mixed | `secrets.toml` exists with permissions `0644` | 1. Run `claudebase daemon serve` | Exit 1; stderr contains `secrets.toml must have permissions 0600`; daemon does NOT start | `echo $?` returns `1`; stderr captured contains `0600`; `pgrep -f "daemon serve"` exits non-zero (no daemon process) |
| TC-4.15 [platform: linux,macos] | — | NFR-ACD-7 | FS | `secrets.toml` written by daemon install flow | 1. Check file permissions on `secrets.toml` | File mode is exactly `0600` (owner read+write only; no group or world bits) | `stat -c '%a' ~/.config/claudebase/secrets.toml` returns `600` (Linux); `stat -f '%A' ~/.config/claudebase/secrets.toml` returns `600` (macOS) |
| TC-4.16 [platform: linux,macos] | UC-3-E3 | FR-ACD-6.10 | Mixed | Daemon running. Bot token configured. Paired user has chat_id=12345. | 1. User sends TG text "first" — observe daemon processes, sqlite row count for thread `telegram:12345` = N. 2. `claudebase daemon stop`. 3. User sends TG text "second" during downtime. 4. `claudebase daemon start`. 5. Wait 5 seconds. | After daemon restart, message "second" appears in chat.db thread `telegram:12345` exactly once. Row count = N+2 (both "first" and "second"). `daemon_state.telegram.last_update_id` increments past the "second" message's update_id. | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages WHERE thread_id='telegram:12345' ORDER BY id DESC LIMIT 2"` → outputs ["second", "first"] in that order (newest first). `sqlite3 ... "SELECT value FROM daemon_state WHERE key='telegram.last_update_id'"` → strictly greater than the post-step-1 value. |

---

## 5. Slice 5 — Agent Registry + Connection Lifecycle

### 5.1 Schema Migration (DB)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-5.1-schema [platform: all] | — | FR-ACD-5.6, §17.7 schema v6 | DB | Slice 5 landed; daemon started | 1. Inspect `~/.claude/knowledge/chat.db` | `agent_registry` table exists with columns: `agent_id`, `agent_name`, `connection_id`, `chat_thread_id`, `permission_relayer`, `spawned_at`, `last_pinged_at`, `state`, `metadata`; partial index `agent_registry_thread_alive_idx` exists | `sqlite3 ~/.claude/knowledge/chat.db ".schema agent_registry"` shows all 9 columns; `sqlite3 ~/.claude/knowledge/chat.db ".indexes"` output contains `agent_registry_thread_alive_idx` |

### 5.2 Agent Registry Tools — Happy Path

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-5.2 [platform: linux,macos] | UC-5 (primary) | FR-ACD-5.1, FR-ACD-5.3 | Mixed | Daemon running; plugin connected | 1. Call `agent_register { "agent_id": "planner-abc123", "name": "planner", "thread": "telegram:99999", "metadata": {"task":"slice-planning"} }` via `tools/call` 2. Call `agent_list_alive {}` | (a) Register returns success; (b) `agent_list_alive` returns row with `agent_id = "planner-abc123"`, `name = "planner"`, `state = "alive"`; (c) DB row inserted | `tools/call` register response has no error; `tools/call` `agent_list_alive` response captured in `tc-5.2-list-alive.json`: `jq '.result.agents[] \| select(.agent_id=="planner-abc123") .state'` returns `"alive"`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'"` returns `alive` |
| TC-5.3 [platform: linux,macos] | UC-5 (primary) | FR-ACD-5.2, FR-ACD-5.3 | Mixed | Daemon running; `planner-abc123` registered (TC-5.2 state) | 1. Call `agent_unregister { "agent_id": "planner-abc123" }` 2. Call `agent_list_alive {}` | Row updated to `state = "dead"`; `agent_list_alive` does NOT include `planner-abc123` | `sqlite3 ~/.claude/knowledge/chat.db "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'"` returns `dead`; `jq '.result.agents[] \| select(.agent_id=="planner-abc123")' tc-5.2-list-alive.json` returns empty (filtered out) |
| TC-5.4 [platform: linux,macos] | — | FR-ACD-5.4 | Mixed | Daemon running; 3 agents registered at `last_pinged_at` > 120 s ago | 1. Call `agent_reap { "older_than": 60 }` (reap agents inactive > 60 s) | Returns count of reaped rows (≥ 3); those rows now have `state = "dead"` | MCP `tools/call` response `jq .result.reaped_count` returns integer ≥ 3; `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM agent_registry WHERE state='dead'"` matches reaped count |
| TC-5.5 [platform: linux,macos] | UC-5-E1, UC-8 | FR-ACD-2.5, FR-ACD-5.5 | Mixed | Daemon running; plugin connected with one registered agent `planner-abc123` | 1. Kill plugin process (SIGKILL) 2. Wait 1 s 3. Query DB | All `agent_registry` rows with the closed connection's `connection_id` updated to `state = "orphaned"` | `sqlite3 ~/.claude/knowledge/chat.db "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'"` returns `orphaned`; daemon logs contain `EOF` and `orphaned` |
| TC-5.6 [platform: linux,macos] | UC-5 (primary), AC-ACD-4 | FR-ACD-4.2, FR-ACD-5.5 | Mixed | Daemon running; one session disconnected (agents orphaned); messages posted to `telegram:99999` during gap | 1. Start new plugin 2. Call `chat_subscribe { "thread": "telegram:99999" }` | Backlog of messages posted during the disconnected period returned; `agent_list_alive {}` returns empty (prior agents all orphaned) | `chat_subscribe` response `jq '.result.messages \| length'` returns count > 0; `agent_list_alive` response `jq '.result.agents \| length'` returns `0`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM agent_registry WHERE state='alive'"` returns `0` |
| TC-5.7 [platform: linux,macos] | UC-5-EC2 | FR-ACD-5.1 | Mixed | Daemon running; `planner-abc123` already registered | 1. Call `agent_register { "agent_id": "planner-abc123", ... }` a second time | No error returned; row updated with new `spawned_at` and `last_pinged_at`; no duplicate rows | MCP response has no error; `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM agent_registry WHERE agent_id='planner-abc123'"` returns `1` (no duplicate) |
| TC-5.9 [platform: linux,macos] | UC-5-EC-3 | FR-ACD-5.7 | DB | Two concurrent claudebase plugin sessions both connected; first one calls `agent_register` for thread `telegram:99999` with name=planner agent_id=`a1`; daemon SQL inserts ok | 1. Session B calls `agent_register` for same thread, same name, agent_id=`b1`. 2. Read response from MCP. | Session B's `agent_register` tool call returns error containing literal `UNIQUE constraint failed`. `agent_registry` SQLite table has exactly 1 row for `(chat_thread_id='telegram:99999', agent_name='planner', state='alive')`. | `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM agent_registry WHERE chat_thread_id='telegram:99999' AND agent_name='planner' AND state='alive'"` → output literal `1`. Also: MCP tool-call response JSON path `.error.message` contains substring `UNIQUE constraint failed`. |

### 5.3 State Machine Integrity

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-5.8 [platform: linux,macos] | — | §17.7 `state CHECK` constraint | DB | `agent_registry` table exists | 1. Attempt direct SQLite insert with invalid state: `sqlite3 ~/.claude/knowledge/chat.db "INSERT INTO agent_registry (agent_id, agent_name, connection_id, spawned_at, last_pinged_at, state) VALUES ('x','x','x',0,0,'invalid')"` | Insert fails with SQLite constraint error; table is unchanged | `sqlite3 --json ~/.claude/knowledge/chat.db "..."` returns error containing `CHECK constraint failed`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT count(*) FROM agent_registry WHERE agent_id='x'"` returns `0` |

---

## 6. Slice 6 — ASR Backends

### 6.1 Whisper Backend — Happy Path (UC-4)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.1 [platform: linux,macos] | UC-4 (primary) | FR-ACD-7.1, FR-ACD-7.3, FR-ACD-7.6, NFR-ACD-2, AC-ACD-6, AC-ACD-15 | Mixed | Daemon running with `backend = "whisper"`; NO whisper model downloaded; authorized user; plugin subscribed | 1. Record timestamp T1 2. Authorized user sends 10-second test Ogg voice note to bot 3. Monitor daemon logs for model download and transcription 4. Record T2 when transcript notification received in plugin | (a) Model auto-downloaded to `~/.claude/tools/claudebase/models/whisper/ggml-medium.bin`; (b) transcript text appears in plugin `notifications/claude/channel` within 30 s of T1; (c) `chat_messages` row with `from_agent = "telegram:<user_id>"` and non-empty `content` (the transcript) | `ls -lh ~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` exits 0 (file ≥ 100 MB); daemon logs `journalctl --user -u claudebase -n 100` contain `download` and `ggml-medium.bin`; T2 - T1 ≤ 30 000 ms; plugin notification `tc-6.1-transcript.json`: `jq '.params.content'` returns non-empty string; Telegram screenshot `tc-6.1-telegram.png` showing voice note and bot transcript response |
| TC-6.2 [platform: linux,macos] | UC-4-A | FR-ACD-7.3 | Mixed | Daemon running with whisper backend; authorized user sends Russian voice note | 1. Send 10-second Russian-language Ogg voice note | Transcript returned in Russian (or bilingual depending on content); daemon does not error; single `chat_messages` row with Russian text | Plugin notification `tc-6.2-ru-transcript.json` contains Cyrillic characters in `jq '.params.content'`; `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` returns Cyrillic text |
| TC-6.3 [platform: linux,macos] | UC-7 (primary), AC-ACD-9 | FR-ACD-7.2, FR-ACD-12.3, AC-ACD-9 | CLI | Daemon running with `backend = "whisper"` | 1. Edit `daemon.toml` to set `backend = "nim"` 2. Run `claudebase daemon restart` 3. Run `claudebase daemon status --json` | After restart: `asr_backend` in status JSON returns `"nim"` | `claudebase daemon status --json \| jq .asr_backend` returns `"nim"`; `cat ~/.config/claudebase/daemon.toml \| grep backend` shows `backend = "nim"` |

### 6.2 NIM Backend (UC-4-B)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.4 [platform: linux,macos] | UC-4-B | FR-ACD-7.5, NFR-ACD-2 | Mixed | Daemon running with `backend = "nim"`; `NVIDIA_API_KEY` set in environment; authorized user | 1. Send 10-second Ogg voice note to bot 2. Monitor plugin for transcript notification | Transcript received within ~3 seconds; `asr_backend` remains `"nim"`; no whisper model downloaded | Plugin notification received within 5 s; `tc-6.4-nim-transcript.json`: `jq '.params.content'` non-empty; `ls ~/.claude/tools/claudebase/models/whisper/` shows no new file downloaded; daemon logs contain `nim` endpoint reference |
| TC-6.5 [platform: linux,macos] | UC-4-C | FR-ACD-7.4 | Mixed | Daemon running with `backend = "sherpa-nemo"`; ONNX files configured in `daemon.toml` and present | 1. Send 10-second Ogg voice note to bot | Transcript received within 5-10 s; `asr_backend` returns `"sherpa-nemo"` | Plugin notification `tc-6.5-sherpa-transcript.json`: `jq '.params.content'` non-empty; `claudebase daemon status --json \| jq .asr_backend` returns `"sherpa-nemo"` |

### 6.3 ASR Error Flows (UC-4-E1..E3)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.6 [platform: linux,macos] | UC-4-E1 | FR-ACD-7.3, NFR-ACD-10 | Mixed | Daemon with `backend = "whisper"`; whisper model file is corrupted (truncated via `truncate -s 100 ggml-medium.bin`) | 1. Send voice note to bot | (a) Daemon logs WARN about corrupted model; (b) daemon deletes corrupted file; (c) daemon re-downloads model; (d) transcription succeeds on retry; (e) daemon does NOT crash | Daemon logs contain `warn` and `corrupted` or `checksum`; `ls -lh ~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` shows full size (≥ 100 MB) after re-download; plugin receives transcript notification; `pgrep claudebase` exits 0 (daemon alive) |
| TC-6.7 [platform: linux,macos] | UC-4-E2 | FR-ACD-7.5, FR-ACD-7.7, NFR-ACD-10 | Mixed | Daemon with `backend = "nim"`; NIM endpoint mocked to return HTTP 500 | 1. Send voice note to bot | (a) Daemon logs WARN; (b) no retry to NIM; (c) `chat_messages` row inserted with `content = "[ASR error: NIM returned 500 Internal Server Error]"` (substring match); (d) daemon alive; (e) Telegram user sees error message from bot | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` contains substring `ASR error`; daemon logs contain `500` and `warn`; daemon alive: `pgrep claudebase` exits 0; Telegram screenshot `tc-6.7-asr-error.png` showing `[ASR error:` message from bot |
| TC-6.8 [platform: linux,macos] | UC-4-E3 | FR-ACD-7.6, FR-ACD-7.7, NFR-ACD-10 | Mixed | Daemon running; prepared zero-byte audio file (not valid Ogg) | 1. Send zero-byte audio file to bot (simulate via mock Telegram handler) | (a) `symphonia` decode error logged at WARN; (b) ASR NOT called; (c) `chat_messages` row with `content = "[ASR error: audio decode failed"` (substring); (d) daemon alive | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` contains `audio decode failed`; `pgrep claudebase` exits 0 |

### 6.4 ASR Edge Cases

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.9 [platform: linux,macos] | UC-4-EC1 | FR-ACD-7.3 | Mixed | Daemon with `backend = "whisper"` | 1. Send 6-minute voice note (> whisper single-pass window) | Transcription proceeds (whisper.cpp handles chunking internally); daemon does NOT timeout or crash; transcript row in `chat_messages` | `sqlite3 ~/.claude/knowledge/chat.db "SELECT length(content) FROM chat_messages ORDER BY created_at DESC LIMIT 1"` returns integer > 0 (non-empty transcript); daemon alive; no SIGKILL in logs |
| TC-6.10 [platform: linux,macos] | UC-4-EC2 | FR-ACD-7.6, FR-ACD-7.7 | Mixed | Daemon running with whisper backend | 1. Send audio file with unsupported codec (e.g., MP3 with wrong extension) | Same error flow as TC-6.8: WARN logged; `[ASR error: audio decode failed` placeholder in `chat_messages`; daemon alive | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` contains `audio decode failed`; `pgrep claudebase` exits 0 |
| TC-6.11 [platform: linux,macos] | UC-4-EC3 | FR-ACD-7.1 | Mixed | Daemon compiled without any ASR feature flag (`--no-default-features`) | 1. Send voice note to bot | `chat_messages` row inserted with `content = "[ASR disabled in this build]"` (substring); text messages continue to work normally | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` contains `ASR disabled in this build`; text message sent in same session appears as separate `chat_messages` row with normal content |

### 6.5 ASR Backend Switch (UC-7)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.12 [platform: linux,macos] | UC-7 (primary), AC-ACD-9 | FR-ACD-6.8, FR-ACD-7.2, AC-ACD-9 | CLI | Daemon running with `backend = "whisper"` | 1. Run `claudebase daemon config edit` (programmatically set `backend = "nim"` via `$EDITOR` script) 2. Run `claudebase daemon restart` 3. Run `claudebase daemon status --json` | `asr_backend` in status JSON returns `"nim"` after restart | `claudebase daemon status --json \| jq .asr_backend` returns literal `"nim"`; `claudebase daemon status --json \| jq .state` returns `"running"` |
| TC-6.13 [platform: linux,macos] | UC-7-A | FR-ACD-7.5, FR-ACD-7.8 | Mixed | Daemon running with `backend = "nim"`; `NVIDIA_API_KEY` NOT set in environment | 1. Send voice note to bot 2. Run `claudebase daemon doctor --asr` | (a) Voice note returns `[ASR error: NVIDIA_API_KEY not set]` placeholder; (b) `daemon doctor --asr` exits 1 with `nim backend: ERROR — NVIDIA_API_KEY environment variable is not set` | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` contains `NVIDIA_API_KEY not set`; `claudebase daemon doctor --asr` exit code `1`; stdout contains `NVIDIA_API_KEY` and `not set` |
| TC-6.14 [platform: linux,macos] | UC-7-B | FR-ACD-6.9 | CLI | Daemon running; `secrets.toml` has bot token | 1. Run `claudebase daemon config show --json` | Bot token masked as `"***"` in output | `claudebase daemon config show --json \| jq .telegram.bot_token` returns `"***"` (literal string `***`) |
| TC-6.15 [platform: linux,macos] | UC-7-E1 | FR-ACD-12.3 | CLI | Daemon running; `$EDITOR` set to script writing malformed TOML | 1. Run `claudebase daemon config edit` with malformed editor | Exit non-zero; error message includes line/column; daemon NOT restarted; running with previous config | `echo $?` non-zero; stderr contains `invalid TOML` and `line`; `claudebase daemon status --json \| jq .state` returns `"running"` |

### 6.6 Daemon Doctor

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-6.16 [platform: linux,macos] | UC-11 (primary) | FR-ACD-7.8, AC-ACD-6 | CLI | Daemon configured with `backend = "whisper"`; model file present and valid | 1. Run `claudebase daemon doctor --asr` | Exit 0; stdout contains `whisper — OK (model loaded successfully)` | `echo $?` returns `0`; stdout contains `OK` and `whisper`; no `MISSING` in stdout |
| TC-6.17 [platform: linux,macos] | UC-11 (primary) | FR-ACD-7.8 | CLI | Daemon configured with `backend = "whisper"`; model file ABSENT | 1. Run `claudebase daemon doctor --asr` | Exit 1; stdout contains `MISSING model file` and `claudebase daemon warmup --asr` suggestion | `echo $?` returns `1`; stdout contains `MISSING` and `warmup` |
| TC-6.18 [platform: linux,macos] | UC-11 (primary) | FR-ACD-7.8, FR-ACD-7.9 | CLI | Daemon configured with `backend = "whisper"`; model absent | 1. Run `claudebase daemon warmup --asr` | Model downloaded to `~/.claude/tools/claudebase/models/whisper/ggml-medium.bin`; exit 0 | `echo $?` returns `0`; `ls -lh ~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` shows file ≥ 100 MB; subsequent `claudebase daemon doctor --asr` exits 0 |

---

## 7. Slice 7 — Subagent Callback Routing

### 7.1 Subagent Routing — Happy Path (UC-5)

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-7.1 [platform: linux,macos] | UC-5 (primary) | FR-ACD-11.1, FR-ACD-11.2, FR-ACD-5.1, AC-ACD-10 | Mixed | Daemon running with Telegram; plugin connected; `planner-abc123` registered via `agent_register` | 1. Authorized user sends `@planner can you split slice 3 differently?` via Telegram | (a) Daemon detects `@planner` mention (case-insensitive); (b) daemon looks up alive `planner` agent in `agent_registry`; (c) notification payload includes `target_agent_id = "planner-abc123"`; (d) plugin forwards notification to Claude Code with `target_agent_id` field | Plugin stdout notification `tc-7.1-notification.json`: `jq '.params.target_agent_id'` returns `"planner-abc123"`; `jq '.params.content'` contains `@planner`; daemon logs contain `routing` and `planner-abc123` |
| TC-7.2 [platform: linux,macos] | UC-5 (primary), AC-ACD-10 | FR-ACD-11.3 | Mixed | TC-7.1 complete; planner receives `target_agent_id` notification | 1. Simulated Mira calls `chat_reply { "thread": "telegram:99999", "content": "Here is my suggestion...", "reply_to": "<message_id>" }` 2. Observe Telegram | Reply appears in user's Telegram chat; `chat_messages` row with planner's response persisted | `sqlite3 ~/.claude/knowledge/chat.db "SELECT content FROM chat_messages ORDER BY created_at DESC LIMIT 1"` returns `"Here is my suggestion..."`; Telegram screenshot `tc-7.2-telegram-reply.png` showing planner's response text in Telegram UI |
| TC-7.7 [platform: linux,macos] | UC-5-EC1 | FR-ACD-11.4 | Mixed | Daemon running; `planner-abc123` registered as `agent_name = "planner"` | 1. Send `@PLANNER split this slice` (all-caps mention) | Case-insensitive lookup finds `planner` agent; `target_agent_id` set to `planner-abc123` in notification | Plugin notification `tc-7.7-notification.json`: `jq '.params.target_agent_id'` returns `"planner-abc123"`; daemon logs contain case-insensitive match note |

### 7.2 Subagent Routing — Alternative Flows

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-7.3 [platform: linux,macos] | UC-5-A | FR-ACD-11.3 | Mixed | Daemon running; `planner-abc123` was alive but just orphaned (kill plugin before routing) | 1. Orphan `planner-abc123` (kill plugin) 2. New session starts 3. User sends `@planner message` 4. Mira receives notification with `target_agent_id` pointing to orphaned agent | Mira should fresh-spawn planner with backlog from `chat_list`; verified by second registration of planner agent under new connection | Plugin notification received with `target_agent_id`; new `agent_register` call for `planner` appears with new `spawned_at`; `sqlite3 chat.db "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'"` returns `orphaned`; `chat_list` response captured includes the unanswered message |
| TC-7.4 [platform: linux,macos] | UC-5-B | FR-ACD-11.1 | Mixed | Two alive `planner` agents registered: `planner-abc123` (`spawned_at = T1`) and `planner-def456` (`spawned_at = T2 > T1`) | 1. Send `@planner message` | Daemon routes to most recently spawned agent (`planner-def456`); notification has `target_agent_id = "planner-def456"` | Plugin notification `tc-7.4-notification.json`: `jq '.params.target_agent_id'` returns `"planner-def456"` (the newer one) |
| TC-7.5 [platform: linux,macos] | UC-5-C | FR-ACD-11.1, FR-ACD-11.2 | Mixed | No agent named `deepresearcher` registered in `agent_registry` | 1. Send `@deepresearcher run a search` | Notification broadcast WITHOUT `target_agent_id` field (null or absent); Mira receives plain message | Plugin notification `tc-7.5-notification.json`: `jq '.params.target_agent_id'` returns `null` or `jq '.params | has("target_agent_id")'` returns `false`; `chat_messages` row persisted normally |

### 7.3 Subagent Routing — Error Flow

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-7.6 [platform: linux,macos] | UC-5-E1 | FR-ACD-2.5, FR-ACD-5.5, AC-ACD-4 | Mixed | Daemon running; Mira's plugin connected; `planner-abc123` alive; user's `@planner` notification sent (step 8 in UC-5); Mira's plugin dies BEFORE sending `chat_reply` | 1. Kill Mira's plugin (SIGKILL after notification received) 2. Check DB and registry 3. Start new plugin 4. Call `chat_subscribe` | (a) Daemon detects EOF; (b) `planner-abc123` → `state = orphaned`; (c) unanswered `@planner` message in `chat_messages`; (d) new session's `chat_subscribe` returns the unanswered message as backlog | `sqlite3 ~/.claude/knowledge/chat.db "SELECT state FROM agent_registry WHERE agent_id='planner-abc123'"` returns `orphaned`; `chat_subscribe` response contains the unanswered message in `result.messages`; daemon alive: `pgrep claudebase` exits 0 |

---

## Visual Quality Test Cases

| TC-ID | Maps to UC | Maps to FR/NFR/AC | Verification Class | Preconditions | Steps | Expected Result | Evidence Required |
|-------|-----------|-------------------|--------------------|---------------|-------|-----------------|-------------------|
| TC-VQ-1 [platform: all] | UC-6 (pairing flow) | §17.8 UI, FR-ACD-6.5 | UI/UX | Daemon running; unknown Telegram user sends `/start` | 1. Send `/start` to bot from a phone's Telegram app 2. Screenshot bot response | Bot response shows inline keyboard with pairing code in a well-formatted message; no text overflow; `claudebase daemon access pair <code>` instruction is readable on mobile screen | Screenshot `tc-vq-1-pairing-ui.png` captured on Telegram mobile (iOS or Android): bot message visible, inline keyboard button text fully visible without truncation; no `...` cutting off the instruction text; pairing code is monospace or visually distinct |
| TC-VQ-2 [platform: all] | UC-4 (transcript delivery) | §17.8 UI, FR-ACD-7.7 | UI/UX | Daemon running; authorized user; whisper backend configured | 1. Send voice note to bot 2. On successful transcription: screenshot Telegram thread | Telegram shows (a) the voice note message; (b) bot's reply with transcript text; transcript text is readable, no character corruption; if `[Transcribing...]` status shown — it precedes the final transcript in the thread | Screenshot `tc-vq-2-transcript-delivery.png` showing Telegram thread with voice note and below it the bot's text reply with transcript content; no `\n` literal characters (actual line breaks rendered correctly); no HTML-encoded entities visible (e.g. no `&amp;` in the message) |
| TC-VQ-3 [platform: all] | UC-4-E2 (ASR error) | §17.8 UI, FR-ACD-7.7 | UI/UX | NIM backend mocked to return 500 | 1. Send voice note to bot 2. Screenshot bot's error reply | Bot sends `[ASR error: NIM returned 500 Internal Server Error]` message; message appears in the correct position in thread; no garbled text | Screenshot `tc-vq-3-asr-error-ui.png` showing bot's `[ASR error:` message in Telegram; error text is on a single readable bubble; no rendering issues |

---

## Cross-Platform Summary

| Platform | Applicable TCs | Notes |
|----------|---------------|-------|
| Linux | TC-1.1, TC-1.3–1.10, TC-2.1, TC-2.3–2.25, TC-3.x, TC-4.x, TC-5.x, TC-6.x, TC-7.x | Primary target; systemd user units |
| macOS | TC-1.1, TC-1.3–1.10, TC-2.2, TC-2.8–2.13, TC-2.15–2.25, TC-3.x, TC-4.x, TC-5.x, TC-6.x, TC-7.x | launchd plist; `stat -f` for permissions |
| Windows | TC-1.2, TC-2.6, TC-2.7 | Windows Service; named pipe; `sc.exe` |
| All | TC-2.17, TC-3.9, TC-3.10, TC-5.1-schema, TC-5.8, TC-VQ-1, TC-VQ-2, TC-VQ-3 | Platform-agnostic (DB, CLI introspection, visual) |
