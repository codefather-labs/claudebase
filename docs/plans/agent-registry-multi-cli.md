# Plan: Multi-CLI agent fleet via claudebase server + channel callbacks

**Owner:** Mira (orchestrator, autonomous — no SDLC pipeline)
**Status:** draft — awaiting operator sign-off
**Created:** 2026-05-24
**Builds on:** [`telegram-rust-port.md`](../../../../claude-code-sdlc/docs/plans/telegram-rust-port.md) (channel-callback infrastructure) + existing `claudebase/src/daemon/{agent_registry,chat,channel_state,server}.rs`

## Goal

Promote the working Telegram channel-callback pattern (sender → claudebase
plugin → `<channel source="..." ...>` callback in receiver's input) from a
single-channel proof-of-concept into a **general inter-CLI message bus**:

> The user, via Telegram, talks to a Mira-orchestrator instance. The
> orchestrator queries claudebase server's agent registry, picks a target
> agent (e.g. "architect for project X"), sends them a message via the same
> channel-callback flow. The target agent (a separate `claude` CLI process,
> possibly on a different machine) receives the message as
> `<channel source="claudebase:agent:architect-projectX" ...>`. Processes
> it. Replies via `mcp__claudebase__chat_reply`. The reply flows back to
> the orchestrator as a channel callback. Orchestrator summarises to the
> user via Telegram.

The orchestrator no longer relies on the in-process `Agent` tool for
delegation — those are one-shot ephemeral subagents. **This system is for
persistent, named, long-lived agents that survive across messages and can
be re-addressed**.

## Use cases

1. **Mobile fleet command.** User on the road talks to orchestrator via TG;
   orchestrator delegates to running planner-instance / architect-instance /
   reviewer-instance on the desktop or cluster.
2. **Specialised long-lived agents.** "rust-specialist" Mira-instance with a
   project pinned, a long context history, project-specific scratchpad and
   memory. Orchestrator asks it questions across days/weeks.
3. **Cross-machine swarm.** Orchestrator on laptop; workers on dedicated
   workstations / cluster. Authoritative state lives on the claudebase
   server.
4. **Async hand-offs.** Orchestrator delegates a long task to planner. Goes
   afk. Planner finishes hours later. Orchestrator gets a "done" notification
   via channel callback. Resumes from there.

## Non-goals (out of scope)

- **Replacing CC's in-process `Agent` tool.** `Agent` stays the right
  primitive for one-shot focused subtasks within a single Mira turn. This
  system is the **complement** — independent processes with own identity.
- **Per-agent UI / fleet manager web app.** v1 monitoring is via the
  orchestrator polling registry events. A visual dashboard is a future
  project, not part of this plan.
- **Conflict resolution on shared workspace edits.** Multiple agents
  writing to the same files simultaneously is the user's responsibility
  (file locks via existing `fslock` if needed; not a v1 feature).
- **Auto-spawn heuristics.** Orchestrator never spawns agents on its own
  initiative — every spawn requires explicit user approval via the
  existing permission-request flow (R10 from `telegram-rust-port.md`).
- **Multi-tenant isolation.** v1 single user, single auth token; no role-
  based access between users.

## Architecture overview

```
                          ┌──────────────────────────────────┐
                          │  claudebase server               │
                          │  (Rust binary, HTTP/WSS + UDS)   │
                          │                                  │
                          │  ┌────────────────┐              │
   user ─── TG ──────┐    │  │ agent_registry │              │
                    │    │  │  (per-instance:│              │
                    ▼    │  │  name, role,   │              │
       ┌──────────────────────┐ session_id,   │              │
       │ Mira-orchestrator    │ description,  │              │
       │ (claude CLI in       │ capabilities, │              │
       │  terminal A)         │ last_seen)    │              │
       └──┬───────────────────┘ └────────────┘              │
          │  ▲                  ┌────────────┐              │
          │  │                  │ messages   │              │
          │  │                  │ (history)  │              │
          │  │                  └────────────┘              │
          │  │                  ┌────────────┐              │
          │  │                  │ knowledge  │              │
          │  │                  │ + insights │              │
          │  │                  └────────────┘              │
          │  │                  ┌────────────┐              │
          ▼  │                  │ channel_   │              │
       ┌──────────────────────┐ │ routing    │              │
       │ Mira-planner         │ └────────────┘              │
       │ (claude CLI in       │                              │
       │  terminal B,         │ ── HTTP/WSS ── any machine ─┘
       │  cwd = project Y)    │
       └──────────────────────┘

       ┌──────────────────────┐
       │ Mira-architect       │
       │ (claude CLI on       │
       │  remote box,         │
       │  same auth token)    │
       └──────────────────────┘
```

**Wire-format consistency.** Every agent-to-agent message lands in the
receiver's input as:

```
<channel source="claudebase:agent:<sender-name>" sender_role="<role>"
         sender_session_id="<...>" ts="<ISO 8601>" message_id="<n>"
         thread="agent:<receiver-name>">
<content>
</channel>
```

— byte-equivalent shape to the Telegram channel callback that already
works. The receiver Mira doesn't need new logic to parse it; she sees
exactly the same kind of input she sees from Telegram, only `source=`
is different.

## Phases

### Phase 1 — Server HTTP/WSS surface (foundation)

Currently `claudebase server` (a.k.a `claudebase daemon`) only accepts
UDS connections on the local box. Add network surface + installable
service mode (per D2).

- `claudebase server --serve [--port N] [--use-ssl] [--data-dir DIR]
   [--foreground]` runs the HTTP/WSS listener.
  - `--use-ssl` enables TLS via rustls; self-signed cert auto-generated
    on first run (`--gen-cert` to regenerate).
  - Auth token is auto-generated on first `claudebase server install`
    and printed once (operator stashes via env var or settings.json
    per Q2 below).
- **Auth is mandatory (D1).** Every HTTP/WSS request needs
  `Authorization: Bearer <token>` — rejected with 401 BEFORE any
  handler runs. No opt-out, no permissive default. Also applies to UDS
  path for uniformity (no "trust localhost" shortcut).
- **Installable as OS service (D2).** Extend the existing `claudebase
  daemon {install,uninstall,start,stop,restart,status,logs}` subcommand
  family to wire HTTP/WSS lifecycle:
  - macOS: `launchd` user agent at `~/Library/LaunchAgents/claudebase.plist`
  - Linux: `systemd` user unit at `~/.config/systemd/user/claudebase.service`
  - **Windows: service registered via `sc.exe create` + SCM API** (uses
    the existing `windows-service` crate already in claudebase deps)
  - Foreground mode (`--foreground`) preserved for dev/debug
- Falls back to UDS for local plugin bridges (existing path).

**Done when:**
1. `claudebase server install` → service registered with OS; first-run
   token printed to stdout
2. `claudebase server start` → service listening on configured port
3. `curl -k -H "Authorization: Bearer <token>" https://localhost:8443/health`
   → 200
4. Same `curl` WITHOUT auth header → 401 (mandatory enforcement)
5. UDS path still works for the existing telegram-plugin-rs bridge
6. `claudebase server status` → "running, PID X, listening on Y" output
7. Windows: service appears in `services.msc` and survives reboot

### Phase 2 — Agent auto-registration on session start

When a `claude` CLI starts AND has `claudebase.serverUrl` + auth token in
`~/.claude/settings.json`, the claudebase plugin auto-registers the
session in the server's `agent_registry`.

- New fields in agent row: `name`, `role`, `description`, `capabilities[]`,
  `cwd`, `host`, `pid`, `started_at`, `last_seen_at`
- Settings.json schema:
  ```json
  {
    "claudebase": {
      "serverUrl": "https://localhost:8443",
      "authToken": "...",
      "agent": {
        "name": "orchestrator",
        "role": "orchestrator",
        "description": "Top-level Mira for fleet command via TG",
        "capabilities": ["delegation", "telegram-bridge"]
      }
    }
  }
  ```
- Plugin sends `POST /agents/register` on startup; deregister on EOF.
- Periodic `last_seen_at` ping every 30s; server reaps on >5min silence
  (uses existing `reap` infra).

**Done when:** start CC with above config → `claudebase agent list` shows
the entry; exit CC → entry removed (or expires).

### Phase 3 — Send/receive between registered agents

The chat bus that powers `telegram-plugin-rs` already exists in
`claudebase/src/daemon/chat.rs`. Extend with `thread = "agent:<name>"`
addressing:

- New MCP tool `mcp__claudebase__agent_message`:
  - `--to <agent-name>` (required, must exist in registry)
  - `--content <text>` (required)
  - `--reply_to <message_id>` (optional, threading)
  - `--ttl_seconds <n>` (optional; default 3600 — undelivered messages
    expire so a long-dead agent doesn't accumulate junk)
- Server validates sender is registered, target exists, both within same
  auth-token scope; persists in `messages` table; routes via existing
  chat_subscribe to recipient's plugin connection → recipient's stdin
  → recipient's input as `<channel source="claudebase:agent:..." ...>`.
- Existing `mcp__claudebase__chat_reply` reused for replies.

**Done when:** two `claude` instances paired with the same server; one
sends `agent_message --to other`; other sees it in input; replies; first
sees reply.

### Phase 4 — Agent discovery + addressing helpers

- `mcp__claudebase__agent_list_alive` (read-only, no args): returns all
  alive agents with `{name, role, description, capabilities, cwd, host,
  last_seen_at}`.
- `mcp__claudebase__agent_query` `--role X` / `--capability Y` /
  `--name-pattern Z`: filtered search.
- Heuristic dispatch is the orchestrator's job; tool surface is
  intentionally minimal-and-flexible. **User's job to spell out which
  agent to delegate to**; orchestrator picks if user said something
  fuzzy ("ask the architect for project X").

**Done when:** orchestrator can answer "list current agents", "find the
planner working on project X" from registry queries alone.

### Phase 5 — Permission-gated spawn

New MCP tool `mcp__claudebase__agent_spawn`:

- Required args: `--name`, `--role`, `--cwd <absolute-path>`
- Optional: `--description`, `--capabilities`, `--launcher` (`claude` |
  `claudebase run` — default the latter), `--prompt-file` (initial
  instruction for the spawned agent), `--terminal` (open in new terminal
  window via `osascript -e 'tell application "Terminal"...'` on macOS;
  `gnome-terminal` / `wt.exe` on linux/windows)
- **Permission gate (mandatory)**: every call goes through the
  notifications/claude/channel/permission_request flow. User sees in TG:
  ```
  🔐 Permission: agent_spawn
  name: planner-projectX
  cwd: /Users/...
  launcher: claudebase run
  [✅ Allow] [❌ Deny] [See more]
  ```
- On Allow: server forks a process that opens the terminal in `cwd` and
  execs the launcher. New CC instance auto-registers via Phase 2.
- On Deny: tool returns error; orchestrator must explain to user why
  spawn was rejected.

**Done when:** orchestrator calls `agent_spawn` → permission prompt in
TG → user taps Allow → new terminal opens on user's desktop → new agent
appears in registry within 30s.

### Phase 6 — Status hooks + monitoring (orchestrator-side)

Two complementary signals:

**Push (agent → orchestrator):** Each Mira-agent on every turn-completion
fires a hook (new SDLC `Stop` or `PostMessage` hook) that emits a
status notification:
```json
{
  "method": "notifications/claude/channel/agent_status",
  "params": {
    "agent_name": "planner-projectX",
    "event": "turn-complete" | "blocked" | "idle" | "context-near-limit",
    "summary": "<one-line description>",
    "ts": "<ISO 8601>"
  }
}
```
Orchestrator subscribes; receives events as channel callbacks; reacts.

**Pull (orchestrator → registry):** `mcp__claudebase__agent_status
--name X` returns last 10 events + current state. Orchestrator polls
when push events are insufficient (e.g. agent crashed and never sent a
"blocked" event).

**Done when:** orchestrator delegates to planner; planner finishes 3
turns later; orchestrator receives "turn-complete" events; orchestrator
summarises to user via TG.

### Phase 7 — Knowledge + insights replication via server

Currently `~/.claude/knowledge/{index,insights}.db` are per-project file
DBs on local FS. For multi-agent fleet on different machines, this
breaks (agents can't see each other's insights). Server hosts a single
authoritative store:

- `claudebase agent insight create` routes through server (cross-machine)
  when `claudebase.serverUrl` is set; falls back to local file when not.
- Same for `agent insight search`, `agent knowledge ingest`,
  `agent knowledge search`.
- Server enforces same dedup / salience semantics as local.
- Per-project scoping preserved via `--project <slug>` arg routed to
  server.

**Done when:** agent A on host A ingests a doc; agent B on host B can
`claudebase search` and find chunks from agent A's ingest.

### Phase 8 — Cross-machine smoke test + parity verification

End-to-end test of the full envelope:

1. Spin up claudebase server on host A (with self-signed cert + token)
2. Start orchestrator on host A (Mira #1) — auto-registers with role
   `orchestrator`
3. Start architect on host B (Mira #2) — auto-registers with role
   `architect` for project Y
4. Both visible in `claudebase agent list_alive --json`
5. User sends TG msg to orchestrator: "spроси у architect какая
   ситуация с сетевым контуром project Y"
6. Orchestrator does `agent_query --role architect` → finds architect
   instance → calls `agent_message --to architect-projectY --content "..."`
7. Architect's CC instance shows the message as channel callback in input
8. Architect investigates, replies via `chat_reply --to orchestrator
   --content "..."`
9. Orchestrator receives reply, summarises to user via TG
10. **Done when:** the full 9-step flow works end-to-end with logged
    evidence at each hop.

## Acceptance per phase (compact)

| # | Phase | Done when |
|---|---|---|
| 1 | Server HTTPS surface | `curl -H "Authorization: Bearer X" https://localhost:8443/health` → 200 |
| 2 | Auto-registration | start CC with config → `claudebase agent list_alive` shows entry; exit → removed |
| 3 | Inter-agent message | A sends, B sees `<channel source="claudebase:agent:A">` in input, replies, A sees reply |
| 4 | Discovery | `agent_query --role architect` returns alive instances |
| 5 | Permission-gated spawn | orchestrator → spawn → TG permission prompt → Allow → new terminal opens → new agent registers |
| 6 | Status hooks | orchestrator subscribes; delegated agent emits turn-complete events; orchestrator reacts |
| 7 | Knowledge/insights replication | cross-machine ingest by agent A; search by agent B finds it |
| 8 | E2E parity | full 9-step flow with logged evidence at each hop |

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Server compromised → all agents exposed | Server requires auth token on every request; TLS for transport; agents don't trust server-side state without sig (deferred to v2 — v1 trusts the server) |
| Auth token leak | Token rotation: `claudebase server rotate-token` + agents re-auth on next ping; document operator hygiene (don't commit, env-only) |
| Agent crashes mid-conversation | Server times out at TTL; orchestrator's poll catches "agent missing" state; auto-respawn is OPT-IN per agent (default: human notified, no auto-action) |
| Runaway spawn cost | Mandatory permission gate (Phase 5) routes every spawn through user-approval flow; no programmatic budget cap in v1 (operator's wallet) |
| `Agent` tool vs `agent_spawn` confusion | Document explicitly in subagent-onboarding rule + tool descriptions: `Agent` for one-shot in-process; `agent_spawn` for persistent independent processes |
| Network partition between agents | Messages have TTL (default 1h); undelivered ones expire; reconnecting agent doesn't replay old messages (operator decides whether to re-trigger task) |
| Knowledge replication conflicts | Server is authoritative; conflicting concurrent writes resolved by timestamp + sha (existing `claudebase insight create` dedup logic extended) |
| Auto-registration leaks credentials | `claudebase.authToken` lives in `~/.claude/settings.json` (chmod 0600); never committed; auto-registration uses it but never echoes back |
| `cwd` argument to spawn = path-traversal vector | Server validates spawn cwd is within an allowlisted directory tree (configured per server: `--spawn-allowed-roots /home/user/projects`); rejects others with clear error |

## Operator-decided (locked-in)

These were settled by operator brief (2026-05-24) — no further debate.

| # | Question | Decision |
|---|---|---|
| **D1** | **Auth required?** | **YES — MANDATORY.** Bearer-token auth on every HTTP/WSS request. NO opt-out flag, NO permissive default. A request without `Authorization: Bearer <token>` is rejected with 401 BEFORE any handler runs. UDS path (local) inherits same enforcement model (token in `~/.claude/settings.json` chmod 0600) — uniform, no "trust localhost" shortcut. |
| **D2** | **Server packaging?** | **Installable daemon / service.** Background-process forever; managed by the OS init system: `launchd` on macOS, `systemd` on linux, **Windows service** (sc.exe / SCM API) on Windows. Foreground mode (`claudebase server --serve --foreground`) preserved for dev/debug. The existing `claudebase daemon install/uninstall/start/stop/restart/status/logs` subcommand surface is the install entrypoint — extended with HTTP/WSS lifecycle. |

## Open questions (still need answer before Phase 1)

1. ~~Server lifecycle / install.~~ — **RESOLVED (D2 above)**.
2. **Auth token storage.** ~~Optional~~ — auth is mandatory per D1. The
   remaining sub-question: env var primary (`CLAUDEBASE_AUTH_TOKEN`) or
   `~/.claude/settings.json` field primary? Leaning env-var-primary
   (matches 12-factor) with settings.json as ergonomic fallback for
   non-headless dev. Server itself generates the token on first
   `claudebase server install` and prints it once — operator stashes it
   wherever.
3. **Wire format for inter-agent vs TG.** Should
   `source="claudebase:agent:X"` use the same `meta` fields as TG
   (chat_id, user, etc) or a slimmer agent-specific schema? Leaning
   slimmer + agent-specific (`{name, role, capabilities, host}`) to
   avoid faking TG fields.
4. **Spawning windows vs background processes.** macOS: `osascript` for
   Terminal. Linux: which terminal emulator (gnome-terminal? xterm?
   tmux session?). Windows: `wt.exe`. Or just spawn as background process
   and log to file (no terminal window)? Leaning hybrid: `--terminal`
   flag chooses; default = background + log file.
5. **Knowledge replication for `index.db` (books) vs `insights.db`.** Books
   corpus is often gigabytes (PDFs); replicating fully across nodes is
   expensive. Maybe books stay local (per-machine), insights replicate
   (small, agent-emitted). Leaning: insights replicate, books local-only
   in v1.

## Files (planned changes)

```
claudebase/
├── src/
│   ├── daemon/
│   │   ├── server.rs              ← extend with HTTP/WSS listener (Phase 1)
│   │   ├── agent_registry.rs      ← add name/role/desc/caps fields (Phase 2)
│   │   ├── chat.rs                ← extend thread="agent:X" routing (Phase 3)
│   │   ├── messages.rs            ← NEW — persistent messages table (Phase 3)
│   │   ├── spawn.rs               ← NEW — process spawn helper (Phase 5)
│   │   └── http_api.rs            ← NEW — HTTP route handlers (Phase 1)
│   ├── plugin/
│   │   ├── bridge.rs              ← register on init, ping every 30s (Phase 2)
│   │   ├── mcp.rs                 ← register agent_* tools (Phases 3-5)
│   │   └── server.rs              ← handle inter-agent channel notif (Phase 3)
│   └── cli.rs                     ← `claudebase agent {list,query,message,spawn,status}` subcommands
├── plugins/
│   └── telegram-rs/               ← unchanged
├── docs/plans/
│   └── agent-registry-multi-cli.md  ← this file
└── tests/
    ├── server_http_test.rs        ← Phase 1
    ├── auto_register_test.rs      ← Phase 2
    ├── agent_message_e2e_test.rs  ← Phase 3
    └── ...

claude-code-sdlc/
├── src/
│   ├── hooks/
│   │   └── sdlc-turn-complete.sh  ← NEW — Stop hook emits agent_status event (Phase 6)
│   ├── rules/
│   │   └── subagent-onboarding.md ← document Agent-tool vs agent_spawn distinction
│   └── claude.md                  ← document the new pattern in pipeline section
└── install.sh / install.ps1       ← deploy the new hook
```

## Effort estimate (rough, operator-aware)

Per-phase, single-developer-week equivalent (Mira working autonomously
with operator approval per slice):

| Phase | Estimate |
|---|---|
| 1 — Server HTTP/WSS | 2-3 days |
| 2 — Auto-registration | 1 day |
| 3 — Agent message send/receive | 2-3 days (most of the wire-format work) |
| 4 — Discovery | 0.5 day (mostly registry-query CLI surface) |
| 5 — Permission-gated spawn | 1-2 days |
| 6 — Status hooks + monitoring | 1-2 days |
| 7 — Knowledge replication | 3-5 days (this is hairy — schema, conflict, multi-machine consistency) |
| 8 — Cross-machine smoke test | 1 day |

**Total realistic: 12-18 working days.** Aggressive parallelisation would
need 2 hands. With one operator + one Mira, 3 weeks of focused work is
the floor.

## Recommended phasing pause-points

Operator-decision gates between phases (each requires explicit go-ahead):
- **After Phase 1**: server HTTPS works. Commit. Live with it for a few
  days. Iterate on edge cases (auth token rotation, error UX) before
  building features on top.
- **After Phase 3**: end-to-end inter-agent message works. **This is the
  smallest viable MVP**. Could stop here. Spawn, monitoring, knowledge
  replication = nice-to-have but not minimum.
- **After Phase 5**: spawn works with permission gating. Now full multi-
  agent UX is real. Could stop here for a long while.
- **Phase 6-7**: polish. Wait until phases 1-5 have lived in operator
  hands for at least a week of real use.

## Facts

### Verified facts

- `claudebase/src/daemon/agent_registry.rs` already provides `register`,
  `unregister`, `list_alive`, `reap`, `AgentRow`, `validate_agent_name`
  — verified by direct grep this session. Salience: high (saves Phase 2
  authoring time).
- `claudebase/src/daemon/chat.rs` already implements a broadcast bus
  (`ChatBus.publish` + `subscribe`) used by the telegram-rs plugin —
  verified earlier this session during R10 implementation. Salience: high.
- `claudebase/src/daemon/server.rs` provides UDS-only IPC currently —
  verified by `ls daemon/` showing `ipc.rs` + `server.rs` + no `http_api.rs`.
  Adding HTTP/WSS is greenfield. Salience: high.
- Permission-request flow with inline TG keyboard works end-to-end (R10
  of telegram-rust-port) — operator confirmed "пермишены приходят в тг
  так что работает как в официальном плагине" this session. Reusable for
  Phase 5 agent_spawn gate. Salience: high.
- Wire format `<channel source="..." chat_id="..." user="..." ts="..."
  message_id="...">` is already parsed natively by Mira (Claude Code's
  channel surface) — verified by every TG callback this session. Reusing
  the same wire format for agent-to-agent (just with
  `source="claudebase:agent:X"`) means Mira needs ZERO new logic to
  receive them. Salience: high.

### External contracts

- Claude Code hooks API — symbol: SessionStart, SubagentStart, Stop,
  PostToolUse — source: https://code.claude.com/docs/en/hooks (fetched
  this session for the systemMessage fix). Stop hook (or PostToolUse on
  Reply) is needed for Phase 6 turn-complete event. Salience: high.
- Claude Code MCP plugin contract — symbol: `notifications/claude/channel`
  custom notification method, custom-method-allowed via plain JSON-RPC
  notification — verified by working telegram-plugin-rs. Salience: high.
- `tokio` v1, `serde_json` v1, `rustls` (via reqwest blocking) — all
  already in claudebase Cargo.toml. Salience: low.
- `osascript` for Terminal.app spawn on macOS — symbol: `tell application
  "Terminal" to do script "<cmd>"`. Standard. Source: Apple AppleScript
  docs. Salience: medium for Phase 5.

### Assumptions

- Server hosting one auth token = single-user scope is acceptable for v1.
  How to verify: revisit if multi-user demand emerges. Salience: medium.
- The OPERATOR controls Anthropic API budget via subscription/key — no
  programmatic cost cap needed in v1. How to verify: operator explicitly
  agreed during this brainstorm ("вопрос средств это вопрос подписки и
  вопрос человека"). Salience: high.
- TLS via rustls (self-signed cert in dev) is enough for v1; production
  CA-signed certs are operator's deployment concern. Salience: medium.
- Linux terminal-emulator choice for spawn (gnome-terminal vs xterm vs
  tmux) is per-operator preference; configure via env or settings.json.
  How to verify: document and let operator pick. Salience: low.

### Open questions

- Should `claudebase server --serve` re-use the existing UDS daemon
  process (extending it) or run as a separate process with its own
  schema? — needs: architect call after Phase 1 scoping. Salience: high.
- For Phase 7 knowledge replication: do we re-purpose the existing
  per-project file DBs as caches of server-authoritative data, or do
  agents query server every time? Caching is faster but invalidation is
  hard. Server-every-time is simpler but slow. Defer. Salience: medium.
- For Phase 5 spawn: should the new terminal inherit the operator's
  shell env (PATH, env vars) or use a sanitised env? Inherit = matches
  user expectation; sanitise = more secure if spawn is exposed to
  untrusted input (which it isn't in v1 since user approves every
  spawn). Defer. Salience: medium.

## Decisions

### Inbound validation

- Operator brief was clear and self-consistent across two rounds of
  discussion. Push-back I raised about cross-machine complexity was
  ADDRESSED by operator's clarification that server-hosted storage solves
  the state-sync problem. Outcome: proceed with the broader cross-machine
  scope from v1 (no longer same-machine-only). Salience: high.

### Decisions made

- **D1 — MANDATORY auth (operator brief 2026-05-24).** Bearer-token
  enforcement on EVERY HTTP/WSS/UDS request. No opt-out, no permissive
  default, no "trust localhost" shortcut. 401 BEFORE handler runs. Q1-Q5:
  not a hack ✓ / proportionate ✓ / alternatives evaluated ✓ (insecure-by-
  default was rejected) / addresses root cause ✓ / n/a. Salience: high.
- **D2 — Installable OS service (operator brief 2026-05-24).** Server
  runs as `launchd` agent (macOS) / `systemd` user unit (linux) /
  Windows service (sc.exe + SCM API via existing `windows-service` crate).
  Foreground mode (`--foreground`) preserved for dev. Q1-Q5: not a hack
  ✓ / proportionate ✓ / alternative (foreground-only) rejected as
  operator-unfriendly / addresses root cause (persistence across
  reboot) ✓ / n/a. Salience: high.
- **Decision:** Build on existing `agent_registry.rs` + `chat.rs` rather
  than parallel-track new infra. Alternatives rejected: building a
  separate "mesh" service alongside existing daemon (would duplicate
  channel routing); refactoring the daemon (out of scope risk). Q1-Q5:
  not a hack ✓ / proportionate ✓ / alternatives evaluated ✓ / addresses
  root cause ✓ / n/a. Salience: high.
- **Decision:** Reuse the EXACT wire format of TG channel callbacks for
  inter-agent messages (just change `source=`). Alternative rejected:
  fancier RPC-style request/response with explicit `correlation_id`. The
  TG format is dirt-simple, already parsed natively by Mira, no new
  envelope needed. Trade-off: less type safety on the wire vs zero new
  parser code. Salience: high.
- **Decision:** Mandatory permission gate on `agent_spawn`. No
  programmatic budget. Cost is on operator's wallet, but every spawn is
  consciously approved. Alternative rejected: cost-cap per session
  (too operator-intrusive for v1). Salience: high.
- **Decision:** Knowledge replication (Phase 7) is the last hard phase
  and the most defer-able. MVP is Phases 1-5; Phases 6-7 are polish.
  Salience: medium.
- **Decision:** This plan supersedes any future "self-hosted marketplace"
  thinking that came up earlier. The agent-channel system IS the
  marketplace — each agent advertises its capabilities, others discover
  + message. Salience: medium.

### Hacks acknowledged

- v1 uses self-signed certs and operator-managed token rotation. Removal
  path: production deployments should use CA-signed certs + a token-
  rotation cron (deferred to operator deployment story).
- v1 has no cost cap. Removal path: add `--max-agents-spawned-per-hour
  N` in Phase 9 if it becomes a real problem.

### Symptom-only patches

(none) — this plan addresses the root design ("how do multiple Mira
instances coordinate") with new infrastructure, not a workaround layered
on top of `Agent` tool.
