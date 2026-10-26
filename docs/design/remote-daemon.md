# Remote daemon — a session on one machine, its daemon on another

Status: implemented, 2026-10-26 (slices 1-6).

## Goal

A Claude session on the operator's machine joins the daemon of another machine
(prod, dev) as a full member: it is listed with that machine's agents, sends and
receives agent-to-agent messages, is reachable from that daemon's Telegram bot
and callbacks, and reads and writes the project journal there — so the local
session can talk to the agents running on that machine and orchestrate them.

```bash
# on prod, once
claudebase daemon remote enable --bind 0.0.0.0:8586
claudebase daemon remote issue laptop --host prod.example   # prints a connection string

# on the laptop, once
claudebase daemon remote add prod             # paste the connection string (stdin)
claudebase daemon remote default prod              # optional

# then
claudebase run --daemon prod                        # this session lives on prod's daemon
claudebase run                                      # local daemon, as today (or the default profile)
```

## Decisions (operator, 2026-10-26)

- **Transport: TLS + per-client token.** Not an SSH tunnel.
- **Selection: named profiles** (`remote add prod`, `run --daemon prod`, an optional default).
- **Journal on the remote daemon**, the project identified by its git remote rather
  than its path, so `/home/me/proj` on the laptop and `/srv/proj` on prod share one
  journal.

## Why this is more than a URL

Today client code shares the daemon's disk. An inventory (2026-10-26) found 26
direct opens of `chat.db` in client-side code, 14 connection sites, and the
supervisor deciding per inbound message whether it is for this session by reading
`chat.db` itself (`supervisor/mod.rs` `thread_belongs_to`, `routing_belongs_to`,
`subscribe_client.rs` `nick_for`). Against a remote daemon each of those would
silently read the laptop's own `chat.db` — wrong nicks, wrong routing, and
withheld messages. Every such read a session needs moves behind a daemon call.

## Design

### 1. Transport

- The daemon gets an optional second listener: TCP + TLS (rustls, already in the
  tree via reqwest), carrying the existing framing (4-byte length + JSON-RPC) and
  the existing dispatcher. Off unless `daemon remote enable`.
- Certificate: self-signed, generated on enable (`rcgen`), kept with the daemon's
  secrets. The client pins its SHA-256 fingerprint from the connection string —
  no CA, no domain, no HTTPS setup on the server needed (prod has no certificate
  today, 2026-10-26). A forged server fails the pin; plain TCP is never accepted.
- Auth: the first frame is `auth {token}`; nothing else is accepted before it.
  One token per client machine (`issue <name>`), stored hashed, revocable
  (`revoke <name>`), listed with last use (`status`).
- Remote connections may call the session surface only (register, subscribe,
  agent/telegram send, journal, describe, rename, …). Operator administration —
  access lists, pairing, bots, callback tokens, remote tokens — stays local to the
  machine that owns the daemon, as it is today.
- Each remote connection and its client name are logged.

### 2. Client side: one endpoint, chosen per session

- `DaemonEndpoint = Local | Remote { profile }`, resolved by every client
  connection (`DaemonClient`, `SubscribeClient`, the liveness probe) from
  `CLAUDEBASE_DAEMON` (a profile name), then the default profile, then local.
- `claudebase run --daemon <profile>` exports `CLAUDEBASE_DAEMON` to `claude`, so
  every CLI call the agent makes inside that session — `agent send`, `journal add`,
  `telegram send` — goes to the same daemon as the session. A plain terminal uses
  the default.
- Profiles: `~/.claude/claudebase/remotes.toml` (0600): url, pin, token.
  `daemon remote add|list|remove|default|test`.
- `run --daemon` against an unreachable remote fails at start with the reason; it
  does not fall back to the local daemon (a session that silently lands elsewhere
  is exactly the failure this avoids).

### 3. Client reads move behind the daemon

New daemon tools replace the client's direct `chat.db` reads used by sessions and
their CLI calls:

- `agent_resolve` (nick → id), `agent_list`, `agent_whoami`;
- `session_threads` (what to subscribe), `thread_owner` (ownership for inbound
  filtering), sender nicks carried in the delivered frame instead of looked up;
- `telegram_target` (default Telegram thread for `telegram send` / `status`);
- `nick_suggest` (disambiguation and the remembered nick at startup);
- `conversation_nick_get|set` (`chat dump` / `chat load`).

These work the same against the local daemon, so there is one code path, not a
local and a remote one. Operator admin commands (`telegram addbot|pair|allow`,
`daemon callback …`, `daemon config`, service control) remain local-only and
refuse `--daemon`.

### 4. Journal by project identity

- A journal is keyed by the project's normalized git remote (`project_id.rs`,
  already used by the registry) when it has one, else by path as today.
- Sessions send their `project_id` at register (already in the protocol).
- Existing path-keyed journals are adopted: opening by `project_id` falls back to
  the journal of the session's path and records the id in it, so nothing is lost.
- `chat dump/load` keep working; on a shared project the import is mostly "already
  there".

### Notes from implementation

- Rather than one code path for local and remote, each registry question has ONE query
  (`agent_registry`) and two ways to reach it: the local supervisor and CLI open `chat.db` as
  before; a remote session asks the `session_*` tools, which call the same functions. Commands
  that print a whole answer (`agent list`, `whoami`, `telegram status`) run their `*_with(conn)`
  body on the daemon and print its text.
- The supervisor's read loop waited on `read_frame` under a timeout, which drops a partly read
  frame; harmless locally, a corrupted stream over TCP. Frames are now read by their own task.
- Inspection commands that read a local `chat.db` (`chat list|threads`, `agent inspect|list-alive`)
  refuse while a remote daemon is selected rather than show local data under the remote's name.
- `nick` of a loaded conversation (`chat load`) is recorded only on a local daemon.

### 5. What does not travel

- The per-session UNIX callback socket exists on the daemon's machine only; a
  remote session is reachable through the daemon's HTTP callback. The channel
  contract hook says which applies (`whoami` reports the daemon).
- `nick_memory` is keyed by the client's host, as today; the remote daemon sees the
  laptop's hostname.
- Local service control and the ASR model stay per machine.

## Slices

1. **Endpoint plumbing**: `DaemonEndpoint`, profiles file and `daemon remote add|list|remove|default`, `run --daemon` exporting `CLAUDEBASE_DAEMON`; all connection sites resolve through it. Local behaviour unchanged.
2. **TLS listener + tokens**: `daemon remote enable|disable|status`, `token issue|revoke|list`, auth frame, session-surface allow-list, connection log; client TLS connect with pinning; `daemon remote test`.
3. **Client reads behind the daemon**: the tools in §3; supervisor, `agent`, `telegram`, `whoami`, `chat dump/load` switched over; admin commands refuse `--daemon`.
4. **Journal by project identity** with adoption of path-keyed journals.
5. **Hooks and docs**: channel contract (callback socket vs HTTP), journal reminder, README, skills; `whoami` shows the daemon.
6. **End to end**: two daemons on one test host (one as "remote", TLS on loopback) — register, list, a2a both ways, Telegram routing ownership, journal shared by project id, revoked token refused, admin tools refused remotely; Windows client compiles and the installer smoke runs.

## Security notes

- A remote token can type into every session on that daemon, including ones run
  with `--dangerously-skip-permissions`. Treat it like an SSH key to that machine:
  issue per client, revoke on loss, never send it over a channel.
- `remote enable` refuses to start without a certificate and at least one token;
  binding non-loopback prints the exposure in plain words.
- Access requests arriving over a channel stay refused (channel contract).
