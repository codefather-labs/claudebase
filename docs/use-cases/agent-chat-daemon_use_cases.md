# Use Cases: Agent Chat Daemon + Telegram Bridge + ASR Pipeline + Claude Code Plugin

> Based on [PRD §17](../PRD.md#17-agent-chat-daemon--telegram-bridge--asr-pipeline--claude-code-plugin) and [.claude/plan.md](../../.claude/plan.md)

---

## UC-1: First-Time Daemon Install

**Actor**: Human user (SDLC operator, logged in as a non-root OS user)
**Preconditions**:
- `claudebase` binary is installed and on PATH
- The user has NOT previously run `claudebase daemon install`
- No service unit files exist at the per-OS locations
- `~/.claude/plugins/claudebase/.mcp.json` does NOT exist

**Trigger**: User runs `claudebase daemon install --yes`

### Primary Flow (Happy Path)

1. The daemon installer detects the current OS (Linux/macOS/Windows).
2. On Linux: creates `~/.config/systemd/user/claudebase.service` with hardened unit directives (`ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths=%h/.claude %h/.config/claudebase`, `NoNewPrivileges=true`, `PrivateTmp=true`). `User=root` is absent.
3. On macOS: creates `~/Library/LaunchAgents/dev.codefather.claudebase.plist` with SIP-aware sandboxing.
4. On Windows: registers a Windows Service running as the current user (NOT `LocalSystem`).
5. Writes `~/.claude/plugins/claudebase/.mcp.json` with content `{ "command": "claudebase", "args": ["plugin", "serve"] }`.
6. On Linux: runs `systemctl --user daemon-reload` then `systemctl --user enable claudebase`.
7. On macOS: runs `launchctl load ~/Library/LaunchAgents/dev.codefather.claudebase.plist`.
8. On Windows: runs `sc create claudebase ...` with current user credentials.
9. Prints confirmation: `claudebase daemon installed. Run 'claudebase daemon start' to start it.`
10. Exits 0.

**Postconditions**:
- Per-OS service unit file exists at the expected path with correct permissions
- `~/.claude/plugins/claudebase/.mcp.json` exists and declares `"command": "claudebase", "args": ["plugin", "serve"]`
- Service is registered with the OS service manager (but NOT yet started — auto-start happens via `daemon start`)
- Running `claudebase daemon install --yes` a second time is a no-op (exits 0, prints "already installed")

**Data Requirements**:
- Input: none beyond CLI flags
- Output: service unit file on disk; `.mcp.json` on disk
- Side Effects: OS service manager registration

**FR Coverage**: FR-ACD-1.1, FR-ACD-8.1, FR-ACD-8.2, FR-ACD-8.3, FR-ACD-8.5, AC-ACD-1

### Alternative Flows

- **UC-1-A: Install with `--no-start`** — install registers the service but does NOT invoke `daemon start`. The service is enabled for auto-start at boot but is not started immediately. Prints: `Daemon installed. Auto-start at boot is configured. To start now: claudebase daemon start`. Exits 0.
- **UC-1-B: Re-run on already-installed system (idempotent)** — all service unit files already exist at the correct content checksum. Installer compares file contents, finds no diff, prints `claudebase daemon: already installed (no changes)`, exits 0. No service unit is overwritten; `.mcp.json` is preserved.
- **UC-1-C: Install with `CLAUDEBASE_INSTALL_DAEMON=1` via `install.sh`** — the post-install hook in `install.sh` invokes `claudebase daemon install --no-start` automatically. The user is not prompted. The service unit is written silently. No start is performed.

### Error Flows

- **UC-1-E1: Missing sudo on a system requiring elevation** — on macOS, the LaunchAgents path is user-writable and elevation is not needed; on Linux systemd user units likewise. However, on Windows, registering a service via `sc create` may require Administrator elevation depending on the user's privileges. If the call fails due to permissions, the daemon exits 1 with: `Error: Windows Service registration requires Administrator elevation. Run this command in an elevated terminal or set the service account manually.`
- **UC-1-E2: `.mcp.json` parent directory does not exist** — installer creates the directory `~/.claude/plugins/claudebase/` with mode `0700` before writing the file. If directory creation fails (e.g., permission denied), exits 1 with: `Error: could not create ~/.claude/plugins/claudebase/: <OS error>`.
- **UC-1-E3: `bot_token` in `secrets.toml` missing at install time** — install does NOT require the bot token to exist. The service is installed without a Telegram token. The daemon will start but `tg_bot_state` will be `"not-configured"`. Telegram functionality is gated by the presence of the token.

### Edge Cases

- **UC-1-EC1**: User runs install on a machine where `systemctl --user` is not available (WSL without systemd, or a container). Installer detects absence of systemd by probing `systemctl --user status 2>&1`. Falls back with: `Warning: systemd user units not supported in this environment. Service auto-start at boot is NOT configured. You can still run 'claudebase daemon serve' manually.`
- **UC-1-EC2**: User runs install as root. Service units with `User=root` are a security violation per FR-ACD-8.1. Installer detects `$UID == 0` and refuses: `Error: do not run 'daemon install' as root. Run as the user who will own the daemon.`

---

## UC-2: User Starts the Daemon

**Actor**: Human user
**Preconditions**:
- `claudebase daemon install` has been run successfully (UC-1)
- Daemon is currently NOT running
- `~/.config/claudebase/daemon.toml` exists (may not have a bot token configured yet)

**Trigger**: User runs `claudebase daemon start`

### Primary Flow (Happy Path)

1. `daemon start` calls the OS-native service activation mechanism:
   - Linux: `systemctl --user start claudebase`
   - macOS: `launchctl load -w ~/Library/LaunchAgents/dev.codefather.claudebase.plist`
   - Windows: `sc start claudebase`
2. The OS service manager launches `claudebase daemon serve` as a background process.
3. `daemon serve` acquires an exclusive `fslock` on `$XDG_RUNTIME_DIR/claudebase/daemon.pid`.
4. `daemon serve` creates the UDS socket at `$XDG_RUNTIME_DIR/claudebase/daemon.sock` (Unix) or `\\.\pipe\claudebase-daemon` (Windows) and begins accepting connections.
5. If a bot token exists in `secrets.toml` (file permissions are `0600`), spawns the Telegram long-polling task.
6. `daemon start` (the CLI command) waits up to 5 seconds for the socket file to appear, then exits 0 with: `claudebase daemon started (pid NNNN)`.
7. `claudebase daemon status --json` returns `{ "state": "running", "pid": NNNN, "uptime": 0, "socket_path": "/run/user/NNNN/claudebase/daemon.sock", "subscriber_count": 0, "tg_bot_state": "connected" | "disconnected", "asr_backend": "whisper" }`.

**Postconditions**:
- Daemon process is running as an OS background service
- UDS socket file exists and accepts connections
- `claudebase daemon status` returns `state: "running"`
- If bot token is present: Telegram long-poll is active (`tg_bot_state: "connected"`)

**Data Requirements**:
- Input: `daemon.toml` config, `secrets.toml` (if present)
- Output: PID file at `$XDG_RUNTIME_DIR/claudebase/daemon.pid`; socket at `daemon.sock`
- Side Effects: OS service manager records service as active

**FR Coverage**: FR-ACD-1.3, FR-ACD-1.8, FR-ACD-2.1, FR-ACD-9.1

### Alternative Flows

- **UC-2-A: Daemon start when already running (idempotent)** — `claudebase daemon start` is called while the daemon is already in `state: "running"`. The CLI detects the running PID via `daemon status` and exits 0 with: `claudebase daemon: already running (pid NNNN)`. No new process is launched. No error.
- **UC-2-B: Manual `daemon serve` invocation (developer mode)** — user runs `claudebase daemon serve` directly in a terminal (not via OS service manager). PID file is acquired. Daemon runs in the foreground; Ctrl-C triggers graceful shutdown per UC-8.

### Error Flows

- **UC-2-E1: Service unit file corrupted or manually modified** — `systemctl --user start claudebase` returns an error. The CLI surfaces the stderr from systemctl verbatim: `Error: service start failed: <systemctl error output>. Run 'claudebase daemon doctor' to diagnose or reinstall with 'claudebase daemon install --yes'.`
- **UC-2-E2: `secrets.toml` has wrong permissions** — daemon `serve` reads `secrets.toml`, finds permissions are not `0600`, refuses to continue the Telegram subsystem: `Error: ~/.config/claudebase/secrets.toml must have permissions 0600 (found 0644). Fix: chmod 0600 ~/.config/claudebase/secrets.toml`. The daemon still starts and the UDS socket is available, but `tg_bot_state` is `"not-configured"`. (AC-ACD-13)
- **UC-2-E3: Socket path does not exist** — `$XDG_RUNTIME_DIR` is not set or the directory does not exist. Daemon falls back to creating `~/.claude/run/claudebase/daemon.sock` with mode `0700`. Logs warning: `XDG_RUNTIME_DIR not set; using ~/.claude/run/claudebase/ for socket`.

### Edge Cases

- **UC-2-EC1**: Two users on the same machine both run `claudebase daemon start`. Because the daemon is a per-user service (per `$HOME`), each user gets a separate daemon process with separate UDS socket paths, separate PID files, and separate `chat.db` files. There is no conflict.
- **UC-2-EC2**: `daemon.toml` has an unknown value for `[asr] backend` (e.g., `backend = "deepgram"`). Daemon startup fails with: `Error: unknown ASR backend "deepgram" in daemon.toml [asr].backend. Allowed values: "whisper", "sherpa-nemo", "nim".` (FR-ACD-12.3)

---

## UC-3: User Sends Telegram Text → Mira Receives → Mira Responds

**Actor**: Human user (Telegram client); Telegram bot; Daemon; Plugin bridge; Mira (Claude Code orchestrator)
**Preconditions**:
- Daemon is running with `tg_bot_state: "connected"` (UC-2)
- User is in the authorized `allowFrom` list in `access.json` (UC-6)
- At least one Claude Code session is running with the plugin loaded and subscribed to thread `telegram:<chat_id>`
- `chat.db` exists and has the thread row for `telegram:<chat_id>`

**Trigger**: User types a text message and sends it to the Telegram bot

### Primary Flow (Happy Path)

1. Telegram delivers the message to the daemon's long-polling loop via `teloxide`.
2. Daemon checks `access.json`: user is authorized. Proceeds.
3. Daemon persists the message to `chat.db`: inserts into `chat_messages` with `thread_id = "telegram:<chat_id>"`, `from_agent = "telegram:<telegram_user_id>"`, `content = <message text>`.
4. Daemon broadcasts a `Notification` frame to all connections subscribed to `telegram:<chat_id>` within 10 ms (FR-ACD-4.6).
5. Plugin bridge receives the notification and forwards it to Claude Code as `notifications/claude/channel` with payload: `{ "thread": "telegram:<chat_id>", "from": "telegram:<user_id>", "content": "<message text>", "message_id": "<uuid>" }`.
6. Mira (in Claude Code) receives the channel event and processes the message.
7. Mira formulates a response and calls the `chat_reply` MCP tool with `{ "thread": "telegram:<chat_id>", "content": "<response>", "reply_to": "<message_id>" }`.
8. The plugin forwards the `chat_reply` tool call to the daemon over UDS.
9. Daemon inserts the reply into `chat_messages` and calls the Telegram bot API to send the reply to the user's chat.
10. The reply appears in the user's Telegram chat within 1 second of the original message send (NFR-ACD-1).

**Postconditions**:
- Two rows in `chat_messages`: original user message and Mira's reply, both with `thread_id = "telegram:<chat_id>"`
- `delivered_at` is populated for the original message
- Telegram user sees the response

**Data Requirements**:
- Input: Telegram message (text, user_id, chat_id)
- Output: Two `chat_messages` rows; Telegram API outbound call
- Side Effects: `chat.db` write; Telegram send

**FR Coverage**: FR-ACD-4.1, FR-ACD-4.3, FR-ACD-4.5, FR-ACD-6.1, FR-ACD-6.2, NFR-ACD-1, AC-ACD-5

### Alternative Flows

- **UC-3-A: Two concurrent Claude Code sessions both receive broadcast** — both session A and session B have subscribed to `telegram:<chat_id>`. Step 4 broadcasts to both connections. Both sessions see the `notifications/claude/channel` event. Both Mira instances have visibility; by convention only one (the active session) is expected to respond. Daemon does not deduplicate responses — if both sessions call `chat_reply`, two Telegram messages are sent. Orchestrator-level deduplication is a Mira responsibility. (AC-ACD-8, NFR-ACD-6)
- **UC-3-B: No Claude Code session is connected** — there are zero subscribers to the thread. The daemon persists the message to `chat.db` but has nobody to broadcast to. The message sits as undelivered in `chat_messages` (no `delivered_at`). When the next Claude Code session starts and calls `chat_subscribe { thread: "telegram:<chat_id>" }`, the tool returns the backlog of undelivered messages. (AC-ACD-4, FR-ACD-4.2)
- **UC-3-C: `reply_to` message id not found in `chat_messages`** — Mira calls `chat_reply` with a stale `reply_to` id (e.g., after DB purge). Daemon accepts the reply and persists it with `reply_to = NULL` (graceful degradation), logs warning. Telegram send proceeds with the reply text but without threading context.

### Error Flows

- **UC-3-E1: Telegram bot token revoked mid-conversation** — teloxide long-poll returns a 401 Unauthorized response. Daemon logs: `[ERROR] Telegram auth failure: 401 Unauthorized. Bot token may have been revoked. tg_bot_state set to "disconnected".` The daemon does NOT crash. Inbound messages from this point are not received. `claudebase daemon status --json` returns `tg_bot_state: "disconnected"`. User must update `secrets.toml` with a new token and run `claudebase daemon restart`. (AC-ACD-2)
- **UC-3-E2: Telegram rate-limit on outbound send (HTTP 429)** — daemon calls Telegram bot API to send reply, receives 429 with `retry_after` header. Daemon backs off per `retry_after` seconds and retries once. If the retry also fails, logs WARN and reports failure to Mira via tool response: `{ "error": "telegram_rate_limited", "retry_after": N }`. Does not crash.

### UC-3-E3 — Daemon restart preserves Telegram message backlog

**Actor:** Human user via Telegram.

**Preconditions:** Daemon running. User sends Telegram text message at time T0, daemon processes it (writes to chat.db, broadcasts). `daemon_state.telegram.last_update_id` is updated to N1 atomically with the batch.

**Main flow:**
1. User invokes `claudebase daemon restart` at time T1 (or daemon crashes — same effect).
2. While daemon is down (T1..T2), user sends another Telegram text message. Telegram retains this message; teloxide's long-poll is the only consumer that has not acknowledged it.
3. Daemon process restarts at T2.
4. On boot, Slice 4 worker reads `telegram.last_update_id` from `daemon_state` → returns N1.
5. Worker calls teloxide `Bot::get_updates(offset=N1+1)` — Telegram returns the message sent during the restart window with `update_id = N1+1`.
6. Worker processes the message normally: writes to chat.db thread `telegram:<id>`, broadcasts to subscribers, atomically updates `daemon_state.telegram.last_update_id = N1+1`.

**Postconditions:** The message sent during the restart window appears in `chat.db` with no duplicate and no loss. Connected Claude Code sessions receive the `notifications/claude/channel` event on reconnect.

**Maps to:** FR-ACD-6.10

### Edge Cases

- **UC-3-EC1**: Message content is an empty string (user sends a sticker or GIF with no caption). Daemon stores `content = ""` and broadcasts. Mira receives the event; the handling of stickers is a Mira-level concern. Daemon does not reject empty-content messages.
- **UC-3-EC2**: Two messages arrive from the same user within 50 ms (rapid typing). Both are persisted and broadcast independently in insertion order. No deduplication, no coalescing.

---

## UC-4: User Sends Telegram Voice Note → ASR → Mira Receives Transcript

**Actor**: Human user (Telegram client); Telegram bot; Daemon; ASR backend; Plugin bridge; Mira
**Preconditions**:
- Daemon is running with `tg_bot_state: "connected"`
- User is authorized (UC-6)
- The ASR backend is configured and operational (whisper model downloaded, or NIM key set)
- At least one Claude Code session subscribed to `telegram:<chat_id>`

**Trigger**: User records a voice note in Telegram and sends it to the bot

### Primary Flow (Happy Path — whisper backend)

1. Telegram delivers the voice note to the daemon's long-poll handler as an Ogg/Opus file reference.
2. Daemon downloads the Ogg file bytes from Telegram's servers via HTTPS.
3. Daemon passes bytes to `symphonia` audio decoder. Decoder converts Opus-in-Ogg → 16 kHz mono PCM `Vec<f32>`.
4. Daemon calls the active `Asr` backend: `whisper.transcribe(pcm, 16_000)` per the architect-resolved trait signature `async fn transcribe(&self, pcm: Vec<f32>, sample_rate: u32) -> Result<String>` (architect [STRUCTURAL] #5).
5. For the `whisper` backend: if `ggml-medium.bin` is absent, auto-downloads from HuggingFace (`https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin`) to `~/.claude/tools/claudebase/models/whisper/`. Download progress is logged at INFO level. After download, loads model and transcribes.
6. Whisper returns the transcript string (e.g., `"Can you please summarize the last three slices?"`).
7. Daemon treats the transcript exactly as an inbound text message (UC-3 steps 3-10): persists to `chat.db`, broadcasts to subscribers, delivers to Mira.
8. Full roundtrip (voice note send → transcript appears in Mira's session) completes within 30 seconds for a 10-second clip (NFR-ACD-2, AC-ACD-6).

**Postconditions**:
- `chat_messages` row with `content = "<transcript>"`, `from_agent = "telegram:<user_id>"`
- Mira receives `notifications/claude/channel` with the transcript text
- User can optionally see a `[Transcribing...]` status message in Telegram (optional UX — not mandated by PRD)

**Data Requirements**:
- Input: Ogg/Opus audio bytes from Telegram; configured ASR backend + model
- Output: transcript text in `chat_messages`; ASR model auto-downloaded if absent
- Side Effects: model file created at `~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` (on first use)

**FR Coverage**: FR-ACD-6.3, FR-ACD-7.1, FR-ACD-7.2, FR-ACD-7.3, FR-ACD-7.6, NFR-ACD-2, AC-ACD-6, AC-ACD-15

### Alternative Flows

- **UC-4-A: Voice note in a language different from configured default** — whisper-rs (`medium` model) supports multilingual transcription by default. No configuration change is needed. The transcript arrives in the language spoken. Sherpa-nemo with Parakeet multilingual ONNX likewise supports multiple languages. NIM Parakeet 1.1b is multilingual by design. All three backends handle this transparently.
- **UC-4-B: NIM backend selected** — steps 4-6 differ: daemon POST `audio/transcriptions` to `https://integrate.api.nvidia.com/v1/audio/transcriptions` with `Authorization: Bearer <NVIDIA_API_KEY>`. Transcription completes in ~1-3 seconds for a 10-second clip (no model loading). Step 5 (auto-download) is skipped. All subsequent steps identical.
- **UC-4-C: sherpa-nemo backend selected** — steps 4-6 use the sherpa-onnx binding with user-provided encoder/decoder ONNX paths from `daemon.toml`. Transcription completes in ~5-10 seconds for a 10-second clip. Streaming-capable model may return partial transcripts (implementation detail).

### Error Flows

- **UC-4-E1: Whisper model file corrupted** — `whisper-rs` returns an error during model load (checksum mismatch or truncated file). Daemon logs WARN, deletes the corrupted model file, re-downloads from HuggingFace, and retries transcription once. If the retry succeeds, flow continues normally. If the re-download also fails (network error), proceeds to UC-4-E2.
- **UC-4-E2: NIM backend returns HTTP 5xx** — the HTTPS POST to NIM fails with a 5xx status. Daemon does not retry (to avoid amplifying server-side issues). Logs the error at WARN. Inserts a placeholder into `chat_messages`: `content = "[ASR error: NIM returned 500 Internal Server Error]"`, broadcasts to subscribers. User sees the error message in the Telegram thread from the bot. Daemon continues running. (FR-ACD-7.7)
- **UC-4-E3: symphonia decoder fails on audio format** — the Ogg file from Telegram is unusable (zero-byte, incomplete download, wrong codec). `symphonia` returns decode error. Daemon logs WARN, does not call ASR. Posts: `[ASR error: audio decode failed — could not parse Ogg/Opus stream]` to `chat_messages` and Telegram. (FR-ACD-7.7)

### Edge Cases

- **UC-4-EC1**: Voice note is longer than 5 minutes (Telegram's documented maximum is up to 20 minutes for voice notes as of 2024, but the daemon places no explicit cap — this is a Telegram-side constraint). Audio files beyond the whisper model's context window (~30 seconds per pass for some models) may produce truncated transcripts. Daemon does not split or chunk audio; transcription proceeds for the full clip and whisper.cpp handles chunking internally.
- **UC-4-EC2**: Audio file is not Ogg/Opus (unsupported codec delivered by Telegram). symphonia attempts decode, fails. Same error flow as UC-4-E3.
- **UC-4-EC3**: Daemon compiled without any ASR feature (`--no-default-features`). Voice notes reach the handler but the `Asr` dispatch returns `Err("ASR disabled in this build")`. Daemon posts `[ASR disabled in this build]` to `chat_messages` and Telegram. Text messages continue to work normally. (UC-EC-4 coverage)

---

## UC-5: Mira Registers a Subagent; User Replies to That Subagent in Telegram

**Actor**: Mira (orchestrator); Subagent (e.g., `planner`); Daemon; Telegram user
**Preconditions**:
- Daemon is running with at least one Claude Code session (Mira) connected
- Mira has spawned a subagent (e.g., `planner`) via Claude Code's `Task` tool
- The subagent is alive within the Claude Code session

**Trigger**: Mira's orchestration code calls the `agent_register` MCP tool for the spawned subagent, then the user sends `@planner <message>` in Telegram

### Primary Flow (Happy Path)

1. Mira calls `agent_register { agent_id: "planner-abc123", name: "planner", thread: "telegram:<chat_id>", metadata: { "task": "slice-planning" } }`.
2. Daemon inserts into `agent_registry`: `agent_id = "planner-abc123"`, `agent_name = "planner"`, `connection_id = <Mira's connection_id>`, `chat_thread_id = "telegram:<chat_id>"`, `state = "alive"`, `spawned_at = now()`.
3. User sends `@planner can you split slice 3 differently?` in Telegram.
4. Daemon's Telegram handler receives the message. Detects `@planner` prefix via case-insensitive match (FR-ACD-11.4).
5. Daemon queries `agent_registry` for alive agents with `agent_name = "planner"` and `chat_thread_id = "telegram:<chat_id>"`. Finds `agent_id = "planner-abc123"`.
6. Daemon sets `target_agent_id = "planner-abc123"` in the notification payload.
7. Persists the message to `chat_messages`, broadcasts to subscribed connections with extended payload: `{ "thread": "telegram:<chat_id>", "from": "telegram:<user_id>", "content": "@planner can you split slice 3 differently?", "message_id": "<uuid>", "target_agent_id": "planner-abc123" }`.
8. Plugin forwards the notification to Mira as `notifications/claude/channel`.
9. Mira sees `target_agent_id = "planner-abc123"` in the event and calls `SendMessage(to="planner-abc123", content="@planner can you split slice 3 differently?")`.
10. The planner subagent receives the message, processes it, formulates a reply.
11. Planner (or Mira on planner's behalf) calls `chat_reply { thread: "telegram:<chat_id>", content: "<planner's response>", reply_to: "<message_id>" }`.
12. Daemon persists the reply, sends it via Telegram bot to the user.
13. User sees planner's reply in Telegram.

**Postconditions**:
- `agent_registry` row exists for `planner-abc123` with `state = "alive"`
- Two `chat_messages` rows: user's `@planner` message and planner's reply
- End-to-end roundtrip visible in both trace logs and Telegram UI (AC-ACD-10)

**Data Requirements**:
- Input: `agent_register` tool call parameters; Telegram `@mention` message
- Output: `agent_registry` row; two `chat_messages` rows; Telegram reply
- Side Effects: `agent_registry` updated; `chat.db` written; Telegram API called

**FR Coverage**: FR-ACD-5.1, FR-ACD-11.1, FR-ACD-11.2, FR-ACD-11.3, FR-ACD-11.4, AC-ACD-10

### Alternative Flows

- **UC-5-A: `target_agent_id` resolves to an orphaned agent** — the planner agent was alive when the user's message arrived, but between routing (step 5) and Mira calling `SendMessage` (step 9), the planner has died or been orphaned. Mira's `SendMessage` fails. Per FR-ACD-11.3, Mira MUST fresh-spawn the agent named `planner` and provide it the backlog from `chat_list { thread: "telegram:<chat_id>", since: <last-message-id> }` as onboarding context. The freshly spawned planner processes the user's message and replies. From the user's perspective in Telegram, the reply arrives with slightly higher latency.
- **UC-5-B: Multiple alive agents with the same name** — if two `planner` subagents are alive in `agent_registry` for the same thread (unlikely but possible with parallel sessions), the daemon picks the most recently spawned one (`MAX(spawned_at)`) as the routing target. The unchosen agent is not notified.
- **UC-5-C: `@mention` with no matching alive agent** — `@deepresearcher` is mentioned but no agent named `deepresearcher` is registered. Daemon broadcasts the notification WITHOUT `target_agent_id`. Mira receives the event as a plain message and decides how to handle it (e.g., reply "I don't have a deepresearcher agent running").

### Error Flows

- **UC-5-E1: Claude Code dies mid-SendMessage** — Mira's process exits after step 8 (notification received) but before step 9 (SendMessage). The plugin's UDS connection drops. Daemon detects EOF on that connection, bulk-updates all `agent_registry` rows for `connection_id = <Mira's connection_id>` to `state = "orphaned"`, including `planner-abc123`. The user's message is in `chat_messages` with no reply. When the next Claude Code session starts and calls `chat_subscribe`, the backlog is delivered and the new Mira session can process the unanswered message. (FR-ACD-2.5, FR-ACD-5.5)

### Edge Cases

- **UC-5-EC1**: User sends `@PLANNER` in all caps. Case-insensitive lookup finds `planner` agent. Routing proceeds. (FR-ACD-11.4)
- **UC-5-EC2**: Mira calls `agent_register` for the same `agent_id` twice (idempotent re-registration). Daemon performs `INSERT OR REPLACE` semantics; the row is updated with the new `spawned_at` and `last_pinged_at`. No error returned.

### UC-5-EC-3 — Concurrent same-name agent registration

**Actor:** Two Claude Code sessions running on the same machine, each spawning a `planner` agent for the same Telegram thread `telegram:12345`.

**Preconditions:** Daemon running. Session A spawned `planner` agent with `agent_id=A1`, called `agent_register {agent_id: 'A1', name: 'planner', chat_thread_id: 'telegram:12345', state: 'alive'}` — succeeded.

**Main flow:**
1. Session B independently spawns its own `planner` agent with `agent_id=B1`.
2. Session B's Mira calls `agent_register {agent_id: 'B1', name: 'planner', chat_thread_id: 'telegram:12345', state: 'alive'}`.
3. Daemon enforces the partial UNIQUE INDEX `agent_registry_thread_name_alive_idx` and rejects the insert with error `UNIQUE constraint failed: agent_registry.chat_thread_id, agent_registry.agent_name`.
4. Session B's plugin propagates the error back to Mira as a `tools/call` error response.
5. Mira logs the conflict and treats B1 as un-registered; subsequent `@planner` mentions in the Telegram thread route to A1 (the alive registered planner). B1 is reachable only via direct SendMessage from within Session B.

**Postconditions:** Exactly one `planner` row in `agent_registry` for thread `telegram:12345` with state='alive'. Routing for `@planner` is deterministic.

**Maps to:** FR-ACD-5.7

---

## UC-6: User Pairs with the Bot (Permission / Pairing Flow)

**Actor**: Human user (Telegram); Telegram bot; Daemon; Human user (terminal)
**Preconditions**:
- Daemon is running with `tg_bot_state: "connected"`
- `daemon.toml` has `[permissions] dmPolicy = "pairing"` (the default)
- The user's Telegram ID is NOT in `access.json` `allowFrom` list

**Trigger**: User sends any message (typically `/start`) to the Telegram bot for the first time

### Primary Flow (Happy Path)

1. Daemon receives the message from an unknown Telegram user.
2. Daemon checks `access.json`: `dmPolicy = "pairing"`, user not in `allowFrom`. Does not deliver the message to `chat.db` yet.
3. Daemon generates a random alphanumeric pairing code (e.g., `X7K2Q9`) and stores it in `pending` map in `access.json` with `{ telegram_user_id, username, code, expires_at: now() + 3600 }`.
4. Daemon sends the pairing code to the user via Telegram inline keyboard: `"To authorize, run in your terminal: claudebase daemon access pair X7K2Q9"`.
5. User runs `claudebase daemon access pair X7K2Q9` in their terminal.
6. The `access pair` CLI command connects to the daemon UDS and submits the code.
7. Daemon validates the code: found in `pending`, not expired.
8. Daemon adds the user to `access.json` `allowFrom` list with `{ telegram_user_id, username, authorized_at }` and removes from `pending`.
9. Daemon sends a Telegram confirmation message: `"You are now authorized. Send me a message to get started."`
10. Subsequent messages from this user bypass the pairing check and go through the normal UC-3 flow.

**Postconditions**:
- User's Telegram ID is in `access.json` `allowFrom`
- `pending` entry removed from `access.json`
- Daemon sends confirmation via Telegram

**Data Requirements**:
- Input: Telegram user_id, username; terminal command with code
- Output: `access.json` updated
- Side Effects: Telegram outbound confirm message

**FR Coverage**: FR-ACD-6.5, FR-ACD-6.6, FR-ACD-6.7, AC-ACD-13

### Alternative Flows

- **UC-6-A: `dmPolicy = "allowlist"`** — only users explicitly pre-added to `allowFrom` may interact. Unknown users receive no pairing code; their messages are silently discarded and an audit log entry is made.
- **UC-6-B: `dmPolicy = "disabled"`** — access control is off; all messages from any Telegram user are accepted without pairing. Intended for single-user private bots only.

### Error Flows

- **UC-6-E1: Pairing code expired (1-hour window)** — user attempts `access pair X7K2Q9` more than 1 hour after the code was issued. Daemon rejects with exit 1: `Error: pairing code X7K2Q9 has expired. Ask the bot for a new code by sending any message.` The bot also sends a Telegram message to the user: `"Your pairing code has expired. Send me a message to receive a new one."` (AC-ACD-13 coverage)
- **UC-6-E2: Pairing code attempted by wrong user (different terminal user)** — the code was issued for Telegram user Alice (user_id 12345) but the `access pair` command is run on a terminal logged in as OS user Bob. Daemon cannot distinguish OS users from Telegram users at the pairing step — the code is user-scoped only by Telegram user_id. The CLI succeeds for whichever OS user runs it first. This is intentional: the code is a shared secret. Security relies on code length (entropy) and 1-hour expiry. An audit log entry records the authorization event.
  - **Alternative: wrong code entirely** — if an unknown code is submitted, daemon exits 1: `Error: unknown pairing code. Codes are case-sensitive and expire after 1 hour.`

### Edge Cases

- **UC-6-EC1**: User sends a second message before pairing. Daemon resends the same pairing code (does not generate a new one while the prior is still pending and unexpired).
- **UC-6-EC2**: Two different Telegram users both receive pairing codes at the same time. Both codes coexist in `pending`. The first `access pair` call honors its matching code; the second does the same. No collision.

---

## UC-7: User Switches ASR Backend via Config Edit

**Actor**: Human user
**Preconditions**:
- Daemon is running
- User has access to `$EDITOR` in their terminal
- Current backend is `whisper` in `daemon.toml`
- The target backend is available (for `nim`: `NVIDIA_API_KEY` is set; for `sherpa-nemo`: ONNX model files exist)

**Trigger**: User runs `claudebase daemon config edit`

### Primary Flow (Happy Path — switch whisper → nim)

1. `daemon config edit` opens `~/.config/claudebase/daemon.toml` in `$EDITOR` (default `vi`). (FR-ACD-6.8)
2. User changes `[asr] backend = "whisper"` to `[asr] backend = "nim"`.
3. User saves and exits the editor.
4. `daemon config edit` exits 0.
5. User runs `claudebase daemon restart`.
6. Daemon reads the updated `daemon.toml`, validates `backend = "nim"`, reads `NVIDIA_API_KEY` from environment.
7. Next voice note goes through the `nim` backend path (UC-4-B).
8. `claudebase daemon status --json` returns `asr_backend: "nim"`. (AC-ACD-9)

**Postconditions**:
- `daemon.toml` updated on disk
- Active ASR backend is `nim` after restart
- `daemon status` reflects the new backend

**Data Requirements**:
- Input: edited `daemon.toml`
- Output: no DB changes; daemon process restarts with new config
- Side Effects: prior `whisper` model remains on disk (not deleted)

**FR Coverage**: FR-ACD-6.8, FR-ACD-6.9, FR-ACD-7.2, FR-ACD-12.3, AC-ACD-9

### Alternative Flows

- **UC-7-A: Switch whisper → nim; `NVIDIA_API_KEY` not set** — after daemon restart with `backend = "nim"`, the daemon starts successfully. The first voice note triggers a call to the NIM backend; `reqwest` cannot read `NVIDIA_API_KEY` (empty string or env var absent). The daemon logs WARN and posts `[ASR error: NVIDIA_API_KEY not set]` to `chat_messages`. Running `claudebase daemon doctor --asr` reports: `nim backend: ERROR — NVIDIA_API_KEY environment variable is not set`. (FR-ACD-7.8, UC-EC coverage)
- **UC-7-B: `config show` before editing** — user runs `claudebase daemon config show --json` to inspect the current config. The bot token appears as `"***"` in the output. `NVIDIA_API_KEY` (if resolved) also appears as `"***"`. (FR-ACD-6.9)

### Error Flows

- **UC-7-E1: Editor saves invalid TOML** — after the editor exits, `daemon config edit` re-parses the TOML. Finds a syntax error (e.g., missing closing bracket). Prints: `Error: daemon.toml is invalid TOML at line 12, column 5: expected closing bracket ']'. The file has been saved but the daemon was NOT restarted. Fix the syntax and run 'claudebase daemon restart' when ready.` Daemon continues running with the PREVIOUS config. (FR-ACD-12.3 coverage: daemon refuses restart on bad config)

---

## UC-8: User Stops and Restarts the Daemon

**Actor**: Human user
**Preconditions**: Daemon is running (UC-2)

**Trigger**: User runs `claudebase daemon stop` followed by `claudebase daemon start`

### Primary Flow (Happy Path)

1. `claudebase daemon stop` calls the OS-native stop:
   - Linux: `systemctl --user stop claudebase`
   - macOS: `launchctl unload ~/Library/LaunchAgents/dev.codefather.claudebase.plist`
   - Windows: `sc stop claudebase`
2. OS sends SIGTERM (or Windows stop event) to the `daemon serve` process.
3. `daemon serve` receives SIGTERM. Begins graceful shutdown:
   a. Stops accepting new UDS connections.
   b. Closes all open connections, sending a `Notification { type: "daemon_shutdown" }` frame to each connected plugin.
   c. Each plugin receives the shutdown notification and transitions to daemon-down mode (UC-EC-1 pattern).
   d. Daemon removes the PID file and unlinks the UDS socket.
   e. Process exits 0.
4. `claudebase daemon stop` exits 0 after the process is gone (polls OS service state).
5. `claudebase daemon status --json` returns `{ "state": "stopped" }`.
6. Any messages arriving via Telegram during the stopped window are NOT received (Telegram long-poll is down).
7. User runs `claudebase daemon start`. Daemon resumes per UC-2.

**Postconditions**:
- PID file and socket file are removed after stop
- All plugin connections transitioned to daemon-down mode during stop
- After restart: UDS socket exists again; new plugin connections are accepted

**FR Coverage**: FR-ACD-1.4, FR-ACD-1.5, FR-ACD-9.3

### Alternative Flows

- **UC-8-A: `daemon restart` shorthand** — `claudebase daemon restart` executes stop then start atomically. Downtime window is approximately the time for the OS to kill the process and start a new one (typically < 2 seconds).
- **UC-8-B: Daemon already stopped** — `claudebase daemon stop` when daemon is not running. CLI checks `daemon status` first, finds `state: "stopped"`, exits 0 with `claudebase daemon: already stopped`. No error.

### Error Flows

- **UC-8-E1: SIGTERM ignored or process hangs** — daemon does not exit within 10 seconds of SIGTERM. On Linux, `systemctl --user stop` sends SIGKILL after `TimeoutStopSec` (default 90s; set to 10s in the unit). The process is forcibly killed. Socket file may need manual cleanup. Logged at WARN in `daemon doctor` output.

---

## UC-9: User Uninstalls the Daemon

**Actor**: Human user
**Preconditions**: Daemon is installed (UC-1)

**Trigger**: User runs `claudebase daemon uninstall [--keep-data]`

### Primary Flow (Happy Path — keep data)

1. `daemon uninstall --keep-data` stops the daemon if running (UC-8 flow).
2. Removes the service unit file from the OS service manager:
   - Linux: `systemctl --user disable --now claudebase`; removes `~/.config/systemd/user/claudebase.service`
   - macOS: `launchctl unload ...`; removes plist file
   - Windows: `sc delete claudebase`
3. Removes `~/.claude/plugins/claudebase/.mcp.json`.
4. Preserves: `chat.db`, `insights.db`, `secrets.toml`, `daemon.toml`, `access.json`.
5. Prints: `claudebase daemon uninstalled. Data files preserved in ~/.config/claudebase/ and ~/.claude/knowledge/. To reinstall: claudebase daemon install.`

**Postconditions**:
- No service unit in OS service manager
- `.mcp.json` removed (new Claude Code sessions will NOT load the plugin)
- `chat.db` and config files preserved if `--keep-data` passed

**FR Coverage**: FR-ACD-1.2

### Alternative Flows

- **UC-9-A: Uninstall without `--keep-data`** — additionally removes `chat.db`, `insights.db`, `secrets.toml`, `daemon.toml`, `access.json`. All chat history is lost. Daemon logs a warning before deletion: `Removing chat.db: this will permanently delete all chat history.`

---

## UC-10: User Reads Daemon Logs

**Actor**: Human user
**Preconditions**: Daemon is installed and has been run at least once (log entries exist)

**Trigger**: User runs `claudebase daemon logs --follow`

### Primary Flow (Happy Path)

1. On Linux: `claudebase daemon logs --follow` exec's `journalctl --user -u claudebase -f`.
2. On macOS: exec's `log stream --predicate 'process == "claudebase"'`.
3. On Windows: runs `Get-WinEvent -LogName Application -Source claudebase -MaxEvents 200 | Format-List` (streaming not natively available; polls every 2 seconds).
4. Log lines stream to the terminal in real time.
5. User presses Ctrl-C to stop following.
6. `claudebase daemon logs --lines 50` (without `--follow`) prints the last 50 log lines and exits.

**FR Coverage**: FR-ACD-1.7

---

## UC-11: User Runs Daemon Doctor

**Actor**: Human user
**Preconditions**: Daemon may be running or stopped

**Trigger**: User runs `claudebase daemon doctor --asr`

### Primary Flow (Happy Path — whisper backend healthy)

1. `daemon doctor --asr` reads `daemon.toml` to determine the configured backend (`whisper`).
2. Checks that `~/.claude/tools/claudebase/models/whisper/ggml-medium.bin` exists and is non-zero bytes.
3. Attempts to initialize the whisper-rs context from the model file (dry-run: no audio provided).
4. If initialization succeeds: prints `ASR backend: whisper — OK (model loaded successfully)` and exits 0.
5. If model absent: prints `ASR backend: whisper — MISSING model file. Run 'claudebase daemon warmup --asr' to download.` and exits 1.
6. For `nim` backend: makes an HTTP HEAD probe to `https://integrate.api.nvidia.com/v1/audio/transcriptions` with a known-bad authorization (just checking reachability, not a valid auth). If HTTPS handshake succeeds: `nim endpoint: reachable — OK`. If connection refused or timeout: `nim endpoint: UNREACHABLE — check network or NIM service status` and exits 1.
7. For `sherpa-nemo` backend: checks that all three config-pointed ONNX files (`encoder_onnx`, `decoder_onnx`, `tokens`) exist on disk. Prints per-file status. Exits 1 if any are missing.

**Postconditions**: No state changes. Report printed. Exit code reflects health.

**FR Coverage**: FR-ACD-7.8, FR-ACD-7.9

---

## Facts

### Verified facts

- Plan source: `.claude/plan.md` (374 lines) read in full this session — 7 slices (Waves 1–6), acceptance criteria (lines 64–76), files affected, risks and dependencies. Source: `.claude/plan.md` lines 1–374 — salience: high.
- PRD §17 source: `docs/PRD.md` lines 407–666, read in full this session — FR-ACD-1 through FR-ACD-13, NFR-ACD-1 through NFR-ACD-12, AC-ACD-1 through AC-ACD-15, schema definitions for `chat.db` (v5) and `agent_registry` (v6). Source: `docs/PRD.md` lines 407–666 — salience: high.
- Existing use-case file in scope: `docs/use-cases/agent-insights-base_use_cases.md` — confirmed via `ls` this session. This file covers the insights corpus domain. The `agent-chat-daemon` feature is a new domain with no existing coverage — CREATE (not UPDATE) is correct. — salience: medium.
- Knowledge base status (claudebase project): `doc_count: 0, chunk_count: 0` — the index exists but is empty. No domain documents to query. Corpus scope relevance: No overlap (empty corpus). Topical queries skipped. — salience: low.
- Prior prd-writer insight (OQ-ACD-4): `chat.db` location ambiguity — the plan places `chat.db` at `<project>/.claude/knowledge/chat.db` (project-scoped) but the daemon is a user-level OS service (`$HOME`-scoped). Architecturally, a user-level service writing to a project-specific path is inconsistent. PRD §17.7 says `<project>/.claude/knowledge/chat.db` per the plan, but this may be resolved to `~/.claude/knowledge/chat.db` by the architect at Slice 3. Source: `claudebase insight get 1` this session — salience: high.
- `daemon status --json` field names verified against PRD §17.3 FR-ACD-1.6 this session: `state` (enum: `"running"`, `"stopped"`, `"not-installed"`), `pid` (integer|null), `uptime` (seconds|null), `socket_path` (string|null), `subscriber_count` (integer), `tg_bot_state` (`"connected"`, `"disconnected"`, `"not-configured"`), `asr_backend` (`"whisper"`, `"sherpa-nemo"`, `"nim"`, `"none"`). — salience: high.
- `agent_registry` schema: SQL block verified against PRD §17.7 (lines 611–628) and plan Slice 5 (lines 169–183). `state` CHECK constraint: `('alive','orphaned','dead')`. — salience: high.
- OQ-ACD-4 RESOLVED — `chat.db` lives at `~/.claude/knowledge/chat.db` (user-level singleton; not per-project). Resolution: architect [STRUCTURAL] #1 at bootstrap Step 3. PRD §17.7 comment block (`docs/PRD.md` line 586) and migration code use `user_level_chat_db_path()`. All UC steps referencing `chat.db` (UC-3, UC-4, UC-5, UC-8, UC-9, UC-10, UC-3-E3) point to this user-level path. — salience: high.
- UDS socket path: `$XDG_RUNTIME_DIR/claudebase/daemon.sock` (Unix) / `\\.\pipe\claudebase-daemon` (Windows). Source: PRD FR-ACD-2.1 (line 443) — salience: high.
- Whisper auto-download URL: `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-<size>.bin`. Source: PRD FR-ACD-7.3 (line 493) — salience: medium.
- NVIDIA NIM assumed endpoint: `https://integrate.api.nvidia.com/v1/audio/transcriptions`. Source: plan line 230 + PRD FR-ACD-7.5 (line 495). Marked as assumed in plan risk 3 (line 288). — salience: high (marked assumption).
- Corpus scope relevance: No overlap; observed corpus domain: empty; task domain: Rust daemon, Telegram, ASR, MCP plugin. — salience: low.
- insights-base: doc#1 sha=d0626a76 agent=prd-writer type=agent-learned — query: "agent chat daemon telegram asr" — verified: yes — salience: high (OQ-ACD-4 chat.db path ambiguity).

### External contracts

- `teloxide` crate — symbol: long-polling loop, `Dispatcher`, `Bot::new(token)` — source: plan line 149 references teloxide; PRD FR-ACD-6.1 line 479. Exact API surface: verified: no — assumption. Risk: `teloxide` API may differ from what is sketched in the plan (version not pinned in plan). Verification path: Slice 4 implementation reads crates.io docs at pin time.
- `whisper-rs` crate — symbol: whisper.cpp Rust binding, model load, transcription — source: PRD FR-ACD-7.3 (line 493); plan Slice 6 (line 196). Exact API surface: verified: no — assumption. Risk: whisper-rs version may affect available methods. Verification path: Slice 6 pre-review.
- `symphonia` crate — symbol: Ogg/Opus decode → PCM — source: PRD FR-ACD-7.6 (line 496); plan Slice 6 (line 234). Exact API surface: verified: no — assumption.
- `fslock` crate — symbol: exclusive file lock for PID file — source: plan line 154; PRD FR-ACD-9.1 (line 511). Exact API surface: verified: no — assumption.
- `interprocess` crate — symbol: UDS accept loop — source: plan Slice 1 (line 99). Exact API surface: verified: no — assumption. Risk: `interprocess` v1 vs v2 have different APIs.
- MCP JSON-RPC 2.0 wire format — symbol: `initialize`, `tools/list`, `tools/call`, `notifications/claude/channel`, `notifications/tools/list_changed` — source: PRD FR-ACD-3.1 (line 451); plan lines 105–108. Plan risk 2 (line 286) flags `claude/channel/permission` as an Anthropic-internal spec that may drift. Verified: no — assumption. Risk: MCP wire format for `notifications/claude/channel` is internal to Anthropic; the spec may change or restrict plugin usage.
- NVIDIA NIM audio transcription endpoint — symbol: `POST https://integrate.api.nvidia.com/v1/audio/transcriptions`, `Authorization: Bearer <key>`, OpenAI-compatible — source: plan Slice 6 (lines 228–232); PRD FR-ACD-7.5 (line 495). Verified: no — assumption (plan risk 3 explicitly flags endpoint 404 at planning time, line 288). If the actual surface is gRPC-only or has a different path, Slice 6 pivots without affecting whisper backend.
- Telegram Bot API — symbol: long-poll `getUpdates`, `sendMessage`, voice note file download — source: PRD FR-ACD-6.1 (line 479). Verified: no — assumption (standard Telegram Bot API; stable but not re-verified in this session).

### Assumptions

- (OQ-ACD-4 chat.db location — RESOLVED, see ### Verified facts above. No longer an assumption; left here as a deliberate redirect for readers who skim assumptions first.)
- The pairing code window is 1 hour. Source: inferred from plan line 154 `access.json` pending model; PRD §17.8 UI section (line 638). Not explicitly stated in FR-ACD-6.5. — risk: implementation may choose a different window — how to verify: QA planner should add an explicit test case for the expiry window.
- `daemon stop` sends SIGTERM and waits up to 10 seconds before SIGKILL (systemd `TimeoutStopSec=10` in the unit). Plan risk 9 (line 302) specifies systemd unit hardening directives but does not specify `TimeoutStopSec`. — risk: longer-than-expected shutdown if daemon is stuck — how to verify: Slice 2 implementation.
- The `daemon install` idempotency check is based on file-content checksum comparison (not just existence). This is inferred from plan AC 1 (line 66) which says "re-running is no-op". The exact idempotency mechanism is assumed. — risk: if idempotency is existence-only, config changes won't be updated without `uninstall` + `reinstall`. — how to verify: Slice 2 test case.

### Open questions

- **OQ-ACD-4 (chat.db location) — RESOLVED**: architect [STRUCTURAL] #1 pinned `chat.db` at `~/.claude/knowledge/chat.db` (user-level singleton). PRD §17.7 schema comment carries the resolution. Closed; entry retained for audit-trail continuity. — salience: high.
- **OQ-ACD-UC-1 (daemon stop timeout)** — `TimeoutStopSec` value for systemd unit not specified in PRD. Needs: architect or security-auditor decision at Slice 2 — salience: medium.
- **OQ-ACD-UC-2 (chat.db > 1GB growth)** — UC-EC-3 mentions a `claudebase chat purge --older-than 30d` subcommand as a plan item. This subcommand does NOT appear in FR-ACD-13 as implemented. It may be deferred. QA planner should flag if DB size management is in scope for v1. Needs: user decision — salience: medium.
- **OQ-ACD-UC-3 (NIM endpoint reachability check in `daemon doctor`)** — UC-11 step 6 describes an HTTP HEAD probe. The actual NIM endpoint behavior on HEAD requests (vs. POST) is unknown. Needs: verification at Slice 6 implementation time — salience: low.

## Decisions

### Inbound validation

- Task received: "Create docs/use-cases/agent-chat-daemon_use_cases.md". Challenged: yes — verified plan.md and PRD §17 are consistent with each other and with the list of use cases to document. The only upstream signal worth surfacing is OQ-ACD-4 (chat.db path ambiguity from prior session's prd-writer insight). Outcome: proceeded with PRD-specified `<project>/.claude/knowledge/chat.db` path throughout, with explicit assumption labelling and open question. — salience: high.

### Decisions made

- Chose to document UC-1 through UC-11 as 11 primary use cases covering all lifecycle operations (install, start, text message, voice, subagent, pairing, config, stop, uninstall, logs, doctor). Alternative considered: merging UC-8 (stop/restart) with UC-2 (start). Rejected: they are distinct lifecycle operations with distinct error modes; merging would obscure UC-8-E1 (stuck process). Q1: not a hack. Q2: proportional — these are genuinely different flows. Q3: alternatives listed. Q4: cause-level. — salience: medium.
- Chose to use PRD §17 FR field labels (FR-ACD-N) in each UC rather than plan Slice labels alone. Reason: FR references are the stable identity for QA planners; plan slice numbers are implementation-scoped. Q1: not a hack. Q2: sane — matches downstream QA agent's expected input. — salience: medium.
- Chose to annotate OQ-ACD-4 in the `chat.db` path assumption rather than silently picking one path. Reason: the ambiguity is load-bearing — test assertions against DB path will fail if the architect resolves differently. Protocol 1 Q4 (audit trail) requires labelling. — salience: high.

### Hacks / workarounds acknowledged

- (none)

### Symptom-only patches (with root-cause links)

- (none)
