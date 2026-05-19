# Plan: Fix Claude Code 2.1.144 channel-surface — claudebase plugin

## Context

claudebase plugin's daemon + bridge stack is **fully functional** end-to-end:

- ✅ Daemon receives TG messages, persists, broadcasts to subscribers
- ✅ UDS forwarder delivers frame to plugin connection mpsc
- ✅ Plugin writes notification to stdout (verified in `/tmp/claudebase-plugin-trace.log`)
- ✅ Wire shape matches official telegram plugin byte-for-byte (chat_id, message_id, user, user_id, ts ISO 8601)
- ✅ `/Library/Application Support/ClaudeCode/managed-settings.json` contains `allowedChannelPlugins: [{plugin:"claudebase", marketplace:"claudebase-dev"}]`
- ❌ Mira's input never receives `<channel source="..." ...>` callbacks

The **official Anthropic telegram plugin** (`telegram@claude-plugins-official`) DOES surface callbacks correctly in the same Claude Code 2.1.144 session — user confirmed seeing `← telegram · codefather_dev: 123`.

Architectural difference between us and the official plugin:

| Aspect | Official telegram | claudebase |
|---|---|---|
| Process model | ONE bun process serves MCP STDIO | TWO claude-code-spawned processes per session (plugin bridge), TWO UDS connections to daemon |
| MCP impl | `@modelcontextprotocol/sdk` Server class | Hand-rolled bridge + length-prefixed UDS leg |
| State location | TG state inside same process | TG state in separate daemon process |
| Subscription model | Implicit (single process IS the listener) | Explicit `chat_subscribe` tool call, registered per UDS connection |

The critical observation from `/tmp/claudebase-plugin-trace.log` (with pid-prefixed tracing):

- Claude Code 2.1.144 spawns two plugin processes (pids A and B)
- BOTH receive `initialize` + `tools/list` from claude code
- Only ONE process (whichever handled `tools/call chat_subscribe`) is subscribed on the daemon-side
- Daemon broadcasts reach `subscribers=1` and forwarder delivers frame to that ONE process
- Frame is written to stdout of that ONE process
- **But Mira never sees the channel event**

## Root-cause hypotheses (ranked by likelihood)

### H1 — Process-asymmetry: tools-process ≠ channel-listener-process (HIGH)

Claude Code 2.1.144 routes notifications-from-server to ONE specific child process. The other child handles tools/call. Our `chat_subscribe` (a tool call) ran in the tools process; subscription registered on its UDS connection; broadcasts go there. The dedicated **channel-listener process** never invoked chat_subscribe → never subscribed → never received the broadcast → Claude Code's channel surface, watching that listener's stdout, sees nothing.

**Evidence:** Both processes get initialize (verified in trace with pid-prefix), but only ONE process gets tools/call entries.

**Fix:** Plugin must auto-subscribe on initialize so BOTH processes are subscribed to all known TG threads — or daemon must broadcast to ALL connections regardless of explicit subscription (wildcard subscriber).

### H2 — protocolVersion mismatch (MEDIUM)

Claude Code 2.1.144 sends `protocolVersion: "2025-11-25"`; our `initialize_response` pins `"2024-11-05"` (`src/plugin/mcp.rs:33`). Channel-surface features may require a newer version.

**Evidence:** Trace confirms client sends new version; our response is older. Per MCP spec, server CAN respond with its supported version (this is what we do). But claude code MAY downgrade channel-surface features when versions don't match.

**Fix:** Bump SUPPORTED_PROTOCOL_VERSION to `"2025-11-25"` (or whatever 2.1.144 accepts). Test if channel surface fires.

### H3 — Missing client-capability acknowledgement (LOW-MEDIUM)

Claude Code 2.1.144 advertises `capabilities: {roots:{}, elicitation:{}}` in initialize (NEW in this version). Our plugin ignores them — neither acknowledges nor handles them. Some channel-related features may require server to respond to `roots/list` or similar.

**Evidence:** Trace shows client capabilities verbatim. Agent 2's binary analysis found `elicitation` checked in tool-use flow (not channel gate), and `roots` checked separately. So this hypothesis is WEAKER than H1.

**Fix:** Add `roots` capability declaration in initialize_response + handle `roots/list` request. Low priority — try H1 + H2 first.

## Recommended implementation order (user-selected: H2 first)

### Step 1 — Bump SUPPORTED_PROTOCOL_VERSION (H2 — cheapest, single-line fix)

**File: `src/plugin/mcp.rs:33`** — change:
```rust
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2024-11-05";
```
to:
```rust
pub const SUPPORTED_PROTOCOL_VERSION: &str = "2025-11-25";
```

Rebuild + restart daemon + copy binary to `~/.claude/tools/claudebase/claudebase`. Re-test channel surface end-to-end (see Verification section).

**If channel surface fires after this single change → root cause was protocolVersion mismatch.** Commit + push, document the bump in CHANGELOG. **STOP — no further fixes needed.**

**If channel surface still does NOT fire after this change → proceed to Step 2 (H1).**

### Step 2 (FALLBACK if Step 1 doesn't fix) — Verify H1 with PID-correlation diagnostic

After Step 1 testing, regardless of outcome, capture trace logs and check pid correlation:

```bash
# pids that received tools/call chat_subscribe:
grep "STDIN→PLUGIN.*chat_subscribe" /tmp/claudebase-plugin-trace.log | grep -oE 'pid=[0-9]+' | sort -u

# pids that wrote UDS→STDOUT notif:
grep "UDS→STDOUT notif" /tmp/claudebase-plugin-trace.log | grep -oE 'pid=[0-9]+' | sort -u
```

If subscriber pid ≠ notif-recipient pid set → **H1 confirmed**. Proceed to Step 3.

If they match → H1 falsified. Skip to Step 4 (H3).

### Step 3 (if H1 confirmed): Auto-subscribe on plugin initialize

Modify the plugin bridge so EVERY plugin process automatically subscribes to active channels at startup, without waiting for an explicit `chat_subscribe` tool call from Claude Code.

**Two options for auto-subscribe semantics:**

A) **Wildcard subscription on daemon side.** Daemon adds a special `_all_telegram` thread (or `*` pattern). Plugin on `notifications/initialized` calls `chat_subscribe { thread: "*" }`. Daemon's broadcast publishes TG events to BOTH the specific `telegram:<chat_id>` thread AND the wildcard. Both subscribers get notifications.

B) **Auto-discover threads + subscribe to all.** On `notifications/initialized`, plugin reads `chat.db` directly (via daemon `chat_list_threads`), iterates threads with `kind="telegram"`, and subscribes to each. Re-runs on a timer (every 30s) to pick up new threads.

**Option A is simpler** — single subscribe call, daemon handles fan-out. **Option B is more granular** — easier to debug per-thread but more complex state.

**Recommendation: Option A.** Implementation in 2 file edits:

**File: `src/daemon/chat.rs`** — extend `ChatBus`:
- Add `WILDCARD_THREAD: &str = "*"` constant
- In `publish()`, after publishing to specific thread, ALSO publish to "*" thread

**File: `src/plugin/bridge.rs`** — on `notifications/initialized` arrival from claude code:
- Send `tools/call chat_subscribe { thread: "*" }` to daemon (via the same forward_to_daemon path as user-initiated tool calls, but synthesised from plugin)
- The response (backlog) is discarded — we just want the subscription registered

### Step 4 (if H1 + H2 both fail): Test H3 — client capabilities

**File: `src/plugin/mcp.rs:initialize_response()`** — declare:
```rust
"capabilities": {
    "tools": {"listChanged": true},
    "experimental": {
        "claude/channel": {},
        "claude/channel/permission": {}
    },
    // Add these:
    "roots": {"listChanged": false},
    "logging": {}
}
```

And in `bridge.rs` handle `roots/list` (return empty array) when claude code sends it.

## Verification

End-to-end:

1. User restarts claude session with `CLAUDEBASE_PLUGIN_TRACE=1 claude --channels plugin:claudebase@claudebase-dev`
2. Mira: subscribes via "посмотри активные каналы и подпишись" → daemon log shows `chat_subscribe registered`
3. User sends TG msg to @huevyidonbassbot
4. Daemon log shows `telegram broadcast subscribers=2` (BOTH plugin processes subscribed via auto-subscribe)
5. Plugin trace log shows `UDS→STDOUT notif` and `PLUGIN→STDOUT` for BOTH pids
6. **Mira's input receives**: `<channel source="claudebase" chat_id="434566766" user="codefather_dev" ts="..." message_id="...">текст</channel>`
7. Mira can respond via `mcp__claudebase__chat_reply { thread: "telegram:434566766", content: "..." }` and the user sees the reply in TG

If step 6 fails after all 3 hypotheses tested, blocker is internal to Claude Code 2.1.144 and not fixable from our side without Anthropic source-level cooperation.

## Critical files

- `src/plugin/bridge.rs` — STDIO MCP bridge, auto-subscribe injection point
- `src/plugin/mcp.rs` — initialize_response, SUPPORTED_PROTOCOL_VERSION, capabilities
- `src/daemon/chat.rs` — ChatBus.publish, ChatBus.subscribe, wildcard thread support
- `src/daemon/server.rs` — chat_subscribe handler (no changes needed if wildcard handled in chat.rs)
- `.claude-plugin/plugin.json` — manifest (no changes anticipated)
- `.claude-plugin/marketplace.json` — already correct

## Out of scope (future / follow-up)

- inter-Mira-CLI коммуникация через claudebase (user's banked idea)
- ASR backend properly configured (whisper feature compiled)
- Removing dual plugin process — that's Claude Code's behavior, we work around it
