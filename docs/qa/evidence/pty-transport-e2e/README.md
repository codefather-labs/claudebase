# Evidence — PTY transport working end-to-end

**Date:** 2026-08-16 · **Claude Code:** 2.1.233 · **claudebase:** working tree (Slices 1–3)
**Verdict: the harness works.** Inbound messages reach a live `claude` session's input, the agent
answers back through the CLI, and delivery survives a daemon restart.

## What was exercised

```
claudebase telegram send  →  daemon (chat_reply → chat.db → bus.publish)
                                 ↓ UDS notification
                          claudebase run (PTY supervisor)
                                 ↓ bracketed paste + delayed CR into the PTY master
                          claude (unmodified, no --channels, no plugin, no MCP)
                                 ↓ agent reads the envelope, runs Bash
                          claudebase telegram send --text "…"  →  daemon  →  chat.db
```

The only link NOT exercised here is daemon ↔ Telegram itself: no bot token is configured on this
machine (`~/.claude/channels/claudebase/` does not exist, no `secrets.toml`). That leg is
pre-existing code the operator has already had working; everything built for this feature sits
downstream of `bus.publish` and is proven below.

## Run 1 — full round trip (`roundtrip-*`)

1. `claudebase run --subscribe telegram:999001` started; log shows identity + subscriptions.
2. `claudebase telegram send --thread telegram:999001 --text "PING-E2E-2 …"` → daemon.
3. Supervisor logged `injected inbound block count=1`.
4. The child rendered the envelope and **acted on the protocol line**, running
   `claudebase telegram send --text "готово"` on its own — visible in the transcript as
   `⎿ $ ./target/release/claudebase telegram send --text "готово"`.
5. `chat_messages` after the run:

```
('cli',                                    'PING-E2E-2 ответь оператору ровно одним …')
('b3e387e2-e25d-4861-821a-60c61ca89cf8',   'готово')
```

The reply row is attributed to the supervisor-generated `agent_id`, which proves
`CLAUDEBASE_AGENT_ID` reached the child's Bash environment. The agent passed **no `--thread`** — the
resolver found the destination by itself.

### F-7 — the echo loop, found and fixed here

The first round trip produced a second `<channel …>` block seconds later containing the agent's own
reply. The daemon broadcasts an outbound `chat_reply` to the same thread the agent just answered on,
so the supervisor was injecting the agent's words back into the agent. Left alone this is a
self-sustaining loop.

Fix: `subscribe_client.rs` skips notifications whose `meta.from_agent` equals this session's
`agent_id`. Filtering is by exact sender, not by "came from an agent" — messages from OTHER agents
are legitimate inbound traffic. After the fix, run 1 shows exactly one injection for one inbound
message.

## Run 2 — daemon restart resilience (`daemon-restart-resilience.log`)

1. message → delivered (`injected inbound block` #1), agent answered `раз`;
2. `claudebase daemon restart`;
3. supervisor logged `daemon connection ended; will reconnect`, then re-subscribed to both threads
   **2.0 s later** — no operator action;
4. second message → delivered (#2), agent answered `два`.

This is the failure mode from `docs/issues/006` (a daemon bounce silently orphaning the
subscription) closed by construction: the supervisor owns the connection and re-registers on every
reconnect.

## Run 3 — draft gate under a REAL tty (`draft-gate-*`)

The supervisor was itself run inside a PTY provided by `spikes/pty_inject` (`--scenario hold`), so
the operator-keystroke path was live. Timeline:

| time | event |
|---|---|
| 12:32:45 | "operator" types a 44-byte line and does NOT submit |
| 12:32:59 | `claudebase telegram send` — message reaches the supervisor |
| 12:33:05 | checked: **0 injections**, message still queued |
| 12:33:10.3 | "operator" presses Enter on their own line |
| 12:33:10.8 | `injected inbound block count=1` — **470 ms after the line cleared** |

The message waited ~11 s and was never lost. Without this gate the earlier spike showed the two
texts silently concatenated into one prompt (finding F-6).

## Run 4 — agent-to-agent over the CLI (`agent-to-agent-*`)

Two supervised sessions (A and B) started in the same repo, both registered with a session token.
Then, with A's identity in the environment (exactly what A's own child sees):

| check | result |
|---|---|
| `--agent claudebase` (both sessions share that name) | **refused**, listing both candidate ids — ambiguity is never resolved silently |
| `--agent-id <B>` with a wrong token | **refused**: `session_token does not match an alive agent` |
| `--agent-id <B>` with the real token | `delivered to 130ab25e-…` |
| B's session | injected the message and **answered back** by running `claudebase agent chat --agent-id <A> --text "принял"` |
| A's session | received B's reply |

So cli-to-cli routing now works with the plugin bridge switched off entirely, which is what allows
Slice 6 to delete it.

The reply first arrived carrying the daemon's raw `{"agent_to_agent":{…}}` preamble in the body.
`subscribe_client.rs` now splits that preamble off and lifts the sender into the envelope, so the
receiving model reads:

```
<channel source="agent" from_agent="35577ca7-…" message_id="ba06327a-…" ts="2026-08-16T12:51:46.565Z">
READABILITY-CHECK ответь одним словом: чисто
</channel>
```

## Automated coverage

- `cargo test --lib` → 210 passed, including 24 new: draft tracking (7), modal detection (6),
  envelope rendering (3), destination resolution (4), preamble splitting (3), token + target
  resolution (6).
- `cargo test --test supervisor_injection_gate_test` → 5 passed: paste framing + separate submit
  (F-1/F-2), hold-while-typing (F-6), hold-while-modal (F-3), burst coalescing, reconnect dedup.

## Still open

- daemon ↔ Telegram with a real bot token (nothing configured on this box today);
- Windows / ConPTY (plan risk R-3);
- the modal gate is proven by tests against recorded modal text, not by a live modal.

## Reproduce

```bash
cargo build --release
./target/release/claudebase run --subscribe telegram:999001 &
./target/release/claudebase telegram send --thread telegram:999001 --text "проверка"
# watch ~/.claude/logs/claudebase-run-<pid>.log for `injected inbound block`
```
